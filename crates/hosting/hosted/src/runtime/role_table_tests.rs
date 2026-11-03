//! The root column of the intent x role table, and the child cells that
//! need a running host or name no child at all. See `crate::role_table`.
//!
//! The root tests drive today's node actor through the host worker: an
//! idle host, or one whose first turn is held at the provider gate. A
//! fence (an empty steer, which every loop rejects in arrival order) marks
//! where the observations of the actions sent before it end.

#![allow(clippy::unwrap_used, reason = "role table tests")]

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use plakous_host_protocol::status::{HostPhase, HostStatusDetail, SteerSource};
use plakous_host_protocol::views::transcript::{NoticeRow, TranscriptRow, TranscriptScope};
use plakous_offline_crash_harness::gate::GateHandle;
use serde_json::json;

use super::recover_tests::{script, scripted_host_with};
use super::test_support::{install_live_descendant_on_hub, send_attached_action};
use crate::intent_dispatch::{DispatchPhase, Disposition, IntentClass, take_next};
use crate::interrupt::{InterruptWatch, InterruptWatchState};
use crate::role_table::{
    Cell, Class, Column, Covered, Row, UserAction, assert_today, child_address, intents_of,
};
use crate::runtime::test_kit::{RecordingSurface, TestExtension};
use crate::runtime::{HostWorkerHandle, spawn_host_worker};

use Class::{B, I, R, U, W};

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;

pub(crate) const COVERED: &[Covered] = &[
    Covered {
        row: Row::Prompt,
        column: Column::Root,
        test: root_prompt_starts_when_idle_and_queues_when_busy,
    },
    Covered {
        row: Row::Steer,
        column: Column::Root,
        test: root_steer_is_taken_only_while_busy,
    },
    Covered {
        row: Row::Interrupt,
        column: Column::Root,
        test: root_interrupt_cancels_the_turn_and_is_observed_by_a_workflow,
    },
    Covered {
        row: Row::Processes,
        column: Column::Root,
        test: root_process_intents_are_handled_while_busy,
    },
    Covered {
        row: Row::Queue,
        column: Column::Root,
        test: root_queue_edits_are_handled_while_busy,
    },
    Covered {
        row: Row::Approval,
        column: Column::Root,
        test: root_approval_reaches_the_running_turns_decisions,
    },
    Covered {
        row: Row::Quit,
        column: Column::Root,
        test: root_quit_keeps_its_place_and_ends_a_busy_host,
    },
    Covered {
        row: Row::Workflow,
        column: Column::Root,
        test: root_workflows_are_refused_while_busy,
    },
    Covered {
        row: Row::Readiness,
        column: Column::Root,
        test: root_readiness_is_read_while_busy_and_retry_attach_waits,
    },
    Covered {
        row: Row::Recover,
        column: Column::Root,
        test: root_recover_is_refused_while_busy,
    },
    Covered {
        row: Row::Navigator,
        column: Column::Root,
        test: root_navigator_is_answered_while_busy,
    },
    Covered {
        row: Row::Transcript,
        column: Column::Root,
        test: root_transcript_resync_is_answered_while_busy,
    },
    Covered {
        row: Row::InvokeTool,
        column: Column::Root,
        test: root_invoke_tool_runs_only_when_idle,
    },
    Covered {
        row: Row::CallDetail,
        column: Column::Root,
        test: root_call_detail_is_answered_while_busy,
    },
    Covered {
        row: Row::Templates,
        column: Column::Root,
        test: root_templates_and_extensions_are_handled_while_busy,
    },
    Covered {
        row: Row::ChildOnly,
        column: Column::Root,
        test: root_has_no_child_only_intent,
    },
    Covered {
        row: Row::Queue,
        column: Column::Child,
        test: child_queue_edits_name_no_child,
    },
    Covered {
        row: Row::Workflow,
        column: Column::Child,
        test: child_workflows_name_no_child,
    },
    Covered {
        row: Row::Readiness,
        column: Column::Child,
        test: child_readiness_names_no_child,
    },
    Covered {
        row: Row::Recover,
        column: Column::Child,
        test: child_recover_names_no_child,
    },
    Covered {
        row: Row::Transcript,
        column: Column::Child,
        test: child_transcript_opens_scoped_while_the_root_is_busy,
    },
    Covered {
        row: Row::CallDetail,
        column: Column::Child,
        test: child_call_detail_is_answered_for_the_child_while_busy,
    },
    Covered {
        row: Row::Templates,
        column: Column::Child,
        test: child_templates_and_extensions_name_no_child,
    },
];

const IDLE_NOTE: &str = ": available when idle (finish or interrupt the turn)";
const WAIT: Duration = Duration::from_secs(30);

struct Host {
    _root: tempfile::TempDir,
    actions: crate::surface_protocol::UserActionSender<TestExtension>,
    events: crate::surface_protocol::HostEventReceiver<TestExtension>,
    worker: HostWorkerHandle,
    gate: GateHandle,
    surface: Arc<RecordingSurface>,
    session_id: String,
}

impl Host {
    /// An idle host whose first turn will hold at the provider gate.
    fn idle() -> Self {
        Self::with(false)
    }

    /// A host whose first turn runs and holds at the provider gate.
    fn busy() -> Self {
        let host = Self::idle();
        host.hold();
        host
    }

    /// Like [`Self::busy`], with one live child on the session's tree.
    fn busy_with_child() -> (Self, String) {
        let host = Self::with(true);
        host.hold();
        let child = crate::topology_runtime::lookup(&host.session_id)
            .unwrap()
            .read(|tree| {
                tree.nodes()
                    .keys()
                    .find(|id| *id != &tree.root_agent_id)
                    .cloned()
                    .unwrap()
            })
            .unwrap();
        (host, child)
    }

    fn with(child: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let gate = GateHandle::new();
        let plan = script(json!([
            {"stream": [{"gate": gate.script()}], "output": {"text": "held answer"}},
            {"output": {"text": "second answer"}},
            {"output": {"text": "third answer"}},
            {"output": {"text": "fourth answer"}}
        ]));
        let (mut config, _) = scripted_host_with(root.path(), plan);
        let surface = RecordingSurface::new();
        config.surface = Some(Box::new(surface.clone()));
        let session_id = config.session.id.clone();
        if child {
            install_live_descendant_on_hub(&session_id, &config.cwd);
        }
        let (actions, events, worker) = spawn_host_worker(config).unwrap();
        Self {
            _root: root,
            actions,
            events,
            worker,
            gate,
            surface,
            session_id,
        }
    }

    fn hold(&self) {
        self.send(UserAction::Prompt("hold".into()));
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async { tokio::time::timeout(WAIT, self.gate.entered()).await })
            .expect("the first turn reaches the provider gate");
    }

    fn send(&self, action: UserAction) {
        send_attached_action(&self.actions, action);
    }

    /// Everything observed up to the fence sent after the actions so far.
    fn fence(&self) -> Vec<HostEvent> {
        self.send(UserAction::Steer("".into()));
        self.until("the fence", |event| {
            matches!(event, HostEvent::DraftRejected(UserAction::Steer(draft)) if draft.text.is_empty())
        })
    }

    /// Everything observed up to and including the first `done` event.
    fn until(&self, what: &str, done: impl Fn(&HostEvent) -> bool) -> Vec<HostEvent> {
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + WAIT;
        while std::time::Instant::now() < deadline {
            if let Ok(event) = self.events.recv_timeout(Duration::from_millis(50)) {
                let hit = done(&event);
                seen.push(event);
                if hit {
                    return seen;
                }
            }
        }
        panic!("timed out waiting for {what}: {seen:#?}");
    }

    fn surface_calls(&self, call: &str) -> usize {
        self.surface
            .calls()
            .into_iter()
            .filter(|seen| *seen == call)
            .count()
    }

    fn quit(self) {
        self.gate.release();
        self.send(UserAction::Quit);
        self.worker.join().unwrap().unwrap();
    }
}

fn notes(seen: &[HostEvent]) -> Vec<&str> {
    seen.iter()
        .filter_map(|event| match event {
            HostEvent::Row(TranscriptRow::Notice(NoticeRow { text, .. })) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn idle_only_notes(seen: &[HostEvent]) -> Vec<&str> {
    notes(seen)
        .into_iter()
        .filter(|text| text.ends_with(IDLE_NOTE))
        .collect()
}

/// B: the busy loop handled every action before the fence; none was
/// answered with the idle-only refusal.
fn assert_handled_busy(seen: &[HostEvent]) {
    assert_eq!(idle_only_notes(seen), [] as [&str; 0], "{seen:#?}");
}

/// I: each action was refused with the dispatch table's noun.
fn assert_refused_busy(seen: &[HostEvent], intents: &[UserAction]) {
    let expected: Vec<String> = intents
        .iter()
        .map(|action| {
            format!(
                "{}{IDLE_NOTE}",
                IntentClass::of(action).idle_only_noun().unwrap()
            )
        })
        .collect();
    assert_eq!(idle_only_notes(seen), expected, "{seen:#?}");
}

/// W: a running workflow observes none of the row's intents and leaves
/// them all, in arrival order, for the loop's next passes.
fn assert_workflow_defers(row: Row) {
    let intents = intents_of(row);
    let (sender, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&receiver, &state);
    watch.begin();
    for action in &intents {
        assert_eq!(
            IntentClass::of(action).disposition(DispatchPhase::Workflow),
            Disposition::Defer,
            "{action:?}"
        );
        sender.send(action.clone()).unwrap();
    }
    watch.poll();
    assert!(!watch.interrupted(), "{row:?}");
    for action in intents {
        let next = take_next(&state, || receiver.try_recv()).unwrap();
        assert_eq!(next.into_parts().0, action);
    }
}

fn status_detail(seen: &[HostEvent], wanted: impl Fn(&HostStatusDetail) -> bool) -> bool {
    seen.iter().any(
        |event| matches!(event, HostEvent::Status { detail: Some(detail), .. } if wanted(detail)),
    )
}

#[test]
fn root_prompt_starts_when_idle_and_queues_when_busy() {
    // M: expects I, B (today a workflow also defers it).
    assert_today(Row::Prompt, Column::Root, Cell::Classes(&[I, B, W]));
    let host = Host::idle();
    // I: the idle prompt starts the turn that reaches the gate.
    host.hold();
    host.send(UserAction::FollowUp("next".into()));
    let seen = host.fence();
    assert!(
        seen.iter()
            .any(|event| matches!(event, HostEvent::DraftAccepted(UserAction::FollowUp(_)))),
        "{seen:#?}"
    );
    assert!(status_detail(&seen, |detail| matches!(
        detail,
        HostStatusDetail::QueuedFollowUp
    )));
    assert_handled_busy(&seen);
    assert_workflow_defers(Row::Prompt);
    host.quit();
}

#[test]
fn root_steer_is_taken_only_while_busy() {
    // M: expects B (today a workflow also defers it).
    assert_today(Row::Steer, Column::Root, Cell::Classes(&[B, W]));
    let host = Host::idle();
    host.send(UserAction::Steer("left".into()));
    let idle = host.fence();
    assert!(status_detail(&idle, |detail| matches!(
        detail,
        HostStatusDetail::NoActiveTurn
    )));
    assert!(
        idle.iter().any(|event| matches!(
            event,
            HostEvent::DraftRejected(UserAction::Steer(draft)) if draft.text == "left"
        )),
        "{idle:#?}"
    );
    host.hold();
    host.send(UserAction::Steer("right".into()));
    let busy = host.fence();
    assert!(status_detail(&busy, |detail| matches!(
        detail,
        HostStatusDetail::Steering {
            source: SteerSource::Direct
        }
    )));
    assert!(busy.iter().any(|event| matches!(
        event,
        HostEvent::DraftAccepted(UserAction::Steer(draft)) if draft.text == "right"
    )));
    assert_workflow_defers(Row::Steer);
    host.quit();
}

#[test]
fn root_interrupt_cancels_the_turn_and_is_observed_by_a_workflow() {
    assert_today(Row::Interrupt, Column::Root, Cell::Classes(&[U]));
    let host = Host::busy();
    host.send(UserAction::Interrupt);
    let seen = host.until("the cancelled turn", |event| {
        matches!(event, HostEvent::TurnLifecycle(lifecycle) if format!("{:?}", lifecycle.phase) == "Cancelled")
    });
    assert!(
        seen.iter().any(|event| matches!(
            event,
            HostEvent::Status {
                phase: HostPhase::Interrupting,
                ..
            }
        )),
        "{seen:#?}"
    );
    let (sender, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let watch = InterruptWatch::new(&receiver, &state);
    watch.begin();
    sender.send(UserAction::Interrupt).unwrap();
    watch.poll();
    assert!(
        watch.interrupted(),
        "a workflow takes the interrupt at once"
    );
    assert!(!state.borrow().has_deferred());
    host.quit();
}

#[test]
fn root_process_intents_are_handled_while_busy() {
    // M: expects B (today a workflow also defers them).
    assert_today(Row::Processes, Column::Root, Cell::Classes(&[B, W]));
    let host = Host::busy();
    host.send(UserAction::Background);
    host.send(UserAction::StopProcesses {
        id: None,
        interrupt: false,
    });
    assert_handled_busy(&host.fence());
    // A stop that also interrupts reaches the running turn.
    host.send(UserAction::StopProcesses {
        id: None,
        interrupt: true,
    });
    host.until("the interrupt", |event| {
        matches!(
            event,
            HostEvent::Status {
                phase: HostPhase::Interrupting,
                ..
            }
        )
    });
    assert_workflow_defers(Row::Processes);
    host.quit();
}

#[test]
fn root_queue_edits_are_handled_while_busy() {
    // M: expects B (today a workflow also defers them).
    assert_today(Row::Queue, Column::Root, Cell::Classes(&[B, W]));
    let host = Host::busy();
    host.send(UserAction::FollowUp("queued".into()));
    host.send(UserAction::DropQueued);
    let dropped = host.fence();
    let snapshots: Vec<usize> = dropped
        .iter()
        .filter_map(|event| match event {
            HostEvent::QueueSnapshot { items, .. } => Some(items.len()),
            _ => None,
        })
        .collect();
    // Queue snapshots are latest-state: the one-item snapshot may be
    // replaced in the channel, so the follow-up's status shows it.
    assert!(status_detail(&dropped, |detail| matches!(
        detail,
        HostStatusDetail::QueuedFollowUp
    )));
    assert_eq!(snapshots.last(), Some(&0), "{dropped:#?}");
    for action in intents_of(Row::Queue) {
        host.send(action);
    }
    assert_handled_busy(&host.fence());
    assert_workflow_defers(Row::Queue);
    host.quit();
}

#[test]
fn root_approval_reaches_the_running_turns_decisions() {
    // M: expects B (today a workflow also defers it).
    assert_today(Row::Approval, Column::Root, Cell::Classes(&[B, W]));
    let host = Host::busy();
    host.send(UserAction::ResolveApproval {
        approval_id: "nothing-pending".into(),
        choice: plakous_host_protocol::ApprovalChoice::Allow,
    });
    let seen = host.fence();
    assert_handled_busy(&seen);
    assert_eq!(notes(&seen), [super::APPROVAL_REFUSED_NOTE], "{seen:#?}");
    assert!(
        !seen.iter().any(|event| matches!(
            event,
            HostEvent::Status {
                phase: HostPhase::Interrupting,
                ..
            }
        )),
        "an unknown decision leaves the turn running"
    );
    assert_workflow_defers(Row::Approval);
    host.quit();
}

#[test]
fn root_quit_keeps_its_place_and_ends_a_busy_host() {
    assert_today(Row::Quit, Column::Root, Cell::Classes(&[U]));
    assert_eq!(
        IntentClass::of(&UserAction::Quit).disposition(DispatchPhase::Workflow),
        Disposition::Defer,
        "a workflow keeps quit's FIFO place"
    );
    let host = Host::busy();
    host.send(UserAction::FollowUp("before quit".into()));
    host.send(UserAction::Quit);
    let seen = host.until("the follow-up taken before quit", |event| {
        matches!(event, HostEvent::DraftAccepted(UserAction::FollowUp(_)))
    });
    assert_handled_busy(&seen);
    // The held turn is cancelled and the host ends without a release.
    host.worker.join().unwrap().unwrap();
}

#[test]
fn root_workflows_are_refused_while_busy() {
    assert_today(Row::Workflow, Column::Root, Cell::Classes(&[I, W]));
    let host = Host::busy();
    let intents = intents_of(Row::Workflow);
    for action in &intents {
        host.send(action.clone());
    }
    assert_refused_busy(&host.fence(), &intents);
    assert_workflow_defers(Row::Workflow);
    host.quit();
}

#[test]
fn root_readiness_is_read_while_busy_and_retry_attach_waits() {
    // M: expects I (today `RefreshReadiness` is answered mid-turn, and a
    // workflow defers both).
    assert_today(Row::Readiness, Column::Root, Cell::Classes(&[I, B, W]));
    let host = Host::busy();
    let before = host.surface_calls("publish_template_readiness");
    host.send(UserAction::RefreshReadiness);
    host.send(UserAction::RetryAttach);
    let seen = host.fence();
    assert_refused_busy(&seen, &[UserAction::RetryAttach]);
    assert_eq!(
        host.surface_calls("publish_template_readiness"),
        before + 1,
        "readiness is republished mid-turn"
    );
    assert_workflow_defers(Row::Readiness);
    host.quit();
}

#[test]
fn root_recover_is_refused_while_busy() {
    // M: expects I (today a workflow also defers it).
    assert_today(Row::Recover, Column::Root, Cell::Classes(&[I, W]));
    let host = Host::busy();
    host.send(UserAction::Recover);
    assert_refused_busy(&host.fence(), &[UserAction::Recover]);
    assert_workflow_defers(Row::Recover);
    host.quit();
}

#[test]
fn root_navigator_is_answered_while_busy() {
    // M: expects B (today a workflow also defers them).
    assert_today(Row::Navigator, Column::Root, Cell::Classes(&[B, W]));
    let host = Host::busy();
    host.send(UserAction::OpenTopologyNavigator);
    let seen = host.fence();
    assert_handled_busy(&seen);
    assert!(
        seen.iter().any(|event| matches!(
            event,
            HostEvent::TopologyNavigatorUnavailable { .. }
                | HostEvent::TopologyNavigatorSnapshot(_)
        )),
        "{seen:#?}"
    );
    assert_workflow_defers(Row::Navigator);
    host.quit();
}

#[test]
fn root_transcript_resync_is_answered_while_busy() {
    // M: expects B (today a workflow also defers them).
    assert_today(Row::Transcript, Column::Root, Cell::Classes(&[B, W]));
    let host = Host::busy();
    host.send(UserAction::ResyncTranscript {
        scope: TranscriptScope::Root,
    });
    host.send(UserAction::CloseAgentTranscript);
    let seen = host.fence();
    assert_handled_busy(&seen);
    assert!(
        seen.iter().any(|event| matches!(
            event,
            HostEvent::PresentationSnapshot(snapshot) if snapshot.scope == TranscriptScope::Root
        )),
        "{seen:#?}"
    );
    assert_workflow_defers(Row::Transcript);
    host.quit();
}

#[test]
fn root_invoke_tool_runs_only_when_idle() {
    // M: expects B.
    assert_today(Row::InvokeTool, Column::Root, Cell::Classes(&[I, W]));
    let invoke = || UserAction::InvokeTool {
        correlation_id: None,
        name: "list_agents".into(),
        arguments: "{}".into(),
        agent_id: None,
    };
    let host = Host::idle();
    host.send(invoke());
    let idle = host.fence();
    assert_eq!(idle_only_notes(&idle), [] as [&str; 0], "{idle:#?}");
    assert!(
        idle.iter().any(|event| matches!(
            event,
            HostEvent::ToolLine { name, state, .. } if name == "list_agents" && state != "start"
        )) || notes(&idle).iter().any(|text| text.contains("invoke_tool")),
        "the idle loop ran it: {idle:#?}"
    );
    host.hold();
    host.send(invoke());
    assert_refused_busy(&host.fence(), &[invoke()]);
    assert_workflow_defers(Row::InvokeTool);
    host.quit();
}

#[test]
fn root_call_detail_is_answered_while_busy() {
    // M: expects B (today a workflow also defers it).
    assert_today(Row::CallDetail, Column::Root, Cell::Classes(&[B, W]));
    let host = Host::busy();
    host.send(UserAction::RequestCallDetail {
        call_id: "no-such-call".into(),
        agent_id: None,
        session_id: host.session_id.clone(),
    });
    let seen = host.until(
        "the call detail",
        |event| matches!(event, HostEvent::CallDetail(detail) if detail.call_id == "no-such-call"),
    );
    assert_handled_busy(&seen);
    let Some(HostEvent::CallDetail(detail)) = seen.last() else {
        unreachable!()
    };
    assert_eq!(detail.agent_id, None);
    assert_eq!(detail.session_id, host.session_id);
    assert_workflow_defers(Row::CallDetail);
    host.quit();
}

#[test]
fn root_templates_and_extensions_are_handled_while_busy() {
    // M: expects B (today a workflow also defers them).
    assert_today(Row::Templates, Column::Root, Cell::Classes(&[B, W]));
    let host = Host::busy();
    let (templates, extensions) = (
        host.surface_calls("publish_template_readiness"),
        host.surface_calls("extension"),
    );
    for action in intents_of(Row::Templates) {
        host.send(action);
    }
    assert_handled_busy(&host.fence());
    assert_eq!(
        host.surface_calls("publish_template_readiness"),
        templates + 1
    );
    assert_eq!(host.surface_calls("extension"), extensions + 1);
    assert_workflow_defers(Row::Templates);
    host.quit();
}

#[test]
fn root_has_no_child_only_intent() {
    assert_today(Row::ChildOnly, Column::Root, Cell::Absent);
    assert_eq!(intents_of(Row::ChildOnly), [] as [UserAction; 0]);
}

/// R: no intent of `row` can name a child, so a child face has no route
/// for any of them; the host applies them to the root session.
fn assert_names_no_child(row: Row) {
    let intents = intents_of(row);
    assert_ne!(intents, [] as [UserAction; 0]);
    for action in intents {
        assert_eq!(child_address(&action), None, "{action:?}");
    }
}

#[test]
fn child_queue_edits_name_no_child() {
    assert_today(Row::Queue, Column::Child, Cell::Classes(&[R]));
    assert_names_no_child(Row::Queue);
}

#[test]
fn child_workflows_name_no_child() {
    assert_today(Row::Workflow, Column::Child, Cell::Classes(&[R]));
    assert_names_no_child(Row::Workflow);
}

#[test]
fn child_readiness_names_no_child() {
    assert_today(Row::Readiness, Column::Child, Cell::Classes(&[R]));
    assert_names_no_child(Row::Readiness);
}

#[test]
fn child_recover_names_no_child() {
    assert_today(Row::Recover, Column::Child, Cell::Classes(&[R]));
    assert_names_no_child(Row::Recover);
}

#[test]
fn child_templates_and_extensions_name_no_child() {
    // M: expects B for the usage extension; the rest stay R.
    assert_today(Row::Templates, Column::Child, Cell::Classes(&[R]));
    assert_names_no_child(Row::Templates);
}

/// B, scoped: a busy root opens a child's transcript, answered in that
/// agent's scope only.
#[test]
fn child_transcript_opens_scoped_while_the_root_is_busy() {
    assert_today(Row::Transcript, Column::Child, Cell::Classes(&[B]));
    let (host, child) = Host::busy_with_child();
    host.send(UserAction::OpenAgentTranscript {
        agent_id: child.clone(),
    });
    let seen = host.until("the agent-scoped snapshot", |event| {
        matches!(event, HostEvent::PresentationSnapshot(snapshot) if snapshot.scope == TranscriptScope::Agent(child.clone()))
    });
    assert_handled_busy(&seen);
    host.send(UserAction::CloseAgentTranscript);
    assert_handled_busy(&host.fence());
    host.quit();
}

/// B: a busy root answers a call-detail read addressed to a child, naming
/// that child.
#[test]
fn child_call_detail_is_answered_for_the_child_while_busy() {
    assert_today(Row::CallDetail, Column::Child, Cell::Classes(&[B]));
    let (host, child) = Host::busy_with_child();
    host.send(UserAction::RequestCallDetail {
        call_id: "child-call".into(),
        agent_id: Some(child.clone()),
        session_id: host.session_id.clone(),
    });
    let seen = host.until(
        "the child's call detail",
        |event| matches!(event, HostEvent::CallDetail(detail) if detail.call_id == "child-call"),
    );
    assert_handled_busy(&seen);
    let Some(HostEvent::CallDetail(detail)) = seen.last() else {
        unreachable!()
    };
    assert_eq!(detail.agent_id.as_deref(), Some(child.as_str()));
    host.quit();
}
