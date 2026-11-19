use super::PreparedCallRecord;
use crate::{
    ContextContractError, ContextStatus, ResolvedModelDescriptor, Timestamp, ValidatedRequestPlan,
};

impl PreparedCallRecord {
    /// Reports fit for this exact projection. The live record can reuse the
    /// sealed status only for the original plan, descriptor and observation
    /// time. This is not a dispatch or credentials check.
    ///
    /// # Errors
    ///
    /// Fails if recomputation cannot construct an immutable identity.
    pub fn context_status(
        &self,
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        now: Timestamp,
    ) -> Result<ContextStatus, ContextContractError> {
        if let Some((prepared_at, status)) = self.live_context.as_deref()
            && *prepared_at == now
            && self.projection.validate_for_plan(descriptor, plan).is_ok()
        {
            return Ok(status.clone());
        }
        descriptor.context_status(plan, &self.projection, now)
    }
}

impl PartialEq for PreparedCallRecord {
    fn eq(&self, other: &Self) -> bool {
        // The live status is not part of the inert record.
        self.semantic_revision == other.semantic_revision
            && self.projection == other.projection
            && self.authority_evidence == other.authority_evidence
            && self.context_admission == other.context_admission
            && self.context_status == other.context_status
            && self.recovery_locator == other.recovery_locator
            && self.identity == other.identity
    }
}

impl Eq for PreparedCallRecord {}
