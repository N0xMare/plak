#![allow(
    clippy::too_many_lines,
    reason = "Host-open scenarios reopen unpublished controls and queued commands without inventing receipts or resubmitting changed bytes."
)]

use super::*;
use crate::surface_protocol::HostEvent;
use plakous::agent::CommandId;

#[test]
fn live_holders_take_precedence_over_both_runner_inspection_refusals() {
    for failure in [
        CliFailure::local_runner_processes(),
        CliFailure::local_runner_inspection("root", "locator", "missing"),
    ] {
        assert_eq!(name_inspection_holder(failure.clone(), None), failure);
        let named = name_inspection_holder(failure, Some("live holder".into()));
        assert_eq!(named.code(), Some("tree_hosted_elsewhere"));
        assert_eq!(named.message(), "live holder");
    }
    let internal = CliFailure::new("unavailable transport");
    assert_eq!(
        name_inspection_holder(internal.clone(), Some("live holder".into())),
        internal
    );
}

fn unpublished_fixture(
    root: &std::path::Path,
) -> (
    std::sync::Arc<dyn crate::session_store::SessionRecordStore>,
    String,
    plakous::sessions::SessionLocator,
) {
    unpublished_control_fixture(root, None)
}

#[derive(Clone, Copy)]
enum RootControlCut {
    Steer,
    Cancel,
    BusyQuit,
    ApprovalAllowed,
    ApprovalDenied,
    Shutdown,
}

fn control_cut_host(
    root: &std::path::Path,
    cut: RootControlCut,
) -> HostConfig<crate::runtime::test_kit::TestExtension> {
    use crate::runtime::test_support::fresh_injected_attached_host;
    use plakous_offline_crash_harness::{BoundaryController, protocol::CrashBoundary};
    let (mut config, injection) = fresh_injected_attached_host(root);
    drop(injection);
    let approval = matches!(
        cut,
        RootControlCut::ApprovalAllowed | RootControlCut::ApprovalDenied
    );
    let controller = BoundaryController::quiet_gated_from_evidence_path(
        if approval {
            CrashBoundary::ProviderDispatchTwo
        } else {
            CrashBoundary::ProviderDispatchOne
        },
        root.join("control-cut.jsonl"),
        root.join("control-release"),
    )
    .unwrap();
    let workspace = config.cwd.clone();
    crate::runtime::test_kit::scripted_mut(&mut config).runtime = std::sync::Arc::new(move |_| {
        if approval {
            let script = serde_json::from_value(serde_json::json!({
                    "version": 1, "tool_policy": "decision_required",
                    "steps": [
                        {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
                        {"output": {"text": "approved terminal"}, "finish": "stop"}
                    ]
                })).unwrap();
            plakous_offline_crash_harness::scripted_runtime_binding(
                &workspace,
                controller.clone(),
                std::sync::Arc::new(script),
            )
            .map_err(|error| error.to_string())
        } else {
            plakous_offline_crash_harness::runtime_binding(&workspace, controller.clone())
                .map_err(|error| error.to_string())
        }
    });
    config
}

fn unpublished_control_fixture(
    root: &std::path::Path,
    cut: Option<RootControlCut>,
) -> (
    std::sync::Arc<dyn crate::session_store::SessionRecordStore>,
    String,
    plakous::sessions::SessionLocator,
) {
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    let config = cut.map_or_else(
        || fresh_injected_attached_host(root).0,
        |cut| control_cut_host(root, cut),
    );
    let evidence = root.join("control-cut.jsonl");
    let gate = root.join("control-release");
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    let store = host.role.config.store.clone();
    let id = host.role.config.session.id.clone();
    let item = sessions::queue::enqueue(&mut host.role.config.session.pending_queue, "unpublished")
        .unwrap();
    store.save(&mut host.role.config.session).unwrap();
    let command = format!("plak.queue.{}.{}", item.id, item.revision);
    let intent = sessions::projector::Submission {
        command_id: command.clone(),
        kind: SubmissionKind::RootStart,
        queue_item_id: item.id.clone(),
        queue_item_revision: item.revision,
        target_turn_id: None,
        canonical_command: item.text.clone(),
        submitted_at_ms: item.submitted_at_ms,
    };
    store
        .save_submission(&mut host.role.config.session, &intent)
        .unwrap();
    let submission = host
        .role
        .executor
        .block_on(
            host.opened
                .agent()
                .unwrap()
                .start(CommandId::new(&command).unwrap(), &item.text),
        )
        .unwrap()
        .unwrap();
    let handle = submission.into_turn().unwrap();
    if let Some(cut) = cut {
        host.role
            .executor
            .block_on(async {
                use plakous_offline_crash_harness::protocol::CrashBoundary;
                let approval = matches!(
                    cut,
                    RootControlCut::ApprovalAllowed | RootControlCut::ApprovalDenied
                );
                if approval {
                    let decision = tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let view = host
                                .opened
                                .agent()
                                .unwrap()
                                .attach(plakous::agent::ViewRequest::metadata())
                                .await
                                .unwrap();
                            if let Some(decision) =
                                view.snapshot().pending_effect_decisions().first()
                            {
                                break decision.decision().clone();
                            }
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    if matches!(cut, RootControlCut::ApprovalAllowed) {
                        handle
                            .control()
                            .allow_effect(
                                decision,
                                plakous::tools::EffectDecisionAuditId::new("cut-approval").unwrap(),
                            )
                            .await
                            .unwrap();
                    } else {
                        handle
                            .control()
                            .deny_tool(
                                decision,
                                plakous::tools::ToolPolicyDenial::new("cut-denial"),
                            )
                            .await
                            .unwrap();
                    }
                }
                let boundary = if approval {
                    CrashBoundary::ProviderDispatchTwo
                } else {
                    CrashBoundary::ProviderDispatchOne
                };
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !plakous_offline_crash_harness::evidence::read_evidence(&evidence)
                        .unwrap()
                        .iter()
                        .any(|row| row.boundary == boundary)
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                match cut {
                    RootControlCut::Steer
                    | RootControlCut::ApprovalAllowed
                    | RootControlCut::ApprovalDenied => {
                        if matches!(cut, RootControlCut::Steer) {
                            handle
                                .control()
                                .steer("direct steering without a delivery intent")
                                .await
                                .unwrap();
                        }
                        std::fs::write(&gate, b"release").unwrap();
                    }
                    RootControlCut::Cancel | RootControlCut::BusyQuit => {
                        handle.control().cancel().await.unwrap();
                    }
                    RootControlCut::Shutdown => {
                        host.opened
                            .agent()
                            .unwrap()
                            .shutdown_with_id(CommandId::new("cut-shutdown").unwrap())
                            .await
                            .unwrap();
                    }
                }
            })
            .unwrap();
    }
    host.role
        .executor
        .block_on(handle.result())
        .unwrap()
        .unwrap();
    let head = host
        .role
        .executor
        .block_on(host.opened.release())
        .unwrap()
        .unwrap();
    (store, id, head)
}

#[test]
fn direct_steer_cut_projects_the_terminal_once() {
    control_cut_publication(RootControlCut::Steer);
}

#[test]
fn cancel_cut_projects_the_terminal_once() {
    control_cut_publication(RootControlCut::Cancel);
}

#[test]
fn shutdown_cut_projects_the_terminal_once() {
    control_cut_publication(RootControlCut::Shutdown);
}

#[test]
fn busy_quit_cancelled_terminal_cut_publishes_once() {
    control_cut_publication(RootControlCut::BusyQuit);
}

#[test]
fn approval_cuts_project_the_terminal_once() {
    control_cut_publication(RootControlCut::ApprovalAllowed);
    control_cut_publication(RootControlCut::ApprovalDenied);
}

fn control_cut_publication(cut: RootControlCut) {
    use crate::runtime::test_kit::TestExtension;
    let root = tempfile::tempdir().unwrap();
    let (store, id, head) = unpublished_control_fixture(root.path(), Some(cut));
    let mut config = control_cut_host(root.path(), cut);
    config.store = store.clone();
    config.session = store.load(&id).unwrap();
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut reopened = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    let published = store.load(&id).unwrap();
    assert_eq!(published.plakous_locator, Some(head));
    assert_eq!(
        published.pending_queue,
        [] as [sessions::queue::QueueItem; 0]
    );
    let rows = published.transcript.len();
    reopened
        .block(async |reopened| reopened.release().await)
        .unwrap();
    let mut config = control_cut_host(root.path(), cut);
    config.store = store.clone();
    config.session = store.load(&id).unwrap();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut reopened = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    assert_eq!(
        store.load(&id).unwrap().transcript.len(),
        rows,
        "no second terminal"
    );
    reopened
        .block(async |reopened| reopened.release().await)
        .unwrap();
}

#[test]
fn terminal_projection_survives_attachment_save_failure_and_repeated_open() {
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::{SaveFailure, fresh_injected_attached_host};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let root = tempfile::tempdir().unwrap();
    let (store, id, head) = unpublished_fixture(root.path());
    let fail = Arc::new(crate::runtime::fault_store::FaultStore::new(
        store.clone(),
        SaveFailure {
            fail: AtomicBool::new(true),
        },
    ));
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    for failing in [true, false, false] {
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.session = store.load(&id).unwrap();
        config.store = fail.clone();
        fail.faults.fail.store(failing, Ordering::SeqCst);
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let opened = RootActor::open_blocking(config, &rx, Sender::over(tx), None);
        if failing {
            assert!(opened.is_err());
        } else {
            let mut opened = opened.unwrap();
            assert_eq!(opened.role.config.session.plakous_locator, Some(head));
            opened.block(async |opened| opened.release().await).unwrap();
        }
        let projected = store.load(&id).unwrap();
        assert_eq!(projected.root_turn_count, 1);
        assert_eq!(
            projected
                .transcript
                .iter()
                .filter(|row| row.text() == Some("loopback turn committed"))
                .count(),
            1,
        );
    }
    assert_eq!(
        store
            .load(&id)
            .unwrap()
            .transcript
            .iter()
            .filter(|row| row.text() == Some("loopback turn committed"))
            .count(),
        1
    );
}

#[test]
fn unrecorded_wake_submission_reopens_without_a_legacy_delivery_receipt() {
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;

    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    let store = host.role.config.store.clone();
    let id = host.role.config.session.id.clone();
    let item = sessions::queue::enqueue(
        &mut host.role.config.session.pending_queue,
        super::super::wake::WAKE_PROMPT,
    )
    .unwrap();
    let submission = sessions::projector::Submission {
        command_id: "wake-without-legacy-receipt".into(),
        kind: SubmissionKind::HostWake,
        queue_item_id: item.id.clone(),
        queue_item_revision: item.revision,
        target_turn_id: None,
        canonical_command: item.text.clone(),
        submitted_at_ms: item.submitted_at_ms,
    };
    store
        .save_submission(&mut host.role.config.session, &submission)
        .unwrap();
    assert_eq!(
        store
            .load_intents(&host.role.config.session.id)
            .unwrap()
            .submissions,
        vec![submission.clone()]
    );
    host.block(async |host| host.release().await).unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store = store.clone();
    config.session = store.load(&id).unwrap();
    let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut reopened =
        RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    assert_eq!(
        reopened.role.config.session.pending_queue,
        [] as [sessions::queue::QueueItem; 0]
    );
    assert_eq!(
        store.load_intents(&id).unwrap().submissions,
        [] as [sessions::projector::Submission; 0]
    );
    assert_eq!(reopened.role.config.session.root_turn_count, 0);
    reopened
        .block(async |reopened| reopened.release().await)
        .unwrap();
}

#[test]
fn pending_commands_reopen_paused_and_resubmit_their_stored_kind_bytes_and_target() {
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    use crate::sessions::projector::{Delivery, Submission};
    for steer in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (event_tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host =
            RootActor::open_blocking(config, &action_rx, Sender::over(event_tx), None).unwrap();
        let store = host.role.config.store.clone();
        let id = host.role.config.session.id.clone();
        let target = host
            .role
            .executor
            .block_on(async {
                let turn = host
                    .opened
                    .agent()
                    .unwrap()
                    .prompt("advance head")
                    .await
                    .unwrap();
                let id = turn.id();
                turn.result().await.unwrap();
                id
            })
            .unwrap();
        let item =
            sessions::queue::enqueue(&mut host.role.config.session.pending_queue, "operator text")
                .unwrap();
        let intent = Submission {
            command_id: "custom-stored-command".into(),
            kind: if steer {
                SubmissionKind::SendNowSteer
            } else {
                SubmissionKind::RootStart
            },
            queue_item_id: item.id.clone(),
            queue_item_revision: item.revision,
            target_turn_id: steer.then(|| target.to_string()),
            canonical_command: "exact stored bytes\nincluding immutable context".into(),
            submitted_at_ms: item.submitted_at_ms,
        };
        store
            .save_submission(&mut host.role.config.session, &intent)
            .unwrap();
        host.block(async |host| host.release().await).unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store = store.clone();
        config.session = store.load(&id).unwrap();
        let (event_tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut reopened =
            RootActor::open_blocking(config, &action_rx, Sender::over(event_tx), None).unwrap();
        assert_eq!(
            reopened.role.config.session.pending_queue,
            vec![item.clone()]
        );
        assert_eq!(
            reopened.role.config.session.queue_drain_state,
            SessionQueueDrainState::RestoredPaused
        );
        assert!(
            reopened
                .role
                .config
                .session
                .queue_commands
                .is_locked(&item.id)
        );
        assert!(reopened.role.pending_start.is_none());
        assert_eq!(
            store.load_intents(&id).unwrap().submissions,
            vec![intent.clone()]
        );
        let count = reopened.role.config.session.root_turn_count;
        actions
            .send(crate::surface_protocol::UserAction::SendQueuedNow {
                id: item.id.clone(),
                revision: item.revision,
            })
            .unwrap();
        let super::super::HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = reopened
            .block(async |reopened| reopened.next_prompt(None).await)
            .unwrap()
        else {
            panic!("explicit resubmission")
        };
        reopened
            .block(async |reopened| {
                reopened
                    .run_prompt(text, queued_delivery, user_echoed, None)
                    .await
            })
            .unwrap();
        let views = store.load_views(&id).unwrap().unwrap();
        if steer {
            assert!(matches!(
                views.state.deliveries.get(&intent.command_id),
                Some(Delivery::Rejected { .. })
            ));
            assert_eq!(reopened.role.config.session.root_turn_count, count);
            assert_eq!(reopened.role.config.session.pending_queue, vec![item]);
        } else {
            assert_eq!(
                views.state.deliveries.get(&intent.command_id),
                Some(&Delivery::Accepted)
            );
            assert_eq!(
                reopened.role.config.session.pending_queue,
                [] as [sessions::queue::QueueItem; 0]
            );
            let duplicate = reopened
                .role
                .executor
                .block_on(reopened.opened.agent().unwrap().command(
                    plakous::agent::TurnCommand::Start {
                        command_id: CommandId::new(&intent.command_id).unwrap(),
                        input: intent.canonical_command.clone().into(),
                    },
                ))
                .unwrap()
                .unwrap();
            assert_eq!(
                duplicate.acknowledgement().disposition(),
                plakous::agent::CommandAckDisposition::Duplicate
            );
            assert!(
                duplicate.acknowledgement().is_accepted(),
                "the runtime accepted the exact stored bytes, not the editable queue text"
            );
            assert_eq!(reopened.role.config.session.root_turn_count, count + 1);
        }
        assert_eq!(store.load_intents(&id).unwrap().submissions, vec![intent]);
        reopened
            .block(async |reopened| reopened.release().await)
            .unwrap();
    }
}

#[test]
fn submission_anchored_below_retention_is_consumed_silently_without_counting_a_turn() {
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (event_tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        RootActor::open_blocking(config, &action_rx, Sender::over(event_tx), None).unwrap();
    let store = host.role.config.store.clone();
    let id = host.role.config.session.id.clone();
    let item = sessions::queue::enqueue(&mut host.role.config.session.pending_queue, "already ran")
        .unwrap();
    let intent = sessions::projector::Submission {
        command_id: "below-retention-root".into(),
        kind: SubmissionKind::RootStart,
        queue_item_id: item.id.clone(),
        queue_item_revision: item.revision,
        target_turn_id: None,
        canonical_command: item.text.clone(),
        submitted_at_ms: item.submitted_at_ms,
    };
    store
        .save_submission(&mut host.role.config.session, &intent)
        .unwrap();
    drop(host.role.live.take());
    host.role
        .executor
        .block_on(async {
            host.opened
                .agent()
                .unwrap()
                .start(
                    CommandId::new(&intent.command_id).unwrap(),
                    &intent.canonical_command,
                )
                .await
                .unwrap()
                .result()
                .unwrap()
                .clone()
                .result()
                .await
                .unwrap();
        })
        .unwrap();
    // Release only the runtime lease: simulates losing the host before its projector pass.
    host.role
        .executor
        .block_on(host.opened.release_opened())
        .unwrap()
        .unwrap();
    let connection = rusqlite::Connection::open(root.path().join("home/runtime.sqlite3")).unwrap();
    connection.execute_batch("DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions); UPDATE sessions SET feed_retained_from=feed_tail, feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);").unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store = store.clone();
    config.session = store.load(&id).unwrap();
    let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut reopened =
        RootActor::open_blocking(config, &action_rx, Sender::over(event_tx), None).unwrap();
    assert_eq!(reopened.role.config.session.root_turn_count, 0);
    actions
        .send(crate::surface_protocol::UserAction::ResumeQueueDrain)
        .unwrap();
    assert!(matches!(
        reopened
            .block(async |reopened| reopened.next_prompt(None).await)
            .unwrap(),
        super::super::HostStep::Continue
    ));
    assert_eq!(reopened.role.config.session.root_turn_count, 0);
    assert_eq!(
        reopened.role.config.session.pending_queue,
        [] as [sessions::queue::QueueItem; 0]
    );
    let intents = store.load_intents(&id).unwrap();
    assert_eq!(intents.submissions, vec![intent.clone()]);
    let views = store.load_views(&id).unwrap().unwrap();
    assert!(intents.submission_consumed(&intent.command_id, views.state.retained_from));
    assert_eq!(
        views.state.deliveries.get(&intent.command_id),
        Some(&sessions::projector::Delivery::Consumed)
    );
    assert_eq!(event_rx.try_iter().filter(|event| matches!(event, HostEvent::Row(row) if row.text() == Some("queued prompt already ran; its result is no longer kept"))).count(), 0);
    assert_eq!(
        store
            .load(&id)
            .unwrap()
            .transcript
            .iter()
            .filter(
                |row| row.text() == Some("queued prompt already ran; its result is no longer kept")
            )
            .count(),
        0
    );
    reopened
        .block(async |reopened| reopened.release().await)
        .unwrap();
}
