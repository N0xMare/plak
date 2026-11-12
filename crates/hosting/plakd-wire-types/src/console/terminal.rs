//! Process-local operator terminal vocabulary, separate from agent observations.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalRef {
    pub id: String,
    pub host_generation: u64,
    pub instance: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalCursor {
    pub instance: String,
    pub offset: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalDimensions {
    pub rows: u16,
    pub columns: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    Starting,
    Running,
    Closing,
    Exited,
    TerminationUnknown,
    Lost,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalSurvivor {
    pub pid: u32,
    pub started: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalView {
    pub terminal: TerminalRef,
    pub session: String,
    pub node: String,
    pub terminal_id: String,
    pub status: TerminalStatus,
    pub oldest: TerminalCursor,
    pub next: TerminalCursor,
    pub input_next: u64,
    pub exit_code: Option<u32>,
    pub survivors: Vec<TerminalSurvivor>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalCredit {
    pub stream: u64,
    pub bytes: u64,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalEvent {
    Data {
        offset: u64,
        bytes_b64: String,
    },
    InputAccepted {
        next: u64,
    },
    OutputGap {
        oldest: TerminalCursor,
    },
    Exit {
        status: TerminalStatus,
        code: Option<u32>,
        survivors: Vec<TerminalSurvivor>,
    },
    Suspended {},
    IdleWarning {
        remaining_ms: u64,
    },
    Refused {
        reason: super::ReadRefusal,
    },
}

impl std::fmt::Debug for TerminalEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TerminalEvent(..)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalLimits {
    pub chunk_bytes: u64,
    pub credit_bytes: u64,
    pub unsent_input_bytes: u64,
    pub ring_bytes: u64,
    pub terminals_per_session: u64,
    pub terminals_per_principal: u64,
    pub terminals_host: u64,
    pub input_bytes_per_second: u64,
    pub output_bytes_per_second: u64,
    pub principal_output_bytes_per_second: u64,
    pub scheduler_messages_per_second: u64,
    pub scheduler_burst: u64,
    pub scheduler_actions: u64,
    pub input_coalesce_min_ms: u64,
    pub input_coalesce_max_ms: u64,
    pub idle_ms: u64,
    pub detach_ms: u64,
    pub namespace_ms: u64,
}
