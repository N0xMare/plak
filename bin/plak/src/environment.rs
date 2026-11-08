//! Process-local product handoff markers, never session authority.

static HOST_KEY_RESOLVER: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[allow(dead_code, reason = "only hosted transports render the restart remedy")]
pub(crate) const HOST_RESTART_REMEDY: &str = "send SIGTERM to the plak host process, or run `systemctl --user restart plak-host.service` if it runs as the installed unit";

#[derive(Clone, Copy)]
pub(crate) enum KeyResolver {
    Cli,
    Host,
    AutoHost,
}

pub(crate) fn key_resolver() -> KeyResolver {
    match HOST_KEY_RESOLVER.load(std::sync::atomic::Ordering::Acquire) {
        1 => KeyResolver::Host,
        2 => KeyResolver::AutoHost,
        _ => KeyResolver::Cli,
    }
}

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

#[cfg(feature = "web")]
pub(crate) mod host {
    use super::HOST_KEY_RESOLVER;

    pub(crate) fn serving_keys(auto_started: bool) {
        HOST_KEY_RESOLVER.store(
            if auto_started { 2 } else { 1 },
            std::sync::atomic::Ordering::Release,
        );
    }

    /// Deliberate OS/tool inputs, never the caller's API credentials or arbitrary
    /// product/reload overrides. The fixture seam is empty in shipped builds.
    pub(crate) fn start_environment() -> Vec<(&'static str, std::ffi::OsString)> {
        const INPUTS: &[&str] = &[
            "HOME",
            "PATH",
            "SHELL",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "LC_MESSAGES",
            "LC_TIME",
            "LC_NUMERIC",
            "LC_COLLATE",
            "LC_MONETARY",
            "TZ",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CONFIG_DIRS",
            "XDG_DATA_DIRS",
            "XDG_CACHE_HOME",
            "XDG_RUNTIME_DIR",
            "SSH_AUTH_SOCK",
            "DBUS_SESSION_BUS_ADDRESS",
            "USER",
            "LOGNAME",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
        ];
        let mut values: Vec<_> = INPUTS
            .iter()
            .filter_map(|&name| std::env::var_os(name).map(|value| (name, value)))
            .collect();
        values.extend(crate::test_hooks::host_start_environment());
        // A missing PATH must not make startup depend on a shell's defaults.
        if !values.iter().any(|(name, _)| *name == "PATH") {
            values.push(("PATH", "/usr/bin:/bin".into()));
        }
        values
    }

    pub(crate) fn remote_shell() -> bool {
        std::env::var_os("SSH_CONNECTION").is_some()
            || std::env::var_os("SSH_TTY").is_some()
            || std::env::var_os("SSH_CLIENT").is_some()
    }

    /// Unit lookup only; this manager override is not passed to an unmanaged host.
    pub(crate) fn unit_path() -> Option<std::ffi::OsString> {
        std::env::var_os("SYSTEMD_UNIT_PATH")
    }
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
