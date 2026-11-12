//! Independent operator residency, kept out of the current client DTO vocabulary.

use super::{Answer, Outcome, ProtocolExtension, ResidentReason, SessionHost};

pub(super) fn resident<P: ProtocolExtension>(
    host: &SessionHost<P>,
    session: &str,
) -> Option<Outcome<P>> {
    // A distinct wire reason arrives only with the terminal client increment.
    host.sessions
        .operator_terminals_blocked(Some(session))
        .then_some(Outcome::Ok(Answer::Resident {
            reason: ResidentReason::Work,
        }))
}
