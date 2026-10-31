//! Process-local product handoff markers, never session authority.

pub(crate) const RELOAD_EPOCH_ENV: &str = "PLAK_RELOAD_APPLY_ORCHESTRATION";

pub(crate) fn reload_epoch_requested() -> bool {
    std::env::var_os("PLAK_RELOAD_APPLY_ORCHESTRATION").as_deref()
        == Some(std::ffi::OsStr::new("1"))
}

/// The host-authored continuation this process's first attach posts, taken
/// once so a later re-attach in the same process never posts it again.
static CONTINUATION: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub(crate) fn set_reload_continuation(text: String) {
    if let Ok(mut slot) = CONTINUATION.lock() {
        *slot = Some(text);
    }
}

pub(crate) fn take_reload_continuation() -> Option<String> {
    CONTINUATION.lock().ok().and_then(|mut slot| slot.take())
}
