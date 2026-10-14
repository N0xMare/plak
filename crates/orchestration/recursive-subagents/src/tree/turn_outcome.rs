//! How a finished child turn ends, on the tree's own words.
//!
//! The turn-ending state machine the host applies after a child turn returns:
//! only the host's own `interrupt_agent`/`close_agent` mark a node, so a cancel
//! on any other status came from outside, and a provider refusal is either a
//! block or an ordinary failure. This vocabulary belongs to subagent orchestration.

use std::time::Duration;

use crate::protocol::NodeStatus;

/// How long a provider must ask a child to wait before the wait is a block
/// rather than an ordinary retryable failure.
pub const RATE_LIMIT_BLOCK_THRESHOLD: Duration = Duration::from_secs(300);

/// How a finished child turn ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnOutcome {
    /// `close_agent` fenced the node while the turn unwound: seal the copy and
    /// finish the attempt as usual.
    Closed,
    /// `interrupt_agent` cancelled *this turn* and nothing more: release the
    /// bound turn, leave the attempt running, seal nothing, push nothing.
    Interrupted,
    /// A cancel nobody here asked for. `AgentErrorKind::Cancelled` is also
    /// minted from provider error text and from a provider-side attempt
    /// cancel, and reading that as an interrupt left the node `Running` with
    /// no live session behind it and a `wait_agent` that never settled
    /// (gate 3). The attempt ends as a failure instead.
    CancelledOutside,
    /// The ordinary path.
    Ran,
}

impl TurnOutcome {
    /// Classifies a finished turn from the node's own status: only the host's
    /// own `interrupt_agent`/`close_agent` set `Interrupted`/`Closed`, so a
    /// cancel on any other status came from outside.
    #[must_use]
    pub const fn classify(node_status: Option<NodeStatus>, cancelled: bool) -> Self {
        match node_status {
            Some(NodeStatus::Closed) => Self::Closed,
            _ if !cancelled => Self::Ran,
            Some(NodeStatus::Interrupted) => Self::Interrupted,
            _ => Self::CancelledOutside,
        }
    }
}

/// Authoritative failure carried by a joined child turn result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FailedTurnDisposition {
    /// All external outcomes are resolved; partial work can be sealed.
    Failed {
        /// Stable product failure category.
        kind: &'static str,
    },
    /// A committed quota refusal retains the provider's retry boundary.
    Blocked(super::BlockedReason),
    /// An external outcome is unresolved; the copy must remain fenced.
    RecoveryRequired,
}

/// How a provider refusal ends the attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnRefusal {
    /// Attempt `blocked { quota, account, resets_at? }`, node `Failed`, no
    /// retry without the user.
    Blocked {
        /// Which quota the provider named.
        quota: &'static str,
        /// Unix seconds, only when the provider said when to come back.
        resets_at: Option<u64>,
    },
    /// An ordinary failed attempt carrying a sharper `failure.kind`.
    Failure {
        /// The stable failure label to record on the attempt.
        kind: &'static str,
    },
}

/// The classification itself, on the provider's own words.
///
/// `QuotaExceeded` is always a block. `RateLimited` is a block only when the
/// provider marked it non-retryable (`Permanent`, `Exhausted`) or told us to
/// wait at least [`RATE_LIMIT_BLOCK_THRESHOLD`]; a short or unqualified 429 is
/// an ordinary failure `{kind: rate_limited}` so the parent can retry it.
/// Classification is by the model error's own kind and retryability, never by
/// its text, so a reworded provider message cannot change an attempt's status.
/// A block's `resets_at` is `committed_at_ms` (the failed terminal's commit)
/// plus the provider's delay, in Unix seconds, so every pass derives the same
/// value.
pub fn classify_provider_refusal(
    kind: plakous::model::ModelErrorKind,
    retryability: Option<plakous::model::ModelRetryability>,
    committed_at_ms: u64,
) -> Option<TurnRefusal> {
    use plakous::model::{ModelErrorKind, ModelRetryability};
    let retry_after = match retryability {
        Some(ModelRetryability::Eligible {
            retry_after: Some(delay),
        }) => Some(Duration::from_millis(delay.as_millis())),
        _ => None,
    };
    let resets_at = || {
        retry_after.map(|delay| {
            committed_at_ms
                .saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
                .div_ceil(1_000)
        })
    };
    match kind {
        ModelErrorKind::Policy => Some(TurnRefusal::Failure { kind: "refused" }),
        ModelErrorKind::QuotaExceeded => Some(TurnRefusal::Blocked {
            quota: "quota_exceeded",
            resets_at: resets_at(),
        }),
        ModelErrorKind::RateLimited => {
            let non_retryable = matches!(
                retryability,
                Some(ModelRetryability::Permanent | ModelRetryability::Exhausted)
            );
            let long_wait = retry_after.is_some_and(|delay| delay >= RATE_LIMIT_BLOCK_THRESHOLD);
            if non_retryable || long_wait {
                Some(TurnRefusal::Blocked {
                    quota: "rate_limited",
                    resets_at: resets_at(),
                })
            } else {
                Some(TurnRefusal::Failure {
                    kind: "rate_limited",
                })
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_policy_refusal_has_a_distinct_failure_kind() {
        assert_eq!(
            classify_provider_refusal(plakous::model::ModelErrorKind::Policy, None, 0),
            Some(TurnRefusal::Failure { kind: "refused" })
        );
    }
}
