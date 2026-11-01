//! Process-local product handoff markers, never session authority.

pub(crate) const RELOAD_EPOCH_ENV: &str = "PLAK_RELOAD_APPLY_ORCHESTRATION";

pub(crate) fn reload_epoch_requested() -> bool {
    std::env::var_os("PLAK_RELOAD_APPLY_ORCHESTRATION").as_deref()
        == Some(std::ffi::OsStr::new("1"))
}
