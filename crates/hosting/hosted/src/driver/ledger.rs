//! Write-ahead command admission without changing either storage authority.

use std::future::Future;
use std::pin::Pin;

use plakous::model::SessionId;
use plakous::sessions::AgentSessions;

use crate::session_store::RecordStoreError;
use crate::sessions::ResultMailBatch;
use crate::sessions::projector::HostPauseCause;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommandKind {
    RootStart,
    RootSteer(plakous::model::TurnId),
    HostWake,
    Resume,
    Start,
    Wake,
    MailStart,
    Followup,
    Corrective,
}

pub(crate) enum BatchRule<'a> {
    /// A root command with no tree batch.
    None,
    RootResults(ResultMailBatch),
    /// A child wake or resume: all unclaimed envelopes in mailbox order.
    WakeAll,
    /// The named primary mail, other non-result mail, then child results.
    MailStart(&'a str),
    /// Only pending child results.
    Results,
    /// Follow-up or steer: the named primary mail, then child results.
    Delivered(&'a str),
}

pub(crate) struct CommandIntent<'a> {
    pub(crate) command_id: &'a str,
    pub(crate) kind: CommandKind,
    pub(crate) base_text: &'a str,
    pub(crate) batch: BatchRule<'a>,
}

/// Exact text and mailbox order, returned unchanged on an exact retry.
#[derive(Clone, Debug)]
pub(crate) struct Prepared {
    pub(crate) prompt: String,
    pub(crate) message_ids: Vec<String>,
    pub(crate) instruction: bool,
}

impl CommandKind {
    pub(crate) const fn is_instruction(self) -> bool {
        matches!(
            self,
            Self::HostWake | Self::Resume | Self::Wake | Self::Corrective
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Settlement {
    Recorded,
    Consumed,
    Refused { pause: Option<HostPauseCause> },
}

/// Storage errors retain the product's definite-refusal classification.
#[derive(Debug)]
pub(crate) enum LedgerError {
    Store(RecordStoreError),
    TreeStorageFull(crate::agent_tree_protocol::TopologyError),
    /// Another live host process holds the tree; the refusal's text.
    TreeHostedElsewhere(String),
    Integrity(String),
    Unavailable(String),
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => error.fmt(f),
            Self::TreeStorageFull(error) => f.write_str(&error.message),
            Self::TreeHostedElsewhere(message)
            | Self::Integrity(message)
            | Self::Unavailable(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for LedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::TreeStorageFull(_)
            | Self::TreeHostedElsewhere(_)
            | Self::Integrity(_)
            | Self::Unavailable(_) => None,
        }
    }
}

impl From<RecordStoreError> for LedgerError {
    fn from(error: RecordStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<crate::agent_tree_protocol::TopologyError> for LedgerError {
    fn from(error: crate::agent_tree_protocol::TopologyError) -> Self {
        if error.code == crate::agent_tree_protocol::TopologyErrorCode::RecoveryStorageUnavailable {
            Self::TreeStorageFull(error)
        } else {
            Self::Unavailable(error.message)
        }
    }
}

impl From<crate::runtime::TreeWriteError> for LedgerError {
    fn from(error: crate::runtime::TreeWriteError) -> Self {
        match error {
            crate::runtime::TreeWriteError::HostedElsewhere(refusal) => {
                Self::TreeHostedElsewhere(refusal.to_string())
            }
            crate::runtime::TreeWriteError::Refused(error) => error.into(),
        }
    }
}

impl From<LedgerError> for RecordStoreError {
    fn from(error: LedgerError) -> Self {
        match error {
            LedgerError::Store(error) => error,
            LedgerError::TreeStorageFull(error) => {
                Self::refused(crate::error::StoreRefusal::StorageFull, error.message)
            }
            LedgerError::TreeHostedElsewhere(message) => {
                Self::refused(crate::error::StoreRefusal::HostedElsewhere, message)
            }
            other => other.to_string().into(),
        }
    }
}

impl From<LedgerError> for String {
    fn from(error: LedgerError) -> Self {
        error.to_string()
    }
}

pub(crate) type LedgerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<bool, LedgerError>> + Send + 'a>>;

/// One write-ahead step of a [`CommandLedger`].
pub(crate) type LedgerStep<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, LedgerError>> + Send + 'a>>;

pub(crate) trait CommandLedger: Send + Sync {
    fn prepare<'a>(&'a mut self, intent: CommandIntent<'a>) -> LedgerStep<'a, Prepared>;
    fn settle<'a>(&'a mut self, command_id: &'a str, settlement: Settlement) -> LedgerStep<'a, ()>;
    fn duplicate_consumed<'a>(
        &'a self,
        sessions: &'a AgentSessions,
        session: SessionId,
        command_id: &'a str,
    ) -> LedgerFuture<'a>;
}

/// Projection is role-specific; this retention law is not. A rejected fact
/// cannot become consumption merely because older feed records were pruned.
pub(crate) enum AcceptanceFact {
    Present,
    Rejected,
    Absent,
}

pub(crate) fn duplicate_consumed(
    fact: AcceptanceFact,
    retained_from: u64,
    missing: &str,
) -> Result<bool, LedgerError> {
    match fact {
        AcceptanceFact::Present => Ok(false),
        AcceptanceFact::Absent if retained_from > 0 => Ok(true),
        _ => Err(LedgerError::Integrity(missing.to_owned())),
    }
}
