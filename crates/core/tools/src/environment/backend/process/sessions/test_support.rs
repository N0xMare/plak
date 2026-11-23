use super::{
    ExecutionBackendError, JoinedProcessSessionId, JoinedProcessSessions, Ordering, PROMOTED,
};

impl JoinedProcessSessions {
    /// Models promotion without the background monitor so daemon tests can
    /// inspect the terminal state before its owner joins. The fixture must
    /// join the process itself.
    ///
    /// # Errors
    /// Rejects an unknown session or a refused promotion.
    #[doc(hidden)]
    pub fn promote_without_monitor_for_test(
        &self,
        id: JoinedProcessSessionId,
    ) -> Result<(), ExecutionBackendError> {
        let session = self.session(id)?;
        let state = super::lock(&self.inner.state);
        if let Some(refusal) = self.promotion_refusal(&state, &session) {
            return refusal;
        }
        session.promoted.store(PROMOTED, Ordering::Release);
        Ok(())
    }
}
