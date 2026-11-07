//! Attachments to a child node of the session's tree (spec 02 §4.4).
//!
//! A child's attachment holds that node's transcript scope and nothing else:
//! its attach snapshot is that transcript's baseline, which the session
//! opens for the scope's first holder and resnapshots for a later one, and
//! it takes no root state, event, approval, wait or answer. The session
//! serves one agent transcript, so another agent's is refused while one is
//! held. Its intents are the node's controls, sent through the session's
//! mailbox tagged with the attachment; it gets back the start of each and
//! only the answers its control's publisher marks as that control's
//! (`Sender::answering`). Mail is tree mail from the operator, which reaches
//! the nodes the tree's root may write to; an interrupt is the tree's
//! interrupt with the operator as its cause, run on the tree itself so it
//! is urgent whatever the root runs, and a close of the attached
//! node is the tree's close; `background`, a process stop and the usage view
//! are the node's own, by node address. A `Quit` is the attachment's
//! detach; the node runs on. Approvals and tool calls are refused until
//! their answers are routed to the node's attachment. Every control is
//! refused, with its own refusal, when the node is closed, retiring, out
//! of reach or unknown.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, PoisonError};

use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptRow, TranscriptScope};
use plakous_host_protocol::wire::{AttachmentId, RequestRefusal};
use plakous_host_protocol::{IntentSender, ProtocolExtension, SessionIntent, SessionObservation};

use super::wire::Face;
use super::{Attachment, HostService, Scopes, origin, refusal_of};

/// Where a child node of the session's tree stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeState {
    /// It takes attachments and controls.
    Open,
    /// Its host is releasing it.
    Retiring,
    /// It never runs again.
    Closed,
    /// The operator does not reach it: the tree's root may not write to it.
    OutOfReach,
    /// The session's tree has no such child.
    Unknown,
}

/// How a node control the service starts ended.
pub type ControlDone = Box<dyn FnOnce(Result<(), String>) + Send>;

/// The session tree's children, as its host service reaches them.
pub trait ChildNodes: Send + Sync {
    /// Live chrome for this node's own saved placement, without opening a runtime.
    fn session_info(
        &self,
        _agent_id: &str,
    ) -> Option<plakous_host_protocol::views::SessionInfoView> {
        None
    }

    /// Where the child `agent_id` stands; the tree's root is no child.
    fn state(&self, agent_id: &str) -> NodeState;

    /// Starts the operator's interrupt of the child `agent_id` on the tree
    /// itself, not behind the root session's mailbox, and returns at once;
    /// `done` takes how it ended.
    fn interrupt(&self, agent_id: &str, done: ControlDone);
}

/// The refusal for a node that does not take an attachment or control.
fn admitted(state: NodeState) -> Result<(), RequestRefusal> {
    match state {
        NodeState::Open => Ok(()),
        NodeState::Retiring => Err(RequestRefusal::NodeRetiring),
        NodeState::Closed => Err(RequestRefusal::NodeClosed),
        NodeState::OutOfReach => Err(RequestRefusal::OutOfReach),
        NodeState::Unknown => Err(RequestRefusal::UnknownNode),
    }
}

/// The role-table row an intent belongs to, as a refusal names it.
const fn row<P: ProtocolExtension>(intent: &SessionIntent<P>) -> &'static str {
    match intent {
        SessionIntent::Prompt(_) | SessionIntent::FollowUp(_) => "prompt",
        SessionIntent::Steer(_) => "steer",
        SessionIntent::Interrupt => "interrupt",
        SessionIntent::StopProcesses { .. }
        | SessionIntent::Background
        | SessionIntent::BackgroundTopologyNode { .. }
        | SessionIntent::StopTopologyNodeProcesses { .. } => "processes",
        SessionIntent::DropQueued
        | SessionIntent::SendQueuedNow { .. }
        | SessionIntent::DeleteQueued { .. }
        | SessionIntent::EditQueued { .. }
        | SessionIntent::MoveQueued { .. }
        | SessionIntent::ResumeQueueDrain => "queue",
        SessionIntent::ResolveApproval { .. } => "approval",
        SessionIntent::Quit
        | SessionIntent::StopSession { .. }
        | SessionIntent::ReleaseForReopen { .. } => "quit",
        SessionIntent::NewSession
        | SessionIntent::Compact
        | SessionIntent::ForkSession { .. }
        | SessionIntent::ResumeSession { .. }
        | SessionIntent::ResumeSessionHandoff { .. } => "workflow",
        SessionIntent::RefreshReadiness
        | SessionIntent::RetryAttach
        | SessionIntent::Promptable => "readiness",
        SessionIntent::Recover => "recover",
        SessionIntent::OpenTopologyNavigator
        | SessionIntent::RefreshTopologyTree
        | SessionIntent::InterruptTopologyNode { .. }
        | SessionIntent::CloseTopologyNode { .. }
        | SessionIntent::MailTopologyNode { .. }
        | SessionIntent::StopAllTopology
        | SessionIntent::RecoverTopologyNode { .. }
        | SessionIntent::ResumeLossContinuation { .. } => "navigator",
        SessionIntent::OpenAgentTranscript { .. }
        | SessionIntent::CloseAgentTranscript
        | SessionIntent::ResyncTranscript { .. } => "transcript",
        SessionIntent::InvokeTool { .. } => "invoke_tool",
        SessionIntent::RequestCallDetail { .. } => "call_detail",
        SessionIntent::RefreshTemplates
        | SessionIntent::Extension(_)
        | SessionIntent::ReadTopologyNodeUsage { .. } => "templates",
    }
}

/// What a child attachment's intent does: the session intent it sends, or
/// nothing more to send.
enum Routed<P: ProtocolExtension> {
    Send(SessionIntent<P>),
    /// The node's own interrupt.
    Interrupt,
    Done,
}

/// The session intent that is `intent` on the child `agent`.
fn route<P: ProtocolExtension>(
    agent: &str,
    intent: SessionIntent<P>,
) -> Result<Routed<P>, RequestRefusal> {
    let node = || agent.to_owned();
    let own = |agent_id: &str| agent_id == agent;
    let routed = match intent {
        SessionIntent::Prompt(draft) | SessionIntent::FollowUp(draft)
            if draft.images.is_empty() =>
        {
            SessionIntent::MailTopologyNode {
                agent_id: node(),
                message: draft.text,
                urgent: false,
            }
        }
        SessionIntent::Steer(draft) if draft.images.is_empty() => SessionIntent::MailTopologyNode {
            agent_id: node(),
            message: draft.text,
            urgent: true,
        },
        SessionIntent::Prompt(_) | SessionIntent::FollowUp(_) | SessionIntent::Steer(_) => {
            return Err(RequestRefusal::Malformed {
                detail: "tree mail carries text only".to_owned(),
            });
        }
        // The node's interrupt is urgent: it runs on the tree, not behind
        // the root session's mailbox.
        SessionIntent::Interrupt => return Ok(Routed::Interrupt),
        SessionIntent::Background => SessionIntent::BackgroundTopologyNode { agent_id: node() },
        SessionIntent::StopProcesses { id, .. } => SessionIntent::StopTopologyNodeProcesses {
            agent_id: node(),
            id,
        },
        SessionIntent::Extension(intent) if P::reads_usage(&intent) => {
            SessionIntent::ReadTopologyNodeUsage { agent_id: node() }
        }
        SessionIntent::InterruptTopologyNode { agent_id } if own(&agent_id) => {
            return Ok(Routed::Interrupt);
        }
        SessionIntent::CloseTopologyNode { agent_id } if own(&agent_id) => {
            SessionIntent::CloseTopologyNode { agent_id }
        }
        SessionIntent::RequestCallDetail {
            call_id,
            session_id,
            ..
        } => SessionIntent::RequestCallDetail {
            call_id,
            agent_id: Some(node()),
            session_id,
        },
        SessionIntent::OpenAgentTranscript { agent_id } if own(&agent_id) => {
            SessionIntent::ResyncTranscript {
                scope: TranscriptScope::Agent(agent_id),
            }
        }
        SessionIntent::ResyncTranscript {
            scope: TranscriptScope::Agent(agent_id),
        } if own(&agent_id) => SessionIntent::ResyncTranscript {
            scope: TranscriptScope::Agent(agent_id),
        },
        // The attachment holds its node's transcript for as long as it is
        // attached.
        SessionIntent::CloseAgentTranscript => return Ok(Routed::Done),
        other => {
            return Err(RequestRefusal::RefusedForRole {
                row: row(&other).to_owned(),
            });
        }
    };
    Ok(Routed::Send(routed))
}

impl<P: ProtocolExtension> HostService<P> {
    /// Attaches to the child `agent`: the attachment holds its transcript,
    /// whose baseline the session is asked for.
    pub(super) fn attach_child(
        &self,
        agent: &str,
        face: Face,
    ) -> Result<AttachmentId, RequestRefusal> {
        let Some(nodes) = &self.nodes else {
            return Err(RequestRefusal::UnknownNode);
        };
        admitted(nodes.state(agent))?;
        let info = nodes.session_info(agent);
        let Some(actions) = self.mailbox() else {
            return Err(RequestRefusal::Disconnected);
        };
        let scope = TranscriptScope::Agent(agent.to_owned());
        // Held across the ask, so the baseline the session answers with
        // finds the attachment holding its scope.
        let mut state = self.lock();
        // The session serves one agent transcript.
        if state.holds_another(agent, None) {
            return Err(RequestRefusal::RefusedForRole {
                row: "transcript".to_owned(),
            });
        }
        // A releasing session takes no attachment; refused before the ask.
        if state.count.releasing() {
            return Err(RequestRefusal::NodeRetiring);
        }
        let id = state.next_attachment;
        let sender = actions.tagged(origin(id));
        let ask = if state.agent_holders.get(agent).copied().unwrap_or(0) > 0 {
            SessionIntent::ResyncTranscript {
                scope: scope.clone(),
            }
        } else {
            SessionIntent::OpenAgentTranscript {
                agent_id: agent.to_owned(),
            }
        };
        sender.send(ask).map_err(|error| refusal_of(&error))?;
        state.next_attachment += 1;
        let waiting = state.holding;
        state.insert_counted(
            id,
            Attachment {
                face,
                sender: Some(sender),
                node: Some(agent.to_owned()),
                queue: VecDeque::new(),
                seq: 0,
                scopes: Scopes::Held(BTreeSet::from([scope])),
                gapped: BTreeSet::new(),
                snapshot_held: 0,
                dropped: false,
                // As a root attachment made before an unresolved
                // opening's binding, it takes nothing until that binding.
                waiting,
            },
        )?;
        if let Some(info) = info
            && let Some(held) = state.attachments.get_mut(&id)
        {
            let observation = SessionObservation::SessionInfo(info);
            let class = observation.class();
            held.offer(observation, class, self.capacity);
        }
        *state.agent_holders.entry(agent.to_owned()).or_default() += 1;
        Ok(id)
    }

    /// `intent` from the attachment to the child `agent`, as that node's
    /// control.
    pub(super) fn child_intent(
        &self,
        attachment: AttachmentId,
        agent: &str,
        sender: &IntentSender<P>,
        intent: SessionIntent<P>,
        client: Option<&str>,
    ) -> Result<(), RequestRefusal> {
        if matches!(intent, SessionIntent::Quit) {
            return self.detach(attachment);
        }
        let Some(nodes) = &self.nodes else {
            return Err(RequestRefusal::UnknownNode);
        };
        admitted(nodes.state(agent))?;
        match route(agent, intent)? {
            Routed::Send(routed) => sender
                .send_correlated(routed, client.map(str::to_owned))
                .map_err(|error| refusal_of(&error)),
            Routed::Interrupt => {
                // Its refusal is the attachment's answer, in a window of its
                // own.
                let (state, capacity) = (Arc::clone(&self.state), self.capacity);
                let client = client.map(str::to_owned);
                nodes.interrupt(
                    agent,
                    Box::new(move |interrupted| {
                        let Err(error) = interrupted else {
                            return;
                        };
                        let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
                        if let Some(held) = state.attachments.get_mut(&attachment) {
                            for answer in [
                                SessionObservation::IntentStarted(client),
                                SessionObservation::Row(TranscriptRow::error(
                                    NoticeScope::Tree,
                                    format!("interrupt failed: {error}"),
                                )),
                            ] {
                                let class = answer.class();
                                held.offer(answer, class, capacity);
                            }
                        }
                    }),
                );
                Ok(())
            }
            Routed::Done => Ok(()),
        }
    }
}
