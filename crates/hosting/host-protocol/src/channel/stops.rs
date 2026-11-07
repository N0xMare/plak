//! Ticket admission for durable stops and process-local launcher release.

use super::{
    IntentReceiver, IntentSendError, IntentSender, ProtocolExtension, SETTLED_STOPS, SessionIntent,
    StopTicket,
};

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

    /// Cancels an uncommitted release fence. If the actor already settled
    /// it, takes that outcome instead; successful release must still be joined.
    pub fn cancel_release_for_reopen(&self, ticket: StopTicket) -> Option<Result<(), String>> {
        let mut book = self
            .0
            .stops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(at) = book.settled.iter().position(|(seen, _)| *seen == ticket) {
            return book.settled.remove(at).map(|(_, outcome)| outcome);
        }
        book.reopens.remove(&ticket);
        None
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
            let ticket = StopTicket(book.issued);
            if reopen.is_some() {
                book.reopens.insert(ticket);
            }
            ticket
        };
        let sent = self.send(if let Some(session) = reopen {
            SessionIntent::ReleaseForReopen {
                ticket,
                session: session.to_owned(),
            }
        } else {
            SessionIntent::StopSession { ticket }
        });
        if let Err(error) = sent {
            self.0
                .stops
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .reopens
                .remove(&ticket);
            return Err(error);
        }
        Ok(ticket)
    }
}

impl<P: ProtocolExtension> IntentReceiver<P> {
    /// Settles a release only if its launcher has not cancelled it. The
    /// actor must quit only when this returns true for a successful outcome.
    #[must_use]
    pub fn settle_release_for_reopen(
        &self,
        ticket: StopTicket,
        outcome: Result<(), String>,
    ) -> bool {
        self.settle_ticket(ticket, outcome, true)
    }

    pub(super) fn settle_ticket(
        &self,
        ticket: StopTicket,
        outcome: Result<(), String>,
        reopen: bool,
    ) -> bool {
        let hook = {
            let mut book = self
                .0
                .stops
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if reopen && !book.reopens.remove(&ticket) {
                return false;
            }
            if book.settled.len() >= SETTLED_STOPS {
                book.settled.pop_front();
            }
            book.settled.push_back((ticket, outcome));
            self.0
                .stop_hook
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        };
        self.0.ready.notify_all();
        self.0.wake_waiter();
        if let Some(hook) = hook {
            hook();
        }
        true
    }
}
