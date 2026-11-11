//! Attachments to a child node: the node's scope only, its intents as the
//! node's controls, and typed refusals for a node that is closed, retiring
//! or unknown.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use plakous_host_protocol::intent::DraftImage;
use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptRow};
use plakous_host_protocol::{Core, Draft, IntentReceiver, intent_channel};

use super::super::*;
use super::{Observation, approval, observations, root, stream};

/// A tree whose children stand where the test puts them, and that ends
/// each interrupt as the test says.
#[derive(Default)]
struct Nodes {
    info: Mutex<BTreeMap<String, plakous_host_protocol::views::SessionInfoView>>,
    states: Mutex<BTreeMap<String, NodeState>>,
    interrupted: Mutex<Vec<String>>,
    refuse: Mutex<Option<String>>,
}

impl Nodes {
    fn set(&self, agent: &str, state: NodeState) {
        self.states.lock().unwrap().insert(agent.to_owned(), state);
    }
}

impl ChildNodes for Nodes {
    fn session_info(
        &self,
        agent_id: &str,
    ) -> Option<plakous_host_protocol::views::SessionInfoView> {
        self.info.lock().unwrap().get(agent_id).cloned()
    }

    fn state(&self, agent_id: &str) -> NodeState {
        self.states
            .lock()
            .unwrap()
            .get(agent_id)
            .copied()
            .unwrap_or(NodeState::Unknown)
    }

    fn interrupt(&self, agent_id: &str, done: ControlDone) {
        self.interrupted.lock().unwrap().push(agent_id.to_owned());
        done(self.refuse.lock().unwrap().clone().map_or(Ok(()), Err));
    }
}

fn node(agent: &str) -> NodeAddress {
    NodeAddress {
        tree_id: "t1".into(),
        agent_id: agent.into(),
    }
}

fn child_service() -> (HostService<Core>, IntentReceiver<Core>, Arc<Nodes>) {
    let (actions, intents) = intent_channel();
    let nodes = Arc::new(Nodes::default());
    nodes.set("c1", NodeState::Open);
    let service = HostService::new(actions, root(), 16).with_nodes(nodes.clone());
    (service, intents, nodes)
}

#[test]
fn child_header_uses_its_own_placement_and_never_the_roots_chrome() {
    use plakous_host_protocol::views::SessionInfoView;
    use plakous_host_protocol::wire::RunnerLabel;

    let (service, _intents, nodes) = child_service();
    let child = SessionInfoView {
        runner: RunnerLabel {
            name: "child-runner".into(),
            runner_id: "child-id".into(),
        },
        working_dir: "/child/workspace".into(),
        ..Default::default()
    };
    nodes
        .info
        .lock()
        .unwrap()
        .insert("c1".into(), child.clone());
    service.observe(Observation::SessionInfo(SessionInfoView {
        runner: RunnerLabel {
            name: "root-runner".into(),
            runner_id: "root-id".into(),
        },
        working_dir: "/root/workspace".into(),
        ..Default::default()
    }));
    let attachment = service.attach(&node("c1"), Face::Remote).unwrap();
    assert_eq!(
        observations(service.take(attachment)),
        [Observation::SessionInfo(child)]
    );
}

fn agent_snapshot(agent: &str) -> Observation {
    Observation::PresentationSnapshot(PresentationSnapshot {
        scope: TranscriptScope::Agent(agent.into()),
        ..PresentationSnapshot::empty(3)
    })
}

/// The intent the session took next, with its `IntentStarted` correlation.
fn next(intents: &IntentReceiver<Core>) -> (SessionIntent<Core>, Option<Observation>) {
    let delivery = intents.try_recv().unwrap();
    let started = delivery.started().map(Observation::from);
    let (intent, _) = delivery.into_parts();
    (intent, started)
}

/// A closed, retiring or unknown node, a node of another tree, and any
/// child of a service without the tree's children are refused, each with
/// its own refusal.
#[test]
fn only_an_open_child_of_the_sessions_tree_attaches() {
    let (service, intents, nodes) = child_service();
    nodes.set("closed", NodeState::Closed);
    nodes.set("retiring", NodeState::Retiring);
    assert_eq!(
        service.attach(&node("closed"), Face::Remote),
        Err(RequestRefusal::NodeClosed)
    );
    assert_eq!(
        service.attach(&node("retiring"), Face::Remote),
        Err(RequestRefusal::NodeRetiring)
    );
    assert_eq!(
        service.attach(&node("nobody"), Face::Remote),
        Err(RequestRefusal::UnknownNode)
    );
    assert_eq!(
        service.attach(
            &NodeAddress {
                tree_id: "t2".into(),
                agent_id: "c1".into(),
            },
            Face::Remote
        ),
        Err(RequestRefusal::UnknownNode)
    );
    assert!(intents.try_recv().is_err(), "a refused attach asks nothing");
    let (actions, _intents) = intent_channel::<Core>();
    let rootless = HostService::new(actions, root(), 16);
    assert_eq!(
        rootless.attach(&node("c1"), Face::Remote),
        Err(RequestRefusal::UnknownNode)
    );
}

/// A child's attachment asks the session for its node's transcript, opened
/// for the first holder and resnapshotted for a later one, and takes that
/// scope and nothing else: no root state, root transcript, approval or
/// event.
#[test]
fn a_child_attach_takes_its_nodes_scope_and_only_that() {
    let (service, intents, _) = child_service();
    service.observe(Observation::SetBusy(true));
    service.observe(Observation::PresentationSnapshot(
        PresentationSnapshot::empty(5),
    ));
    service.observe(approval("a1"));
    let child = service.attach(&node("c1"), Face::Remote).unwrap();
    let (asked, started) = next(&intents);
    assert_eq!(
        asked,
        SessionIntent::OpenAgentTranscript {
            agent_id: "c1".into()
        }
    );
    assert_eq!(
        started,
        Some(Observation::IntentStarted(Some(format!(
            "remote-attachment:{child}"
        ))))
    );
    let second = service.attach(&node("c1"), Face::Remote).unwrap();
    assert_eq!(
        next(&intents).0,
        SessionIntent::ResyncTranscript {
            scope: TranscriptScope::Agent("c1".into())
        }
    );
    service.observe(agent_snapshot("c1"));
    service.observe(agent_snapshot("c2"));
    service.observe(stream(TranscriptScope::Root));
    service.observe(Observation::SetBusy(false));
    service.observe(approval("a2"));
    service.observe(stream(TranscriptScope::Agent("c1".into())));
    let expected = [
        agent_snapshot("c1"),
        stream(TranscriptScope::Agent("c1".into())),
    ];
    assert_eq!(observations(service.take(child)), expected);
    assert_eq!(observations(service.take(second)), expected);
}

/// Each control a child attachment sends, and the session intent it is.
fn controls() -> Vec<(SessionIntent<Core>, SessionIntent<Core>)> {
    vec![
        (
            SessionIntent::Steer(Draft::from("look again")),
            SessionIntent::MailTopologyNode {
                agent_id: "c1".into(),
                message: "look again".into(),
                urgent: true,
            },
        ),
        (
            SessionIntent::FollowUp(Draft::from("then the docs")),
            SessionIntent::MailTopologyNode {
                agent_id: "c1".into(),
                message: "then the docs".into(),
                urgent: false,
            },
        ),
        (
            SessionIntent::Background,
            SessionIntent::BackgroundTopologyNode {
                agent_id: "c1".into(),
            },
        ),
        (
            SessionIntent::StopProcesses {
                id: Some("p1".into()),
                interrupt: true,
            },
            SessionIntent::StopTopologyNodeProcesses {
                agent_id: "c1".into(),
                id: Some("p1".into()),
            },
        ),
        (
            SessionIntent::CloseTopologyNode {
                agent_id: "c1".into(),
            },
            SessionIntent::CloseTopologyNode {
                agent_id: "c1".into(),
            },
        ),
        (
            SessionIntent::RequestCallDetail {
                call_id: "call-1".into(),
                agent_id: None,
                session_id: "s1".into(),
            },
            SessionIntent::RequestCallDetail {
                call_id: "call-1".into(),
                agent_id: Some("c1".into()),
                session_id: "s1".into(),
            },
        ),
        (
            SessionIntent::OpenAgentTranscript {
                agent_id: "c1".into(),
            },
            SessionIntent::ResyncTranscript {
                scope: TranscriptScope::Agent("c1".into()),
            },
        ),
    ]
}

/// A child attachment's intents are its node's controls, sent tagged with
/// the attachment: mail (follow-up or steer), its interrupt, its close,
/// its background and process stop, call detail as the node, and its
/// transcript's resync. The rest is
/// refused for its row, approvals and tool calls included, and a quit
/// detaches it and, as the last holder, closes the node's view.
#[test]
fn a_child_attachments_intents_are_its_nodes_controls() {
    let (service, intents, _) = child_service();
    let child = service.attach(&node("c1"), Face::Remote).unwrap();
    let _ = next(&intents);
    for (intent, control) in controls() {
        service
            .intent_correlated(child, intent, Some("r1"))
            .unwrap();
        assert_eq!(
            next(&intents),
            (
                control,
                Some(Observation::IntentStarted(Some(format!(
                    "remote-attachment:{child}:r1"
                ))))
            )
        );
    }
    let refused = [
        (SessionIntent::Compact, "workflow"),
        (SessionIntent::DropQueued, "queue"),
        (SessionIntent::StopAllTopology, "navigator"),
        (
            SessionIntent::CloseTopologyNode {
                agent_id: "c2".into(),
            },
            "navigator",
        ),
        (
            SessionIntent::OpenAgentTranscript {
                agent_id: "c2".into(),
            },
            "transcript",
        ),
        (SessionIntent::RefreshTemplates, "templates"),
        (
            SessionIntent::ResolveApproval {
                approval_id: "a1".into(),
                choice: plakous_host_protocol::ApprovalChoice::Allow,
            },
            "approval",
        ),
        (
            SessionIntent::InvokeTool {
                correlation_id: None,
                name: "list_agents".into(),
                arguments: "{}".into(),
                agent_id: None,
            },
            "invoke_tool",
        ),
    ];
    for (intent, row) in refused {
        assert_eq!(
            service.intent(child, intent),
            Err(RequestRefusal::RefusedForRole { row: row.into() })
        );
    }
    let image = DraftImage {
        name: "a.png".into(),
        media_type: "image/png".into(),
        bytes: vec![1],
        source_dimensions: None,
    };
    assert!(matches!(
        service.intent(
            child,
            SessionIntent::Steer(Draft::new("see".into(), vec![image]))
        ),
        Err(RequestRefusal::Malformed { .. })
    ));
    assert_eq!(
        service.intent(child, SessionIntent::CloseAgentTranscript),
        Ok(())
    );
    assert!(
        intents.try_recv().is_err(),
        "nothing refused reached the session"
    );
    assert_eq!(service.intent(child, SessionIntent::Quit), Ok(()));
    assert!(!service.attached(child), "a quit detaches the child's face");
    assert_eq!(
        next(&intents),
        (
            SessionIntent::CloseAgentTranscript,
            Some(Observation::IntentStarted(None))
        ),
        "the last holder's quit closes the view, as no attachment's intent"
    );
    assert!(intents.try_recv().is_err(), "and never quits the session");
}

/// Once the node is closed, retiring or gone, its attachment's controls
/// are refused with that refusal; its quit still detaches it.
#[test]
fn controls_of_a_closed_retiring_or_unknown_node_are_refused() {
    let (service, intents, nodes) = child_service();
    let child = service.attach(&node("c1"), Face::Remote).unwrap();
    let _ = next(&intents);
    for (state, refusal) in [
        (NodeState::Retiring, RequestRefusal::NodeRetiring),
        (NodeState::Closed, RequestRefusal::NodeClosed),
        (NodeState::OutOfReach, RequestRefusal::OutOfReach),
        (NodeState::Unknown, RequestRefusal::UnknownNode),
    ] {
        nodes.set("c1", state);
        for intent in [
            SessionIntent::Interrupt,
            SessionIntent::Steer(Draft::from("still there?")),
            SessionIntent::ResyncTranscript {
                scope: TranscriptScope::Agent("c1".into()),
            },
        ] {
            assert_eq!(service.intent(child, intent), Err(refusal.clone()));
        }
    }
    assert!(intents.try_recv().is_err());
    assert_eq!(service.intent(child, SessionIntent::Quit), Ok(()));
    assert!(!service.attached(child));
    nodes.set("c1", NodeState::OutOfReach);
    assert_eq!(
        service.attach(&node("c1"), Face::Remote),
        Err(RequestRefusal::OutOfReach),
        "attach and controls agree on the operator's reach"
    );
}

/// One root and two child attachments: a child gets the start of its own
/// intent and only the answers marked as its control's; whatever else the
/// session sends in that window, addressed answers and error rows
/// included, is the root's, and one child never sees the other's answers.
#[test]
fn only_a_childs_marked_answers_reach_it() {
    let (service, intents, _) = child_service();
    let face = service.attach(&root(), Face::Remote).unwrap();
    let first = service.attach(&node("c1"), Face::Remote).unwrap();
    let second = service.attach(&node("c1"), Face::Remote).unwrap();
    while intents.try_recv().is_ok() {}
    service.take(face);
    service.take(first);
    service.take(second);
    service
        .intent_correlated(first, SessionIntent::Steer(Draft::from("now")), Some("r1"))
        .unwrap();
    let started = intents.try_recv().unwrap().started().unwrap();
    service.observe(started.into());
    let restored = Observation::DraftRestored(Draft::from("root's draft"));
    let root_failure =
        Observation::Row(TranscriptRow::error(NoticeScope::Host, "root turn failed"));
    let reset = Observation::PromptCorrelationReset;
    for observation in [restored.clone(), root_failure.clone(), reset.clone()] {
        service.observe(observation);
    }
    // The control's publisher answers in a marked window of its own.
    let accepted = Observation::DraftAccepted(SessionIntent::Steer(Draft::from("now")));
    let refused = Observation::Row(TranscriptRow::error(NoticeScope::Tree, "mail refused"));
    service.observe(Observation::IntentStarted(Some(format!(
        "{ANSWER}attachment:{first}:r1"
    ))));
    service.observe(accepted.clone());
    service.observe(refused.clone());
    service.observe(Observation::IntentStarted(None));
    service.observe(Observation::AssistantDone);
    assert_eq!(
        observations(service.take(first)),
        [
            Observation::IntentStarted(Some("r1".into())),
            Observation::IntentStarted(Some("r1".into())),
            accepted,
            refused,
        ]
    );
    assert_eq!(observations(service.take(second)), []);
    assert_eq!(
        observations(service.take(face)),
        [
            restored,
            root_failure,
            reset,
            Observation::IntentStarted(None),
            Observation::AssistantDone,
        ]
    );
}

/// The session serves one agent transcript: while one is held, another is
/// refused to a child's attach and to a root attachment's open alike; the
/// last holder's detach closes the view, after which another may open.
#[test]
fn a_second_agent_view_is_refused_while_one_is_held() {
    let (service, intents, nodes) = child_service();
    nodes.set("c2", NodeState::Open);
    let face = service.attach(&root(), Face::Remote).unwrap();
    let child = service.attach(&node("c1"), Face::Remote).unwrap();
    while intents.try_recv().is_ok() {}
    let transcript = RequestRefusal::RefusedForRole {
        row: "transcript".into(),
    };
    assert_eq!(
        service.attach(&node("c2"), Face::Remote),
        Err(transcript.clone())
    );
    assert_eq!(
        service.intent(
            face,
            SessionIntent::OpenAgentTranscript {
                agent_id: "c2".into()
            }
        ),
        Err(transcript)
    );
    assert!(
        intents.try_recv().is_err(),
        "nothing refused reached the session"
    );
    // The same agent is shared.
    service
        .intent(
            face,
            SessionIntent::OpenAgentTranscript {
                agent_id: "c1".into(),
            },
        )
        .unwrap();
    assert_eq!(
        next(&intents).0,
        SessionIntent::ResyncTranscript {
            scope: TranscriptScope::Agent("c1".into())
        }
    );
    service.detach(child).unwrap();
    assert!(intents.try_recv().is_err(), "the root still holds the view");
    service
        .intent(face, SessionIntent::CloseAgentTranscript)
        .unwrap();
    assert_eq!(next(&intents).0, SessionIntent::CloseAgentTranscript);
    let other = service.attach(&node("c2"), Face::Remote).unwrap();
    assert_eq!(
        next(&intents).0,
        SessionIntent::OpenAgentTranscript {
            agent_id: "c2".into()
        }
    );
    service.detach(other).unwrap();
    assert_eq!(next(&intents).0, SessionIntent::CloseAgentTranscript);
}

/// A child attachment's interrupt, or the navigator's interrupt naming its
/// node, is the node's own: it runs on the tree, not through the root
/// session's mailbox, and a refusal comes back to the attachment alone.
#[test]
fn a_childs_interrupt_runs_on_its_node_not_the_root_mailbox() {
    let (service, intents, nodes) = child_service();
    let face = service.attach(&root(), Face::Remote).unwrap();
    let child = service.attach(&node("c1"), Face::Remote).unwrap();
    while intents.try_recv().is_ok() {}
    service.take(face);
    service.take(child);
    service.intent(child, SessionIntent::Interrupt).unwrap();
    service
        .intent(
            child,
            SessionIntent::InterruptTopologyNode {
                agent_id: "c1".into(),
            },
        )
        .unwrap();
    assert_eq!(*nodes.interrupted.lock().unwrap(), ["c1", "c1"]);
    assert!(
        intents.try_recv().is_err(),
        "the root session's mailbox takes no node interrupt"
    );
    assert_eq!(observations(service.take(child)), []);
    *nodes.refuse.lock().unwrap() = Some("tree publish refused".into());
    service
        .intent_correlated(child, SessionIntent::Interrupt, Some("r9"))
        .unwrap();
    assert_eq!(
        observations(service.take(child)),
        [
            Observation::IntentStarted(Some("r9".into())),
            Observation::Row(TranscriptRow::error(
                NoticeScope::Tree,
                "interrupt failed: tree publish refused"
            )),
        ]
    );
    assert_eq!(observations(service.take(face)), []);
}
