//! Process-local product handoff markers, never session authority.

/// This process's `PATH`, when set and valid text.
pub(crate) fn inherited_path() -> Option<String> {
    std::env::var_os("PATH").and_then(|value| value.into_string().ok())
}

/// This process's `HOME`, when set and valid text.
pub(crate) fn inherited_home() -> Option<String> {
    std::env::var_os("HOME").and_then(|value| value.into_string().ok())
}

/// The user's login shell, when `SHELL` names an absolute path.
pub(crate) fn login_shell() -> Option<String> {
    std::env::var_os("SHELL")
        .and_then(|value| value.into_string().ok())
        .filter(|shell| shell.starts_with('/'))
}

#[cfg_attr(
    not(feature = "tui"),
    allow(dead_code, reason = "only the interactive launch reloads")
)]
pub(crate) fn reload_epoch_requested() -> bool {
    std::env::var_os("PLAK_RELOAD_APPLY_ORCHESTRATION").as_deref()
        == Some(std::ffi::OsStr::new("1"))
}
