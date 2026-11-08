//! Process-local product handoff markers, never session authority.

#[allow(dead_code, reason = "only the web host starts a separate runner")]
pub(crate) fn service_managed() -> bool {
    std::env::var_os("INVOCATION_ID").is_some()
}

#[allow(dead_code, reason = "only gate builds select detached runner fixtures")]
pub(crate) fn test_runner_detached() -> bool {
    std::env::var_os("PLAK_TEST_RUNNER_DETACHED").as_deref() == Some(std::ffi::OsStr::new("1"))
}

/// This process's `PATH`, when set and valid text.
pub(crate) fn inherited_path() -> Option<String> {
    std::env::var_os("PATH").and_then(|value| value.into_string().ok())
}

/// This process's `HOME`, when set and valid text.
pub(crate) fn inherited_home() -> Option<String> {
    std::env::var_os("HOME").and_then(|value| value.into_string().ok())
}

/// Only the transport variables needed to reach the caller's user manager.
pub(crate) fn user_manager_environment() -> Vec<(&'static str, std::ffi::OsString)> {
    [
        ("XDG_RUNTIME_DIR", std::env::var_os("XDG_RUNTIME_DIR")),
        (
            "DBUS_SESSION_BUS_ADDRESS",
            std::env::var_os("DBUS_SESSION_BUS_ADDRESS"),
        ),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|value| (name, value)))
    .collect()
}

pub(crate) fn xdg_config_home() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from)
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
