//! Session lease override for `test-hooks` builds.
//!
//! Other builds do not compile this module. The production lease stays 30 s.

/// `PLAK_TEST_SESSION_LEASE_SECONDS`, when set.
pub(crate) fn session_lease_seconds_override() -> Option<String> {
    std::env::var("PLAK_TEST_SESSION_LEASE_SECONDS").ok()
}
