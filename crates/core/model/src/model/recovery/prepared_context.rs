use super::PreparedCallRecord;
use crate::{
    ContextContractError, ContextStatus, ResolvedModelDescriptor, Timestamp, ValidatedRequestPlan,
};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub(super) struct LivePreparedContext(Option<Arc<(Timestamp, ContextStatus)>>);

impl LivePreparedContext {
    pub(super) fn new(prepared_at: Timestamp, status: ContextStatus) -> Self {
        Self(Some(Arc::new((prepared_at, status))))
    }

    fn status_at(&self, now: Timestamp) -> Option<&ContextStatus> {
        let (prepared_at, status) = self.0.as_deref()?;
        (*prepared_at == now).then_some(status)
    }
}

// The transient observation does not enter an inert record's equality or codec.
impl PartialEq for LivePreparedContext {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for LivePreparedContext {}

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
        if let Some(status) = self.live_context.status_at(now)
            && self.projection.validate_for_plan(descriptor, plan).is_ok()
        {
            return Ok(status.clone());
        }
        descriptor.context_status(plan, &self.projection, now)
    }
}
