//! Exact overflow, compaction and reopen evidence without a terminal.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "offline runtime qualification"
)]
use crate::product_host::{HostWorkflow, ScriptedRuntimeFactory};
use crate::product_paths::PlakPaths;
use crate::product_runtime_e2e_tests::{drive_injected_binding_host, host_config};
use crate::product_sessions::{self, SessionRecord};
use plakous::agent::TurnResultOutcome;
use plakous::application::ApplicationExecutor;
use plakous::model::{ContentBlock, ContextStatusKind};
use plakous::sessions::{
    AgentSessions, ContextQuery, ContextStatusRequest, CreateSessionOutcome, CreateSessionRequest,
    OpenSessionOutcome, OpenSessionRequest, SessionBackend, SessionContextStatusKind,
    SessionOperationId,
};
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{
    BoundaryController, compaction_runtime_binding_with_projected_input,
    compaction_runtime_binding_with_projected_input_and_identity_namespace,
};
use std::error::Error;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

static BINDING_SEQUENCE: AtomicU64 = AtomicU64::new(1);

struct Journey<'a> {
    paths: &'a PlakPaths,
    workspace: &'a Path,
    evidence: &'a Path,
    boundaries: &'a Path,
    ceiling: u64,
}

impl Journey<'_> {
    fn run(&self, record: SessionRecord, prompts: &[&str]) -> TestResult {
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, self.evidence)?;
        let target = compaction_runtime_binding_with_projected_input(
            self.workspace,
            controller.clone(),
            self.ceiling,
        )?
        .target_binding();
        let workspace = self.workspace.to_path_buf();
        let ceiling = self.ceiling;
        let paths = self.paths.clone();
        let id = record.id.clone();
        let boundaries = self.boundaries.to_path_buf();
        let injection = Arc::new(ScriptedRuntimeFactory {
            runtime: Arc::new(move |_| {
                compaction_runtime_binding_with_projected_input_and_identity_namespace(
                    &workspace,
                    controller.clone(),
                    ceiling,
                    (u128::from(std::process::id()) << 32)
                        | u128::from(BINDING_SEQUENCE.fetch_add(1, Ordering::Relaxed)),
                )
                .map_err(|error| error.to_string())
            }),
            rebind_target: target,
            observe: Arc::new(move |workflow, boundary| {
                if workflow != HostWorkflow::Compact {
                    return;
                }
                let locator = product_sessions::load_session(&paths, &id)
                    .unwrap()
                    .plakous_locator
                    .unwrap();
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&boundaries)
                    .unwrap();
                writeln!(file, "{boundary:?}\t{locator}").unwrap();
                file.sync_data().unwrap();
            }),
            child_runtime: None,
            rebind_builds: Mutex::new(Vec::new()),
        });
        let mut config = host_config(self.paths, self.workspace, record);
        config.compaction_enabled = true;
        let events = drive_injected_binding_host(
            config,
            injection,
            prompts
                .iter()
                .map(|prompt| {
                    (
                        crate::surface_protocol::UserAction::Prompt((*prompt).into()),
                        Box::new(|event: &crate::surface_protocol::HostEvent| {
                            matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
                        })
                            as Box<dyn Fn(&crate::surface_protocol::HostEvent) -> bool>,
                    )
                })
                .collect(),
        )?;
        assert!(
            !events
                .iter()
                .any(|event| format!("{event:?}").contains("RecoveryRequired")),
            "{events:#?}"
        );
        Ok(())
    }

    fn dispatches(&self) -> usize {
        read_evidence(self.evidence)
            .unwrap()
            .iter()
            .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
            .count()
    }
}

#[test]
fn exact_overflow_compacts_reopens_and_continues_without_terminal() -> TestResult {
    let root = PlakPaths::isolated_tempdir();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    let ceiling = calibrate_ceiling(
        &workspace,
        &root.path().join("calibration.jsonl"),
        &root.path().join("calibration.sqlite3"),
    )?;
    let evidence = root.path().join("physical.jsonl");
    let boundaries = root.path().join("boundaries.tsv");
    let journey = Journey {
        paths: &paths,
        workspace: &workspace,
        evidence: &evidence,
        boundaries: &boundaries,
        ceiling,
    };
    let mut record = SessionRecord::new(workspace.clone());
    record.account = Some("codex".into());
    record.model = Some("offline-loopback-v1".into());
    record.effort = Some("medium".into());
    product_sessions::save_session(&paths, &mut record)?;
    let id = record.id.clone();
    journey.run(record, &PROMPTS[..2])?;
    let source = product_sessions::load_session(&paths, &id)?
        .plakous_locator
        .unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let executor = ApplicationExecutor::new()?;
    let OpenSessionOutcome::Opened { session, .. } =
        executor.block_on(sessions.open(OpenSessionRequest::new(
            SessionOperationId::new("compact-runtime:source")?,
            source,
            compaction_runtime_binding_with_projected_input(
                &workspace,
                controller.clone(),
                ceiling,
            )?,
        )))??
    else {
        return Err("source did not reopen exactly".into());
    };
    for (label, query, overflow) in [
        ("current", ContextQuery::Current, false),
        (
            "pending",
            ContextQuery::WithPendingUserInput(PROMPTS[2].into()),
            true,
        ),
    ] {
        let status = executor.block_on(sessions.context_status(ContextStatusRequest::new(
            SessionOperationId::new(format!("compact-runtime:{label}"))?,
            source,
            query,
        )))??;
        assert!(
            matches!(status.kind(), SessionContextStatusKind::Available(status)
            if if overflow { matches!(status.kind(), ContextStatusKind::WouldOverflow(_)) }
            else { matches!(status.kind(), ContextStatusKind::Fits { .. }) })
        );
    }
    assert_eq!(journey.dispatches(), 2);
    assert_eq!(executor.block_on(session.release())??, source);
    journey.run(product_sessions::load_session(&paths, &id)?, &PROMPTS[2..])?;
    assert_eq!(journey.dispatches(), 4);
    verify_compacted_tail(&journey, &id, source, &sessions, &executor, controller)
}

pub(super) const PROMPTS: [&str; 3] = [
    "compaction intent group alpha with deliberately substantial replaceable context AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "compaction intent group beta",
    "compaction retained causal tail gamma",
];
pub(super) const FOLLOWING_PROMPT: &str = "qz";
pub(super) const SUMMARY: &str = "older intent groups compacted";

fn exact_measurement(status: &plakous::model::ContextStatus) -> TestResult<u64> {
    Ok(status
        .measurement()
        .ok_or("context status omitted its exact prepared measurement")?
        .bound()
        .upper_bound())
}

pub(super) fn calibrate_ceiling(
    workspace: &Path,
    evidence: &Path,
    database: &Path,
) -> TestResult<u64> {
    let controller = BoundaryController::quiet_from_evidence_path(CrashBoundary::None, evidence)?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(database)?)?;
    let executor = ApplicationExecutor::new()?;
    let CreateSessionOutcome::Created { session, .. } =
        executor.block_on(sessions.create(CreateSessionRequest::new(
            SessionOperationId::new("compact-pty-calibration:create")?,
            compaction_runtime_binding_with_projected_input(
                workspace,
                controller.clone(),
                10_000_000,
            )?,
        )))??
    else {
        return Err("calibration session creation returned an unknown outcome".into());
    };
    for prompt in &PROMPTS[..2] {
        let result = executor
            .block_on(async { session.agent().prompt(*prompt).await?.result().await })??;
        if !matches!(result.outcome(), TurnResultOutcome::Completed(_)) {
            return Err(
                format!("calibration turn did not complete: {:?}", result.outcome()).into(),
            );
        }
    }
    let current = executor.block_on(sessions.context_status(ContextStatusRequest::new(
        SessionOperationId::new("compact-pty-calibration:current-two")?,
        session.locator(),
        ContextQuery::Current,
    )))??;
    let SessionContextStatusKind::Available(current) = current.kind() else {
        return Err("calibration current projection was unavailable".into());
    };
    let current_measurement = exact_measurement(current)?;
    let pending = executor.block_on(sessions.context_status(ContextStatusRequest::new(
        SessionOperationId::new("compact-pty-calibration:pending-third")?,
        session.locator(),
        ContextQuery::WithPendingUserInput(PROMPTS[2].to_owned().into()),
    )))??;
    let SessionContextStatusKind::Available(pending) = pending.kind() else {
        return Err("calibration pending projection was unavailable".into());
    };
    let pending_measurement = exact_measurement(pending)?;
    if pending_measurement <= current_measurement {
        return Err("fixed pending prompt did not add measurable context pressure".into());
    }
    let ceiling = current_measurement + (pending_measurement - current_measurement) / 2;
    let third =
        executor.block_on(async { session.agent().prompt(PROMPTS[2]).await?.result().await })??;
    if !matches!(third.outcome(), TurnResultOutcome::Completed(_)) {
        return Err("calibration third turn did not complete".into());
    }
    let completed = executor.block_on(sessions.context_status(ContextStatusRequest::new(
        SessionOperationId::new("compact-pty-calibration:completed-third")?,
        session.locator(),
        ContextQuery::Current,
    )))??;
    let SessionContextStatusKind::Available(completed) = completed.kind() else {
        return Err("calibration completed projection was unavailable".into());
    };
    if exact_measurement(completed)? <= pending_measurement {
        return Err(
            "fixed completed group did not grow beyond its admitted pending projection".into(),
        );
    }
    let _ = executor.block_on(session.release())??;
    Ok(ceiling)
}

fn verify_compacted_tail(
    journey: &Journey<'_>,
    id: &str,
    source: plakous::sessions::SessionLocator,
    sessions: &AgentSessions,
    executor: &ApplicationExecutor,
    controller: BoundaryController,
) -> TestResult {
    let paths = journey.paths;
    let workspace = journey.workspace;
    let evidence = journey.evidence;
    let boundaries = journey.boundaries;
    let record = product_sessions::load_session(paths, id)?;
    let target = record.plakous_locator.unwrap();
    assert_ne!(target, source);
    let mut store = crate::store::ProductStore::open(paths)?;
    let intents = store.load_intents(id)?;
    let request = intents
        .workflow_requests
        .iter()
        .find(|request| {
            matches!(
                request.kind,
                product_sessions::SessionWorkflowKind::Compact(
                    product_sessions::ProductCompactReason::Scheduled
                )
            )
        })
        .ok_or("scheduled compaction missing")?;
    let views = store.load_views(id)?.ok_or("compaction views missing")?;
    let Some(plakous_hosted::sessions::projector::WorkflowSettlement::Compacted { .. }) =
        views.state.workflows[&request.operation_id].settled
    else {
        panic!("scheduled compaction did not settle");
    };
    let rows = std::fs::read_to_string(boundaries)?;
    let compact_source = request.source_locator;
    let compact_rows = rows
        .lines()
        .skip_while(|line| *line != format!("Prepared\t{compact_source}"))
        .take(4)
        .collect::<Vec<_>>();
    assert_eq!(compact_rows.len(), 4, "{rows}");
    assert!(
        compact_rows[1].starts_with("RuntimeReturned\t"),
        "{compact_rows:?}"
    );
    let compact = compact_rows[2]
        .strip_prefix("ProductPublished\t")
        .ok_or("publication boundary missing")?
        .parse::<plakous::sessions::SessionLocator>()?;
    assert_ne!(compact, source);
    assert_ne!(compact, target);
    assert_eq!(compact_rows[3], format!("ProductCommitted\t{compact}"));
    verify_compacted_history(
        sessions,
        executor,
        target,
        workspace,
        controller,
        journey.ceiling,
    )?;
    journey.run(record, &[FOLLOWING_PROMPT])?;
    assert_eq!(journey.dispatches(), 6);
    let final_record = product_sessions::load_session(paths, id)?;
    assert_eq!(
        final_record.prompt_history.last().map(String::as_str),
        Some(FOLLOWING_PROMPT)
    );
    assert_eq!(
        final_record.pending_queue,
        [] as [plakous_hosted::sessions::QueueItem; 0]
    );
    assert_eq!(
        read_evidence(evidence)?
            .iter()
            .filter(|row| row.kind == EvidenceKind::EffectDispatched)
            .count(),
        0
    );
    Ok(())
}

fn verify_compacted_history(
    sessions: &AgentSessions,
    executor: &ApplicationExecutor,
    target: plakous::sessions::SessionLocator,
    workspace: &Path,
    controller: BoundaryController,
    ceiling: u64,
) -> TestResult {
    let OpenSessionOutcome::Opened { session, .. } =
        executor.block_on(sessions.open(OpenSessionRequest::new(
            SessionOperationId::new("compact-runtime:target")?,
            target,
            compaction_runtime_binding_with_projected_input(workspace, controller, ceiling)?,
        )))??
    else {
        return Err("target did not reopen exactly".into());
    };
    let history = session.agent().conversation();
    let texts = history
        .items()
        .into_iter()
        .flat_map(plakous::model::ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for retained in [SUMMARY, PROMPTS[1], PROMPTS[2]] {
        assert!(texts.contains(&retained), "{texts:?}");
    }
    assert!(!texts.contains(&PROMPTS[0]), "{texts:?}");
    assert_eq!(executor.block_on(session.release())??, target);
    Ok(())
}
