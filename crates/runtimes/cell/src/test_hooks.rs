//! Cell handshake bound for `test-hooks` builds.
//!
//! Other builds do not compile this module. The production bound stays 30 s.

/// `PLAK_TEST_CELL_HELLO_MS`, when set.
pub(crate) fn cell_hello_millis() -> Option<String> {
    std::env::var("PLAK_TEST_CELL_HELLO_MS").ok()
}
