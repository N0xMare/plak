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
    let first = ContinueIds::of("line", "turn-a", "codex/work");
    assert_eq!(first, ContinueIds::of("line", "turn-a", "codex/work"));
    assert_ne!(first, ContinueIds::of("line", "turn-b", "codex/work"));
    assert_ne!(first, ContinueIds::of("line", "turn-a", "codex/other"));
    assert_ne!(
        first,
        ContinueIds::of("fork", "turn-a", "codex/work"),
        "a fork that inherited the turn continues under its own ids"
    );
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

    fn alternatives_read(&self, account: &str) -> Option<surface::AlternativesRead> {
        let account = account.to_owned();
        Some(Box::new(move || {
            ["codex", "codex/b"]
                .into_iter()
                .filter(|other| *other != account)
                .map(str::to_owned)
                .collect()
        }))
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
    continue_host_scripted(root, cut, primary_account, &continue_steps(), 1)
}

/// Each prompt fails on its usage limit; each continue then answers.
fn continue_steps() -> serde_json::Value {
    serde_json::json!([
        {"fault": {"at": "pre_stream", "kind": "quota_exceeded"}},
        {"output": {"text": "continued"}},
        {"fault": {"at": "pre_stream", "kind": "quota_exceeded"}},
        {"output": {"text": "continued again"}}
    ])
}

/// [`continue_host`] over its own script `steps`, whose shared cursor starts
/// at `first_step`, so a reopened host takes the script up where the cut
/// one left it.
fn continue_host_scripted(
    root: &std::path::Path,
    cut: Option<(
        crate::runtime::HostWorkflow,
        crate::runtime::HostBoundary,
        usize,
    )>,
    primary_account: &'static [u8],
    steps: &serde_json::Value,
    first_step: u64,
) -> (crate::runtime::HostConfig<TestExtension>, Arc<Chooser>) {
    let (mut config, _) = fresh_injected_attached_host(root);
    let workspace = config.cwd.clone();
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        root.join("continue-evidence.jsonl"),
    )
    .unwrap();
    let script: Arc<plakous_offline_crash_harness::script::ScriptPlan> = Arc::new(
        serde_json::from_value(serde_json::json!({"version": 1, "steps": steps})).unwrap(),
    );
    let cursor = Arc::new(AtomicU64::new(first_step));
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

/// Hands the host a continue onto `codex/b` and waits for its turn to
/// complete; returns where its events start.
fn continue_on_b(
    surface: &Chooser,
    actions: &crate::surface_protocol::UserActionSender<TestExtension>,
    events: &crate::surface_protocol::HostEventReceiver<TestExtension>,
    seen: &mut Vec<HostEvent>,
) -> usize {
    *surface.next.lock().unwrap() = Some(SurfaceAccountChoice::Root {
        account: "codex/b".into(),
        continue_failed: true,
    });
    let before = seen.len();
    send_attached_action(actions, UserAction::Extension(ProbeIntent::InspectUsage));
    wait_for(events, seen, before, |events| {
        idle_after(events, TurnPhase::Completed)
    });
    before
}

/// Hands the host a continue onto `codex/b` that it refuses.
fn refused_continue(
    surface: &Chooser,
    actions: &crate::surface_protocol::UserActionSender<TestExtension>,
) -> AccountChoiceOutcome {
    *surface.next.lock().unwrap() = Some(SurfaceAccountChoice::Root {
        account: "codex/b".into(),
        continue_failed: true,
    });
    let before = surface.outcomes.lock().unwrap().len();
    send_attached_action(actions, UserAction::Extension(ProbeIntent::InspectUsage));
    let deadline = Instant::now() + Duration::from_secs(30);
    while surface.outcomes.lock().unwrap().len() == before {
        assert!(
            Instant::now() < deadline,
            "no outcome for the refused continue"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    surface.outcomes.lock().unwrap().last().cloned().unwrap()
}

/// The line head's projected facts name `turn` as its failed last root turn
/// with its exact prompt and nothing committed after it.
fn assert_failed_turn(store: &dyn SessionRecordStore, session: &str, turn: &str) {
    let last = store
        .load_views(session)
        .unwrap()
        .unwrap()
        .state
        .last_root_turn;
    assert!(
        last.as_ref().is_some_and(|last| last.turn_id == turn
            && last.outcome == Some(LastRootOutcome::Failed)
            && last.prompt.as_deref() == Some("do the thing")
            && !last.committed_after_prompt),
        "the failed turn's facts: {last:?}"
    );
}

struct ReadinessGate {
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

struct BlockingReadiness {
    gate: Mutex<Option<ReadinessGate>>,
    phase: std::sync::mpsc::Sender<crate::runtime::SurfacePhase>,
}

impl SurfaceHandler<TestExtension> for BlockingReadiness {
    fn accounts_read(
        &self,
        _: &ProbeIntent,
        _: &SurfaceFacts<'_>,
    ) -> Option<surface::AccountsRead> {
        let ReadinessGate { entered, release } = self.gate.lock().unwrap().take().unwrap();
        Some(Box::new(move || {
            entered.send(()).unwrap();
            release.recv().map_err(|error| error.to_string())?;
            Ok(Vec::new())
        }))
    }

    fn dispatch(&self, _: ProbeIntent, facts: SurfaceFacts<'_>, _: &Sender) -> SurfaceEffect {
        assert!(matches!(facts.accounts, Some(Ok(rows)) if rows.is_empty()));
        self.phase.send(facts.phase).unwrap();
        SurfaceEffect::default()
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

#[test]
fn a_blocked_readiness_read_does_not_hold_the_root_turn_or_its_controls() {
    let root = tempfile::tempdir().unwrap();
    let gate = root.path().join("release-turn");
    let (mut config, _) = continue_host_scripted(
        root.path(),
        None,
        b"offline-loopback-account",
        &serde_json::json!([{
            "stream": [{"gate": {"file": gate}}],
            "output": {"text": "finished"},
        }]),
        1,
    );
    let (entered, reading) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (phase, dispatched) = std::sync::mpsc::channel();
    config.surface = Some(Box::new(BlockingReadiness {
        gate: Mutex::new(Some(ReadinessGate {
            entered,
            release: released,
        })),
        phase,
    }));
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Extension(ProbeIntent::InspectUsage));
    reading.recv_timeout(Duration::from_secs(30)).unwrap();
    send_attached_action(&actions, UserAction::Prompt("go on while waiting".into()));
    wait_for(&events, &mut seen, 0, |events| {
        !lifecycle(events, TurnPhase::Admitted).is_empty()
    });
    release.send(()).unwrap();
    assert_eq!(
        dispatched.recv_timeout(Duration::from_secs(30)).unwrap(),
        crate::runtime::SurfacePhase::Busy
    );
    std::fs::write(gate, "finish").unwrap();
    wait_for(&events, &mut seen, 0, |events| {
        idle_after(events, TurnPhase::Completed)
    });
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
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
    assert_failed_turn(store.as_ref(), &source.id, &failed_turn);
    let epoch_before = epoch();

    let before = continue_on_b(&surface, &actions, &events, &mut seen);
    assert_eq!(lifecycle(&seen[before..], TurnPhase::Admitted).len(), 1);
    let head = line_head(store.as_ref(), &tree);
    let ids = ContinueIds::of(&source.id, &failed_turn, "codex/b");
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
    let before = continue_on_b(&surface, &actions, &events, &mut seen);
    assert_eq!(lifecycle(&seen[before..], TurnPhase::Admitted).len(), 1);
    let again = ContinueIds::of(&head.id, &second_turn, "codex/b");
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
    assert_eq!(
        refused_continue(&surface, &actions),
        AccountChoiceOutcome::Refused(AccountChoiceRefusal::NoFailedTurn)
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

fn assistant_rows(store: &dyn SessionRecordStore, id: &str) -> Vec<String> {
    store
        .load_views(id)
        .unwrap()
        .unwrap()
        .rows
        .values()
        .filter_map(|row| match row {
            TranscriptRow::Assistant { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// A turn whose tool round committed before its usage limit: the continue
/// resubmits the pinned continuation, read through the real projector's
/// `committed_after_prompt`, and the new account's request omits the
/// reasoning the old account's model bound to itself (chapter 01 §5.7). On
/// the old account the same reasoning is replayed, so the report is not
/// trivially empty.
#[test]
fn a_continue_after_a_committed_round_sends_the_continuation_without_the_old_accounts_reasoning() {
    let root = tempfile::tempdir().unwrap();
    let tool_round = serde_json::json!({"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"});
    let (config, surface) = continue_host_scripted(
        root.path(),
        None,
        b"offline-loopback-account",
        &serde_json::json!([
            {"account_reasoning": true, "output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": "first done"}},
            {"output": "reasoning_report"},
            tool_round,
            {"fault": {"at": "pre_stream", "kind": "quota_exceeded"}},
            {"output": "reasoning_report"}
        ]),
        1,
    );
    let store = Arc::clone(&config.store);
    let tree = config.session.id.clone();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    for prompt in ["do the thing", "check"] {
        send_attached_action(&actions, UserAction::Prompt(prompt.into()));
        let before = seen.len();
        wait_for(&events, &mut seen, before, |events| {
            idle_after(events, TurnPhase::Completed)
        });
    }
    let source = line_head(store.as_ref(), &tree);
    assert_eq!(
        assistant_rows(store.as_ref(), &source.id)
            .last()
            .map(String::as_str),
        Some("reasoning blocks: 1"),
        "the producing account replays its own reasoning"
    );
    send_attached_action(&actions, UserAction::Prompt("next".into()));
    let before = seen.len();
    wait_for(&events, &mut seen, before, |events| {
        idle_after(events, TurnPhase::Failed)
    });
    let last = store
        .load_views(&source.id)
        .unwrap()
        .unwrap()
        .state
        .last_root_turn
        .unwrap();
    assert!(
        last.outcome == Some(LastRootOutcome::Failed)
            && last.prompt.as_deref() == Some("next")
            && last.committed_after_prompt,
        "the projector saw the committed round: {last:?}"
    );

    let before = continue_on_b(&surface, &actions, &events, &mut seen);
    assert_eq!(lifecycle(&seen[before..], TurnPhase::Admitted).len(), 1);
    let head = line_head(store.as_ref(), &tree);
    assert_eq!(head.account.as_deref(), Some("codex/b"));
    assert_eq!(
        user_rows(store.as_ref(), &head.id),
        ["do the thing", "check", "next", CONTINUATION_PROMPT],
        "a committed round is continued, not repeated"
    );
    assert_eq!(
        assistant_rows(store.as_ref(), &head.id)
            .last()
            .map(String::as_str),
        Some("reasoning blocks: 0"),
        "the new account's request omits the old account's reasoning"
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
    let ids = ContinueIds::of("source-line", "turn-a", "codex/b");
    session.source_product_session_id = Some("source-line".into());
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
        repeated: Option<AccountChoiceOutcome>,
        repeat_events: Vec<HostEvent>,
    }

    /// What the reopened host does once it settled: nothing, or the same
    /// continue again, which is refused or runs.
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Repeat {
        No,
        Refused,
        Runs,
    }

    fn cut_at(workflow: HostWorkflow, boundary: HostBoundary, nth: usize) -> Cut {
        cut_then(workflow, boundary, nth, Repeat::No)
    }

    fn cut_then(workflow: HostWorkflow, boundary: HostBoundary, nth: usize, repeat: Repeat) -> Cut {
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
        let (actions, events, worker, surface) = loop {
            // The cut host took the first step, the failed prompt.
            let (mut config, surface) =
                continue_host_scripted(root.path(), None, account, &continue_steps(), 2);
            config.store = Arc::clone(&store);
            config.state_roots = state_roots.clone();
            config.session = line_head(store.as_ref(), &tree);
            match spawn_host_worker(config) {
                Ok((actions, events, worker)) => break (actions, events, worker, surface),
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
        let mut repeat_events = Vec::new();
        let repeated = match repeat {
            Repeat::No => None,
            Repeat::Refused => Some(refused_continue(&surface, &actions)),
            Repeat::Runs => {
                continue_on_b(&surface, &actions, &events, &mut repeat_events);
                surface.outcomes.lock().unwrap().last().cloned()
            }
        };
        send_attached_action(&actions, UserAction::Quit);
        worker.join().unwrap().unwrap();
        Cut {
            store,
            tree,
            source,
            failed_turn,
            reopened,
            repeated,
            repeat_events,
        }
    }

    impl Cut {
        fn head(&self) -> SessionRecord {
            line_head(self.store.as_ref(), &self.tree)
        }

        fn ids(&self) -> ContinueIds {
            ContinueIds::of(&self.source, &self.failed_turn, "codex/b")
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
        assert_eq!(head.pending_queue.len(), 0);
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
        assert_eq!(head.pending_queue.len(), 0);
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
        cut.reopen_admitted_nothing();
        let head = cut.head();
        let views = cut.store.load_views(&head.id).unwrap().unwrap();
        assert!(views.state.deliveries.contains_key(&cut.ids().command_id()));
        assert!(!cut.continue_notice());
        let root_starts = cut
            .store
            .load_intents(&head.id)
            .unwrap()
            .submissions
            .iter()
            .map(|submission| submission.command_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            root_starts,
            [cut.ids().command_id()],
            "exactly one resubmission, the continue's"
        );
        let prompts = user_rows(cut.store.as_ref(), &head.id);
        assert!(
            prompts
                .iter()
                .filter(|text| *text == "do the thing")
                .count()
                <= 2,
            "at most the prompt and its one resubmission: {prompts:?}"
        );
        assert!(
            !head
                .pending_queue
                .iter()
                .any(|item| item.id == cut.ids().item_id),
            "an admitted continue leaves no queued copy"
        );
    }

    /// A repeat of a continue whose root-start is queued is refused
    /// `already_continued` and queues nothing more.
    #[test]
    fn a_repeat_of_a_queued_continue_is_refused_already_continued() {
        let cut = cut_then(
            HostWorkflow::RootDelivery,
            HostBoundary::Prepared,
            2,
            Repeat::Refused,
        );
        cut.assert_one_rebind();
        cut.reopen_admitted_nothing();
        assert_eq!(
            cut.repeated,
            Some(AccountChoiceOutcome::Refused(
                AccountChoiceRefusal::AlreadyContinued
            ))
        );
        let head = cut.head();
        assert_eq!(
            head.pending_queue
                .iter()
                .filter(|item| item.id == cut.ids().item_id)
                .count(),
            1,
            "the queued continue stays the only one"
        );
    }

    /// A repeat of a continue whose rebind published but whose root-start
    /// was never queued does not rebind again: it resubmits the prompt once
    /// on the continue's successor under the continue's own ids.
    #[test]
    fn a_repeat_of_a_published_continue_resubmits_once_without_a_second_rebind() {
        let cut = cut_then(
            HostWorkflow::Rebind,
            HostBoundary::ProductPublished,
            1,
            Repeat::Runs,
        );
        cut.reopen_admitted_nothing();
        assert!(cut.continue_notice(), "{:?}", cut.reopened);
        let head = cut.head();
        assert_ne!(head.id, cut.source, "the first attempt's rebind published");
        cut.assert_one_rebind();
        assert_eq!(lifecycle(&cut.repeat_events, TurnPhase::Admitted).len(), 1);
        assert!(matches!(
            &cut.repeated,
            Some(AccountChoiceOutcome::Chosen { root: Some(root), .. }) if root == "codex/b"
        ));
        let views = cut.store.load_views(&head.id).unwrap().unwrap();
        assert!(views.state.deliveries.contains_key(&cut.ids().command_id()));
        assert_eq!(
            user_rows(cut.store.as_ref(), &head.id),
            ["do the thing", "do the thing"],
            "the prompt and its one resubmission"
        );
    }
}
