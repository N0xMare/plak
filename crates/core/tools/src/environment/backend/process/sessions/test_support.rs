use super::{
    ExecutionBackendError, ExecutionBackendErrorKind, FOREGROUND, JoinedProcessSessionId,
    JoinedProcessSessions, Ordering, PROMOTED, PROMOTING, backend_error,
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
        {
            let state = super::lock(&self.inner.state);
            if let Some(refusal) = Self::promotion_state_refusal(&state, &session) {
                return refusal;
            }
            if session
                .promoted
                .compare_exchange(FOREGROUND, PROMOTING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(backend_error(
                    ExecutionBackendErrorKind::Cancelled,
                    "foreground cancellation won before job promotion",
                ));
            }
        }
        if let Err(error) = self.observer_has_room() {
            session.promoted.store(FOREGROUND, Ordering::Release);
            return Err(error);
        }
        session.promoted.store(PROMOTED, Ordering::Release);
        Ok(())
    }
}
