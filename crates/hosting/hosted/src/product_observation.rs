//! Plak-only content-free product observations.

use std::time::Duration;

use plakous::model::{
    AgentId, BranchId, ModelRetryCause, ModelRetryLoss, SessionId, Timestamp, TurnId,
};

use crate::agent_tree_protocol::NodeStatus;

/// Fixed-capacity UTF-8 text for `Copy` product observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundedText64 {
    bytes: [u8; 64],
    len: u8,
}

impl BoundedText64 {
    /// Stores up to 64 UTF-8 bytes; longer inputs are rejected.
    pub fn new(text: &str) -> Option<Self> {
        if text.is_empty() || text.len() > 64 {
            return None;
        }
        let mut bytes = [0_u8; 64];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Some(Self {
            bytes,
            len: u8::try_from(text.len()).ok()?,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

/// Process resource sample phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceSamplePhase {
    TurnStarted,
    TurnFinished,
    CompactionFinished,
    ChildExited,
}

impl ResourceSamplePhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnStarted => "turn_started",
            Self::TurnFinished => "turn_finished",
            Self::CompactionFinished => "compaction_finished",
            Self::ChildExited => "child_exited",
        }
    }
}

/// Closed child-process kind for `resource_samples.child_kind`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildKind {
    JsEvalSidecar,
    PythonWorker,
    Unknown,
}

impl ChildKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JsEvalSidecar => "js_eval_sidecar",
            Self::PythonWorker => "python_worker",
            Self::Unknown => "unknown",
        }
    }

    #[must_use]
    pub const fn from_agent_child(child: plakous::agent::ChildProcessKind) -> Self {
        match child {
            plakous::agent::ChildProcessKind::JsEvalSidecar => Self::JsEvalSidecar,
            plakous::agent::ChildProcessKind::PythonWorker => Self::PythonWorker,
            _ => Self::Unknown,
        }
    }
}

/// Closed JSONL agent-protocol command names observed at admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonlCommandKind {
    Prompt,
    FollowUp,
    Steer,
    Queue,
    QueueEdit,
    ResumeQueue,
    Compact,
    Clear,
    Fork,
    Rebind,
    Tree,
    ResumeTree,
    InvokeTool,
    Quit,
    Other,
}

impl JsonlCommandKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::FollowUp => "follow_up",
            Self::Steer => "steer",
            Self::Queue => "queue",
            Self::QueueEdit => "queue_edit",
            Self::ResumeQueue => "resume_queue",
            Self::Compact => "compact",
            Self::Clear => "clear",
            Self::Fork => "fork",
            Self::Rebind => "rebind",
            Self::Tree => "tree",
            Self::ResumeTree => "resume_tree",
            Self::InvokeTool => "invoke_tool",
            Self::Quit => "quit",
            Self::Other => "other",
        }
    }

    #[must_use]
    pub fn from_command_name(name: &str) -> Self {
        match name {
            "prompt" => Self::Prompt,
            "follow_up" => Self::FollowUp,
            "steer" => Self::Steer,
            "queue" => Self::Queue,
            "queue_edit" => Self::QueueEdit,
            "resume_queue" => Self::ResumeQueue,
            "compact" => Self::Compact,
            "clear" => Self::Clear,
            "fork" => Self::Fork,
            "rebind" => Self::Rebind,
            "tree" => Self::Tree,
            "resume_tree" => Self::ResumeTree,
            "invoke_tool" => Self::InvokeTool,
            "quit" => Self::Quit,
            _ => Self::Other,
        }
    }
}

/// Terminal node outcome for `tree_node_detached` rows (content-free).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeDetachStatus {
    Completed,
    Failed,
    Closed,
    RecoveryRequired,
    Shutdown,
}

impl NodeDetachStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Closed => "closed",
            Self::RecoveryRequired => "recovery_required",
            Self::Shutdown => "shutdown",
        }
    }

    #[must_use]
    pub fn from_node_status(status: NodeStatus) -> Option<Self> {
        match status {
            NodeStatus::Completed => Some(Self::Completed),
            NodeStatus::Failed => Some(Self::Failed),
            NodeStatus::Closed => Some(Self::Closed),
            NodeStatus::RecoveryRequired => Some(Self::RecoveryRequired),
            NodeStatus::Pending
            | NodeStatus::Running
            | NodeStatus::Paused
            | NodeStatus::Sealing
            | NodeStatus::Interrupted => None,
        }
    }
}

/// Content-free terminal lifecycle measurement phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TuiTimingPhase {
    ProcessStart,
    FirstFrameLocal,
    ReadyToTypeLocal,
    HostReadyBackground,
    KeyToRestore,
    RestoreToExit,
}

impl TuiTimingPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProcessStart => "tui_process_start",
            Self::FirstFrameLocal => "tui_first_frame_local",
            Self::ReadyToTypeLocal => "tui_ready_to_type_local",
            Self::HostReadyBackground => "tui_host_ready_background",
            Self::KeyToRestore => "tui_key_to_restore",
            Self::RestoreToExit => "tui_restore_to_exit",
        }
    }
}

/// Closed operator bounds for a non-interactive run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunBound {
    Timeout,
    MaxTurns,
}
impl RunBound {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::MaxTurns => "max_turns",
        }
    }
}

/// Typed, content-free Plak product observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductObservation {
    /// Exact terminal usage, authored only by a winning feed projector.
    TurnUsage {
        at: Timestamp,
        session_id: SessionId,
        branch_id: BranchId,
        turn_id: TurnId,
        usage: plakous::agent::TurnUsage,
    },
    RunBoundExit {
        at: Timestamp,
        session_id: SessionId,
        bound: RunBound,
    },
    TuiTiming {
        at: Timestamp,
        session_id: SessionId,
        phase: TuiTimingPhase,
        resumed: bool,
        elapsed: Duration,
    },
    ModelRetry {
        at: Timestamp,
        session_id: SessionId,
        branch_id: BranchId,
        agent_id: AgentId,
        turn_id: TurnId,
        next_attempt: u32,
        max_attempts: u32,
        cause: ModelRetryCause,
        loss: ModelRetryLoss,
    },
    TreeNodeSpawned {
        at: Timestamp,
        session_id: SessionId,
        tree_id: BoundedText64,
        node_id: BoundedText64,
        parent_node_id: Option<BoundedText64>,
        template: BoundedText64,
    },
    TreeNodeAttached {
        at: Timestamp,
        session_id: SessionId,
        tree_id: BoundedText64,
        node_id: BoundedText64,
        parent_node_id: Option<BoundedText64>,
        elapsed: Duration,
    },
    TreeNodeDetached {
        at: Timestamp,
        session_id: SessionId,
        tree_id: BoundedText64,
        node_id: BoundedText64,
        parent_node_id: Option<BoundedText64>,
        status: NodeDetachStatus,
    },
    TreeRebound {
        at: Timestamp,
        session_id: SessionId,
        tree_id: BoundedText64,
    },
    TreeEpochAdvanced {
        at: Timestamp,
        session_id: SessionId,
        tree_id: BoundedText64,
    },
    JsonlCommand {
        at: Timestamp,
        session_id: SessionId,
        kind: JsonlCommandKind,
        admitted_to_completed: Duration,
    },
    SessionOpened {
        at: Timestamp,
        session_id: SessionId,
        elapsed: Duration,
    },
    SessionResumed {
        at: Timestamp,
        session_id: SessionId,
        elapsed: Duration,
    },
    ResourceSample {
        at: Timestamp,
        session_id: SessionId,
        agent_id: Option<AgentId>,
        phase: ResourceSamplePhase,
        child_kind: Option<ChildKind>,
        rss_bytes: Option<u64>,
        peak_rss_bytes: Option<u64>,
        user_cpu: Duration,
        system_cpu: Duration,
        threads: Option<u64>,
        fds: Option<u64>,
    },
}

impl ProductObservation {
    #[must_use]
    pub const fn at(self) -> Timestamp {
        match self {
            Self::TurnUsage { at, .. }
            | Self::RunBoundExit { at, .. }
            | Self::TuiTiming { at, .. }
            | Self::ModelRetry { at, .. }
            | Self::TreeNodeSpawned { at, .. }
            | Self::TreeNodeAttached { at, .. }
            | Self::TreeNodeDetached { at, .. }
            | Self::TreeRebound { at, .. }
            | Self::TreeEpochAdvanced { at, .. }
            | Self::JsonlCommand { at, .. }
            | Self::SessionOpened { at, .. }
            | Self::SessionResumed { at, .. }
            | Self::ResourceSample { at, .. } => at,
        }
    }

    #[must_use]
    pub const fn session_id(self) -> SessionId {
        match self {
            Self::TurnUsage { session_id, .. }
            | Self::RunBoundExit { session_id, .. }
            | Self::TuiTiming { session_id, .. }
            | Self::ModelRetry { session_id, .. }
            | Self::TreeNodeSpawned { session_id, .. }
            | Self::TreeNodeAttached { session_id, .. }
            | Self::TreeNodeDetached { session_id, .. }
            | Self::TreeRebound { session_id, .. }
            | Self::TreeEpochAdvanced { session_id, .. }
            | Self::JsonlCommand { session_id, .. }
            | Self::SessionOpened { session_id, .. }
            | Self::SessionResumed { session_id, .. }
            | Self::ResourceSample { session_id, .. } => session_id,
        }
    }
}

/// Parses a product session UUID from durable tree/session text.
pub fn session_id_from_text(text: &str) -> Option<SessionId> {
    uuid::Uuid::parse_str(text)
        .ok()
        .and_then(|uuid| SessionId::try_from(uuid).ok())
}

/// Wall-clock timestamp for product-side observations.
#[allow(
    clippy::unwrap_used,
    reason = "Timestamp::new only fails on invalid nanos"
)]
pub fn wall_timestamp() -> Timestamp {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp::new(now.as_secs(), now.subsec_nanos())
        .unwrap_or_else(|_| Timestamp::new(1, 0).unwrap_or_else(|_| Timestamp::new(0, 1).unwrap()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn bounded_text_round_trips() {
        let text = BoundedText64::new("root").unwrap();
        assert_eq!(text.as_str(), "root");
    }

    #[test]
    fn product_observation_is_copy() {
        let session_id = SessionId::try_from(Uuid::from_u128(1)).unwrap();
        let obs = ProductObservation::SessionOpened {
            at: Timestamp::new(1, 2).unwrap(),
            session_id,
            elapsed: Duration::from_millis(3),
        };
        let copied = obs;
        assert_eq!(copied.session_id(), session_id);
    }
}
