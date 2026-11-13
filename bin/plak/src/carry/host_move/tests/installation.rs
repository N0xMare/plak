use super::*;
use crate::carry::upgrade::receipt::{Group, GroupPhase, Mapping, Phase, Receipt};
use crate::home::paths::PlakPaths;

#[test]
fn move_passive_recovery_uses_the_frozen_stage_and_abandons_only_before_ready() -> TestResult {
    let root = PathFixture(tempfile::tempdir()?);
    let executor = plakous::application::ApplicationExecutor::new()?;
    let mut capsule = executor.block_on(source(&root))??;
    capsule.source_key = "r1.e1.p1.t1".into();
    let paths = PlakPaths::explicit(root.0.path().join("destination"));
    paths.ensure_layout()?;
    let services = crate::shell::services::Services::for_home(paths.clone());
    let services = crate::commands::runner::services::host::custody(&services)?;
    capsule.source_host = crate::commands::runner::host_id(&paths)?;
    let native = executor.block_on(reconstructed(&root, &capsule))??;
    let (record, _, _) = capsule.products[0].clone().into_cut()?.into_parts();
    let bytes = serde_json::to_vec(&capsule)?;
    let mut receipt = Receipt {
        version: crate::carry::upgrade::receipt::VERSION,
        id: uuid::Uuid::new_v4(),
        sequence: 1,
        source_key: capsule.source_key.clone(),
        source_instance: capsule.source_instance.clone(),
        source_binary: capsule.source_binary.clone(),
        source_executable: std::path::PathBuf::from("/fixture/source/plak"),
        host_argv: vec!["host".into()],
        destination_key: crate::home::state::state_key(),
        destination_binary: crate::build_identity::binary_id().into(),
        destination: std::env::current_exe()?,
        phase: Phase::Committed,
        source_archival_pending: false,
        mappings: vec![Mapping {
            ready: false,
            abandoned: false,
            runner: "local".into(),
            source: record.id.clone(),
            source_node: plakous_host_protocol::wire::NodeAddress {
                tree_id: record.node.tree_id.clone(),
                agent_id: record.node.agent_id.clone(),
            },
            successor: record.id.clone(),
            digest: plakous::model::sha256_hex(&[&bytes]),
        }],
        groups: vec![Group {
            id: capsule.group.clone(),
            operation: capsule.operation,
            trees: [record.node.tree_id.clone()].into(),
            products: [record.id.clone()].into(),
            sessions: capsule
                .sessions
                .iter()
                .map(|session| session.id.clone())
                .collect(),
            lines: capsule.lines.iter().map(|line| line.id.clone()).collect(),
            runners: ["local".to_owned()].into(),
            source_digest: Some("a".repeat(64)),
            source_retired: true,
            target_digest: Some(plakous::model::hex_lower(native.digest().bytes())),
            phase: GroupPhase::Pending,
            transfers: BTreeMap::new(),
            abandon_error: None,
            abandon_retry_after: None,
        }],
    };
    receipt.save(&paths)?;
    std::fs::write(
        receipt
            .path(&paths)
            .with_file_name(format!("{}.json", capsule.group)),
        bytes,
    )?;
    let sessions = services.state_roots()?.runtime_sessions()?;
    assert_retirement_required(&services, &mut receipt);
    let operation = MoveOperation::new(capsule.operation)?;
    executor.block_on(sessions.stage_move(operation, native.clone()))??;
    // This is the crash window: native commit succeeded, its receipt acknowledgement did not.
    assert!(
        executor
            .block_on(sessions.line_head(native.lines()[0].line.line()))?
            .is_err()
    );
    for _ in 0..2 {
        let _lock = crate::carry::upgrade::receipt::publication::lock(&paths)?;
        super::super::installation::install(&services, &mut receipt, 0)?;
        assert_eq!(receipt.groups[0].phase, GroupPhase::Published);
        assert_eq!(
            executor
                .block_on(sessions.recover_move(operation))??
                .digest(),
            native.digest()
        );
        let reopened = services.session_records()?.load(&record.id)?;
        assert_eq!(reopened.id, record.id);
        assert_eq!(reopened.node, record.node);
        assert_ne!(reopened.plakous_locator, record.plakous_locator);
        assert!(crate::carry::upgrade::receipt::ensure_open(&paths, &reopened).is_err());
    }
    abandon_twice(&services, &mut receipt)?;
    assert_eq!(receipt.phase, Phase::Acknowledged);
    assert!(
        executor
            .block_on(sessions.line_head(native.lines()[0].line.line()))?
            .is_err()
    );
    assert!(crate::carry::upgrade::receipt::ensure_open(&paths, &record).is_err());
    Ok(())
}

fn assert_retirement_required(services: &crate::shell::services::Services, receipt: &mut Receipt) {
    receipt.groups[0].source_retired = false;
    let refused = super::super::installation::install(services, receipt, 0).unwrap_err();
    assert!(
        refused.to_string().contains("native retirement"),
        "{refused}"
    );
    receipt.groups[0].source_retired = true;
}

fn abandon_twice(services: &crate::shell::services::Services, receipt: &mut Receipt) -> TestResult {
    let _lock = crate::carry::upgrade::receipt::publication::lock(services.paths()?)?;
    disposition::abandon(services, receipt, 0)?;
    disposition::abandon(services, receipt, 0)?;
    Ok(())
}

async fn reconstructed(
    root: &PathFixture,
    capsule: &Capsule,
) -> TestResult<plakous::sessions::host_move::MoveInstall> {
    let copy: Capsule = serde_json::from_value(serde_json::to_value(capsule)?)?;
    let mut sessions = Vec::new();
    for session in copy.sessions {
        let header = session.header()?;
        let compiler = AgentSessions::move_preflight_compiler(session.durability.decode())?;
        let mut branches = Vec::new();
        let mut checkpoints = Vec::new();
        for head in session.heads {
            let controller = BoundaryController::quiet_from_evidence_path(
                CrashBoundary::None,
                root.0.path().join("reconstruction-evidence.jsonl"),
            )?;
            let runtime = plakous_offline_crash_harness::text_runtime_binding(
                root.0.path(),
                controller,
                "not dispatched",
                "not dispatched",
            )?;
            let rebuilt = compiler
                .reconstruct_move_head(
                    runtime,
                    head.into_request(MoveOperation::new(copy.operation)?)?,
                )
                .await?;
            branches.push(rebuilt.branch);
            checkpoints.push(rebuilt.checkpoint);
        }
        sessions.push(plakous::sessions::host_move::MoveSession {
            session: header,
            feed_tail_floor: session.feed_floor,
            branches,
            checkpoints,
        });
    }
    let lines = copy
        .lines
        .into_iter()
        .map(Line::decode)
        .collect::<Result<_, _>>()?;
    Ok(plakous::sessions::host_move::MoveInstall::new(
        sessions, lines,
    )?)
}
