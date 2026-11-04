//! What the repository gate's build reports that a production build drops
//! silently (`test-support`, never in `default`).

/// A credential-wait observation (a park when `waiting`, else its clear) the
/// session's observation mailbox refused: the face never sees the wait
/// change, so a gate run names the refusal on stderr.
pub(crate) fn credential_wait_refused(waiting: bool) {
    let which = if waiting { "park" } else { "clear" };
    eprintln!(
        "plakous-hosted test-support: a credential-wait {which} was refused by the observation mailbox"
    );
}
