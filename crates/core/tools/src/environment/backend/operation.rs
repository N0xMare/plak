//! The context every environment operation carries: who it runs for, which
//! request class keys it, how long its caller allows, and its cooperative
//! cancellation and yield signals (spec 01 §8.4).
//!
//! The owner and class key are carried, not yet enforced: they are the fence
//! and idempotency key a runner checks once execution can leave the process.

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, Timestamp};

use super::{ExecutionBackendError, ExecutionBackendErrorKind, backend_error};
use crate::JobOwner;

/// The agent-line head an operation runs for: the line, its generation, the
/// head branch and that branch's lease fence. Owners of one line are ordered
/// by `(generation, fence)`, so a successor head is always newer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LineOwner {
    line: LineId,
    generation: u64,
    branch: BranchId,
    fence: u64,
}

impl LineOwner {
    /// One line head's owner identity. `fence` is 0 for an ephemeral agent.
    #[must_use]
    pub const fn new(line: LineId, generation: u64, branch: BranchId, fence: u64) -> Self {
        Self {
            line,
            generation,
            branch,
            fence,
        }
    }

    /// The agent line.
    #[must_use]
    pub const fn line(&self) -> LineId {
        self.line
    }

    /// The line generation of the owning head branch.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The head branch.
    #[must_use]
    pub const fn branch(&self) -> BranchId {
        self.branch
    }

    /// The head branch's lease fence.
    #[must_use]
    pub const fn fence(&self) -> u64 {
        self.fence
    }

    /// Whether `self` is a strictly newer owner of the same line than
    /// `other`: a later generation, or the same generation under a later
    /// fence.
    #[must_use]
    pub fn is_newer_than(&self, other: &Self) -> bool {
        self.line == other.line && (self.generation, self.fence) > (other.generation, other.fence)
    }
}

impl From<&JobOwner> for LineOwner {
    fn from(owner: &JobOwner) -> Self {
        Self::new(owner.line, owner.generation, owner.branch_id, owner.fence)
    }
}

/// Which request class keys an operation, with its key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum OperationClass {
    /// One tool call's effect, keyed by the call and its attempt within the
    /// owning line.
    EffectCall {
        /// The tool call.
        call_id: CallId,
        /// The call's attempt.
        attempt_id: AttemptId,
    },
    /// Work no tool call started (a pooled service, a host-side probe,
    /// teardown): it carries no runner key.
    Unkeyed,
}

/// Cooperative context shared by all environment operations.
#[derive(Clone, Debug)]
pub struct OperationContext {
    owner: Option<LineOwner>,
    class: OperationClass,
    budget: Option<Timestamp>,
    cancellation: CancellationToken,
    preempt: tokio::sync::watch::Receiver<crate::ToolPreempt>,
}

impl OperationContext {
    /// An unkeyed operation with no owner and no deadline, cancelled by
    /// `cancellation`.
    #[must_use]
    pub fn new(cancellation: CancellationToken) -> Self {
        Self {
            owner: None,
            class: OperationClass::Unkeyed,
            budget: None,
            cancellation,
            preempt: tokio::sync::watch::channel(crate::ToolPreempt::None).1,
        }
    }

    /// Names the line head this operation runs for.
    #[must_use]
    pub const fn with_owner(mut self, owner: LineOwner) -> Self {
        self.owner = Some(owner);
        self
    }

    /// Keys this operation by its request class.
    #[must_use]
    pub const fn with_class(mut self, class: OperationClass) -> Self {
        self.class = class;
        self
    }

    /// Bounds this operation by its caller's absolute deadline.
    #[must_use]
    pub const fn with_budget(mut self, deadline: Timestamp) -> Self {
        self.budget = Some(deadline);
        self
    }

    /// Binds a yield-only control signal for process promotion.
    #[must_use]
    pub fn with_preempt(
        mut self,
        preempt: tokio::sync::watch::Receiver<crate::ToolPreempt>,
    ) -> Self {
        self.preempt = preempt;
        self
    }

    /// The line head this operation runs for, when one started it.
    #[must_use]
    pub const fn owner(&self) -> Option<&LineOwner> {
        self.owner.as_ref()
    }

    /// The request class and key.
    #[must_use]
    pub const fn class(&self) -> OperationClass {
        self.class
    }

    /// The caller's absolute deadline, when it set one.
    #[must_use]
    pub const fn budget(&self) -> Option<Timestamp> {
        self.budget
    }

    /// Returns the yield request independently from the kill token.
    #[must_use]
    pub const fn preempt(&self) -> &tokio::sync::watch::Receiver<crate::ToolPreempt> {
        &self.preempt
    }

    /// Returns cooperative cancellation.
    #[must_use]
    pub const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Refuses once the operation's cancellation token is observed.
    ///
    /// # Errors
    ///
    /// Returns a cancelled backend error when the token has fired.
    pub fn check_cancelled(&self) -> Result<(), ExecutionBackendError> {
        if self.cancellation.is_cancelled() {
            Err(backend_error(
                ExecutionBackendErrorKind::Cancelled,
                "execution environment operation was cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
