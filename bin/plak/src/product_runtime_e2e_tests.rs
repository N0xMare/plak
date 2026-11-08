//! Product runtime qualifications independent of terminal modes or rendering.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    missing_docs,
    reason = "offline runtime qualification fixtures"
)]

use crate::product_host::{HostConfig, RuntimeRole, ScriptedRuntimeFactory};
use crate::product_paths::PlakPaths;
use crate::product_sessions::{self, SessionRecord, TranscriptRow};
use plakous::agent::ReasoningEffort;
use plakous::application::ApplicationExecutor;
use plakous::sessions::{
    AgentSessions, OpenSessionOutcome, OpenSessionRequest, SessionBackend, SessionOperationId,
};
use plakous::subscriptions::NativeProvider;
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{
    BoundaryController, exited_child_process_runtime_binding, runtime_binding_for_account,
};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
const WAIT: Duration = Duration::from_secs(45);

type RuntimeResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

struct LongWork {
    _root: tempfile::TempDir,
    paths: PlakPaths,
    workspace: PathBuf,
    evidence: PathBuf,
    release: PathBuf,
}

impl LongWork {
    fn new() -> RuntimeResult<Self> {
        let root = PlakPaths::isolated_tempdir();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;
        Ok(Self {
            paths: PlakPaths::explicit(root.path().join("home")),
            workspace,
            evidence: root.path().join("physical.jsonl"),
            release: root.path().join("release"),
            _root: root,
        })
    }

    fn count(&self, kind: EvidenceKind) -> usize {
        read_evidence(&self.evidence)
            .unwrap()
            .iter()
            .filter(|row| row.kind == kind)
            .count()
    }

    fn committed(&self, prompt: &str) -> RuntimeResult<SessionRecord> {
        let until = Instant::now() + WAIT;
        loop {
            for row in product_sessions::list_sessions_for_cwd(&self.paths, &self.workspace)? {
                let record = product_sessions::load_session(&self.paths, &row.id)?;
                if record
                    .prompt_history
                    .last()
                    .is_some_and(|last| last == prompt)
                    && record
                        .transcript
                        .iter()
                        .filter(|row| {
                            row.text()
                                .is_some_and(|text| text.contains("loopback turn committed"))
                        })
                        .count()
                        >= record.prompt_history.len()
                {
                    return Ok(record);
                }
            }
            assert!(Instant::now() < until, "prompt did not commit: {prompt}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn factory(&self) -> RuntimeResult<Arc<ScriptedRuntimeFactory>> {
        let controller = BoundaryController::quiet_gated_from_evidence_path(
            CrashBoundary::ProviderDispatchOne,
            &self.evidence,
            &self.release,
        )?;
        let target =
            runtime_binding_for_account(&self.workspace, controller.clone(), b"long-work-target")?
                .target_binding();
        let workspace = self.workspace.clone();
        let rebound = AtomicBool::new(false);
        let generation = AtomicU64::new(1);
        Ok(Arc::new(ScriptedRuntimeFactory {
            runtime: Arc::new(move |role| {
                if role == RuntimeRole::Rebind {
                    rebound.store(true, Ordering::Release);
                }
                let account = if rebound.load(Ordering::Acquire) {
                    b"long-work-target".as_slice()
                } else {
                    b"long-work-source".as_slice()
                };
                plakous_offline_crash_harness::runtime_binding_for_account_with_identity_namespace(
                    &workspace,
                    controller.clone(),
                    account,
                    u128::from(generation.fetch_add(1, Ordering::AcqRel)),
                )
                .map_err(|error| error.to_string())
            }),
            rebind_target: target,
            observe: Arc::new(|_, _| {}),
            child_runtime: None,
            rebind_builds: Mutex::new(Vec::new()),
        }))
    }
}

impl Drop for LongWork {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.release, "release");
    }
}

#[test]
fn gated_provider_queue_fork_and_clear_keep_ordered_workflows_and_exact_reopen() -> RuntimeResult {
    let work = LongWork::new()?;
    let mut source = SessionRecord::new(work.workspace.clone());
    source.account = Some("codex".into());
    source.model = Some("offline-loopback-v1".into());
    source.effort = Some("medium".into());
    product_sessions::save_session(&work.paths, &mut source)?;
    let mut config = host_config(&work.paths, &work.workspace, source);
    config.ready_catalog = long_work_ready_catalog();
    config.runtime_factory = work.factory()?;
    let (actions, events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(config)?);
    let mut seen = Vec::new();
    gated_queue_and_settings(&work, &actions, &events, &mut seen)?;
    let queued = work.committed("long work queued follow-up")?;
    assert_eq!(
        queued.prompt_history,
        ["long work first root", "long work queued follow-up"]
    );
    send_action(
        &actions,
        crate::surface_protocol::UserAction::ForkSession {
            account: None,
            compact: false,
        },
    )?;
    wait_for_event(&events, &mut seen, |event| {
        matches!(event,
        crate::surface_protocol::HostEvent::Binding { session_id: Some(id), .. } if *id != queued.id)
    })?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("prompt after exact fork".into()),
    )?;
    wait_for_event(&events, &mut seen, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    let forked = work.committed("prompt after exact fork")?;
    assert_eq!(forked.prompt_history, ["prompt after exact fork"]);
    verify_successor_workflow(
        &work.paths,
        &queued.id,
        &forked,
        product_sessions::SessionWorkflowKind::Fork,
    )?;
    send_action(&actions, crate::surface_protocol::UserAction::NewSession)?;
    wait_for_event(&events, &mut seen, |event| {
        matches!(event,
        crate::surface_protocol::HostEvent::Binding { session_id: Some(id), .. } if *id != forked.id)
    })?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("prompt after clear".into()),
    )?;
    wait_for_event(&events, &mut seen, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    let cleared = work.committed("prompt after clear")?;
    assert_eq!(cleared.prompt_history, ["prompt after clear"]);
    verify_successor_workflow(
        &work.paths,
        &forked.id,
        &cleared,
        product_sessions::SessionWorkflowKind::New,
    )?;
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "long work host panicked")?
        .map_err(|error| error.message().to_owned())?;
    verify_long_work_reopens(&work)
}

fn gated_queue_and_settings(
    work: &LongWork,
    actions: &crate::surface_protocol::FaceActionSender,
    events: &crate::surface_protocol::HostEventReceiver,
    seen: &mut Vec<crate::surface_protocol::HostEvent>,
) -> RuntimeResult {
    send_action(
        actions,
        crate::surface_protocol::UserAction::Prompt("long work first root".into()),
    )?;
    let until = Instant::now() + WAIT;
    while work.count(EvidenceKind::ProviderDispatched) == 0 {
        assert!(Instant::now() < until, "provider did not reach its gate");
        std::thread::sleep(Duration::from_millis(10));
    }
    let acknowledged = Instant::now();
    send_action(
        actions,
        crate::surface_protocol::UserAction::FollowUp("long work queued follow-up".into()),
    )?;
    wait_for_event(events, seen, |event| {
        matches!(event, crate::surface_protocol::HostEvent::QueueSnapshot { items, .. }
        if format!("{items:?}").contains("long work queued follow-up"))
    })?;
    assert!(acknowledged.elapsed() <= crate::test_support::latency_budget(Duration::from_secs(3)));
    let settings = Instant::now();
    send_action(
        actions,
        crate::surface_protocol::UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::OpenSettings,
        ),
    )?;
    wait_for_event(events, seen, |event| {
        matches!(
            event,
            crate::surface_protocol::HostEvent::Extension(
                crate::surface_protocol::SurfaceObservation::SettingsView(_)
            )
        )
    })?;
    assert!(settings.elapsed() <= crate::test_support::latency_budget(Duration::from_secs(3)));
    assert_eq!(work.count(EvidenceKind::ProviderDispatched), 1);
    assert_eq!(work.count(EvidenceKind::ProviderOutcome), 0);
    std::fs::write(&work.release, "release")?;
    for _ in 0..2 {
        wait_for_event(events, seen, |event| {
            matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
        })?;
    }
    Ok(())
}

fn verify_successor_workflow(
    paths: &PlakPaths,
    source: &str,
    target: &SessionRecord,
    kind: product_sessions::SessionWorkflowKind,
) -> RuntimeResult {
    let mut store = crate::store::ProductStore::open(paths)?;
    let intents = store.load_intents(source)?;
    let request = intents
        .workflow_requests
        .iter()
        .find(|request| request.kind == kind)
        .ok_or("missing workflow request")?;
    assert_eq!(request.successor_product_id(), target.id);
    if kind == product_sessions::SessionWorkflowKind::New {
        assert_eq!(
            target.create_operation_id,
            format!("{}:create", request.operation_id)
        );
    }
    let views = store.load_views(source)?.ok_or("missing source views")?;
    assert!(
        views.state.workflows[&request.operation_id]
            .settled
            .is_some()
    );
    Ok(())
}

fn verify_long_work_reopens(work: &LongWork) -> RuntimeResult {
    let records = product_sessions::list_sessions_for_cwd(&work.paths, &work.workspace)?
        .into_iter()
        .map(|row| product_sessions::load_session(&work.paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(records.len(), 3, "source, fork, clear only");
    assert_eq!(work.count(EvidenceKind::ProviderDispatched), 7);
    assert_eq!(work.count(EvidenceKind::EffectDispatched), 3);
    let before = read_evidence(&work.evidence)?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(work.paths.runtime_database())?)?;
    let executor = ApplicationExecutor::new()?;
    for (index, record) in records.iter().enumerate() {
        let locator = record
            .plakous_locator
            .ok_or("surviving record has no locator")?;
        let controller =
            BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &work.evidence)?;
        let binding =
            runtime_binding_for_account(&work.workspace, controller, b"long-work-source")?;
        let opened = executor.block_on(sessions.open(OpenSessionRequest::new(
            SessionOperationId::new(format!("long-work-runtime:reopen-{index}"))?,
            locator,
            binding,
        )))??;
        let OpenSessionOutcome::Opened { session, .. } = opened else {
            return Err(format!("not quiescent: {opened:?}").into());
        };
        assert_eq!(executor.block_on(session.release())??, locator);
    }
    assert_eq!(read_evidence(&work.evidence)?, before);
    Ok(())
}

fn profile_fork_action(name: &str) -> crate::surface_protocol::UserAction {
    crate::surface_protocol::UserAction::Extension(
        crate::surface_protocol::SurfaceIntent::ApplyProfile {
            name: name.to_owned(),
            apply: crate::surface_protocol::ProfileApply::Fork,
        },
    )
}

#[test]
fn exited_child_output_over_8192_commits_and_reopens_without_recovery_required()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let root = PlakPaths::isolated_tempdir();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let paths = PlakPaths::explicit(root.path().join("home"));
    let evidence = root.path().join("process-physical.jsonl");
    let controller = BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)?;
    let mut record = SessionRecord::new(workspace.clone());
    record.account = Some("codex".into());
    record.model = Some("offline-loopback-v1".into());
    record.effort = Some("medium".into());
    product_sessions::save_session(&paths, &mut record)?;
    let id = record.id.clone();
    let target =
        exited_child_process_runtime_binding(&workspace, controller.clone())?.target_binding();
    let runtime_workspace = workspace.clone();
    let runtime_controller = controller.clone();
    let injection = Arc::new(ScriptedRuntimeFactory {
        runtime: Arc::new(move |_| {
            exited_child_process_runtime_binding(&runtime_workspace, runtime_controller.clone())
                .map_err(|error| error.to_string())
        }),
        rebind_target: target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: Mutex::new(Vec::new()),
    });
    let events = drive_injected_binding_host(
        host_config(&paths, &workspace, record),
        injection,
        vec![(
            crate::surface_protocol::UserAction::Prompt(
                "reproduce the exited child continuation".into(),
            ),
            Box::new(|event| matches!(event, crate::surface_protocol::HostEvent::AssistantDone)),
        )],
    )?;
    assert!(
        !events
            .iter()
            .any(|event| format!("{event:?}").contains("RecoveryRequired")),
        "{events:#?}"
    );
    let record = product_sessions::load_session(&paths, &id)?;
    let activities: Vec<_> = record
        .transcript
        .iter()
        .filter_map(|row| row.as_tool())
        .collect();
    assert_eq!(activities.len(), 2, "{activities:#?}");
    assert_eq!(
        (activities[0].name.as_str(), activities[0].state.as_str()),
        ("process_session_start", "succeeded")
    );
    assert_eq!(
        (activities[1].name.as_str(), activities[1].state.as_str()),
        ("process_session_poll", "succeeded")
    );
    assert!(record.transcript.iter().any(|row| {
        row.text()
            .is_some_and(|text| text.contains("large exited child continued and joined"))
    }));
    let dispatches = || {
        read_evidence(&evidence)
            .unwrap()
            .iter()
            .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
            .count()
    };
    assert_eq!(dispatches(), 3);
    let locator = record
        .plakous_locator
        .ok_or("process journey omitted its runtime locator")?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let executor = ApplicationExecutor::new()?;
    let opened = executor.block_on(sessions.open(OpenSessionRequest::new(
        SessionOperationId::new("process-runtime:reopen-terminal")?,
        locator,
        exited_child_process_runtime_binding(&workspace, controller)?,
    )))??;
    let OpenSessionOutcome::Opened { session, .. } = opened else {
        return Err(format!("process locator is not terminally reopenable: {opened:?}").into());
    };
    let _ = executor.block_on(session.release())??;
    assert_eq!(dispatches(), 3, "exact reopen redispatched provider work");
    Ok(())
}

fn account_injection(
    workspace: &Path,
    controller: &BoundaryController,
    primary: &'static [u8],
    target: &'static [u8],
) -> Result<Arc<ScriptedRuntimeFactory>, Box<dyn Error + Send + Sync>> {
    let rebind_target =
        runtime_binding_for_account(workspace, controller.clone(), target)?.target_binding();
    let workspace = workspace.to_path_buf();
    let controller = controller.clone();
    Ok(Arc::new(ScriptedRuntimeFactory {
        runtime: Arc::new(move |role| {
            let account = if role == RuntimeRole::Rebind {
                target
            } else {
                primary
            };
            runtime_binding_for_account(&workspace, controller.clone(), account)
                .map_err(|error| error.to_string())
        }),
        rebind_target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: Mutex::new(Vec::new()),
    }))
}

pub(super) fn host_config(
    paths: &PlakPaths,
    workspace: &Path,
    session: SessionRecord,
) -> HostConfig<crate::surface_protocol::PlakExtension> {
    HostConfig {
        surface: Some(crate::shell::settings::ProductSurface::boxed(paths)),
        auto_continue: true,
        resume_after_loss: plakous_hosted::runtime::ResumeAfterLoss::Hold,
        topology_admission: None,
        state_roots: crate::shell::host::product_state_roots(paths),
        product_home: paths.home().to_path_buf(),
        home: paths.session_home(),
        store: Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        )),
        session,
        target: plakous_hosted::runtime::test_kit::registered_authority(NativeProvider::Codex),
        model_id: "offline-loopback-v1".to_owned(),
        effort: Some(ReasoningEffort::Medium),
        effort_explicit: true,
        compaction_enabled: false,
        max_model_rounds: crate::product_agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_enabled: crate::product_config::SessionPolicy::load(paths)
            .unwrap()
            .topology_enabled,
        root_template: None,
        attached_one_shot: false,
        interactive: true,
        cwd: workspace.to_path_buf(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
        resume_publication: None,
        runtime_factory: Arc::new(plakous_hosted::runtime::ProductionRuntimeFactory),
        ready_path: crate::product_host::ReadyPathTiming::default(),
        seed_history: None,
        seed_line: None,
        source_releases: crate::product_host::SourceReleases::default(),
        idle_release: None,
        lease_holder: None,
        continuation: None,
        event_renderer: None,
    }
}

fn dual_binding_ready_catalog() -> plakous_host_protocol::views::ReadyCatalog {
    [
        ("binding-a", "binding-a-model", "medium"),
        ("binding-b", "binding-b-model", "low"),
    ]
    .into_iter()
    .map(
        |(account, model, effort)| plakous_host_protocol::views::ProviderSection {
            account: account.to_owned(),
            label: account.to_owned(),
            ready: true,
            entries: vec![plakous_host_protocol::views::ModelEntry {
                id: model.to_owned(),
                tag: "offline exact binding".to_owned(),
                reasoning: true,
                effort_options: vec![effort.to_owned()],
                extras: Vec::new(),
                unavailable_reason: None,
            }],
        },
    )
    .collect::<Vec<_>>()
    .into()
}

pub(super) fn long_work_ready_catalog() -> plakous_host_protocol::views::ReadyCatalog {
    vec![plakous_host_protocol::views::ProviderSection {
        account: "codex".to_owned(),
        label: "Codex".to_owned(),
        ready: true,
        entries: vec![
            plakous_host_protocol::views::ModelEntry {
                id: "offline-loopback-v1".to_owned(),
                tag: "offline source binding".to_owned(),
                reasoning: true,
                effort_options: vec!["medium".to_owned()],
                extras: Vec::new(),
                unavailable_reason: None,
            },
            plakous_host_protocol::views::ModelEntry {
                id: "offline-loopback-v2".to_owned(),
                tag: "offline target binding".to_owned(),
                reasoning: true,
                effort_options: vec!["low".to_owned()],
                extras: Vec::new(),
                unavailable_reason: None,
            },
        ],
    }]
    .into()
}

fn wait_for_event(
    events: &crate::surface_protocol::HostEventReceiver,
    observed: &mut Vec<crate::surface_protocol::HostEvent>,
    done: impl Fn(&crate::surface_protocol::HostEvent) -> bool,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!("timed out waiting for host event: {observed:#?}").into());
        }
        let event = events.recv_timeout(remaining).map_err(|error| {
            std::io::Error::other(format!(
                "host event wait failed ({error:?}); observed: {observed:#?}"
            ))
        })?;
        let complete = done(&event);
        observed.push(event);
        if complete {
            return Ok(());
        }
    }
}

fn send_action(
    actions: &crate::surface_protocol::FaceActionSender,
    action: crate::surface_protocol::UserAction,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    actions.send(action).map_err(|error| {
        std::io::Error::other(format!(
            "host action queue refused qualification input: {error:?}"
        ))
        .into()
    })
}

#[test]
fn exact_resume_is_published_only_by_successfully_attached_runtime_host()
-> Result<(), Box<dyn Error + Send + Sync>> {
    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&workspace)?;
    let evidence = root.path().join("resume-attachment.jsonl");
    let controller = BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)?;
    let paths = PlakPaths::explicit(home);
    let mut session = SessionRecord::new(workspace.clone());
    session.account = Some("binding-a".to_owned());
    session.model = Some("binding-a-model".to_owned());
    session.effort = Some("medium".to_owned());
    product_sessions::save_session(&paths, &mut session)?;
    let id = session.id.clone();
    let revision = session.store_revision;
    let publication = Arc::new(AtomicBool::new(false));

    let mut config = host_config(&paths, &workspace, session);
    config.runtime_factory = account_injection(
        &workspace,
        &controller,
        b"binding-a-account",
        b"binding-b-account",
    )?;
    config.resume_publication = Some(Arc::clone(&publication));
    let (actions, _events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(config)?);

    assert!(publication.load(Ordering::Acquire));
    assert!(product_sessions::load_session(&paths, &id)?.store_revision > revision);
    assert!(
        crate::product_session_log::read_tail(&paths.session_home(), &id, 16)?
            .iter()
            .any(|line| line.contains("session.resumed"))
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "resume host panicked")?
        .map_err(|error| error.message().to_owned())?;
    Ok(())
}

/// A profile Fork now carries the conversation onto the profile's root: the
/// history and the queue drained under the source continue on the branch, and
/// a second Fork now back onto the first root keeps every exact branch.
#[test]
fn injected_dual_profile_fork_continues_history_queue_and_reverse_switch()
-> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"binding-a-account";
    const B: &[u8] = b"binding-b-account";
    const A_RETURN: &[u8] = b"binding-a-return-account";
    let (root, paths, workspace) = profile_home("low")?;
    std::fs::write(
        paths.home().join("agents/pa-root.toml"),
        "account = \"codex\"\nmodel = \"offline-loopback-v1\"\neffort = \"medium\"\ndescription = \"root\"\n",
    )?;
    std::fs::write(
        paths.home().join("profiles/pa.toml"),
        "root = \"pa-root\"\nworking_set = [\"pb-worker\"]\n",
    )?;
    let evidence = root.path().join("dual-binding.jsonl");
    let controller = BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &evidence)?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("offline-loopback-v1".to_owned());
    source.effort = Some("medium".to_owned());
    product_sessions::save_session(&paths, &mut source)?;
    let source_id = source.id.clone();
    let fork_onto = profile_fork_action;

    let mut first_config = host_config(&paths, &workspace, source);
    first_config.ready_catalog = profile_ready_catalog();
    first_config.runtime_factory = account_injection(&workspace, &controller, A, B)?;
    let (actions, events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(first_config)?);
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("binding A first turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::SetBusy(true))
    })?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::FollowUp(
            "queued while binding A is busy".to_owned().into(),
        ),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, fork_onto("pb"))?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert_eq!(
        observed.iter().rev().find_map(profile_outcome),
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{observed:#?}"
    );
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("binding B continued turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "binding B host panicked")?
        .map_err(|error| error.message().to_owned())?;

    let branch_b_id = verify_binding_b(&paths, &workspace, &source_id)?;

    verify_reverse_binding(
        &paths,
        &workspace,
        &source_id,
        &branch_b_id,
        &evidence,
        account_injection(&workspace, &controller, B, A_RETURN)?,
    )
}

/// Fork now from a Fast (`NativePriority`) root onto a non-fast profile lands
/// `Native`, and the reverse onto a `fast_mode = true` root lands
/// `NativePriority`.
#[test]
fn profile_fork_maps_the_fast_lane_on_and_off() -> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"fast-source-account";
    const B: &[u8] = b"standard-target-account";
    const FAST: &[u8] = b"fast-return-account";
    let (root, paths, workspace) = profile_home("low")?;
    std::fs::write(
        paths.home().join("agents/pa-root.toml"),
        "account = \"codex\"\nmodel = \"offline-loopback-v1\"\neffort = \"medium\"\nfast_mode = true\ndescription = \"fast root\"\n",
    )?;
    std::fs::write(
        paths.home().join("profiles/pa.toml"),
        "root = \"pa-root\"\nworking_set = [\"pb-worker\"]\n",
    )?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("fast-lane.jsonl"),
    )?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("offline-loopback-v1".to_owned());
    source.effort = Some("medium".to_owned());
    source.target_selection = plakous_hosted::binding::TargetSelection::NativePriority;
    product_sessions::save_session(&paths, &mut source)?;
    let source_id = source.id.clone();
    let fork_onto = profile_fork_action;

    let mut first = host_config(&paths, &workspace, source);
    first.ready_catalog = profile_ready_catalog();
    first.runtime_factory = account_injection(&workspace, &controller, A, B)?;
    let (actions, events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(first)?);
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("fast source turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, fork_onto("pb"))?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert_eq!(
        observed.iter().rev().find_map(profile_outcome),
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{observed:#?}"
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "fast-lane host panicked")?
        .map_err(|error| error.message().to_owned())?;

    verify_fast_return(
        &paths,
        &workspace,
        &source_id,
        account_injection(&workspace, &controller, B, FAST)?,
    )
}

/// A native-ptc prefix forks onto a Direct profile root, and the next turn
/// commits on that composition.
#[test]
fn profile_fork_from_native_ptc_onto_direct_commits_a_turn()
-> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"native-ptc-source";
    const B: &[u8] = b"direct-profile-root";
    let (root, paths, workspace) = profile_home("low")?;
    std::fs::write(
        paths.home().join("agents/pb-root.toml"),
        "account = \"codex\"\nmodel = \"binding-b-model\"\neffort = \"low\"\ncomposition = \"direct\"\ndescription = \"direct root\"\n",
    )?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("code-mode-fork.jsonl"),
    )?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("offline-loopback-v1".to_owned());
    source.effort = Some("medium".to_owned());
    source.composition = plakous_hosted::binding::CompositionSelection::NativePtc;
    product_sessions::save_session(&paths, &mut source)?;
    let source_id = source.id.clone();
    let mut config = host_config(&paths, &workspace, source);
    config.ready_catalog = profile_ready_catalog();
    config.runtime_factory = account_injection(&workspace, &controller, A, B)?;
    let (actions, events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(config)?);
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("code mode prefix".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::ApplyProfile {
                name: "pb".to_owned(),
                apply: crate::surface_protocol::ProfileApply::Fork,
            },
        ),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert_eq!(
        observed.iter().rev().find_map(profile_outcome),
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{observed:#?}"
    );
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("direct turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "code-mode fork host panicked")?
        .map_err(|error| error.message().to_owned())?;

    let rows = product_sessions::list_sessions_for_cwd(&paths, &workspace)?
        .into_iter()
        .map(|row| product_sessions::load_session(&paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?;
    let kept = rows
        .iter()
        .find(|row| row.id == source_id)
        .ok_or("native-ptc source missing")?;
    assert_eq!(
        kept.composition,
        plakous_hosted::binding::CompositionSelection::NativePtc
    );
    let target = rows
        .iter()
        .find(|row| row.id != source_id)
        .ok_or("direct branch missing")?;
    assert_eq!(
        target.composition,
        plakous_hosted::binding::CompositionSelection::Direct
    );
    assert!(
        target
            .transcript
            .iter()
            .any(|line| line.text().is_some_and(|text| text.contains("direct turn")))
    );
    assert!(target.transcript.iter().any(|line| {
        line.text()
            .is_some_and(|text| text.contains("loopback turn committed"))
    }));
    Ok(())
}

type BindingHostStep = (
    crate::surface_protocol::UserAction,
    Box<dyn Fn(&crate::surface_protocol::HostEvent) -> bool>,
);

pub(super) fn drive_injected_binding_host(
    mut config: HostConfig<crate::surface_protocol::PlakExtension>,
    injection: Arc<ScriptedRuntimeFactory>,
    steps: Vec<BindingHostStep>,
) -> Result<Vec<crate::surface_protocol::HostEvent>, Box<dyn Error + Send + Sync>> {
    config.runtime_factory = injection;
    let (actions, events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(config)?);
    let mut observed = Vec::new();
    for (action, completed) in steps {
        send_action(&actions, action)?;
        let deadline = Instant::now() + WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(
                    format!("timed out waiting for binding host event: {observed:#?}").into(),
                );
            }
            let event = events.recv_timeout(remaining).map_err(|error| {
                std::io::Error::other(format!(
                    "binding host event wait failed ({error:?}): {observed:#?}"
                ))
            })?;
            let done = completed(&event);
            observed.push(event);
            if done {
                break;
            }
        }
    }
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| std::io::Error::other("injected binding host panicked"))?
        .map_err(|error| error.message().to_owned())?;
    observed.extend(events.try_iter());
    Ok(observed)
}

/// A home with a user profile `pb` whose root binds `binding-b-model` on the
/// codex account at `root_effort`, with one worker card, and topology on.
fn profile_home(
    root_effort: &str,
) -> Result<(tempfile::TempDir, PlakPaths, PathBuf), Box<dyn Error + Send + Sync>> {
    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    let paths = PlakPaths::explicit(home.clone());
    paths.ensure_private_state_home()?;
    std::fs::create_dir_all(home.join("agents"))?;
    std::fs::create_dir_all(home.join("profiles"))?;
    std::fs::create_dir_all(&workspace)?;
    std::fs::write(
        home.join("agents/pb-root.toml"),
        format!(
            "account = \"codex\"\nmodel = \"binding-b-model\"\neffort = \"{root_effort}\"\ndescription = \"root\"\n"
        ),
    )?;
    std::fs::write(
        home.join("agents/pb-worker.toml"),
        "account = \"codex\"\nmodel = \"binding-b-model\"\neffort = \"low\"\ndescription = \"Profile B worker card.\"\n",
    )?;
    std::fs::write(
        home.join("profiles/pb.toml"),
        "root = \"pb-root\"\nworking_set = [\"pb-worker\"]\n",
    )?;
    let mut config = crate::product_config::ProductConfig::load(&paths)?;
    config.set_key("orchestration.subagents", "true")?;
    config.save(&paths)?;
    Ok((root, paths, workspace))
}

fn profile_ready_catalog() -> plakous_host_protocol::views::ReadyCatalog {
    vec![plakous_host_protocol::views::ProviderSection {
        account: "codex".to_owned(),
        label: "codex".to_owned(),
        ready: true,
        entries: ["offline-loopback-v1", "binding-b-model"]
            .into_iter()
            .map(|model| plakous_host_protocol::views::ModelEntry {
                id: model.to_owned(),
                tag: "offline exact binding".to_owned(),
                reasoning: true,
                effort_options: vec!["low".to_owned(), "medium".to_owned()],
                extras: Vec::new(),
                unavailable_reason: None,
            })
            .collect(),
    }]
    .into()
}

fn profile_outcome(
    event: &crate::surface_protocol::HostEvent,
) -> Option<Result<crate::surface_protocol::ProfileOutcome, String>> {
    use crate::surface_protocol::SurfaceObservation;
    match event {
        crate::surface_protocol::HostEvent::Extension(SurfaceObservation::ProfileApplied {
            outcome,
            ..
        }) => Some(Ok(*outcome)),
        crate::surface_protocol::HostEvent::Extension(SurfaceObservation::ProfileRefused {
            reason,
            ..
        }) => Some(Err(reason.clone())),
        _ => None,
    }
}

/// Fork now on an attached session branches onto the profile with a tree of
/// its own, saves the profile only after the branch lands, and leaves the
/// source on its own tree so it still reopens under the strict tool check.
#[test]
fn profile_fork_now_branches_onto_its_own_tree_and_leaves_the_default()
-> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"profile-source-account";
    const B: &[u8] = b"profile-target-account";
    let (root, paths, workspace) = profile_home("low")?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("profile-fork.jsonl"),
    )?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("offline-loopback-v1".to_owned());
    source.effort = Some("medium".to_owned());
    product_sessions::save_session(&paths, &mut source)?;
    let source_id = source.id.clone();

    let mut config = host_config(&paths, &workspace, source);
    config.ready_catalog = profile_ready_catalog();
    // The source attachment join is held half a second past runtime release, so
    // the quit below always lands while it is pending.
    config.runtime_factory = account_injection(&workspace, &controller, A, B)?
        .with_source_release_delay(Duration::from_millis(500));
    let (actions, events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(config)?);
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("source turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    let config_before = std::fs::read_to_string(paths.config_file())?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::ApplyProfile {
                name: "pb".to_owned(),
                apply: crate::surface_protocol::ProfileApply::Fork,
            },
        ),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    let outcome = observed.iter().rev().find_map(profile_outcome);
    assert_eq!(
        outcome,
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{observed:#?}"
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    let quit_at = Instant::now();
    worker
        .join()
        .map_err(|_| "profile host panicked")?
        .map_err(|error| error.message().to_owned())?;
    // The host ended only after the source attachment join finished, inside the
    // launcher's worker join deadline.
    assert!(quit_at.elapsed() < crate::commands::support::HOST_WORKER_JOIN_TIMEOUT);
    verify_profile_tree_and_reopen(
        &paths,
        &workspace,
        &source_id,
        &config_before,
        account_injection(&workspace, &controller, A, B)?,
    )
}

/// A Fork now the host refuses saves nothing: the default for the next
/// launch is unchanged and no new session exists.
#[test]
fn a_refused_profile_fork_saves_nothing() -> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"profile-refused-source";
    const B: &[u8] = b"profile-refused-target";
    // `high` is absent from the catalog row, so the rebind refuses the root.
    let (root, paths, workspace) = profile_home("high")?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("profile-refused.jsonl"),
    )?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("offline-loopback-v1".to_owned());
    source.effort = Some("medium".to_owned());
    product_sessions::save_session(&paths, &mut source)?;
    let config_before = std::fs::read_to_string(paths.config_file())?;

    let mut config = host_config(&paths, &workspace, source);
    config.ready_catalog = profile_ready_catalog();
    config.runtime_factory = account_injection(&workspace, &controller, A, B)?;
    let (actions, events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(config)?);
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::ApplyProfile {
                name: "pb".to_owned(),
                apply: crate::surface_protocol::ProfileApply::Fork,
            },
        ),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert!(
        matches!(
            observed.iter().rev().find_map(profile_outcome),
            Some(Err(_))
        ),
        "{observed:#?}"
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "refused profile host panicked")?
        .map_err(|error| error.message().to_owned())?;
    assert_eq!(std::fs::read_to_string(paths.config_file())?, config_before);
    assert_eq!(
        product_sessions::list_sessions_for_cwd(&paths, &workspace)?.len(),
        1
    );
    Ok(())
}

/// Fork now onto the profile this session already runs, root and cards, is
/// not a fork: nothing branches and the outcome says so.
#[test]
fn fork_now_onto_the_running_profile_is_already_active() -> Result<(), Box<dyn Error + Send + Sync>>
{
    same_profile_fork(true)
}

#[test]
fn fork_now_onto_a_treeless_running_profile_seeds_its_own_tree()
-> Result<(), Box<dyn Error + Send + Sync>> {
    same_profile_fork(false)
}

fn same_profile_fork(with_tree: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"profile-same-source";
    const B: &[u8] = b"profile-new-tree";
    let (root, paths, workspace) = profile_home("low")?;
    let mut config = crate::product_config::ProductConfig::load(&paths)?;
    config.set_key("root.agent", "pb-root")?;
    config.set_key("roster.working_set", "pb-worker")?;
    config.save(&paths)?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("profile-same.jsonl"),
    )?;
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("codex".to_owned());
    source.model = Some("binding-b-model".to_owned());
    source.effort = Some("low".to_owned());
    product_sessions::save_session(&paths, &mut source)?;

    let source_id = source.id.clone();
    if with_tree {
        let roster = crate::roster_input::roster_input_from_config(
            &config,
            "codex",
            "binding-b-model",
            Some("low".to_owned()),
        );
        let mut tree = plakous_recursive_subagents::tree::block_on_for_test(
            plakous_hosted::agent_tree::ProductAgentTree::bootstrap_root(
                &source.id,
                &source.id,
                "root",
                workspace.clone(),
                "binding-b-model",
                roster.writer_mode,
            ),
        )
        .map_err(|error| error.message)?;
        tree.install_snapshot(plakous_hosted::orchestration::OrchestrationSnapshot::freeze(roster));
        let _revision = crate::store::tree::tree_store(&paths)?
            .save_in_custody(&tree)
            .map_err(|error| error.detail)?;
    }

    let mut host = host_config(&paths, &workspace, source);
    host.ready_catalog = profile_ready_catalog();
    host.runtime_factory =
        account_injection(&workspace, &controller, A, if with_tree { A } else { B })?;
    let (actions, events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(host)?);
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("profile turn".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Extension(
            crate::surface_protocol::SurfaceIntent::ApplyProfile {
                name: "pb".to_owned(),
                apply: crate::surface_protocol::ProfileApply::Fork,
            },
        ),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert_eq!(
        observed.iter().rev().find_map(profile_outcome),
        Some(Ok(if with_tree {
            crate::surface_protocol::ProfileOutcome::AlreadyActive
        } else {
            crate::surface_protocol::ProfileOutcome::Forked
        })),
        "{observed:#?}"
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "same-profile host panicked")?
        .map_err(|error| error.message().to_owned())?;
    assert_eq!(
        product_sessions::list_sessions_for_cwd(&paths, &workspace)?.len(),
        if with_tree { 1 } else { 2 }
    );
    if !with_tree {
        let target = product_sessions::list_sessions_for_cwd(&paths, &workspace)?
            .into_iter()
            .find(|row| row.id != source_id)
            .ok_or("profile fork missing")?;
        assert!(plakous_hosted::runtime::TreeRows::tree(&paths, &target.id)?.is_some());
        assert!(plakous_hosted::runtime::TreeRows::tree(&paths, &source_id)?.is_none());
    }
    Ok(())
}

/// A session resumed with a pause left over from earlier work (and
/// nothing queued) drains a follow-up typed during a live turn; it does not
/// sit "paused with the tree" until the operator resumes it by hand.
#[test]
fn a_live_follow_up_drains_after_a_resume_with_a_stale_pause()
-> Result<(), Box<dyn Error + Send + Sync>> {
    const A: &[u8] = b"stale-pause-account";
    let root = PlakPaths::isolated_tempdir();
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&workspace)?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("stale-pause.jsonl"),
    )?;
    let paths = PlakPaths::explicit(home);
    let mut source = SessionRecord::new(workspace.clone());
    source.account = Some("binding-a".to_owned());
    source.model = Some("binding-a-model".to_owned());
    source.effort = Some("medium".to_owned());
    product_sessions::save_session(&paths, &mut source)?;
    crate::store::ProductStore::open(&paths)?.write_host_controls(
        &source.id,
        false,
        Some(
            plakous_hosted::sessions::projector::HostPauseCause::UnrecordedRefusal {
                reason: "earlier refusal".to_owned(),
            },
        ),
    )?;
    let id = source.id.clone();
    let source = product_sessions::load_session(&paths, &id)?;
    assert_eq!(
        source.queue_drain_state,
        plakous_hosted::sessions::QueueDrainState::RestoredPaused
    );

    let mut config = host_config(&paths, &workspace, source);
    config.ready_catalog = dual_binding_ready_catalog();
    config.runtime_factory = account_injection(&workspace, &controller, A, A)?;
    let (actions, events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(config)?);
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("first".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::SetBusy(true))
    })?;
    send_action(
        &actions,
        crate::surface_protocol::UserAction::FollowUp("typed while busy".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "stale-pause host panicked")?
        .map_err(|error| error.message().to_owned())?;
    let after = product_sessions::load_session(&paths, &id)?;
    assert_eq!(
        after.pending_queue,
        [] as [plakous_hosted::sessions::QueueItem; 0]
    );
    assert_eq!(
        after.prompt_history.last().map(String::as_str),
        Some("typed while busy")
    );
    Ok(())
}

fn verify_binding_b(
    paths: &PlakPaths,
    workspace: &Path,
    source_id: &str,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let after_b = product_sessions::list_sessions_for_cwd(paths, workspace)?
        .into_iter()
        .map(|row| product_sessions::load_session(paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(after_b.len(), 2, "A→B must create one branch: {after_b:#?}");
    let branch_b = after_b
        .iter()
        .find(|row| row.model.as_deref() == Some("binding-b-model"))
        .ok_or("binding B branch missing")?;
    assert_ne!(branch_b.id, source_id);
    assert_eq!(
        branch_b.pending_queue,
        [] as [plakous_hosted::sessions::QueueItem; 0]
    );
    assert_eq!(
        branch_b.prompt_history,
        [
            "binding A first turn",
            "queued while binding A is busy",
            "binding B continued turn"
        ]
    );
    for prompt in &branch_b.prompt_history {
        assert!(
            branch_b
                .transcript
                .iter()
                .any(|line| line.text().is_some_and(|text| text.contains(prompt)))
        );
    }
    Ok(branch_b.id.clone())
}

fn verify_reverse_binding(
    paths: &PlakPaths,
    workspace: &Path,
    source_id: &str,
    branch_b_id: &str,
    evidence: &Path,
    injection: Arc<ScriptedRuntimeFactory>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut reverse_config = host_config(
        paths,
        workspace,
        product_sessions::load_session(paths, branch_b_id)?,
    );
    reverse_config.ready_catalog = profile_ready_catalog();
    // `host_config` fixes the source tuple; this host reopens the B branch.
    reverse_config.model_id = "binding-b-model".to_owned();
    reverse_config.effort = Some(ReasoningEffort::Low);
    let reverse_events = drive_injected_binding_host(
        reverse_config,
        injection,
        vec![
            (
                profile_fork_action("pa"),
                Box::new(|event| profile_outcome(event).is_some()),
            ),
            (
                crate::surface_protocol::UserAction::Prompt(
                    "binding A resumed after reverse switch".to_owned().into(),
                ),
                Box::new(|event| {
                    matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
                }),
            ),
        ],
    )?;
    assert_eq!(
        reverse_events.iter().find_map(profile_outcome),
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{reverse_events:#?}"
    );
    assert!(reverse_events.iter().any(|event| matches!(event,
        crate::surface_protocol::HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
            if note.contains("Model switched") && note.contains("available to resume")
    )));
    let after_a = product_sessions::list_sessions_for_cwd(paths, workspace)?
        .into_iter()
        .map(|row| product_sessions::load_session(paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(after_a.len(), 3, "A→B→A must retain all exact branches");
    let final_a = after_a
        .iter()
        .find(|row| {
            row.id != source_id
                && row.id != branch_b_id
                && row.model.as_deref() == Some("offline-loopback-v1")
        })
        .ok_or("reverse binding A branch missing")?;
    assert_eq!(final_a.prompt_history.len(), 4);
    assert_eq!(
        final_a.prompt_history.last().map(String::as_str),
        Some("binding A resumed after reverse switch")
    );
    assert_eq!(
        final_a.pending_queue,
        [] as [plakous_hosted::sessions::QueueItem; 0]
    );
    let physical = read_evidence(evidence)?;
    assert_eq!(
        physical
            .iter()
            .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
            .count(),
        7,
        "four root turns plus each of three binding incarnations' first-turn tool continuation"
    );
    Ok(())
}

fn verify_fast_return(
    paths: &PlakPaths,
    workspace: &Path,
    source_id: &str,
    injection: Arc<ScriptedRuntimeFactory>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let rows = product_sessions::list_sessions_for_cwd(paths, workspace)?
        .into_iter()
        .map(|row| product_sessions::load_session(paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?;
    let kept = rows
        .iter()
        .find(|row| row.id == source_id)
        .ok_or("fast source missing")?;
    assert_eq!(
        kept.target_selection,
        plakous_hosted::binding::TargetSelection::NativePriority
    );
    let standard = rows
        .iter()
        .find(|row| row.id != source_id)
        .ok_or("non-fast branch missing")?;
    assert_eq!(
        standard.target_selection,
        plakous_hosted::binding::TargetSelection::Native
    );
    let standard_id = standard.id.clone();

    let mut reverse = host_config(
        paths,
        workspace,
        product_sessions::load_session(paths, &standard_id)?,
    );
    reverse.ready_catalog = profile_ready_catalog();
    reverse.model_id = "binding-b-model".to_owned();
    reverse.effort = Some(ReasoningEffort::Low);
    reverse.runtime_factory = injection;
    let (actions, events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(reverse)?);
    let mut observed = Vec::new();
    send_action(&actions, profile_fork_action("pa"))?;
    wait_for_event(&events, &mut observed, |event| {
        profile_outcome(event).is_some()
    })?;
    assert_eq!(
        observed.iter().find_map(profile_outcome),
        Some(Ok(crate::surface_protocol::ProfileOutcome::Forked)),
        "{observed:#?}"
    );
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "fast return host panicked")?
        .map_err(|error| error.message().to_owned())?;

    let fast = product_sessions::list_sessions_for_cwd(paths, workspace)?
        .into_iter()
        .map(|row| product_sessions::load_session(paths, &row.id))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|row| row.id != source_id && row.id != standard_id)
        .ok_or("fast branch missing")?;
    assert_eq!(
        fast.target_selection,
        plakous_hosted::binding::TargetSelection::NativePriority
    );
    assert_eq!(fast.model.as_deref(), Some("offline-loopback-v1"));
    Ok(())
}

fn verify_profile_tree_and_reopen(
    paths: &PlakPaths,
    workspace: &Path,
    source_id: &str,
    config_before: &str,
    injection: Arc<ScriptedRuntimeFactory>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let source = product_sessions::load_session(paths, source_id)?;
    let store: Arc<dyn plakous_hosted::session_store::SessionRecordStore> = Arc::new(
        crate::store::session_records::SqliteSessionStore::new(paths.clone()),
    );
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let projected = ApplicationExecutor::new()?
        .block_on(plakous_hosted::runtime::project_session(
            &sessions, &store, &source,
        ))?
        .map_err(|error| format!("source projection failed: {error}"))?
        .ok_or("source projection missing")?;
    let source_views = store.load_views(source_id)?.ok_or("source views missing")?;
    assert_eq!(
        source_views.state.head.map(|head| head.checkpoint_id),
        Some(projected.head.checkpoint_id())
    );

    // Profile choice is per session: the home's default is untouched.
    assert_eq!(std::fs::read_to_string(paths.config_file())?, config_before);
    let target = product_sessions::list_sessions_for_cwd(paths, workspace)?
        .into_iter()
        .find(|row| row.id != source_id)
        .ok_or("the profile fork published no new session")?;
    let target = product_sessions::load_session(paths, &target.id)?;
    assert_eq!(target.model.as_deref(), Some("binding-b-model"));
    // The new session's tree is its own and froze the profile's card.
    let target_tree = plakous_hosted::runtime::TreeRows::tree(paths, &target.id)?
        .ok_or("the profile fork opened no tree of its own")?;
    assert!(
        target_tree
            .current_epoch()
            .roster
            .keys()
            .any(|name| name.as_str() == "pb-worker")
    );
    // The fork's own profile persists with its tree.
    assert_eq!(
        target_tree.current_epoch().root_template.as_deref(),
        Some("pb-root")
    );
    // The source's tree, if it has one, never received the profile's cards.
    if let Some(source_tree) = plakous_hosted::runtime::TreeRows::tree(paths, source_id)? {
        assert!(
            !source_tree
                .current_epoch()
                .roster
                .keys()
                .any(|name| name.as_str() == "pb-worker")
        );
    }

    // The source reopens and runs a turn on its own binding.
    let mut reopen = host_config(
        paths,
        workspace,
        product_sessions::load_session(paths, source_id)?,
    );
    reopen.ready_catalog = profile_ready_catalog();
    reopen.runtime_factory = injection;
    let (actions, events, worker) =
        crate::surface_protocol::face_of(crate::product_host::spawn_host_worker(reopen)?);
    let mut observed = Vec::new();
    send_action(
        &actions,
        crate::surface_protocol::UserAction::Prompt("source again".to_owned().into()),
    )?;
    wait_for_event(&events, &mut observed, |event| {
        matches!(event, crate::surface_protocol::HostEvent::AssistantDone)
    })?;
    send_action(&actions, crate::surface_protocol::UserAction::Quit)?;
    worker
        .join()
        .map_err(|_| "reopened source host panicked")?
        .map_err(|error| error.message().to_owned())?;
    Ok(())
}
