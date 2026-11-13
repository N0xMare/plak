//! The producer reads only its own native state; target compilation has no driver.
use super::*;
use plakous::sessions::feed::{FeedLocator, FeedSeq};
use plakous::sessions::host_move::{MoveDisposition, MoveHeadFacts};
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionLocator,
    SessionOperationId,
};
use plakous_hosted::sessions::projector::{LineageStep, ProductIntents, SessionViews};
use plakous_hosted::sessions::{
    SessionRecord,
    host_move::{DormantSession, MoveIdentityClaim, MoveIdentityKind},
};
use plakous_offline_crash_harness::BoundaryController;
use plakous_offline_crash_harness::protocol::CrashBoundary;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

async fn source(root: &PathFixture) -> TestResult<Capsule> {
    source_with_fidelity(root, false).await
}

async fn source_with_fidelity(root: &PathFixture, rich: bool) -> TestResult<Capsule> {
    let runtime = text_binding(root, "evidence.jsonl")?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(
        root.0.path().join("source.sqlite3"),
    )?)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("move:source")?,
            runtime,
        ))
        .await?
    else {
        return Err("source did not create".into());
    };
    let locator = session.release().await?;
    let hold = sessions
        .hold_branch(&SessionOperationId::new("move:hold")?, locator)
        .await?;
    let operation = MoveOperation::new([13; 32])?;
    let snapshot = sessions.prepare_move(operation, &[&hold]).await?;
    let digest = snapshot.digest();
    let snapshot = if rich {
        let mut native = snapshot.sessions().to_vec();
        native[0].checkpoints[0] = plakous_offline_crash_harness::move_fixture::rich_checkpoint(
            &native[0].checkpoints[0],
        )?;
        plakous::sessions::host_move::MoveInstall::new(native, snapshot.lines().to_vec())?
    } else {
        snapshot
    };
    let native = &snapshot.sessions()[0];
    let branch = &native.branches[0];
    let locator = SessionLocator::new(locator.session_id(), locator.branch_id(), branch.checkpoint);
    let mut record = SessionRecord::new(root.0.path().into());
    record.plakous_locator = Some(locator);
    let mut views = SessionViews::empty();
    views.cursor = FeedSeq::new(native.feed_tail_floor);
    views.state.runtime_session_id = Some(locator.session_id());
    views.state.head = Some(FeedLocator {
        session_id: locator.session_id(),
        branch_id: locator.branch_id(),
        checkpoint_id: locator.checkpoint_id(),
    });
    views.state.lineage.push(LineageStep {
        branch_id: locator.branch_id(),
        from_seq: 1,
    });
    views.state.released = true;
    let claim = MoveIdentityClaim {
        identity: record.create_operation_id.clone(),
        kind: MoveIdentityKind::Create,
        owner: record.id.clone(),
        binding: record.id.clone(),
        disposition: None,
        outcome: None,
    };
    let context = HeadContext {
        product: record.id.clone(),
        tree: record.node.tree_id.clone(),
        agent: record.node.agent_id.clone(),
        workspace: root.0.path().into(),
        runner: "local".into(),
        plan_authority: record.id.clone(),
        configuration: configuration::Configuration::capture(&record)?,
    };
    let cut = DormantSession::prepare(
        record,
        views,
        ProductIntents::default(),
        FeedSeq::new(native.feed_tail_floor),
    )?
    .with_identity_claims(vec![claim])?;
    let trees = if rich {
        vec![rich_tree(&context, native.feed_tail_floor)?]
    } else {
        vec![]
    };
    let capsule = producer::SourceCut {
        operation: [13; 32],
        key: "r28.e38.p38.t11".into(),
        binary: "111111111111".into(),
        host: "source-home".into(),
        instance: "1-0000000000000001".into(),
        products: vec![cut.document()?],
        trees,
        contexts: BTreeMap::from([((locator.session_id(), locator.branch_id()), context)]),
        owners: BTreeMap::new(),
    }
    .capsule(&snapshot)?;
    sessions
        .finish_move(operation, digest, MoveDisposition::Abort)
        .await?;
    hold.release().await?;
    Ok(capsule)
}

fn rich_tree(context: &HeadContext, floor: u64) -> TestResult<Tree> {
    use plakous_recursive_subagents::tree::ProductAgentTree;
    let tree = ProductAgentTree::bootstrap_root_for_test(
        &context.tree,
        &context.agent,
        "owner",
        context.workspace.clone(),
        "selected-tuple",
        plakous_recursive_subagents::protocol::WriterMode::SingleLease,
    )
    .map_err(|error| error.message)?;
    let mut value = serde_json::to_value(DormantTree::capture(&tree)?)?;
    value["forked_from"] = serde_json::json!({"import": {"provenance": "retained tree source"}});
    value["messages"] = serde_json::json!({"mail": {
        "message_id": "mail", "from": context.agent, "to": context.agent, "body": "retained mail",
        "purpose": "coordinate", "priority": "deferred", "in_reply_to": null,
        "disposition": "queued", "durable": "accepted"
    }});
    value["decisions"] = serde_json::json!([{
        "operation_id": "retained-decision", "agent_id": context.agent,
        "fold": {"seq": floor, "order": 0}, "effects_reserved": true,
        "body": {"kind": "operator", "value": {"attempt_id": null, "decision": {"Interrupt": {"effective": false, "cause": null}}}}
    }]);
    let seed: DormantTree = serde_json::from_value(value)?;
    let mut source = ProductAgentTree::reopen(seed.installed_rows()?);
    let _ = source.take_effects();
    Ok(Tree {
        cut: DormantTree::capture(&source)?,
        custody: Custody {
            fence: 2,
            host: "source-home".into(),
            instance: "1-0000000000000001".into(),
            binary: Some("111111111111".into()),
            machine: "fixture-machine".into(),
            claimed_ms: 1,
            renewed_ms: 1,
        },
    })
}

#[tokio::test]
async fn move_capsule_fidelity_preserves_all_native_semantic_families() -> TestResult {
    let root = PathFixture(tempfile::tempdir()?);
    let capsule = source_with_fidelity(&root, true).await?;
    let head = &capsule.sessions[0].heads[0];
    let expected = serde_json::to_value(head)?;
    let restored: Capsule = serde_json::from_slice(&serde_json::to_vec(&capsule)?)?;
    assert_tree_fidelity(&capsule, &restored)?;
    let request = restored
        .sessions
        .into_iter()
        .next()
        .ok_or("session")?
        .heads
        .into_iter()
        .next()
        .ok_or("head")?
        .into_request(MoveOperation::new(capsule.operation)?)?;
    let source = request.source;
    let fork = request.source_fork;
    assert!(
        fork.is_some(),
        "source fork provenance was dropped by capture"
    );
    let history = request.history.segments().to_vec();
    let named = request.history.named_state()?;
    let jobs = request.jobs.clone();
    let compaction = request.compaction.clone().ok_or("native selection")?;
    assert_eq!(compaction.trailing_segments, 1);
    assert_eq!(compaction.replaced_segments, Some(5));
    assert_eq!(
        compaction.strategy,
        Some(plakous::sessions::host_move::MoveCompactionStrategy::NativeResponsesTriggerV1)
    );
    let compiler = AgentSessions::move_preflight_compiler(capsule.sessions[0].durability.decode())?;
    let runtime = || text_binding(&root, "target-evidence.jsonl");
    let rebuilt = compiler.reconstruct_move_head(runtime()?, request).await?;
    assert_product_fidelity(
        &capsule,
        SessionLocator::new(
            source.session_id(),
            source.branch_id(),
            rebuilt.checkpoint.id(),
        ),
    )?;
    let manifest = rebuilt.checkpoint.manifest();
    assert_eq!(manifest.topology().fork, fork);
    assert_eq!(
        manifest.topology().parent_checkpoint_id,
        Some(source.checkpoint_id())
    );
    assert_eq!(manifest.canonical().history().segments().to_vec(), history);
    assert_eq!(manifest.jobs(), &jobs);
    assert_native_fidelity(
        MoveHeadFacts::capture(&rebuilt.checkpoint, &[])?,
        &expected,
        named,
        compaction,
    )?;
    let mut another: Capsule = serde_json::from_slice(&serde_json::to_vec(&capsule)?)?;
    let mut head = another
        .sessions
        .remove(0)
        .heads
        .remove(0)
        .into_request(MoveOperation::new(capsule.operation)?)?;
    head.source = SessionLocator::new(
        source.session_id(),
        source.branch_id(),
        uuid::Uuid::from_u128(999).try_into()?,
    );
    let different = compiler.reconstruct_move_head(runtime()?, head).await?;
    assert_ne!(rebuilt.checkpoint.id(), different.checkpoint.id());
    assert_eq!(
        std::fs::read(root.0.path().join("target-evidence.jsonl"))?,
        [] as [u8; 0]
    );
    Ok(())
}

struct PathFixture(tempfile::TempDir);

fn text_binding(
    root: &PathFixture,
    evidence: &str,
) -> TestResult<plakous::sessions::SessionRuntimeBinding> {
    plakous_offline_crash_harness::text_runtime_binding(
        root.0.path(),
        BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            root.0.path().join(evidence),
        )?,
        "not dispatched",
        "not dispatched",
    )
}

#[tokio::test]
async fn move_capsule_round_trips_only_semantic_data_and_is_not_an_ordinary_import() -> TestResult {
    let root = PathFixture(tempfile::tempdir()?);
    let capsule = source(&root).await?;
    let bytes = serde_json::to_vec(&capsule)?;
    let text = std::str::from_utf8(&bytes)?;
    for forbidden in [
        "registry_bytes",
        "harness_plan_component",
        "model_observation",
        "credential_generation",
        "runtime_objects",
    ] {
        assert!(
            !text.contains(forbidden),
            "native authority leaked: {forbidden}"
        );
    }
    let path = root.0.path().join("move.json");
    std::fs::write(&path, bytes)?;
    let restored = read_if_move(&path)?.ok_or("move document not recognized")?;
    assert_eq!(restored.products[0].id(), capsule.products[0].id());
    assert_eq!(
        restored.sessions[0].heads[0].source,
        capsule.sessions[0].heads[0].source
    );
    assert!(
        crate::carry::read_document(&path)
            .expect_err("ordinary import must refuse move kind")
            .to_string()
            .contains("authorized host ownership-move")
    );
    for mutate in 0..4 {
        let mut value = serde_json::to_value(&capsule)?;
        match mutate {
            0 => {
                value["sessions"][0]["heads"][0]["context"]["native_registry_bytes"] =
                    serde_json::json!([]);
            }
            1 => value["group"] = serde_json::json!("group-foreign"),
            2 => value["lines"][0]["generation"] = serde_json::json!(2),
            _ => value["products"][0]["floor"] = serde_json::json!(0),
        }
        std::fs::write(&path, serde_json::to_vec(&value)?)?;
        assert!(
            read_if_move(&path).is_err(),
            "invalid capsule {mutate} was admitted"
        );
    }
    assert!(
        std::fs::read(root.0.path().join("evidence.jsonl"))?.is_empty(),
        "constructing and exporting a source must not dispatch"
    );
    Ok(())
}

#[tokio::test]
async fn move_capsule_fresh_head_preserves_ids_but_not_the_checkpoint_or_fence() -> TestResult {
    let root = PathFixture(tempfile::tempdir()?);
    let capsule = source(&root).await?;
    let session = capsule
        .sessions
        .into_iter()
        .next()
        .ok_or("missing native session")?;
    let head = session
        .heads
        .into_iter()
        .next()
        .ok_or("missing native head")?;
    let source: SessionLocator = head.source.parse()?;
    let source_fence = head.fence_floor;
    let compiler = AgentSessions::move_preflight_compiler(session.durability.decode())?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.0.path().join("target-evidence.jsonl"),
    )?;
    let runtime = plakous_offline_crash_harness::text_runtime_binding(
        root.0.path(),
        controller,
        "not dispatched",
        "not dispatched",
    )?;
    let head = compiler
        .reconstruct_move_head(
            runtime,
            head.into_request(MoveOperation::new(capsule.operation)?)?,
        )
        .await?;
    assert_eq!(head.branch.session.session_id(), source.session_id());
    assert_eq!(head.branch.session.branch_id(), source.branch_id());
    assert_ne!(head.checkpoint.id(), source.checkpoint_id());
    assert!(head.branch.fence_floor > source_fence);
    let facts = MoveHeadFacts::capture(&head.checkpoint, &[])?;
    assert_eq!(facts.jobs.rows(), []);
    assert_eq!(
        std::fs::read(root.0.path().join("target-evidence.jsonl"))?,
        Vec::<u8>::new()
    );
    Ok(())
}

fn assert_tree_fidelity(capsule: &Capsule, restored: &Capsule) -> TestResult {
    let tree = serde_json::to_value(&capsule.trees[0].cut)?;
    assert!(!tree["messages"].as_object().ok_or("mail")?.is_empty());
    assert_ne!(
        tree["decisions"].as_array().ok_or("decisions")?.as_slice(),
        [] as [serde_json::Value; 0]
    );
    assert!(!tree["forked_from"].is_null());
    let moved = restored.trees[0]
        .cut
        .clone()
        .reconstruct(&BTreeMap::new())?;
    let mut expected_tree = tree;
    expected_tree["revision"] =
        serde_json::json!(expected_tree["revision"].as_u64().ok_or("revision")? + 1);
    expected_tree["host_generation"] = serde_json::json!(
        expected_tree["host_generation"]
            .as_u64()
            .ok_or("generation")?
            + 1
    );
    // Ordinary reopen joins the retained host intent and advances bookkeeping.
    // Compare the carried semantics separately from those new local revisions.
    let native = plakous_recursive_subagents::tree::ProductAgentTree::reopen(moved);
    let mut actual_tree = serde_json::to_value(DormantTree::capture(&native)?)?;
    assert_eq!(
        actual_tree["revision"].as_u64(),
        expected_tree["revision"]
            .as_u64()
            .map(|revision| revision + 1)
    );
    actual_tree["revision"] = expected_tree["revision"].clone();
    actual_tree["host_generation"] = expected_tree["host_generation"].clone();
    assert_eq!(actual_tree, expected_tree);
    Ok(())
}

fn assert_native_fidelity(
    facts: MoveHeadFacts,
    expected: &serde_json::Value,
    named: BTreeMap<String, serde_json::Value>,
    compaction: plakous::sessions::host_move::ImportedCompactionFacts,
) -> TestResult {
    assert_eq!(facts.history.named_state()?, named);
    assert_eq!(
        named,
        BTreeMap::from([(
            "saved".into(),
            serde_json::json!({"nested": [true, 42, "kept"]})
        )])
    );
    assert_eq!(facts.compaction, Some(compaction));
    assert_eq!(
        serde_json::to_value(Jobs::capture(&facts.jobs)?)?,
        expected["jobs"]
    );
    assert_eq!(
        serde_json::to_value(Presentation::capture(facts.presentation)?)?,
        expected["presentation"]
    );
    assert_eq!(
        serde_json::to_value(facts.interpreter)?,
        expected["interpreter"]
    );
    Ok(())
}

fn assert_product_fidelity(capsule: &Capsule, target: SessionLocator) -> TestResult {
    let source = capsule.products[0].clone();
    let mut expected = serde_json::to_value(&source)?;
    let floor = source.floor();
    expected["locator"] = serde_json::json!(target.to_string());
    expected["state"]["head"]["checkpoint_id"] = serde_json::json!(target.checkpoint_id());
    expected["state"]["history_through"] = serde_json::json!(floor);
    expected["state"]["imported_through"] = serde_json::json!(floor);
    let actual = source.into_cut()?.reconstruct(target)?.document()?;
    assert_eq!(serde_json::to_value(actual)?, expected);
    Ok(())
}
