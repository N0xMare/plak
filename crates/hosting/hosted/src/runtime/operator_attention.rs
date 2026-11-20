use crate::agent_tree::ProductAgentTree;
use crate::agent_tree_protocol::{MessagePurpose, NodeStatus};
use crate::sessions::projector::{HostPauseCause, PauseCause, ProductIntents, SessionViews};
use plakous_host_protocol::notice_remedy::NoticeRemedy;
use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptRow};
use plakous_recursive_subagents::tree::{ContinuationCause, LossContinuationState};

use super::root_loss::{
    RootLossState, RootStanding, is_root_loss_command, owner_loss_at_pause,
    root_standing_from_intents,
};

/// Why the host is waiting for input. Contains no mail or provider text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionReason {
    /// Root turn was fenced, with an uncertain outcome.
    RecoveryRequired,
    /// Root reached context capacity.
    ContextCapacity,
    /// A recorded session-wide command refusal.
    SessionRefusal,
    /// Compaction did not apply.
    CompactionNotApplied,
    /// A session-wide refusal without a runtime fact.
    UnrecordedRefusal,
    /// An automatic wake failed preflight.
    WakePreflightFailed,
    /// Root holds after owner loss.
    HeldRoot,
    /// One or more children hold after owner loss.
    HeldChild,
    /// One or more children need explicit recovery.
    ChildRecovery,
    /// Consent was recorded, but the root continuation could not be prepared.
    RootContinuationPreparationFailed,
}

impl AttentionReason {
    pub(crate) const fn pauses_root_wakes(self) -> bool {
        !matches!(self, Self::HeldRoot | Self::HeldChild | Self::ChildRecovery)
    }

    /// The stable diagnostic reason label.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::RecoveryRequired => "recovery_required",
            Self::ContextCapacity => "context_capacity",
            Self::SessionRefusal => "session_refusal",
            Self::CompactionNotApplied => "compaction_not_applied",
            Self::UnrecordedRefusal => "unrecorded_refusal",
            Self::WakePreflightFailed => "wake_preflight_failed",
            Self::HeldRoot => "root_held_after_loss",
            Self::HeldChild => "child_held_after_loss",
            Self::ChildRecovery => "child_recovery_required",
            Self::RootContinuationPreparationFailed => "root_continuation_preparation_failed",
        }
    }

    /// The operator action that applies to this reason.
    #[must_use]
    pub const fn action(self) -> &'static str {
        match self {
            Self::HeldRoot => "choose Resume after loss, or send a new prompt",
            Self::HeldChild => "choose Resume after loss for the held children",
            Self::ChildRecovery => "open Agents and retry recovery for the affected child",
            Self::RootContinuationPreparationFailed => {
                "choose Resume after loss to retry, or Stop to leave the turn interrupted"
            }
            _ => "send a new prompt to continue",
        }
    }
}

/// A current blocker and how much work is waiting behind it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionAttention {
    /// The blocker.
    pub reason: AttentionReason,
    /// Pending mail or wakes, not the number of blocked agents.
    pub count: u32,
}

impl SessionAttention {
    /// A bounded, content-free explanation shared by the CLI and session notice.
    #[must_use]
    pub fn detail(self) -> String {
        format!(
            "{}: {} pending mail/wake {}; {}",
            self.reason.code(),
            self.count,
            if self.count == 1 { "item" } else { "items" },
            self.reason.action()
        )
    }

    pub(crate) fn notice(self) -> TranscriptRow {
        let row = TranscriptRow::warning(
            NoticeScope::Host,
            format!("Needs operator · {}", self.detail()),
        );
        match self.reason {
            AttentionReason::HeldRoot
            | AttentionReason::HeldChild
            | AttentionReason::RootContinuationPreparationFailed => {
                row.with_remedy(NoticeRemedy::ResumeAfterLoss {})
            }
            // Retrying a named recovery is distinct from resuming a lost turn.
            _ => row,
        }
    }
}

/// The root blocker visible from joined product facts, independent of mail.
#[must_use]
pub fn root_reason(
    views: &SessionViews,
    intents: &ProductIntents,
    policy: super::ResumeAfterLoss,
) -> Option<AttentionReason> {
    let standing = root_standing_from_intents(intents, views);
    if standing == RootStanding::ContinuationNeedsResume
        || (policy == super::ResumeAfterLoss::Hold
            && standing
                == (RootStanding::Continuing {
                    handoff: false,
                    held: true,
                }))
    {
        return Some(AttentionReason::HeldRoot);
    }
    Some(match views.pause_cause(intents)? {
        PauseCause::RecoveryRequired { turn_id }
            if matches!(
                owner_loss_at_pause(intents, views, &turn_id),
                Some(RootLossState::Held | RootLossState::Pending(_))
            ) =>
        {
            return None;
        }
        PauseCause::RecoveryRequired { .. } => AttentionReason::RecoveryRequired,
        PauseCause::ContextCapacity { .. } => AttentionReason::ContextCapacity,
        PauseCause::SessionRefusal { .. } => AttentionReason::SessionRefusal,
        PauseCause::CompactionNotApplied { .. } => AttentionReason::CompactionNotApplied,
        PauseCause::Host(cause @ HostPauseCause::UnrecordedRefusal { .. }) => {
            if cause.root_preparation_refusal_command().is_some() {
                AttentionReason::RootContinuationPreparationFailed
            } else {
                AttentionReason::UnrecordedRefusal
            }
        }
        PauseCause::Host(HostPauseCause::WakePreflightFailed { .. }) => {
            AttentionReason::WakePreflightFailed
        }
    })
}

/// Work observed in the same tree read as each blocked child.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PendingTreeWork {
    /// Results queued for the root.
    pub results: u32,
    /// Questions queued for the root.
    pub questions: u32,
    child_recovery_mail: u32,
    child_held_mail: u32,
}

impl PendingTreeWork {
    /// Read only addresses and counts, not message bodies.
    #[must_use]
    pub fn read(tree: &ProductAgentTree, policy: super::ResumeAfterLoss) -> Self {
        let mut work = Self::default();
        for mail in tree.unread_mail(&tree.root_agent_id) {
            match mail.purpose {
                MessagePurpose::Result => work.results = work.results.saturating_add(1),
                MessagePurpose::Question => work.questions = work.questions.saturating_add(1),
                _ => {}
            }
        }
        for (id, node) in tree.nodes() {
            if *id == tree.root_agent_id {
                continue;
            }
            let count = if node.status == NodeStatus::RecoveryRequired {
                &mut work.child_recovery_mail
            } else if tree.loss_continuation(id).is_some_and(|loss| {
                loss.state == LossContinuationState::Held
                    && loss.cause == ContinuationCause::OwnerLost
                    && (policy == super::ResumeAfterLoss::Hold || loss.interrupted_continuation)
            }) {
                &mut work.child_held_mail
            } else {
                continue;
            };
            *count = count
                .saturating_add(u32::try_from(tree.unread_mail(id).count()).unwrap_or(u32::MAX));
        }
        work
    }

    /// Alert on work addressed to a blocked node, not unrelated mail to a runnable node.
    #[must_use]
    pub fn attention(
        self,
        root: Option<AttentionReason>,
        process_exits: u32,
        prepared_wakes: u32,
    ) -> Option<SessionAttention> {
        let root_count = self
            .results
            .saturating_add(self.questions)
            .saturating_add(process_exits)
            .max(prepared_wakes);
        if let Some(reason) = root
            && root_count > 0
        {
            return Some(SessionAttention {
                reason,
                count: root_count,
            });
        }
        if self.child_recovery_mail > 0 {
            return Some(SessionAttention {
                reason: AttentionReason::ChildRecovery,
                count: self.child_recovery_mail,
            });
        }
        (self.child_held_mail > 0).then_some(SessionAttention {
            reason: AttentionReason::HeldChild,
            count: self.child_held_mail,
        })
    }
}

/// Count prepared wakes only while they lack a delivery verdict.
#[must_use]
pub fn prepared_wake_count(session: &crate::sessions::SessionRecord) -> u32 {
    let count = session
        .pending_queue
        .iter()
        .filter(|item| {
            session
                .queue_commands
                .pending_for(item)
                .is_some_and(|submission| {
                    submission.kind == crate::sessions::projector::SubmissionKind::HostWake
                        && !is_root_loss_command(&submission.command_id)
                })
        })
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}
