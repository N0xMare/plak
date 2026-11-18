#![allow(clippy::unwrap_used, reason = "unit tests")]

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::runtime::test_kit::{self, ProbeIntent, TestExtension};
use crate::runtime::test_support::{fresh_injected_attached_host, send_attached_action};
use crate::runtime::{
    ReadinessRead, RuntimeRole, SurfaceEffect, SurfaceFacts, SurfaceHandler, spawn_host_worker,
};
use crate::session_store::SessionRecordStore;
use plakous_host_protocol::TurnPhase;

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

#[test]
fn continue_ids_are_stable_per_turn_and_account() {
    let first = ContinueIds::of("turn-a", "codex/work");
    assert_eq!(first, ContinueIds::of("turn-a", "codex/work"));
    assert_ne!(first, ContinueIds::of("turn-b", "codex/work"));
    assert_ne!(first, ContinueIds::of("turn-a", "codex/other"));
    assert!(first.operation_id.starts_with(CONTINUE_OPERATION));
    assert!(uuid::Uuid::parse_str(&first.item_id).is_ok());
    assert_eq!(
        first.command_id(),
        format!("plak.queue.{}.1", first.item_id)
    );
}

fn turn(prompt: Option<&str>, committed: bool) -> LastRootTurn {
    LastRootTurn {
        turn_id: "t".into(),
        command_id: "c".into(),
        outcome: Some(LastRootOutcome::Failed),
        prompt: prompt.map(str::to_owned),
        committed_after_prompt: committed,
        history_seen: true,
    }
}

#[test]
fn a_continue_resubmits_the_exact_prompt_or_the_pinned_continuation() {
    assert_eq!(continue_text(&turn(Some("do it"), false)), "do it");
    assert_eq!(
        continue_text(&turn(Some("do it"), true)),
        CONTINUATION_PROMPT
    );
    assert_eq!(continue_text(&turn(None, false)), CONTINUATION_PROMPT);
}

/// Hands the host one queued account choice per probe and records outcomes.
#[derive(Default)]
struct Chooser {
    next: Mutex<Option<SurfaceAccountChoice>>,
    outcomes: Mutex<Vec<AccountChoiceOutcome>>,
}

impl SurfaceHandler<TestExtension> for Arc<Chooser> {
    fn dispatch(&self, _: ProbeIntent, _: SurfaceFacts<'_>, _: &Sender) -> SurfaceEffect {
        let Some(choice) = self.next.lock().unwrap().take() else {
            return SurfaceEffect::default();
        };
        let recorder = Arc::clone(self);
        SurfaceEffect {
            account: Some(surface::SurfaceAccountEffect {
                choice,
                finish: Box::new(move |outcome| recorder.outcomes.lock().unwrap().push(outcome)),
            }),
            ..SurfaceEffect::default()
        }
    }

    fn ready_alternatives(&self, account: &str) -> Vec<String> {
        ["codex", "codex/b"]
            .into_iter()
            .filter(|other| *other != account)
            .map(str::to_owned)
            .collect()
    }

    fn publish_template_readiness(
        &self,
        _: &[plakous_host_protocol::views::ProviderSection],
        _: &Sender,
    ) {
    }

    fn readiness_read(&self) -> ReadinessRead {
        Box::new(|| Ok((Vec::new(), Vec::new())))
    }
}

fn wait_for(
    events: &crate::surface_protocol::HostEventReceiver<TestExtension>,
    seen: &mut Vec<HostEvent>,
    from: usize,
    mut done: impl FnMut(&[HostEvent]) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done(&seen[from..]) {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "host event timed out: {:?}", seen.last());
        seen.push(events.recv_timeout(left).unwrap());
    }
}

fn lifecycle(events: &[HostEvent], phase: TurnPhase) -> Vec<plakous_host_protocol::TurnLifecycle> {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::TurnLifecycle(lifecycle) if lifecycle.phase == phase => {
                Some(lifecycle.clone())
            }
            _ => None,
        })
        .collect()
}

fn idle_after(events: &[HostEvent], phase: TurnPhase) -> bool {
    events
        .iter()
        .rposition(|event| matches!(event, HostEvent::TurnLifecycle(l) if l.phase == phase))
        .is_some_and(|at| {
            events[at..].iter().any(|event| {
                matches!(
                    event,
                    HostEvent::Status {
                        phase: HostPhase::Idle,
                        ..
                    }
                )
            })
        })
}

fn line_head(store: &dyn SessionRecordStore, tree: &str) -> SessionRecord {
    store.root_line_head(tree).unwrap().unwrap()
}

fn user_rows(store: &dyn SessionRecordStore, id: &str) -> Vec<String> {
    store
        .load_views(id)
        .unwrap()
        .unwrap()
        .rows
        .values()
        .filter_map(|row| match row {
            TranscriptRow::User { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// A host whose scripted model fails the first turn on its usage limit and
/// answers the next, on `codex` and, after a rebind, on `codex/b`; `cut`
/// stops the host at the nth crossing of one workflow boundary, and
/// `primary_account` is the identity a reopened head was recorded under.
fn continue_host(
    root: &std::path::Path,
    cut: Option<(
        crate::runtime::HostWorkflow,
        crate::runtime::HostBoundary,
        usize,
    )>,
    primary_account: &'static [u8],
) -> (crate::runtime::HostConfig<TestExtension>, Arc<Chooser>) {
    let (mut config, _) = fresh_injected_attached_host(root);
    let workspace = config.cwd.clone();
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        root.join("continue-evidence.jsonl"),
    )
    .unwrap();
    let script: Arc<plakous_offline_crash_harness::script::ScriptPlan> = Arc::new(
        serde_json::from_value(serde_json::json!({"version": 1, "steps": [
            {"fault": {"at": "pre_stream", "kind": "quota_exceeded"}},
            {"output": {"text": "continued"}},
            {"fault": {"at": "pre_stream", "kind": "quota_exceeded"}},
            {"output": {"text": "continued again"}}
        ]}))
        .unwrap(),
    );
    let cursor = Arc::new(AtomicU64::new(1));
    let bind = move |account: &'static [u8]| {
        let (workspace, controller, script, cursor) = (
            workspace.clone(),
            controller.clone(),
            Arc::clone(&script),
            Arc::clone(&cursor),
        );
        move || {
            plakous_offline_crash_harness::scripted_runtime_binding_for_account(
                &workspace,
                controller.clone(),
                Arc::clone(&script),
                Arc::clone(&cursor),
                account,
            )
            .map_err(|error| error.to_string())
        }
    };
    let primary = bind(primary_account);
    let successor = bind(b"codex/b");
    let injection = test_kit::scripted_mut(&mut config);
    injection.rebind_target = successor().unwrap().target_binding();
    injection.runtime = Arc::new(move |role| {
        if role == RuntimeRole::Rebind {
            successor()
        } else {
            primary()
        }
    });
    if let Some((workflow, boundary, nth)) = cut {
        let crossed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        injection.observe = Arc::new(move |seen_workflow, seen_boundary| {
            if (seen_workflow, seen_boundary) == (workflow, boundary)
                && crossed.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == nth
            {
                std::panic::panic_any("account continue crash cut");
            }
        });
    }
    let surface = Arc::new(Chooser::default());
    config.surface = Some(Box::new(Arc::clone(&surface)));
    (config, surface)
}

/// A usage-limit failure, then `ChooseAccount` with a continue: one rebind
/// onto the new account and exactly one admitted root turn of the exact
/// prompt on the line head. The root's node, profile and tree epoch do not
/// move. A later continue on the same account gets fresh ids.
#[test]
fn a_continue_after_a_usage_limit_rebinds_once_and_resubmits_the_exact_prompt() {
    let root = tempfile::tempdir().unwrap();
    let (config, surface) = continue_host(root.path(), None, b"offline-loopback-account");
    let store = Arc::clone(&config.store);
    let state_roots = config.state_roots.clone();
    let tree = config.session.id.clone();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();

    send_attached_action(&actions, UserAction::Prompt("do the thing".into()));
    wait_for(&events, &mut seen, 0, |events| {
        idle_after(events, TurnPhase::Failed)
    });
    let failed = lifecycle(&seen, TurnPhase::Failed);
    let usage = failed[0]
        .usage_limit
        .clone()
        .expect("the quota failure names its usage limit");
    assert_eq!(
        (
            usage.kind.as_str(),
            usage.account.as_str(),
            usage.alternatives
        ),
        ("codex", "codex", vec!["codex/b".to_owned()])
    );
    let failed_turn = failed[0].turn_id.clone();
    let rows = crate::runtime::HostRows::new(&state_roots, store.as_ref());
    let epoch = || {
        serde_json::to_string(
            &crate::runtime::topology_state::current_epoch_of(&rows, &tree).unwrap(),
        )
        .unwrap()
    };
    let source = line_head(store.as_ref(), &tree);
    let last = store
        .load_views(&source.id)
        .unwrap()
        .unwrap()
        .state
        .last_root_turn;
    assert!(
        last.as_ref().is_some_and(|last| last.turn_id == failed_turn
            && last.outcome == Some(LastRootOutcome::Failed)
            && last.prompt.as_deref() == Some("do the thing")
            && !last.committed_after_prompt),
        "the failed turn's facts: {last:?}"
    );
    let epoch_before = epoch();

    *surface.next.lock().unwrap() = Some(SurfaceAccountChoice::Root {
        account: "codex/b".into(),
        continue_failed: true,
    });
    let before = seen.len();
    send_attached_action(&actions, UserAction::Extension(ProbeIntent::InspectUsage));
    wait_for(&events, &mut seen, before, |events| {
        idle_after(events, TurnPhase::Completed)
    });
    assert_eq!(lifecycle(&seen[before..], TurnPhase::Admitted).len(), 1);
    let head = line_head(store.as_ref(), &tree);
    let ids = ContinueIds::of(&failed_turn, "codex/b");
    assert_eq!(head.account.as_deref(), Some("codex/b"));
    assert_eq!(
        head.source_product_session_id.as_deref(),
        Some(source.id.as_str())
    );
    assert_eq!(
        head.source_workflow_operation_id.as_deref(),
        Some(ids.operation_id.as_str())
    );
    assert_eq!(head.node, source.node, "the root keeps its node");
    assert_eq!(head.root_profile, source.root_profile);
    assert_eq!(
        epoch(),
        epoch_before,
        "the tree's epoch and cards do not move"
    );
    let views = store.load_views(&head.id).unwrap().unwrap();
    assert!(views.state.deliveries.contains_key(&ids.command_id()));
    assert_eq!(
        user_rows(store.as_ref(), &head.id),
        ["do the thing", "do the thing"],
        "the exact prompt is resubmitted once; the duplicate is the accepted continue"
    );
    assert!(matches!(
        surface.outcomes.lock().unwrap().as_slice(),
        [AccountChoiceOutcome::Chosen { root: Some(root), .. }] if root == "codex/b"
    ));

    // The next limit on the same account: no second rebind, new ids.
    send_attached_action(&actions, UserAction::Prompt("next".into()));
    let before = seen.len();
    wait_for(&events, &mut seen, before, |events| {
        idle_after(events, TurnPhase::Failed)
    });
    let second_turn = lifecycle(&seen[before..], TurnPhase::Failed)[0]
        .turn_id
        .clone();
    *surface.next.lock().unwrap() = Some(SurfaceAccountChoice::Root {
        account: "codex/b".into(),
        continue_failed: true,
    });
    let before = seen.len();
    send_attached_action(&actions, UserAction::Extension(ProbeIntent::InspectUsage));
    wait_for(&events, &mut seen, before, |events| {
        idle_after(events, TurnPhase::Completed)
    });
    let again = ContinueIds::of(&second_turn, "codex/b");
    assert_ne!(again, ids);
    assert_eq!(
        line_head(store.as_ref(), &tree).id,
        head.id,
        "no second rebind"
    );
    let views = store.load_views(&head.id).unwrap().unwrap();
    assert!(views.state.deliveries.contains_key(&again.command_id()));
    assert_eq!(
        user_rows(store.as_ref(), &head.id),
        ["do the thing", "do the thing", "next", "next"]
    );

    // A continue needs a failed last root turn.
    *surface.next.lock().unwrap() = Some(SurfaceAccountChoice::Root {
        account: "codex/b".into(),
        continue_failed: true,
    });
    let before = surface.outcomes.lock().unwrap().len();
    send_attached_action(&actions, UserAction::Extension(ProbeIntent::InspectUsage));
    let deadline = Instant::now() + Duration::from_secs(30);
    while surface.outcomes.lock().unwrap().len() == before {
        assert!(
            Instant::now() < deadline,
            "no outcome for the refused continue"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        surface.outcomes.lock().unwrap().last(),
        Some(&AccountChoiceOutcome::Refused(
            AccountChoiceRefusal::NoFailedTurn
        ))
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

/// A non-root choice is written through the host's own record, so the next
/// prompt's save in the same attachment succeeds.
#[test]
fn a_non_root_choice_then_a_prompt_in_the_same_attachment() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::clone(&config.store);
    let tree = config.session.id.clone();
    let surface = Arc::new(Chooser::default());
    config.surface = Some(Box::new(Arc::clone(&surface)));
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    *surface.next.lock().unwrap() = Some(SurfaceAccountChoice::Store {
        kind: "claude".into(),
        account: "claude/alt".into(),
    });
    send_attached_action(&actions, UserAction::Extension(ProbeIntent::InspectUsage));
    send_attached_action(&actions, UserAction::Prompt("hello".into()));
    wait_for(&events, &mut seen, 0, |events| {
        idle_after(events, TurnPhase::Completed)
    });
    let head = line_head(store.as_ref(), &tree);
    assert_eq!(
        head.account_choices,
        std::collections::BTreeMap::from([("claude".to_owned(), "claude/alt".to_owned())])
    );
    assert!(matches!(
        surface.outcomes.lock().unwrap().as_slice(),
        [AccountChoiceOutcome::Chosen { choices, .. }] if choices.contains_key("claude")
    ));
    assert_eq!(user_rows(store.as_ref(), &head.id), ["hello"]);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

/// Reopen after a continue's rebind published without its root-start: one
/// recovery notice keyed by the derived command, and nothing is resent.
#[test]
fn an_unsent_continue_is_reported_on_reopen_and_never_resubmitted() {
    let mut session = SessionRecord::new(std::path::PathBuf::from("/tmp/plak-continue"));
    session.account = Some("codex/b".into());
    let ids = ContinueIds::of("turn-a", "codex/b");
    session.source_workflow_operation_id = Some(ids.operation_id.clone());
    let mut views = crate::sessions::projector::SessionViews::empty();
    views.state.last_root_turn = Some(LastRootTurn {
        turn_id: "turn-a".into(),
        ..turn(Some("do it"), false)
    });
    let row = unsent_continue_notice(&session, &views).expect("the unsent continue is reported");
    let TranscriptRow::Notice(notice) = row else {
        panic!("a notice row");
    };
    assert!(matches!(
        notice.remedy,
        Some(plakous_host_protocol::notice_remedy::NoticeRemedy::ContinueOnAccount {
            continuation
        }) if continuation.account == "codex/b" && continuation.command_id == ids.command_id()
    ));
    views.state.deliveries.insert(
        ids.command_id(),
        crate::sessions::projector::Delivery::Accepted,
    );
    assert!(
        unsent_continue_notice(&session, &views).is_none(),
        "a delivered continue is done"
    );
    views.state.deliveries.clear();
    session
        .enqueue_live_as("do it".into(), &ids.item_id)
        .unwrap();
    assert!(
        unsent_continue_notice(&session, &views).is_none(),
        "a queued continue waits"
    );
    session.source_workflow_operation_id = Some("plak:rebind:other".into());
    session.pending_queue.clear();
    assert!(unsent_continue_notice(&session, &views).is_none());
}

/// Crash cuts of a continue (spec 02 §4.4.1): the host stops at the rebind's
/// written request, its publication, the root-start's written submission and
/// its admitted turn; a host then reopens the line head from the same stores.
/// Each shows one rebind, at most one resubmitted prompt, and no resubmission
/// by the reopen itself.
mod account_continue_crash_cuts {
    use super::*;
    use crate::runtime::{HostBoundary, HostWorkflow};

    struct Cut {
        store: Arc<dyn SessionRecordStore>,
        tree: String,
        source: String,
        failed_turn: String,
        reopened: Vec<HostEvent>,
    }

    fn cut_at(workflow: HostWorkflow, boundary: HostBoundary, nth: usize) -> Cut {
        let root = tempfile::tempdir().unwrap();
        let (config, surface) = continue_host(
            root.path(),
            Some((workflow, boundary, nth)),
            b"offline-loopback-account",
        );
        let store = Arc::clone(&config.store);
        let state_roots = config.state_roots.clone();
        let tree = config.session.id.clone();
        let (actions, events, worker) = spawn_host_worker(config).unwrap();
        let mut seen = Vec::new();
        send_attached_action(&actions, UserAction::Prompt("do the thing".into()));
        wait_for(&events, &mut seen, 0, |events| {
            idle_after(events, TurnPhase::Failed)
        });
        let failed_turn = lifecycle(&seen, TurnPhase::Failed)[0].turn_id.clone();
        let source = line_head(store.as_ref(), &tree).id;
        *surface.next.lock().unwrap() = Some(SurfaceAccountChoice::Root {
            account: "codex/b".into(),
            continue_failed: true,
        });
        let _ = actions.send(UserAction::Extension(ProbeIntent::InspectUsage));
        assert!(worker.join().is_err(), "the host stops at its cut");
        drop((actions, events));

        let head = line_head(store.as_ref(), &tree);
        let account: &'static [u8] = if head.id == source {
            b"offline-loopback-account"
        } else {
            b"codex/b"
        };
        // A cut inside a turn leaves its branch lease held until it lapses.
        let deadline = Instant::now() + Duration::from_secs(120);
        let (actions, events, worker) = loop {
            let (mut config, _) = continue_host(root.path(), None, account);
            config.store = Arc::clone(&store);
            config.state_roots = state_roots.clone();
            config.session = line_head(store.as_ref(), &tree);
            match spawn_host_worker(config) {
                Ok(host) => break host,
                Err(failure) if Instant::now() < deadline => {
                    assert!(
                        failure.message().contains("held by another plakd process"),
                        "the line head reopens: {}",
                        failure.message()
                    );
                    std::thread::sleep(Duration::from_secs(2));
                }
                Err(failure) => panic!("the line head reopens: {}", failure.message()),
            }
        };
        let mut reopened = Vec::new();
        let settle = Instant::now() + Duration::from_secs(3);
        while Instant::now() < settle {
            if let Ok(event) = events.recv_timeout(Duration::from_millis(100)) {
                reopened.push(event);
            }
        }
        send_attached_action(&actions, UserAction::Quit);
        worker.join().unwrap().unwrap();
        Cut {
            store,
            tree,
            source,
            failed_turn,
            reopened,
        }
    }

    impl Cut {
        fn head(&self) -> SessionRecord {
            line_head(self.store.as_ref(), &self.tree)
        }

        fn ids(&self) -> ContinueIds {
            ContinueIds::of(&self.failed_turn, "codex/b")
        }

        /// One rebind at most: the head is the source or its one successor.
        fn assert_one_rebind(&self) {
            let head = self.head();
            if head.id != self.source {
                assert_eq!(
                    head.source_product_session_id.as_deref(),
                    Some(self.source.as_str())
                );
                assert_eq!(
                    head.source_workflow_operation_id.as_deref(),
                    Some(self.ids().operation_id.as_str())
                );
            }
        }

        fn reopen_admitted_nothing(&self) {
            assert!(
                lifecycle(&self.reopened, TurnPhase::Admitted).is_empty(),
                "reopen never submits the continue: {:?}",
                self.reopened
            );
        }

        fn continue_notice(&self) -> bool {
            self.reopened.iter().any(|event| {
                matches!(
                    event,
                    HostEvent::Row(TranscriptRow::Notice(notice)) if matches!(
                        &notice.remedy,
                        Some(plakous_host_protocol::notice_remedy::NoticeRemedy::ContinueOnAccount {
                            continuation
                        }) if continuation.command_id == self.ids().command_id()
                    )
                )
            })
        }
    }

    #[test]
    fn a_cut_after_the_rebind_request_is_written_withdraws_it_and_sends_nothing() {
        let cut = cut_at(HostWorkflow::Rebind, HostBoundary::Prepared, 1);
        cut.assert_one_rebind();
        cut.reopen_admitted_nothing();
        let head = cut.head();
        assert_eq!(
            head.id, cut.source,
            "an unpublished rebind has no successor"
        );
        assert!(
            cut.store
                .load_intents(&head.id)
                .unwrap()
                .workflow_request(&cut.ids().operation_id)
                .is_none(),
            "the unpublished request is withdrawn"
        );
        assert!(head.pending_queue.is_empty());
        assert_eq!(user_rows(cut.store.as_ref(), &head.id), ["do the thing"]);
    }

    #[test]
    fn a_cut_after_the_rebind_publishes_reports_the_unsent_continue_once_opened() {
        let cut = cut_at(HostWorkflow::Rebind, HostBoundary::ProductPublished, 1);
        cut.assert_one_rebind();
        cut.reopen_admitted_nothing();
        let head = cut.head();
        assert_ne!(head.id, cut.source, "the rebind published");
        assert_eq!(head.account.as_deref(), Some("codex/b"));
        assert!(head.pending_queue.is_empty());
        assert!(cut.continue_notice(), "{:?}", cut.reopened);
    }

    #[test]
    fn a_cut_after_the_root_start_is_written_keeps_it_queued_and_paused() {
        let cut = cut_at(HostWorkflow::RootDelivery, HostBoundary::Prepared, 2);
        cut.assert_one_rebind();
        cut.reopen_admitted_nothing();
        let head = cut.head();
        assert!(
            head.pending_queue
                .iter()
                .any(|item| item.id == cut.ids().item_id),
            "the written root-start waits for the operator"
        );
        assert!(
            !cut.continue_notice(),
            "a queued continue is not reported as unsent"
        );
    }

    #[test]
    fn a_cut_after_the_root_turn_is_admitted_runs_it_once() {
        let cut = cut_at(HostWorkflow::RootDelivery, HostBoundary::RuntimeReturned, 2);
        cut.assert_one_rebind();
        let head = cut.head();
        let views = cut.store.load_views(&head.id).unwrap().unwrap();
        assert!(views.state.deliveries.contains_key(&cut.ids().command_id()));
        assert!(!cut.continue_notice());
        let prompts = user_rows(cut.store.as_ref(), &head.id);
        assert!(
            prompts
                .iter()
                .filter(|text| *text == "do the thing")
                .count()
                <= 2,
            "at most one resubmitted prompt: {prompts:?}"
        );
        assert!(
            !head
                .pending_queue
                .iter()
                .any(|item| item.id == cut.ids().item_id),
            "an admitted continue leaves no queued copy"
        );
    }
}
