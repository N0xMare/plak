//! The tool environment a host's sessions get: the login shell's `PATH`
//! and the user's `HOME`, captured once at start, as `plak runner serve`
//! does when `runner.toml` sets no `path` (spec 02 §4.6). Not a session's
//! own: a host never sees the shell a TUI was started in.

/// Where the host's `PATH` came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PathSource {
    /// The user's login shell answered.
    LoginShell,
    /// No login shell answered in time; this process's own.
    Inherited,
    /// Neither: the minimal system path.
    Minimal,
}

/// The environment captured at start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HostEnvironment {
    path: String,
    home: Option<String>,
    source: PathSource,
}

/// The `PATH` when nothing else names one.
const MINIMAL_PATH: &str = "/usr/bin:/bin";

impl HostEnvironment {
    pub(super) fn backend(
        &self,
        client: plakous::runner::RunnerClient,
        runtime: tokio::runtime::Handle,
    ) -> plakous::tools::runner::LocalDaemonBackend {
        plakous::tools::runner::LocalDaemonBackend::new(
            std::sync::Arc::new(client),
            runtime,
            self.path.clone(),
            self.home.as_ref().map(std::path::PathBuf::from),
        )
    }
    /// Asks the login shell once, with the runner's 5 s bound.
    pub(super) fn capture() -> Self {
        Self::from_sources(
            crate::commands::runner::login_shell_path(),
            crate::environment::inherited_path(),
            crate::environment::inherited_home(),
        )
    }

    fn from_sources(
        login: Option<String>,
        inherited: Option<String>,
        home: Option<String>,
    ) -> Self {
        let (path, source) = match (login, inherited) {
            (Some(path), _) => (path, PathSource::LoginShell),
            (None, Some(path)) => (path, PathSource::Inherited),
            (None, None) => (MINIMAL_PATH.to_owned(), PathSource::Minimal),
        };
        Self { path, home, source }
    }

    /// What the start line says about it; never the values themselves.
    pub(super) fn describe(&self) -> String {
        let path = match self.source {
            PathSource::LoginShell => "the login shell's PATH",
            PathSource::Inherited => "this process's PATH (no login shell answered)",
            PathSource::Minimal => "the minimal PATH (no login shell answered)",
        };
        let entries = self
            .path
            .split(':')
            .filter(|entry| !entry.is_empty())
            .count();
        let home = if self.home.is_some() {
            "HOME"
        } else {
            "no HOME"
        };
        format!("{path} ({entries} entries) and {home}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    /// The login shell's answer wins; without one the process's own `PATH`,
    /// and without that the minimal one. The start line names the source,
    /// not the directories.
    #[test]
    fn the_login_shell_path_wins_and_the_source_is_named() {
        let login = HostEnvironment::from_sources(
            Some("/opt/bin:/usr/bin".into()),
            Some("/usr/bin".into()),
            Some("/home/u".into()),
        );
        assert_eq!(login.path, "/opt/bin:/usr/bin");
        assert_eq!(
            login.describe(),
            "the login shell's PATH (2 entries) and HOME"
        );
        assert!(!login.describe().contains("/opt/bin"));
        let inherited = HostEnvironment::from_sources(None, Some("/usr/bin".into()), None);
        assert_eq!(inherited.source, PathSource::Inherited);
        assert!(inherited.describe().ends_with("and no HOME"));
        let minimal = HostEnvironment::from_sources(None, None, None);
        assert_eq!(minimal.path, MINIMAL_PATH);
        assert_eq!(minimal.source, PathSource::Minimal);
    }
}
