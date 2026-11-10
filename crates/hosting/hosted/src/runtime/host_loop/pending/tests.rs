use super::super::RootActor;
use super::{AUTO_CONTINUE_PAUSE_TEXT, PendingContinuation};
use crate::agent_tree::ProductAgentTree;
use crate::agent_tree_protocol::{
    MessagePurpose, SendAgentMessageRequest, SpawnAgentRequest, SubmitResultRequest, WriterMode,
};
use crate::runtime::test_kit::BlockOn as _;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::fresh_injected_attached_host;
use crate::sessions::{NoticeScope, NoticeTone, TranscriptRow};
use crate::surface_protocol::ContentEvent;
use crate::surface_protocol::{HostEventReceiver, UserAction, UserActionSender};
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) fn snapshots(observed: &HostEventReceiver<TestExtension>) -> Vec<PendingContinuation> {
    observed
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::PendingContinuation {
                results,
                questions,
                process_exits,
                auto_continue_paused,
            } => Some(PendingContinuation {
                results,
                questions,
                process_exits,
                auto_continue_paused,
            }),
            _ => None,
        })
        .collect()
}

pub(super) fn pause_rows(session: &crate::sessions::SessionRecord, text: &str) -> usize {
    session
        .transcript
        .rows()
        .iter()
        .filter(|(row, _)| {
            matches!(
                row,
                TranscriptRow::Notice(notice)
                    if notice.scope == NoticeScope::Tree
                        && notice.tone == NoticeTone::Warning
                        && notice.text == text
            )
        })
        .count()
}

fn with_tree(
    results: usize,
    test: impl FnOnce(
        &mut RootActor<'_, TestExtension>,
        &Arc<crate::topology_runtime::ProductTopologyRuntime>,
        &UserActionSender<TestExtension>,
        &HostEventReceiver<TestExtension>,
    ),
) {
    with_configured_tree(results, |_| {}, test);
}

pub(super) fn with_configured_tree(
    results: usize,
    configure: impl FnOnce(&mut crate::runtime::HostConfig<TestExtension>),
    test: impl FnOnce(
        &mut RootActor<'_, TestExtension>,
        &Arc<crate::topology_runtime::ProductTopologyRuntime>,
        &UserActionSender<TestExtension>,
        &HostEventReceiver<TestExtension>,
    ),
) {
    let dir = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(dir.path());
    configure(&mut config);
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        &config.session.id,
        "root",
        "owner",
        config.session.workspace_root.clone(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    for _ in 0..results {
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
    }
    tree.settle_for_test();
    let runtime = crate::topology_runtime::insert_root(config.session.id.clone(), tree, None)
        .block()
        .unwrap();
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(
        config,
        &receiver,
        crate::runtime::Sender::over(events),
        None,
    )
    .unwrap();
    test(&mut host, &runtime, &actions, &observed);
    host.block(async |host| host.release().await).unwrap();
}

pub(super) fn reopen(
    host: &mut RootActor<'_, TestExtension>,
    test: impl FnOnce(&mut RootActor<'_, TestExtension>),
) {
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    host.role
        .executor
        .block_on(host.opened.detach())
        .unwrap()
        .unwrap();
    let root = host.role.config.product_home.parent().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root);
    config.state_roots = host.role.config.state_roots.clone();
    config.store = host.role.config.store.clone();
    config.session = config.store.load(&host.role.config.session.id).unwrap();
    config.runtime_factory = host.role.config.runtime_factory.clone();
    config.interactive = host.role.config.interactive;
    let mut restored =
        RootActor::open_blocking(config, host.role.actions, host.role.events.clone(), None)
            .unwrap();
    test(&mut restored);
    restored
        .role
        .executor
        .block_on(restored.opened.detach())
        .unwrap()
        .unwrap();
    host.opened = restored.opened;
    host.role.config.session = restored.role.config.session;
}

/// A running child: something that could wake the root, with nothing
/// pending yet.
pub(super) fn spawn_child(runtime: &crate::topology_runtime::ProductTopologyRuntime) {
    runtime
        .mutate_then_emit(false, |tree| {
            tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .map_err(|error| error.message)
        })
        .block()
        .unwrap();
}

fn notes(events: &[HostEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::Row(TranscriptRow::Notice(notice)) => Some(notice.text.clone()),
            _ => None,
        })
        .collect()
}

fn take_prompt(step: super::super::HostStep) -> (String, Option<crate::sessions::QueueItem>, bool) {
    let super::super::HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = step
    else {
        panic!("expected a prompt")
    };
    (text, queued_delivery, user_echoed)
}

fn last_prompt_times(observed: &HostEventReceiver<TestExtension>) -> Vec<Option<u64>> {
    observed
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::LastUserPromptAt(sent) => Some(sent),
            _ => None,
        })
        .collect()
}

/// The offline fixture's driver clock: every entry identity it mints is
/// a `UUIDv7` stamped at this Unix time.
const FIXTURE_CLOCK_MS: u64 = 10_000;

/// Spec 02 §5.4: the idle clock survives reopen. The attach reads the
/// operator's last prompt time from the entry identity in canonical
/// history; nothing new is stored.
#[test]
fn reopen_restores_the_stored_time_of_the_last_operator_prompt() {
    with_tree(0, |host, _, actions, observed| {
        assert_eq!(last_prompt_times(observed), [None], "no prompt yet");
        actions.send(UserAction::Prompt("hello".into())).unwrap();
        let (text, item, echoed) = take_prompt(
            host.block(async |host| host.next_prompt(None).await)
                .unwrap(),
        );
        host.block(async |host| host.run_prompt(text, item, echoed, None).await)
            .unwrap();
        let _ = last_prompt_times(observed);
        reopen(host, |_| {
            assert_eq!(last_prompt_times(observed), [Some(FIXTURE_CLOCK_MS)]);
        });
    });
}

/// An auto-continue wake is host-authored: a session whose only turn was
/// a wake has no operator prompt to restore.
#[test]
fn a_wake_turn_restores_no_prompt_time() {
    with_tree(1, |host, _, _, observed| {
        assert_eq!(last_prompt_times(observed), [None]);
        let now = Instant::now();
        host.block(async |host| host.child_wake_prompt(now).await)
            .unwrap();
        let (text, item, echoed) = take_prompt(
            host.block(async |host| host.child_wake_prompt(now + Duration::from_secs(2)).await)
                .unwrap(),
        );
        assert!(item.is_some(), "the wake is a host-authored delivery");
        host.block(async |host| host.run_prompt(text, item, echoed, None).await)
            .unwrap();
        assert_eq!(host.role.config.session.root_turn_count, 1, "the wake ran");
        reopen(host, |_| {
            assert_eq!(last_prompt_times(observed), [None]);
        });
    });
}

#[test]
fn unread_results_raise_the_indicator_clear_after_the_turn_and_restore() {
    with_tree(1, |host, _, _, observed| {
        let raised = snapshots(observed);
        assert_eq!(raised.last().map(|pending| pending.results), Some(1));
        reopen(host, |restored| {
            let again = snapshots(observed);
            assert_eq!(again.last().map(|pending| pending.results), Some(1));
            let now = Instant::now();
            restored
                .block(async |restored| restored.child_wake_prompt(now).await)
                .unwrap();
            let (text, item, echoed) = take_prompt(
                restored
                    .block(async |restored| {
                        restored
                            .child_wake_prompt(now + Duration::from_secs(2))
                            .await
                    })
                    .unwrap(),
            );
            restored
                .block(async |restored| restored.run_prompt(text, item, echoed, None).await)
                .unwrap();
            let cleared = snapshots(observed);
            assert!(
                cleared.iter().any(|pending| pending.results == 0),
                "result indicator did not clear: {cleared:?}"
            );
        });
    });
}

#[test]
fn pending_store_observations_refresh_acknowledged_results() {
    use crate::runtime::topology_state::TreeRows;

    let dir = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(dir.path());
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        &config.session.id,
        "root",
        "owner",
        config.session.workspace_root.clone(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap();
    tree.submit_result(
        &child.agent_id,
        &SubmitResultRequest {
            turn_token: 1,
            output_json: r#"{"ok":true}"#.into(),
        },
        None,
    )
    .unwrap();
    tree.record_turn_completed(&child.agent_id).unwrap();
    tree.settle_for_test();
    config.topology_rows().save_tree(&mut tree).unwrap();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(
        config,
        &receiver,
        crate::runtime::Sender::over(events),
        None,
    )
    .unwrap();
    assert!(crate::topology_runtime::lookup(&host.role.config.session.id).is_none());
    host.publish_pending_continuation();
    assert_eq!(
        snapshots(&observed).last().map(|pending| pending.results),
        Some(1)
    );
    let rows = host.role.config.topology_rows();
    let mut tree = rows.tree(&host.role.config.session.id).unwrap().unwrap();
    let mail = tree.unread_mail("root").next().unwrap().message_id.clone();
    tree.acknowledge_mail(&mail).unwrap();
    rows.save_tree(&mut tree).unwrap();
    host.role.pending_track.store_observed_at =
        Instant::now().checked_sub(super::super::TREE_OBSERVE_INTERVAL);
    host.publish_pending_continuation();
    assert_eq!(
        snapshots(&observed).last().map(|pending| pending.results),
        Some(0)
    );
    host.block(async |host| host.release().await).unwrap();
}

#[test]
fn consumed_child_result_zero_survives_lifecycle_saturation() {
    with_tree(1, |host, runtime, _, observed| {
        assert_eq!(snapshots(observed).last().unwrap().results, 1);
        runtime
            .mutate_then_emit(false, |tree| {
                let id = tree.unread_mail("root").next().unwrap().message_id.clone();
                tree.acknowledge_mail(&id).map_err(|error| error.message)
            })
            .block()
            .unwrap();
        host.publish_pending_continuation();
        assert_eq!(host.role.pending_track.last.unwrap().results, 0);
        // The face still displays one. Admission traffic used to evict the
        // accepted zero, which unchanged facts never caused the host to resend.
        for _ in 0..1024 {
            host.role.events.send(ContentEvent::IntentStarted(None));
        }
        let delivered = snapshots(observed);
        host.publish_pending_continuation();
        assert_eq!(delivered.last().map(|pending| pending.results), Some(0));
        assert_eq!(snapshots(observed), [] as [PendingContinuation; 0]);
    });
}

#[test]
fn pending_refused_zero_is_resent_when_facts_are_unchanged() {
    with_tree(1, |host, runtime, _, observed| {
        assert_eq!(snapshots(observed).last().unwrap().results, 1);
        runtime
            .mutate_then_emit(false, |tree| {
                let id = tree.unread_mail("root").next().unwrap().message_id.clone();
                tree.acknowledge_mail(&id).map_err(|error| error.message)
            })
            .block()
            .unwrap();
        // Terminal receipts cannot be evicted to admit the zero snapshot.
        while host
            .role
            .events
            .send(ContentEvent::ModelRefusal { detail: None })
        {}
        host.publish_pending_continuation();
        let _ = snapshots(observed);
        host.publish_pending_continuation();
        assert_eq!(
            snapshots(observed).last().map(|pending| pending.results),
            Some(0)
        );
    });
}

#[test]
fn pending_wake_and_run_prompt_stays_clear_across_idle_iterations() {
    with_tree(1, |host, _, actions, observed| {
        assert_eq!(snapshots(observed).last().unwrap().results, 1);
        let now = Instant::now();
        host.block(async |host| host.child_wake_prompt(now).await)
            .unwrap();
        let (text, item, echoed) = take_prompt(
            host.block(async |host| host.child_wake_prompt(now + Duration::from_secs(2)).await)
                .unwrap(),
        );
        host.block(async |host| host.run_prompt(text, item, echoed, None).await)
            .unwrap();
        let mut seen = snapshots(observed);
        for _ in 0..10 {
            actions.send(UserAction::Interrupt).unwrap();
            let _ = host
                .block(async |host| host.next_prompt(None).await)
                .unwrap();
            seen.extend(snapshots(observed));
            assert_eq!(host.role.pending_track.last.unwrap().results, 0);
        }
        assert_eq!(seen.last().map(|pending| pending.results), Some(0));
        assert!(seen.iter().all(|pending| pending.results == 0), "{seen:?}");
    });
}

#[test]
fn unread_questions_raise_and_clear_the_indicator() {
    with_tree(0, |host, runtime, _, observed| {
        let _ = snapshots(observed);
        let child = runtime
            .mutate_then_emit(false, |tree| {
                tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .map_err(|error| error.message)
            })
            .block()
            .unwrap();
        runtime
            .mutate_then_emit(false, {
                let child = child.clone();
                move |tree| {
                    tree.send_agent_message(
                        &child.agent_id,
                        &SendAgentMessageRequest {
                            refresh: None,
                            agent_id: "root".into(),
                            message: "question".into(),
                            purpose: Some(MessagePurpose::Question),
                            priority: None,
                            in_reply_to: None,
                            output_schema_json: None,
                        },
                        None,
                    )
                    .map_err(|error| error.message)
                }
            })
            .block()
            .unwrap();
        host.publish_pending_continuation();
        let raised = snapshots(observed);
        assert_eq!(raised.last().map(|pending| pending.questions), Some(1));
        let now = Instant::now();
        host.block(async |host| host.child_wake_prompt(now).await)
            .unwrap();
        let (text, item, echoed) = take_prompt(
            host.block(async |host| host.child_wake_prompt(now + Duration::from_secs(2)).await)
                .unwrap(),
        );
        host.block(async |host| host.run_prompt(text, item, echoed, None).await)
            .unwrap();
        let cleared = snapshots(observed);
        assert!(
            cleared.iter().any(|pending| pending.questions == 0),
            "question indicator did not clear: {cleared:?}"
        );
    });
}

#[test]
fn auto_continue_pause_raises_a_notice_and_restores_the_same_row() {
    with_tree(0, |host, runtime, actions, observed| {
        spawn_child(runtime);
        let _ = snapshots(observed);
        actions.send(UserAction::Interrupt).unwrap();
        assert!(matches!(
            host.block(async |host| host.next_prompt(None).await)
                .unwrap(),
            super::super::HostStep::Continue
        ));
        let raised = snapshots(observed);
        assert!(
            raised
                .last()
                .is_some_and(|pending| pending.auto_continue_paused)
        );
        assert_eq!(
            pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
            1
        );
        reopen(host, |restored| {
            assert_eq!(
                pause_rows(&restored.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                1,
                "restore duplicated or dropped the pause notice"
            );
            let again = snapshots(observed);
            assert!(
                again
                    .last()
                    .is_some_and(|pending| pending.auto_continue_paused)
            );
            assert!(
                restored
                    .role
                    .config
                    .session
                    .transcript
                    .rows()
                    .iter()
                    .any(|(row, _)| {
                        *row == &TranscriptRow::warning(NoticeScope::Tree, AUTO_CONTINUE_PAUSE_TEXT)
                    })
            );
        });
    });
}

/// The attach-time paused-queue note restates state every open
/// re-announces, so it is live only: reopening a still-paused session
/// never adds a copy.
#[test]
fn reopening_a_paused_queue_does_not_grow_the_transcript() {
    with_tree(0, |host, _, _, _| {
        host.role.config.session.queue_drain_state =
            crate::sessions::QueueDrainState::RestoredPaused;
        host.role
            .config
            .session
            .pending_queue
            .push(crate::sessions::QueueItem::new("held prompt"));
        let queue_notes = |session: &crate::sessions::SessionRecord| {
            session
                .transcript
                .iter()
                .filter(|row| {
                    matches!(
                        row,
                        TranscriptRow::Notice(notice)
                            if notice.scope == NoticeScope::Queue
                                && notice.text.contains("the queue is paused")
                    )
                })
                .count()
        };
        for _ in 0..3 {
            reopen(host, |restored| {
                restored.commit_notice_intents();
            });
        }
        let stored = host
            .role
            .config
            .store
            .load(&host.role.config.session.id)
            .unwrap();
        assert_eq!(
            queue_notes(&stored),
            0,
            "three opens of one paused session stored copies of the open note"
        );
    });
}

/// Spec 02 §5.4: a notice an open re-announces is live only, and the
/// interactive pause row persists once per episode, so after the first
/// open of a paused session later opens add nothing to its transcript.
#[test]
fn reopening_an_unchanged_paused_session_stores_nothing_new() {
    with_tree(0, |host, _, _, _| {
        host.role.config.session.queue_drain_state =
            crate::sessions::QueueDrainState::RestoredPaused;
        host.role
            .config
            .session
            .pending_queue
            .push(crate::sessions::QueueItem::new("held prompt"));
        reopen(host, |restored| {
            restored.commit_notice_intents();
        });
        let first = host
            .role
            .config
            .store
            .load(&host.role.config.session.id)
            .unwrap();
        for _ in 0..2 {
            reopen(host, |restored| {
                restored.commit_notice_intents();
            });
        }
        let stored = host
            .role
            .config
            .store
            .load(&host.role.config.session.id)
            .unwrap();
        assert_eq!(stored.transcript, first.transcript);
    });
}

#[test]
fn a_jsonl_face_writes_no_pause_rows() {
    with_configured_tree(
        0,
        |config| config.interactive = false,
        |host, runtime, actions, observed| {
            spawn_child(runtime);
            actions.send(UserAction::Interrupt).unwrap();
            let _ = host
                .block(async |host| host.next_prompt(None).await)
                .unwrap();
            host.publish_pending_continuation();
            reopen(host, |restored| {
                let events: Vec<_> = observed.try_iter().collect();
                let notes = notes(&events);
                assert!(
                    !notes.iter().any(|note| note == AUTO_CONTINUE_PAUSE_TEXT),
                    "a JSONL face gained a pause line: {notes:?}"
                );
                assert_eq!(
                    pause_rows(&restored.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                    0
                );
            });
        },
    );
}

#[test]
fn interrupt_with_nothing_that_could_wake_shows_no_pause() {
    with_tree(0, |host, _, actions, observed| {
        let before = host.role.config.session.clone();
        actions.send(UserAction::Interrupt).unwrap();
        let _ = host
            .block(async |host| host.next_prompt(None).await)
            .unwrap();
        assert!(!host.role.config.session.child_wake_suppressed);
        assert_eq!(host.role.config.session, before);
        assert!(
            !snapshots(observed)
                .iter()
                .any(|pending| pending.auto_continue_paused)
        );
        assert_eq!(
            pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
            0
        );
    });
}

#[test]
fn a_new_suppression_after_an_admitted_prompt_is_a_new_pause_line() {
    with_tree(0, |host, runtime, actions, _| {
        spawn_child(runtime);
        actions.send(UserAction::Interrupt).unwrap();
        let _ = host
            .block(async |host| host.next_prompt(None).await)
            .unwrap();
        assert_eq!(
            pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
            1
        );
        // An admitted user prompt ends the episode; an interrupt that the
        // idle loop never saw unpaused starts the next one.
        host.role.config.session.root_turn_count += 1;
        host.publish_pending_continuation();
        assert_eq!(
            pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
            2
        );
        host.publish_pending_continuation();
        assert_eq!(
            pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
            2
        );
    });
}

#[test]
fn a_pause_the_wake_path_already_reported_writes_no_second_line() {
    with_tree(0, |host, runtime, _, observed| {
        spawn_child(runtime);
        host.role.config.session.child_wake_suppressed = true;
        host.auto_continue_pause_reported();
        host.publish_pending_continuation();
        assert!(
            snapshots(observed)
                .last()
                .is_some_and(|pending| pending.auto_continue_paused)
        );
        assert_eq!(
            pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
            0
        );
    });
}

/// An in-place resume is an attach of the resumed session; it keeps that
/// session's already recorded pause row single.
///
/// The clear's source attachment release is held half a second past its runtime
/// release, so the resume always arrives while it is still pending: the
/// switch joins it before reading the source, never a copy whose clear
/// workflow is still unresolved.
#[test]
fn in_place_resume_keeps_a_recorded_pause_row_single() {
    let slow_release = |config: &mut crate::runtime::HostConfig<TestExtension>| {
        config.runtime_factory = crate::runtime::test_kit::scripted(config)
            .with_source_release_delay(Duration::from_millis(500));
    };
    with_configured_tree(0, slow_release, |host, runtime, actions, _| {
        spawn_child(runtime);
        actions.send(UserAction::Interrupt).unwrap();
        let _ = host
            .block(async |host| host.next_prompt(None).await)
            .unwrap();
        assert_eq!(
            pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
            1
        );
        let paused = host.role.config.session.id.clone();
        actions.send(UserAction::NewSession).unwrap();
        let _ = host
            .block(async |host| host.next_prompt(None).await)
            .unwrap();
        assert_ne!(host.role.config.session.id, paused, "clear did not switch");
        let _ = host
            .block(async |host| host.next_prompt(None).await)
            .unwrap();
        actions
            .send(UserAction::ResumeSession {
                id: Some(paused.clone()),
            })
            .unwrap();
        let _ = host
            .block(async |host| host.next_prompt(None).await)
            .unwrap();
        assert_eq!(host.role.config.session.id, paused, "resume did not switch");
        assert_eq!(
            host.role.config.session.plakous_locator,
            Some(host.opened.locator())
        );
        assert!(host.role.config.session.child_wake_suppressed);
        let _ = host
            .block(async |host| host.next_prompt(None).await)
            .unwrap();
        assert_eq!(
            pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
            1,
            "in-place resume wrote a second pause row"
        );
    });
}

/// The injection the attachment release tests below run under: `on_release`
/// runs on every source attachment release's thread after runtime
/// release, before the host's own observer.
fn release_hook(
    config: &mut crate::runtime::HostConfig<TestExtension>,
    on_release: impl Fn() + Send + Sync + 'static,
) {
    let injection = crate::runtime::test_kit::scripted(config);
    let observe = Arc::clone(&injection.observe);
    config.runtime_factory = Arc::new(crate::runtime::ScriptedRuntimeFactory {
        runtime: Arc::clone(&injection.runtime),
        rebind_target: injection.rebind_target.clone(),
        observe: Arc::new(move |workflow, boundary| {
            if workflow == crate::runtime::HostWorkflow::SourceRelease {
                on_release();
            }
            observe(workflow, boundary);
        }),
        child_runtime: injection.child_runtime.clone(),
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
}

/// Only the first release (the clear's, of the source) runs
/// `on_release`; the resume's own release of the clear target
/// finishes normally, so teardown stays short and clean.
fn first_release_hook(
    config: &mut crate::runtime::HostConfig<TestExtension>,
    on_release: impl Fn() + Send + Sync + 'static,
) {
    let first = std::sync::atomic::AtomicBool::new(true);
    release_hook(config, move || {
        if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            on_release();
        }
    });
}

/// Clears off the paused session, then resumes back onto it in place.
/// Returns the source id and how long the resume step held the host.
fn clear_then_resume_back(
    host: &mut RootActor<'_, TestExtension>,
    runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    actions: &UserActionSender<TestExtension>,
) -> (String, Duration) {
    spawn_child(runtime);
    actions.send(UserAction::Interrupt).unwrap();
    let _ = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();
    let source = host.role.config.session.id.clone();
    actions.send(UserAction::NewSession).unwrap();
    let _ = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();
    assert_ne!(host.role.config.session.id, source, "clear did not switch");
    let _ = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();
    let started = Instant::now();
    resume(host, actions, &source);
    (source, started.elapsed())
}

fn resume(
    host: &mut RootActor<'_, TestExtension>,
    actions: &UserActionSender<TestExtension>,
    id: &str,
) {
    actions
        .send(UserAction::ResumeSession {
            id: Some(id.to_owned()),
        })
        .unwrap();
    let _ = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();
}

fn error_rows(observed: &HostEventReceiver<TestExtension>) -> Vec<String> {
    observed
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(TranscriptRow::Notice(notice)) if notice.tone == NoticeTone::Error => {
                Some(notice.text)
            }
            _ => None,
        })
        .collect()
}

fn assert_attached_resolved(host: &RootActor<'_, TestExtension>, source: &str, errors: &[String]) {
    assert_eq!(
        host.role.config.session.id, source,
        "resume did not switch back onto the source: {errors:?}"
    );
    assert_eq!(
        host.role.config.session.plakous_locator,
        Some(host.opened.locator())
    );
    let sessions = host.role.config.state_roots.runtime_sessions().unwrap();
    let projected = host
        .role
        .executor
        .block_on(crate::runtime::projection::project_session(
            &sessions,
            &host.role.config.store,
            &host.role.config.session,
        ))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(projected.head, host.opened.locator());
}

/// Releasing an attachment never competes with an operator intent write.
#[test]
fn source_release_does_not_race_an_operator_intent_write() {
    let racing_writer = |config: &mut crate::runtime::HostConfig<TestExtension>| {
        let store = Arc::clone(&config.store);
        let source = config.session.id.clone();
        first_release_hook(config, move || {
            let mut stored = store.load(&source).unwrap();
            store.save(&mut stored).unwrap();
        });
    };
    with_configured_tree(0, racing_writer, |host, runtime, actions, observed| {
        let (source, _) = clear_then_resume_back(host, runtime, actions);
        let errors = error_rows(observed);
        assert!(
            errors.is_empty(),
            "attachment release must not save the source: {errors:?}"
        );
        assert_attached_resolved(host, &source, &errors);
    });
}

/// A release-thread panic is reported; switch-back still follows the feed.
#[test]
fn a_panicked_source_release_is_reported_and_resume_projects_the_head() {
    let panicking = |config: &mut crate::runtime::HostConfig<TestExtension>| {
        first_release_hook(config, || panic!("injected source release panic"));
    };
    with_configured_tree(0, panicking, |host, runtime, actions, observed| {
        let (source, _) = clear_then_resume_back(host, runtime, actions);
        let errors = error_rows(observed);
        assert!(
            errors
                .iter()
                .any(|text| text.contains("source release panicked")),
            "a panicked source attachment release left no notice: {errors:?}"
        );
        assert_attached_resolved(host, &source, &errors);
    });
}

/// A release that does not finish holds a switch back onto its
/// source for `SETTLE_DEADLINE` at most: the resume is refused and the
/// release stays tracked, so a retry once it has landed attaches
/// the source from its projected head.
#[test]
fn a_resume_onto_a_stuck_source_release_is_refused_within_its_bound() {
    let (release_attachment, held_attachment) = std::sync::mpsc::channel::<()>();
    let held_attachment = std::sync::Mutex::new(held_attachment);
    let stuck = move |config: &mut crate::runtime::HostConfig<TestExtension>| {
        first_release_hook(config, move || {
            let _ = held_attachment
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(60));
        });
    };
    with_configured_tree(0, stuck, |host, runtime, actions, observed| {
        let (source, held) = clear_then_resume_back(host, runtime, actions);
        assert!(
            held < crate::runtime::HOST_RELEASE_DEADLINE,
            "the resume held the host worker for {held:?} waiting on the source's attachment release"
        );
        assert_ne!(
            host.role.config.session.id, source,
            "the resume attached a source whose finalization was still running"
        );
        let errors = error_rows(observed);
        assert!(
            errors
                .iter()
                .any(|text| text.contains("still finishing its previous switch")),
            "the refused resume did not say why: {errors:?}"
        );
        drop(release_attachment);
        resume(host, actions, &source);
        assert_attached_resolved(host, &source, &error_rows(observed));
    });
}

/// With auto-continue disabled an interrupt has nothing to pause.
#[test]
fn interrupt_without_auto_continue_shows_no_pause() {
    let dir = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(dir.path());
    config.auto_continue = false;
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(
        config,
        &receiver,
        crate::runtime::Sender::over(events),
        None,
    )
    .unwrap();
    assert!(!host.role.config.prompt_host_facts().host_wakes);
    actions.send(UserAction::Interrupt).unwrap();
    let _ = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();
    let seen = snapshots(&observed);
    let rows = pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT);
    host.block(async |host| host.release().await).unwrap();
    assert!(
        !seen.iter().any(|pending| pending.auto_continue_paused) && rows == 0,
        "auto-continue disabled, yet indicator {seen:?} and {rows} pause rows"
    );
}

/// A tree lock held elsewhere does not end the pause episode: the HUD
/// does not flap and no second row is written.
///
/// Each round the holder takes the tree lock first and only then lets the
/// host publish, so every publish starts against a held lock. The holder
/// releases when the publish finishes (a read that does not wait) or after
/// a short hold (a read that waits), and takes the lock again only after
/// the publish returned: a holder that re-locked in a tight loop could
/// starve the host's wait indefinitely on a loaded machine.
#[test]
fn contended_tree_lock_neither_flaps_nor_rewrites_the_pause_row() {
    const ROUNDS: usize = 200;
    const HOLD: Duration = Duration::from_millis(1);
    with_tree(0, |host, runtime, actions, observed| {
        spawn_child(runtime);
        actions.send(UserAction::Interrupt).unwrap();
        let _ = host
            .block(async |host| host.next_prompt(None).await)
            .unwrap();
        host.publish_pending_continuation();
        assert_eq!(
            pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
            1
        );
        let _ = snapshots(observed);
        let (held, held_rx) = std::sync::mpsc::channel::<()>();
        let (published, published_rx) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|scope| {
            // Owned here, so a panicking side drops its end and the other
            // side fails instead of waiting forever.
            let (published, held_rx) = (published, held_rx);
            scope.spawn(move || {
                for _ in 0..ROUNDS {
                    let finished_under_hold = runtime
                        .read(|_| {
                            held.send(()).unwrap();
                            published_rx.recv_timeout(HOLD).is_ok()
                        })
                        .unwrap();
                    if !finished_under_hold {
                        published_rx.recv().unwrap();
                    }
                }
            });
            for _ in 0..ROUNDS {
                held_rx.recv().unwrap();
                host.publish_pending_continuation();
                published.send(()).unwrap();
            }
        });
        let flaps = snapshots(observed)
            .iter()
            .filter(|pending| !pending.auto_continue_paused)
            .count();
        assert_eq!(
            (
                pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                flaps
            ),
            (1, 0),
            "(pause rows, unpaused snapshots) under tree lock contention"
        );
    });
}

/// A root turn that fails with exhausted quota pauses auto-continue live:
/// one pause line while the child that could wake it runs, and no second
/// copy after reopen.
#[test]
fn a_quota_exhausted_root_turn_writes_one_live_pause_line() {
    let quota = |config: &mut crate::runtime::HostConfig<TestExtension>| {
        let workspace = config.cwd.clone();
        let root = config.product_home.parent().unwrap().to_path_buf();
        let controller =
            plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                root.join("quota-events.jsonl"),
            )
            .unwrap();
        let binding =
            |workspace: &std::path::Path,
             controller: plakous_offline_crash_harness::BoundaryController| {
                plakous_offline_crash_harness::projected_terminal_failure_runtime_binding(
                    workspace,
                    controller,
                    plakous::model::ModelErrorKind::QuotaExceeded,
                )
            };
        let target = binding(&workspace, controller.clone())
            .unwrap()
            .target_binding();
        config.runtime_factory = Arc::new(crate::runtime::ScriptedRuntimeFactory {
            rebind_target: target,
            runtime: Arc::new(move |_| {
                binding(&workspace, controller.clone()).map_err(|error| error.to_string())
            }),
            observe: Arc::new(|_, _| {}),
            child_runtime: None,
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        });
    };
    with_configured_tree(0, quota, |host, runtime, actions, _| {
        spawn_child(runtime);
        actions
            .send(UserAction::Prompt("work until the limit".into()))
            .unwrap();
        let (text, queued_delivery, user_echoed) = take_prompt(
            host.block(async |host| host.next_prompt(None).await)
                .unwrap(),
        );
        host.block(async |host| {
            host.run_prompt(text, queued_delivery, user_echoed, None)
                .await
        })
        .unwrap();
        assert!(host.role.config.session.child_wake_suppressed);
        host.publish_pending_continuation();
        host.publish_pending_continuation();
        assert_eq!(
            pause_rows(&host.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
            1
        );
        reopen(host, |restored| {
            assert!(restored.role.config.session.child_wake_suppressed);
            restored.publish_pending_continuation();
            assert_eq!(
                pause_rows(&restored.role.config.session, AUTO_CONTINUE_PAUSE_TEXT),
                1
            );
        });
    });
}
