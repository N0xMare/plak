//! What the repository gate's build reports that a production build drops
//! silently (`test-hooks`, never in `default`).

/// A credential-wait clear the event hub refused: the host keeps showing the
/// wait, so a gate run names the refusal on stderr.
pub(crate) fn credential_wait_clear_refused(error: &dyn std::fmt::Debug) {
    eprintln!("plakous-agent test-hooks: a credential-wait clear was not published: {error:?}");
}
