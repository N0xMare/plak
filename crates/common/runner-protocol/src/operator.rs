//! Validated operator-only terminal vocabulary; never an agent process request.

use serde::{Deserialize, Serialize};

/// Fixed operator verbs, independent of agent process/session verbs.
pub const VERBS: &[&str] = &[
    "operator.namespace.renew",
    "operator.namespace.close",
    "operator.terminal.open",
    "operator.terminal.list",
    "operator.terminal.attach",
    "operator.terminal.resize",
    "operator.terminal.close",
];

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "exact wire fixtures"
)]
mod pinned_tests;

/// A bounded opaque operator identity component.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct OperatorId(String);

impl TryFrom<String> for OperatorId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() || value.len() > 128 || value.bytes().any(|b| b.is_ascii_control()) {
            return Err("operator identity must contain 1..=128 non-control bytes");
        }
        Ok(Self(value))
    }
}

impl From<OperatorId> for String {
    fn from(value: OperatorId) -> Self {
        value.0
    }
}

impl OperatorId {
    /// The validated identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Host-local state and authenticated principal. Host id/generation come from Bind.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorNamespace {
    /// Product state namespace.
    pub state_key: OperatorId,
    /// Authenticated operator, asserted only by the paired host.
    pub principal: OperatorId,
}

/// Identity within an operator namespace.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalKey {
    /// Session, not an agent line.
    pub session: OperatorId,
    /// Workspace node.
    pub node: OperatorId,
    /// Client-generated id; never changed to recover a lost open answer.
    pub terminal_id: OperatorId,
}

/// Validated character-cell dimensions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "DimensionsWire", into = "DimensionsWire")]
pub struct TerminalDimensions {
    rows: u16,
    columns: u16,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DimensionsWire {
    rows: u16,
    columns: u16,
}

impl TerminalDimensions {
    /// Validates rows 1..=500 and columns 1..=1000.
    ///
    /// # Errors
    /// A dimension is outside the fixed policy.
    pub fn new(rows: u16, columns: u16) -> Result<Self, &'static str> {
        if !(1..=500).contains(&rows) || !(1..=1000).contains(&columns) {
            return Err("terminal dimensions outside policy");
        }
        Ok(Self { rows, columns })
    }

    /// Rows.
    #[must_use]
    pub const fn rows(self) -> u16 {
        self.rows
    }

    /// Columns.
    #[must_use]
    pub const fn columns(self) -> u16 {
        self.columns
    }
}

impl TryFrom<DimensionsWire> for TerminalDimensions {
    type Error = &'static str;
    fn try_from(value: DimensionsWire) -> Result<Self, Self::Error> {
        Self::new(value.rows, value.columns)
    }
}

impl From<TerminalDimensions> for DimensionsWire {
    fn from(value: TerminalDimensions) -> Self {
        Self {
            rows: value.rows,
            columns: value.columns,
        }
    }
}

/// Runner-instance-qualified position in a terminal's raw output.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalCursor {
    /// Instance from Hello; mismatch is loss, not an empty output ring.
    pub instance: OperatorId,
    /// Absolute raw byte position.
    pub offset: u64,
}

/// Fixed operator operations. The variant's verb must match Operation.verb.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorOperation {
    /// Renews the 90-awake-second namespace and reconciles this connection.
    Renew,
    /// Closes every terminal of this principal's generation.
    NamespaceClose,
    /// Opens the runner user's interactive shell; no command or environment payload.
    Open {
        /// Idempotency identity.
        key: TerminalKey,
        /// Canonical admitted workspace cwd.
        workspace: String,
        /// Initial dimensions.
        dimensions: TerminalDimensions,
    },
    /// Bounded namespace table, also a reconciliation barrier.
    List,
    /// Acquires the single controller. Never creates a terminal.
    Attach {
        /// Existing terminal.
        key: TerminalKey,
        /// Output position, qualified by runner instance.
        cursor: TerminalCursor,
    },
    /// Coalesces the latest requested size of the current controller.
    Resize {
        /// Existing terminal.
        key: TerminalKey,
        /// New dimensions.
        dimensions: TerminalDimensions,
    },
    /// Idempotent whole-session cleanup.
    Close {
        /// Existing terminal.
        key: TerminalKey,
    },
}

impl OperatorOperation {
    /// The only runner verb that accepts this payload.
    #[must_use]
    pub const fn verb(&self) -> &'static str {
        match self {
            Self::Renew => "operator.namespace.renew",
            Self::NamespaceClose => "operator.namespace.close",
            Self::Open { .. } => "operator.terminal.open",
            Self::List => "operator.terminal.list",
            Self::Attach { .. } => "operator.terminal.attach",
            Self::Resize { .. } => "operator.terminal.resize",
            Self::Close { .. } => "operator.terminal.close",
        }
    }
}

/// Terminal state is independent of agent job and turn terminals.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    /// Quota and workspace pin reserved before spawn.
    Starting,
    /// Interactive shell alive.
    Running,
    /// Cleanup has begun but is not proven.
    Closing,
    /// Whole-session absence proven by two complete scans.
    Exited,
    /// Cleanup cannot be proven; quota and workspace pin remain held.
    TerminationUnknown,
}

/// One row of the bounded principal-private terminal table.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalRow {
    /// Client identity.
    pub key: TerminalKey,
    /// Lifecycle state.
    pub status: TerminalStatus,
    /// Oldest retained raw byte.
    pub oldest: TerminalCursor,
    /// Next raw byte.
    pub next: TerminalCursor,
    /// Accepted input byte count. It does not authorize lost-input replay.
    pub input_next: u64,
    /// Whether an input/resize controller is attached.
    pub attached: bool,
    /// Shell exit code, only separate from the raw output.
    pub exit_code: Option<u32>,
}

/// A proved-no-effect operator refusal, never an agent backend failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorRefusal {
    /// Invalid payload, dimensions or digest.
    InvalidRequest,
    /// Workspace is no longer admitted or protected state overlaps it.
    WorkspaceUnavailable,
    /// Quota, including starting and unresolved closing terminals.
    Quota,
    /// Terminal already has a controller.
    TerminalBusy,
    /// Missing terminal or changed runner instance.
    TerminalLost,
    /// Namespace expired or was closed.
    NamespaceExpired,
    /// Reconcile this connection before an open, attach or input.
    ReconcileRequired,
    /// Reused identity with a different operation digest or input bytes.
    Conflict,
    /// Non-contiguous input or impossible output cursor.
    OffsetGap,
    /// Queue or decoded-byte rate/credit limit; nothing accepted.
    Backpressure,
}

/// Operator response. Unknown termination is a row state, not no-effect refusal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperatorAnswer {
    /// Principal-private, bounded table.
    Table {
        /// Rows.
        terminals: Vec<TerminalRow>,
    },
    /// One terminal and optionally a freshly attached stream.
    Terminal {
        /// Current row.
        terminal: TerminalRow,
        /// Controller stream, for Attach only.
        stream: Option<u64>,
    },
    /// No effect.
    Refused {
        /// Why.
        refusal: OperatorRefusal,
    },
}

/// Terminal Data payload inside the shared `StreamEvent` envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "terminal_event", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalEvent {
    /// Input or merged PTY output; strict base64, at most 16 KiB decoded.
    Data {
        /// Absolute raw byte offset.
        offset: u64,
        /// RFC 4648 bytes.
        bytes_b64: String,
    },
    /// Accepted into the bounded PTY input queue, not execution proof.
    InputAccepted {
        /// Next accepted input offset.
        next: u64,
    },
    /// Reader fell behind the ring; reset emulator before replay.
    OutputGap {
        /// New oldest cursor.
        oldest: TerminalCursor,
    },
    /// Shell status, after cleanup, separate from terminal bytes.
    Exit {
        /// Cleanup disposition.
        status: TerminalStatus,
        /// Shell code when known.
        code: Option<u32>,
    },
    /// Rejected input did not advance the input offset.
    Refused {
        /// Why.
        refusal: OperatorRefusal,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "validated wire fixtures")]
mod tests {
    use super::*;

    #[test]
    fn operator_wire_rejects_dimensions_and_unbounded_identity() {
        for (rows, columns) in [(0, 1), (501, 80), (1, 0), (20, 1001)] {
            let value = serde_json::json!({"rows":rows,"columns":columns});
            assert!(serde_json::from_value::<TerminalDimensions>(value).is_err());
        }
        assert!(TerminalDimensions::new(500, 1000).is_ok());
        assert!(OperatorId::try_from("a".repeat(129)).is_err());
        assert!(OperatorId::try_from("bad\n".to_owned()).is_err());
        assert!(OperatorId::try_from(String::new()).is_err());
    }

    #[test]
    fn operator_open_has_no_bootstrap_command_or_environment() {
        let value = serde_json::json!({"operation":"open","key":{
            "session":"s","node":"n","terminal_id":"t"},"workspace":"/workspace",
            "dimensions":{"rows":24,"columns":80},"command":"untrusted"});
        assert!(serde_json::from_value::<OperatorOperation>(value).is_err());
    }
}
