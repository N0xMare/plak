//! Ticket admission for durable stops and process-local launcher release.

use super::{IntentSendError, IntentSender, ProtocolExtension, SessionIntent, StopTicket};

impl<P: ProtocolExtension> IntentSender<P> {
    /// Asks the actor to release only if no turn or child work runs.
    /// This is process-local and cannot be constructed by a wire client.
    /// `session` is the requester's observed identity. A successful ticket
    /// selects release; the launcher must still join that worker before opening.
    ///
    /// # Errors
    /// The control mailbox refused admission.
    #[allow(clippy::result_large_err, reason = "same owned refusal as send")]
    pub fn request_release_for_reopen(
        &self,
        session: &str,
    ) -> Result<StopTicket, IntentSendError<P>> {
        self.request_ticket(Some(session))
    }

    #[allow(clippy::result_large_err, reason = "same owned refusal as send")]
    pub(super) fn request_ticket(
        &self,
        reopen: Option<&str>,
    ) -> Result<StopTicket, IntentSendError<P>> {
        let ticket = {
            let mut book = self
                .0
                .stops
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            book.issued += 1;
            StopTicket(book.issued)
        };
        self.send(if let Some(session) = reopen {
            SessionIntent::ReleaseForReopen {
                ticket,
                session: session.to_owned(),
            }
        } else {
            SessionIntent::StopSession { ticket }
        })?;
        Ok(ticket)
    }
}
