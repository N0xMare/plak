//! Session lease override for `test-hooks` builds.
//!
//! Other builds do not compile this module. The production lease stays 30 s.

use std::cell::Cell;

thread_local! {
    static LEASE_SECONDS: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Run a synchronous fixture setup with a thread-local lease override.
pub fn with_test_session_lease_seconds<T>(seconds: u64, setup: impl FnOnce() -> T) -> T {
    struct Reset(Option<u64>);
    impl Drop for Reset {
        fn drop(&mut self) {
            LEASE_SECONDS.set(self.0);
        }
    }
    let _reset = Reset(LEASE_SECONDS.replace(Some(seconds)));
    setup()
}

/// The scoped fixture value, or `PLAK_TEST_SESSION_LEASE_SECONDS` when set.
pub(crate) fn session_lease_seconds_override() -> Option<String> {
    LEASE_SECONDS
        .get()
        .map(|seconds| seconds.to_string())
        .or_else(|| std::env::var("PLAK_TEST_SESSION_LEASE_SECONDS").ok())
}
