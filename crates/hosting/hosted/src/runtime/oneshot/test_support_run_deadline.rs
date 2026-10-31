//! File gate that collapses a configured non-interactive run deadline.
//!
//! Compiled only with `test-support`, which `plak`'s `test-hooks` enables.
//! Release builds use the stub in `oneshot` and never read the variable.

pub(super) fn elapsed() -> bool {
    std::env::var_os("PLAK_TEST_RUN_DEADLINE")
        .is_some_and(|path| !path.is_empty() && std::path::Path::new(&path).exists())
}
