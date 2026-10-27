use super::*;
use crate::surface_protocol::UserAction;
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};
use std::time::Duration;

fn compacting_config(
    root: &std::path::Path,
) -> (
    crate::runtime::HostConfig<TestExtension>,
    std::path::PathBuf,
) {
    let script: Arc<ScriptPlan> = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version":SCRIPT_VERSION, "maximum_projected_input":100_000,
            "content_sensitive_projection":true, "fold":{"stall":1000},
            "steps":[{"output":{"text":"x".repeat(70_000)}},
                {"output":{"text":"latest protected answer"}},
                {"output":{"text":"must not dispatch"}}]
        }))
        .unwrap(),
    );
    let (mut config, evidence) =
        crate::runtime::recover_tests::scripted_host_with(root, script.clone());
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        evidence.clone(),
    )
    .unwrap();
    let workspace = config.cwd.clone();
    let cursor = Arc::new(std::sync::atomic::AtomicU64::new(1));
    config.runtime_factory = Arc::new(crate::runtime::ScriptedRuntimeFactory {
        rebind_target: crate::runtime::test_kit::scripted(&config)
            .rebind_target
            .clone(),
        runtime: Arc::new(move |role| {
            if role == crate::runtime::RuntimeRole::Compact {
                plakous_offline_crash_harness::scripted_compaction_runtime_binding(
                    &workspace,
                    controller.clone(),
                    &script,
                    None,
                    None,
                    Vec::new(),
                )
            } else {
                plakous_offline_crash_harness::scripted_runtime_binding_with_cursor(
                    &workspace,
                    controller.clone(),
                    script.clone(),
                    cursor.clone(),
                )
            }
            .map_err(|error| error.to_string())
        }),
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: Mutex::new(Vec::new()),
    });
    config.compaction_enabled = true;
    (config, evidence)
}

#[test]
fn stop_during_paid_scheduled_compaction_finishes_its_receipt_without_a_root_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let (config, evidence) = compacting_config(root.path());
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        HostLoop::open(config, &receiver, crate::runtime::Sender::over(wire), None).unwrap();
    for text in ["older intent", "latest intent"] {
        host.executor
            .block_on(async {
                host.opened
                    .agent()
                    .unwrap()
                    .prompt(text)
                    .await
                    .unwrap()
                    .result()
                    .await
                    .unwrap()
            })
            .unwrap();
    }
    let source = host.opened.locator();
    let item = host
        .config
        .session
        .enqueue_live("p".repeat(50_000))
        .unwrap();
    host.config.store.save(&mut host.config.session).unwrap();
    events.try_iter().for_each(drop);
    let path = evidence.clone();
    let control = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if read_evidence(&path)
                .unwrap_or_default()
                .iter()
                .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
                .count()
                == 3
            {
                actions.send(UserAction::Interrupt).unwrap();
                break;
            }
            assert!(
                Instant::now() < deadline,
                "scheduled compaction never dispatched"
            );
            std::thread::park_timeout(Duration::from_millis(5));
        }
    });
    host.run_prompt(item.text.clone(), Some(item.clone()), true, None)
        .unwrap();
    control.join().unwrap();
    assert_ne!(
        host.opened.locator(),
        source,
        "the paid fold must reach its receipt"
    );
    assert_eq!(
        read_evidence(&evidence)
            .unwrap()
            .iter()
            .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
            .count(),
        3,
        "two seeded turns, exactly one fold, no new root or follow-on"
    );
    assert_eq!(host.config.session.pending_queue, [item]);
    assert_eq!(
        host.config.session.queue_drain_state,
        SessionQueueDrainState::RestoredPaused
    );
    let observed: Vec<_> = events.try_iter().collect();
    assert!(!observed.iter().any(|event| matches!(
        event,
        HostEvent::PromptAdmitted(_) | HostEvent::TurnLifecycle(_)
    )));
    assert!(
        matches!(observed.iter().rev().find(|event| matches!(event, HostEvent::Status {..})), Some(HostEvent::Status {phase, detail: Some(detail), ..}) if phase == "idle" && detail.contains("preparation stopped"))
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event, HostEvent::SetBusy(false)))
            .count(),
        1
    );
    host.release().unwrap();
}
