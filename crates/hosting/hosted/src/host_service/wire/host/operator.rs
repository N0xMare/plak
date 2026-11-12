//! Independent operator residency, separate from agent activity and accounting.

use super::{Answer, Outcome, ProtocolExtension, ResidentReason, SessionHost};

pub(super) fn resident<P: ProtocolExtension>(
    host: &SessionHost<P>,
    session: &str,
) -> Option<Outcome<P>> {
    host.sessions
        .operator_terminals_blocked(Some(session))
        .then_some(Outcome::Ok(Answer::Resident {
            reason: ResidentReason::OperatorTerminal,
        }))
}
