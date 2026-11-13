//! Unsupported host-layer semantic cut; not an ordinary export or an execution grant.

use super::projector::{ProductIntents, SessionViews, SubmissionKind};
use super::{ProductTranscript, QueueCommands, QueueDrainState, SessionRecord};
use plakous::sessions::SessionLocator;
use plakous::sessions::feed::{FeedLocator, FeedSeq};

mod claims;
mod document;
pub use claims::{MoveIdentityClaim, MoveIdentityKind};
mod groups;
mod tree;
#[doc(hidden)]
pub use tree::project_move_tree;
pub(crate) mod locator_serde;
pub use document::DormantSessionDocument;
pub use groups::{GroupPlanError, MoveGroup, MoveGroups};

/// A settled product cut with explicit intent and projection anchors.
/// This is host composition data, not a supported embedding API or a wire codec.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct DormantSession {
    record: SessionRecord,
    views: SessionViews,
    intents: ProductIntents,
    floor: FeedSeq,
    claims: Vec<MoveIdentityClaim>,
    plan_authority: Option<String>,
    acknowledgements: Vec<MoveAgentAcknowledgement>,
}

/// One acknowledged attention occurrence, retaining its original operator stamp.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveAgentAcknowledgement {
    pub agent_id: String,
    pub attempt_id: String,
    pub attention: String,
    pub ended_at: Option<u64>,
    pub acknowledged_at: u64,
}

#[cfg(test)]
mod tests;

impl DormantSession {
    /// Attaches acknowledged attention without inventing or replaying operator actions.
    ///
    /// # Errors
    /// Refuses invalid identities, timestamps or duplicate attention occurrences.
    pub fn with_acknowledgements(
        mut self,
        rows: Vec<MoveAgentAcknowledgement>,
    ) -> Result<Self, String> {
        let mut identities = std::collections::BTreeSet::new();
        for row in &rows {
            if [&row.agent_id, &row.attempt_id, &row.attention]
                .into_iter()
                .any(|value| value.is_empty() || value.len() > 256)
                || row.ended_at.is_some_and(|stamp| stamp > i64::MAX as u64)
                || row.acknowledged_at > i64::MAX as u64
                || !identities.insert((
                    &row.agent_id,
                    &row.attempt_id,
                    &row.attention,
                    row.ended_at,
                ))
            {
                return Err("invalid move agent acknowledgement".into());
            }
        }
        self.acknowledgements = rows;
        Ok(self)
    }

    /// Acknowledged attention in this product's tree, not a current execution grant.
    #[must_use]
    pub fn acknowledgements(&self) -> &[MoveAgentAcknowledgement] {
        &self.acknowledgements
    }

    /// Preserves the source's named-state namespace, including a deleted fork ancestor.
    ///
    /// # Errors
    /// Refuses an invalid existing product plan-state identity.
    pub fn with_plan_authority(mut self, authority: String) -> Result<Self, String> {
        if authority.is_empty()
            || authority.len() > 59
            || !authority
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err("invalid move plan-state namespace".into());
        }
        self.plan_authority = Some(authority);
        Ok(self)
    }
    /// Attaches the producer's complete permanent claims, including compacted tombstones.
    ///
    /// # Errors
    /// Refuses duplicate, malformed or foreign-owned identities.
    pub fn with_identity_claims(
        mut self,
        mut claims: Vec<MoveIdentityClaim>,
    ) -> Result<Self, String> {
        claims.sort_by(|left, right| left.identity.cmp(&right.identity));
        for claim in &claims {
            claim.validate(&self.record.id)?;
        }
        if claims
            .windows(2)
            .any(|pair| pair[0].identity == pair[1].identity)
        {
            return Err("duplicate ownership-move identity claim".into());
        }
        self.claims = claims;
        Ok(self)
    }

    /// Permanent source content bindings; never re-executed as a journal.
    #[must_use]
    pub fn identity_claims(&self) -> &[MoveIdentityClaim] {
        &self.claims
    }

    /// Ensures export never drops a permanent binding required by a retained intent.
    ///
    /// # Errors
    /// Refuses missing or contradictory creation, queue, command or workflow claims.
    pub fn validate_identity_claims(&self) -> Result<(), String> {
        let exact = |identity: &str, kind, binding: &str| {
            self.claims.iter().any(|claim| {
                claim.identity == identity && claim.kind == kind && claim.binding == binding
            })
        };
        if !exact(
            &self.record.create_operation_id,
            MoveIdentityKind::Create,
            &self.record.id,
        ) || self.record.pending_queue.iter().any(|item| {
            !self.claims.iter().any(|claim| {
                claim.identity == item.id
                    && claim.kind == MoveIdentityKind::Queue
                    && claim.binding == self.record.id
                    && claim.disposition.is_none()
            })
        }) || self.intents.submissions.iter().any(|submission| {
            !exact(
                &submission.binding_identity(),
                MoveIdentityKind::Command,
                &submission.binding_digest(&self.record.id),
            )
        }) || self.intents.workflow_requests.iter().any(|workflow| {
            !exact(
                &workflow.operation_id,
                MoveIdentityKind::Workflow,
                &workflow.binding_digest(&self.record.id),
            )
        }) {
            return Err(
                "ownership-move product cut is missing an immutable identity binding".into(),
            );
        }
        Ok(())
    }
    /// Read-only semantic projection base; not a synthetic historical feed.
    #[must_use]
    pub const fn views(&self) -> &SessionViews {
        &self.views
    }
    /// Validates a projected source cut without changing its durable data.
    ///
    /// # Errors
    /// Refuses an incomplete projection, active turn/effect/workflow, or ambiguous steer.
    pub fn prepare(
        record: SessionRecord,
        views: SessionViews,
        intents: ProductIntents,
        floor: FeedSeq,
    ) -> Result<Self, String> {
        record.validate_for_store()?;
        let locator = record
            .plakous_locator
            .ok_or("dormant session has no runtime")?;
        if floor.get() == u64::MAX
            || views.cursor != floor
            || views.state.runtime_session_id != Some(locator.session_id())
            || views.state.head
                != Some(FeedLocator {
                    session_id: locator.session_id(),
                    branch_id: locator.branch_id(),
                    checkpoint_id: locator.checkpoint_id(),
                })
            || views.state.branch() != Some(locator.branch_id())
        {
            return Err("dormant session projection does not cover its exact feed cut".into());
        }
        if record.archived
            || record.carry_intent.is_some()
            || record.queue_projection_debt
            || views.state.head_working
            || !views.state.open_turns.is_empty()
            || views.state.calls.values().any(|call| !call.settled)
            || views
                .state
                .processes
                .values()
                .any(|process| !process.reported)
            || views
                .state
                .workflows
                .values()
                .any(|workflow| workflow.settled.is_none())
            || intents.workflow_requests.iter().any(|workflow| {
                views
                    .state
                    .workflows
                    .get(&workflow.operation_id)
                    .is_none_or(|completion| completion.settled.is_none())
            })
            || intents.submissions.iter().any(|submission| {
                submission.kind == SubmissionKind::SendNowSteer
                    && !views.state.deliveries.contains_key(&submission.command_id)
            })
        {
            return Err("dormant session still has unsettled work".into());
        }
        if intents
            .submission_anchors
            .values()
            .chain(intents.workflow_anchors.values())
            .any(|anchor| *anchor > floor.get())
            || intents
                .host_intent_positions()
                .any(|(anchor, _)| anchor > floor.get())
            || intents
                .notices
                .iter()
                .any(|notice| notice.anchor > floor.get())
        {
            return Err("dormant session has an intent beyond its feed cut".into());
        }
        Ok(Self {
            record,
            views,
            intents,
            floor,
            claims: Vec::new(),
            plan_authority: None,
            acknowledgements: Vec::new(),
        })
    }

    /// Compiles the product seed for a newly reconstructed checkpoint, without I/O.
    /// The caller stages it while the whole connected group is withheld.
    ///
    /// # Errors
    /// Refuses changed public runtime identities or reuse of the immutable source head.
    pub fn reconstruct(mut self, target: SessionLocator) -> Result<Self, String> {
        let source = self
            .record
            .plakous_locator
            .ok_or("dormant session has no runtime")?;
        if source.session_id() != target.session_id()
            || source.branch_id() != target.branch_id()
            || source.checkpoint_id() == target.checkpoint_id()
        {
            return Err(
                "dormant move must preserve the session/branch and replace its checkpoint".into(),
            );
        }
        self.record.store_revision = 0;
        self.record.plakous_locator = Some(target);
        self.record.first_prompt = None;
        self.record.queue_commands = QueueCommands::default();
        self.record.queue_projection_debt = false;
        self.record.queue_drain_state = QueueDrainState::RestoredPaused;
        self.record.root_turn_count = self.views.state.counters.root_turns;
        self.record.tool_call_count = self.views.state.counters.tool_calls;
        self.record.compaction_count = self.views.state.counters.compactions;
        self.views.state.head = Some(FeedLocator {
            session_id: target.session_id(),
            branch_id: target.branch_id(),
            checkpoint_id: target.checkpoint_id(),
        });
        self.views.state.history_through = Some(self.floor.get());
        self.views.state.imported_through = Some(self.floor.get());
        self.record.transcript = ProductTranscript::stored(self.views.transcript(&self.intents));
        self.record.transcript.mark_committed();
        // Keep each delivery verdict and the original retention floor. Advancing
        // that floor to tail+1 would silently consume still-owed submissions.
        Ok(self)
    }

    /// The product, views and immutable intents to install atomically.
    #[must_use]
    pub fn into_parts(self) -> (SessionRecord, SessionViews, ProductIntents) {
        (self.record, self.views, self.intents)
    }
}
