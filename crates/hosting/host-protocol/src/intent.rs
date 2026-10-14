//! Intents: what a driver asks of the hosted session.

use std::fmt;

/// The product-specific payloads a driver threads through the protocol's
/// mailboxes beside the closed intents and observations.
///
/// The protocol never reads them. The bounded channels only need to classify
/// an observation for eviction, which is what [`ObservationExtension`] says.
/// [`Core`] is the extension-free protocol every external client speaks.
pub trait ProtocolExtension: Clone + fmt::Debug + Eq + Send + Sync + 'static {
    /// Driver-owned intent payload.
    type Intent: Clone + fmt::Debug + Eq + Send + 'static;
    /// Driver-owned observation payload.
    type Observation: ObservationExtension;
}

/// Mailbox classification the observation channel applies to an extension.
pub trait ObservationExtension: Clone + fmt::Debug + Eq + Send + 'static {
    /// Whether the observation replaces its own older value in the mailbox
    /// (a snapshot) rather than accumulating (a delta or a note).
    fn latest_state(&self) -> bool;
    /// Whether two latest-state observations replace each other.
    fn same_kind(&self, other: &Self) -> bool;
    /// Content-free kind label for diagnostics.
    fn kind_label(&self) -> &'static str;
}

/// The closed protocol with no extension traffic.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Core;

/// Uninhabited: the [`Core`] protocol has no extension payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Never {}

impl ObservationExtension for Never {
    fn latest_state(&self) -> bool {
        match *self {}
    }

    fn same_kind(&self, _other: &Self) -> bool {
        match *self {}
    }

    fn kind_label(&self) -> &'static str {
        match *self {}
    }
}

impl ProtocolExtension for Core {
    type Intent = Never;
    type Observation = Never;
}

/// Typed intents a driver sends to the hosted session (today the TUI, the
/// JSONL face, and `plak run`).
///
/// The closed protocol is every variant but [`Self::Extension`]; that slot
/// carries the driver's own product traffic (settings, overlays) through the
/// same bounded mailbox without the protocol interpreting it.
#[derive(Clone, Eq, PartialEq)]
pub enum SessionIntent<P: ProtocolExtension = Core> {
    /// Idle submit — start a new turn.
    Prompt(String),
    /// Busy submit — queue for delivery on turn complete.
    FollowUp(String),
    /// Busy "send now" — after current tool; cancel remaining tools in compose.
    Steer(String),
    /// Promote only top-level process calls already in flight; otherwise no-op.
    Background,
    /// Full interrupt while busy (session stays open).
    Interrupt,
    /// Stop one exact background process, or all when `id` is `None`.
    /// `interrupt` also interrupts the root turn and its descendants.
    StopProcesses {
        /// Host-local identity from the process snapshot.
        id: Option<String>,
        /// Also stop the turn and agents (the Esc popup's `p`).
        interrupt: bool,
    },
    /// Legacy `/drop` parser token. The UI translates this to [`Self::DeleteQueued`]
    /// before sending an action to the host.
    #[doc(hidden)]
    DropQueued,
    /// Send one exact queued item at the next safe boundary.
    SendQueuedNow {
        /// Stable queue item identity.
        id: String,
        /// Revision observed in the authoritative host snapshot.
        revision: u64,
    },
    /// Delete one exact queued item.
    DeleteQueued {
        /// Stable queue item identity.
        id: String,
        /// Revision observed in the authoritative host snapshot.
        revision: u64,
    },
    /// Replace the text of one exact queued item.
    EditQueued {
        /// Stable queue item identity.
        id: String,
        /// Revision observed in the authoritative host snapshot.
        revision: u64,
        /// Replacement prompt text.
        text: String,
    },
    /// Move one exact queued item to an absolute FIFO index.
    MoveQueued {
        /// Stable queue item identity.
        id: String,
        /// Revision observed in the authoritative host snapshot.
        revision: u64,
        /// Requested zero-based position in the queue.
        new_index: usize,
    },
    /// Explicitly authorize delivery of a restored, paused queue.
    ResumeQueueDrain,
    /// Decide one pending effect approval the host offered.
    ///
    /// `approval_id` is the host-minted handle carried by
    /// [`SessionObservation::ApprovalRequired`](crate::observation::SessionObservation::ApprovalRequired).
    /// The TUI never sees, constructs or
    /// forwards a runtime `EffectDecisionRef`; an unknown or already-retired
    /// handle is refused by the host without contacting the runtime, so this
    /// intent carries a decision *request* and never a grant.
    ResolveApproval {
        /// Host-minted approval handle.
        approval_id: String,
        /// Operator choice.
        choice: ApprovalChoice,
    },
    /// Leave the TUI.
    Quit,
    /// Clear / new in-process session (host drops conversation attachment).
    ClearSession,
    /// Trigger product compaction.
    Compact,
    /// Resume a session by uuid, or list cwd sessions when `id` is `None`.
    ResumeSession {
        /// Session uuid to load; `None` shows the numbered list (`/resume <uuid>`).
        id: Option<String>,
    },
    /// Leave raw mode and relaunch an exact differently-bound session.
    ResumeSessionHandoff {
        /// Exact durable product session id.
        id: String,
    },
    /// Fork the current session (optional rebind account).
    ForkSession {
        /// Optional account for the forked session (the first argument).
        /// A cross-account fork is refused; changing the binding is a profile fork.
        account: Option<String>,
        /// After the fork attaches, compact that new session only.
        compact: bool,
    },
    /// Re-read every vertical's credential readiness and republish both the
    /// provider snapshot and the agent-template readiness projection.
    RefreshReadiness,
    /// Exit the TUI and ask the app host to re-resolve OAuth + re-attach the agent.
    RetryAttach,
    /// Recover an interrupted turn now: reopen the session, record once what
    /// the turn left unobserved, and continue. The next prompt does the same
    /// on its own; this is the explicit control.
    Recover,
    /// Open the inert agent-tree navigator overlay.
    OpenTopologyNavigator,
    /// Open one agent's transcript for the main view. The host answers with one
    /// `AgentTranscript` of its committed history, then streams the agent's live
    /// turn as `ChildLive` ops. Opening another agent replaces it.
    OpenAgentTranscript {
        /// Exact agent id from the tree.
        agent_id: String,
    },
    /// Close the open agent transcript and drop its live subscription.
    CloseAgentTranscript,
    /// Ask the host to re-emit the current tree snapshot (canvas `R`). The host
    /// answers even when the revision has not advanced, so a dropped push can
    /// always be recovered by hand.
    RefreshTopologyTree,
    /// Resume a `RestoredPaused` tree (`ResumeTree`).
    ResumeTopologyTree,
    /// Interrupt one exact node after overlay confirmation.
    InterruptTopologyNode {
        /// Exact agent id.
        agent_id: String,
    },
    /// Close one exact node (and its subtree) after overlay confirmation.
    CloseTopologyNode {
        /// Exact agent id.
        agent_id: String,
    },
    /// Host stop-all: postorder-close every non-root non-terminal node.
    StopAllTopology,
    /// Ask the host for the current agent-template readiness projection
    /// (spec 02 §5.5). Answered with [`SessionObservation::TemplateReadiness`](crate::observation::SessionObservation::TemplateReadiness).
    RefreshTemplates,
    /// Maintainer JSONL: run one granted topology tool on the hub tree.
    ///
    /// Idle-only. `name` is a closed topology tool. `arguments` is canonical JSON
    /// object text (the same fields the model would send). `agent_id` is the
    /// caller; `None` means the attached root. Not a coding-tool inject.
    InvokeTool {
        /// Optional surface command identity, separate from the tool operation UUID.
        correlation_id: Option<String>,
        /// Closed topology name (`spawn_agent`, `submit_result`, …).
        name: String,
        /// Canonical JSON object text.
        arguments: String,
        /// Caller agent id; `None` means the attached root.
        agent_id: Option<String>,
    },
    /// Read one tool call's full input and result from the attached session,
    /// or from one child node's own session. Answered with at most one
    /// [`SessionObservation::CallDetail`](crate::observation::SessionObservation::CallDetail)
    /// naming the same call and agent and the session the host resolved
    /// against. A missing answer (saturation, a coalesced duplicate, or a read
    /// dropped because that session is no longer attached) is recovered by
    /// asking again.
    RequestCallDetail {
        /// Exact call id from a transcript tool row.
        call_id: String,
        /// A child node of the attached agent tree whose session holds the
        /// call; `None` means the attached session itself.
        agent_id: Option<String>,
        /// The product session the surface has attached and is asking about.
        session_id: String,
    },
    /// Product-owned traffic the protocol carries but never interprets.
    ///
    /// Ordinary lane: never urgent, never coalesced, never non-repeatable.
    Extension(P::Intent),
}

impl<P: ProtocolExtension> fmt::Debug for SessionIntent<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Prompt(_) => "Prompt(..)",
            Self::FollowUp(_) => "FollowUp(..)",
            Self::Steer(_) => "Steer(..)",
            Self::Background => "Background",
            Self::Interrupt => "Interrupt",
            Self::StopProcesses { .. } => "StopProcesses(..)",
            Self::DropQueued => "DropQueued",
            Self::SendQueuedNow { .. } => "SendQueuedNow(..)",
            Self::DeleteQueued { .. } => "DeleteQueued(..)",
            Self::EditQueued { .. } => "EditQueued(..)",
            Self::MoveQueued { .. } => "MoveQueued(..)",
            Self::ResumeQueueDrain => "ResumeQueueDrain",
            Self::ResolveApproval { .. } => "ResolveApproval(..)",
            Self::Quit => "Quit",
            Self::ClearSession => "ClearSession",
            Self::Compact => "Compact",
            Self::ResumeSession { .. } => "ResumeSession(..)",
            Self::ResumeSessionHandoff { .. } => "ResumeSessionHandoff(..)",
            Self::ForkSession { .. } => "ForkSession(..)",
            Self::RefreshReadiness => "RefreshReadiness",
            Self::RetryAttach => "RetryAttach",
            Self::Recover => "Recover",
            Self::OpenTopologyNavigator => "OpenTopologyNavigator",
            Self::OpenAgentTranscript { .. } => "OpenAgentTranscript(..)",
            Self::CloseAgentTranscript => "CloseAgentTranscript",
            Self::RefreshTopologyTree => "RefreshTopologyTree",
            Self::ResumeTopologyTree => "ResumeTopologyTree",
            Self::InterruptTopologyNode { .. } => "InterruptTopologyNode(..)",
            Self::CloseTopologyNode { .. } => "CloseTopologyNode(..)",
            Self::StopAllTopology => "StopAllTopology",
            Self::RefreshTemplates => "RefreshTemplates",
            Self::InvokeTool { .. } => "InvokeTool(..)",
            Self::RequestCallDetail { .. } => "RequestCallDetail(..)",
            Self::Extension(extension) => return write!(formatter, "Extension({extension:?})"),
        })
    }
}

/// Closed operator decision for one pending effect approval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalChoice {
    /// Authorize the exact pending invocation once.
    Allow,
    /// Refuse the exact pending invocation.
    Deny,
}
