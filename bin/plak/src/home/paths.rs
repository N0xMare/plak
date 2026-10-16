//! Product filesystem layout under `~/.plak/`.
//!
//! ## Querying telemetry
//!
//! `subagents.sqlite3` holds the agent tree (schema v2).
//! Schema-versioned telemetry files hold disposable analytics. Use
//! `plak telemetry sql "<query>"` for read-only SQL with `product` and `runtime`
//! ATTACH aliases over `product.sqlite3` and `runtime.sqlite3`.
//!
//! Example — cache hit rate per model (`agent_nodes.tuple_id`) per session:
//!
//! ```sql
//! SELECT n.tuple_id AS model, t.session_id,
//!   ROUND(100.0 * SUM(t.cached_input_tokens) / NULLIF(SUM(t.input_tokens), 0), 2) AS cache_hit_pct
//! FROM v_turn_costs t
//! JOIN product.agent_nodes n ON n.agent_id = t.agent_id
//! GROUP BY n.tuple_id, t.session_id;
//! ```
//!
//! Example — average TTFT per model:
//!
//! ```sql
//! SELECT n.tuple_id AS model, AVG(m.ttft_ns) / 1e6 AS avg_ttft_ms
//! FROM model_attempts m
//! JOIN product.agent_nodes n ON n.agent_id = m.agent_id
//! WHERE m.ttft_ns IS NOT NULL
//! GROUP BY n.tuple_id;
//! ```
//!
//! Example — tool success rate per agent:
//!
//! ```sql
//! SELECT v.agent_id,
//!   ROUND(100.0 * SUM(v.completed) / NULLIF(SUM(v.total), 0), 2) AS success_pct
//! FROM v_tool_success v
//! GROUP BY v.agent_id;
//! ```

use std::fs;
use std::path::{Path, PathBuf};

/// Well-known product paths under the Plak home directory, plus the project
/// root whose `.plak/agents/` may override user templates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlakPaths {
    home: PathBuf,
    project_root: Option<PathBuf>,
}

impl PlakPaths {
    #[must_use]
    pub fn explicit(home: PathBuf) -> Self {
        Self {
            home,
            project_root: None,
        }
    }

    /// Resolve the nearest project configuration home from a workspace.
    #[must_use]
    pub fn for_workspace(home: PathBuf, workspace: &Path) -> Self {
        let project_root = discover_project_root(workspace, &[home.as_path()], None);
        Self { home, project_root }
    }

    /// The same home with `root` as the project whose `.plak/agents/` is read.
    #[must_use]
    pub fn with_project_root(mut self, root: PathBuf) -> Self {
        self.project_root = Some(root);
        self
    }

    /// Resolve product home: `$PLAK_HOME` or `$HOME/.plak`.
    pub fn resolve(
        plak_home: Option<&std::ffi::OsStr>,
        home: Option<&std::ffi::OsStr>,
    ) -> Result<Self, String> {
        if let Some(explicit) = plak_home.filter(|v| !v.is_empty()) {
            let path = PathBuf::from(explicit);
            if !path.is_absolute() {
                return Err("PLAK_HOME must be an absolute path".to_owned());
            }
            return Ok(Self {
                home: path,
                project_root: None,
            });
        }
        let home = home
            .filter(|v| !v.is_empty())
            .ok_or_else(|| "HOME is unavailable".to_owned())?;
        let home_path = PathBuf::from(home);
        if !home_path.is_absolute() {
            return Err("HOME must be an absolute path".to_owned());
        }
        Ok(Self {
            home: home_path.join(".plak"),
            project_root: None,
        })
    }

    /// `$PLAK_HOME` or `$HOME/.plak`, with the nearest ancestor carrying a
    /// project `.plak` home as the project override root. An ancestor is not a
    /// project home merely because `.plak` exists: it must hold `config.toml`
    /// or an `agents` directory.
    pub fn from_env() -> Result<Self, String> {
        let plak_home = std::env::var_os("PLAK_HOME");
        let home = std::env::var_os("HOME");
        let paths = Self::resolve(plak_home.as_deref(), home.as_deref())?;
        Ok(match std::env::current_dir() {
            Ok(cwd) => paths.discovered_from(home.as_deref(), &cwd),
            Err(_) => paths,
        })
    }

    /// Discovery from the launch directory. The default `$HOME/.plak` is a
    /// product home even when `PLAK_HOME` points elsewhere, so it is never
    /// taken as the project override of everything below `$HOME`.
    fn discovered_from(self, home: Option<&std::ffi::OsStr>, cwd: &Path) -> Self {
        let default_home = home
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join(".plak"));
        let mut product_homes = vec![self.home.as_path()];
        product_homes.extend(default_home.as_deref());
        let project_root = discover_project_root(cwd, &product_homes, None);
        Self {
            project_root,
            ..self
        }
    }

    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    #[must_use]
    pub fn from_home(home: PathBuf) -> Self {
        Self {
            home,
            project_root: None,
        }
    }

    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    /// User template files, one `<name>.toml` each.
    #[must_use]
    pub fn user_agents_dir(&self) -> PathBuf {
        self.home.join("agents")
    }

    /// Project template files; a name here wins over the user file when the
    /// containing project is trusted.
    #[must_use]
    pub fn project_agents_dir(&self) -> Option<PathBuf> {
        self.project_root
            .as_ref()
            .map(|root| root.join(".plak/agents"))
    }

    /// The project override document, or `None` for the global configuration.
    #[must_use]
    pub fn project_config_file(&self) -> Option<PathBuf> {
        self.project_root
            .as_ref()
            .map(|root| root.join(".plak/config.toml"))
    }

    /// The project home itself. It is configuration only: sessions, secrets,
    /// databases, logs, telemetry and cache all remain under `home`.
    #[must_use]
    pub fn project_home(&self) -> Option<PathBuf> {
        self.project_root.as_ref().map(|root| root.join(".plak"))
    }

    /// The discovered project root, canonicalized at discovery.
    #[must_use]
    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    /// The global trust registry: the canonical project roots whose `.plak`
    /// configuration the operator has admitted. It lives under the private
    /// product home, never inside a project.
    #[must_use]
    pub fn trusted_projects_file(&self) -> PathBuf {
        self.home.join("trusted-projects.toml")
    }

    #[must_use]
    pub fn secrets_dir(&self) -> PathBuf {
        self.home.join("secrets")
    }

    #[must_use]
    pub fn api_key_file(&self, account: &str) -> PathBuf {
        self.secrets_dir().join(format!("{account}.key"))
    }

    /// `secrets/named/`: the operator's named secrets (`plak secret set`).
    #[must_use]
    pub fn named_secrets_dir(&self) -> PathBuf {
        self.secrets_dir().join("named")
    }

    /// One named secret's file. The caller has validated `name`.
    #[must_use]
    pub fn named_secret_file(&self, name: &str) -> PathBuf {
        self.named_secrets_dir().join(format!("{name}.secret"))
    }

    /// Creates `secrets/named/` 0700 under the private layout, and refuses
    /// one that exists but is not private, exactly as the layout refuses the
    /// secrets directory itself.
    pub fn ensure_named_secrets_dir(&self) -> Result<(), String> {
        self.ensure_layout()?;
        let dir = self.named_secrets_dir();
        match fs::symlink_metadata(&dir) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_private_directory(&dir)
                    .or_else(|error| {
                        if error.kind() == std::io::ErrorKind::AlreadyExists {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })
                    .map_err(|_| {
                        "could not create the private named secrets directory".to_owned()
                    })?;
            }
            Err(_) => return Err("could not inspect the named secrets directory".to_owned()),
        }
        require_private_directory(&dir, "Plak named secrets directory")
    }

    /// Whether `secrets/named/` exists, verified private along with the
    /// secrets directory above it. Creates nothing.
    pub fn verify_named_secrets_dir(&self) -> Result<bool, String> {
        let dir = self.named_secrets_dir();
        match fs::symlink_metadata(&dir) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err("could not inspect the named secrets directory".to_owned()),
        }
        require_private_directory(&self.secrets_dir(), "Plak secrets directory")?;
        require_private_directory(&dir, "Plak named secrets directory")?;
        Ok(true)
    }

    /// Parent of every build's state directory.
    #[must_use]
    pub fn state_root(&self) -> PathBuf {
        self.home.join("state")
    }

    /// This build's durable state: the runtime, product and tree databases and
    /// the session event logs, under a directory named by their formats
    /// ([`crate::home::state::state_key`]).
    #[must_use]
    pub fn state_dir(&self) -> PathBuf {
        self.state_root().join(crate::home::state::state_key())
    }

    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.state_dir().join("sessions")
    }

    /// The hosted session's view of this home: its log directory, and nothing
    /// else it knows.
    ///
    /// An inherent method rather than `impl From<&PlakPaths> for SessionHome`,
    /// because the orphan rule forbids that once `SessionHome` lives in
    /// `plakous-hosted`: the self type would be foreign and in first position.
    #[must_use]
    pub fn session_home(&self) -> plakous_hosted::session_home::SessionHome {
        plakous_hosted::session_home::SessionHome::new(self.sessions_dir())
    }

    /// Append-only agent-readable event log for one session.
    #[must_use]
    pub fn session_events_file(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{id}.events.jsonl"))
    }

    /// Private root of promoted jobs' output files, one directory per runtime session.
    #[must_use]
    pub fn job_outputs(&self) -> PathBuf {
        self.state_dir().join("jobs")
    }

    #[must_use]
    pub fn runtime_database(&self) -> PathBuf {
        self.state_dir().join("runtime.sqlite3")
    }

    /// Plak-owned product authority. This is deliberately distinct from the
    /// Plakous semantic runtime database above.
    #[must_use]
    pub fn product_database(&self) -> PathBuf {
        self.state_dir().join("product.sqlite3")
    }

    /// The agent tree's own authority. A separate file from
    /// `product.sqlite3` because it carries its own schema version and
    /// keeps its own file: no transaction ever spanned session rows and tree
    /// rows, so nothing was gained by sharing one.
    #[must_use]
    pub fn subagents_database(&self) -> PathBuf {
        self.state_dir().join("subagents.sqlite3")
    }

    /// Disposable analytics root; never recovery authority. It sits outside
    /// `state/` so clearing it can never touch the paired authorities.
    #[must_use]
    pub fn telemetry_dir(&self) -> PathBuf {
        self.home.join("telemetry")
    }

    /// Disposable analytics store; never recovery authority.
    #[must_use]
    pub fn telemetry_database(&self) -> PathBuf {
        self.telemetry_dir().join(format!(
            "telemetry-v{}.sqlite3",
            crate::telemetry_store::TELEMETRY_SCHEMA_VERSION
        ))
    }

    /// Append-only diagnostics log (`PLAK_LOG`); content-free boundary spans only.
    #[must_use]
    pub fn diagnostics_log_file(&self) -> PathBuf {
        self.home.join("logs/plak.log")
    }

    /// Establish the state-using startup boundary without following or
    /// repairing an injected Plak-home entry. An existing home with anything
    /// in it must already be private; only a home created by this call, or an
    /// empty directory the user owns (`mkdir h; PLAK_HOME=h plak …`), has its
    /// mode established, because nothing in it can have been exposed yet.
    pub fn ensure_private_state_home(&self) -> Result<(), String> {
        // `symlink_metadata(home)` does not reveal a symlink in an ancestor.
        // Refuse those too: otherwise an absolute PLAK_HOME such as
        // `/safe/link/plak` could redirect state writes outside the
        // operator-selected namespace before the leaf is inspected.
        for ancestor in self.home.ancestors().skip(1) {
            let metadata = fs::symlink_metadata(ancestor)
                .map_err(|_| "could not inspect Plak home ancestry".to_owned())?;
            if metadata.file_type().is_symlink() {
                // macOS exposes `/var` and `/tmp` as symlinks into `/private/*`.
                // Reject operator-placed ancestors only.
                let ancestor = ancestor.to_string_lossy();
                if matches!(
                    ancestor.as_ref(),
                    "/var" | "/tmp" | "/etc" | "/private/var" | "/private/tmp" | "/private/etc"
                ) {
                    continue;
                }
                return Err(format!(
                    "Plak home ancestor {} is not a direct directory path; set PLAK_HOME to a path without symlinked parents",
                    ancestor
                ));
            }
            if !metadata.file_type().is_dir() {
                return Err(format!(
                    "Plak home ancestor {} is not a directory; set PLAK_HOME to a path without symlinked parents",
                    ancestor.display()
                ));
            }
        }
        match fs::symlink_metadata(&self.home) {
            Ok(_) => {
                adopt_empty_directory(&self.home);
                require_private_directory(&self.home, "Plak home")
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_private_directory(&self.home)
                    .or_else(|error| {
                        if error.kind() == std::io::ErrorKind::AlreadyExists {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })
                    .map_err(|_| "could not create private Plak home".to_owned())?;
                require_private_directory(&self.home, "Plak home")
            }
            Err(_) => Err("could not inspect Plak home".to_owned()),
        }
    }

    /// Creates the product layout under a private home. Like every other
    /// entry point it refuses, never repairs, a home that is not private.
    pub fn ensure_layout(&self) -> Result<(), String> {
        self.ensure_private_state_home()?;
        match fs::symlink_metadata(self.secrets_dir()) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_private_directory(&self.secrets_dir())
                    .or_else(|error| {
                        if error.kind() == std::io::ErrorKind::AlreadyExists {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })
                    .map_err(|_| "could not create the private secrets directory".to_owned())?;
            }
            Err(_) => return Err("could not inspect plak product directories".to_owned()),
        }
        require_private_directory(&self.secrets_dir(), "Plak secrets directory")?;
        for dir in [
            &self.state_dir(),
            &self.sessions_dir(),
            &self.user_agents_dir(),
            &self.telemetry_dir(),
            &self.home.join("logs"),
        ] {
            match fs::symlink_metadata(dir) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Ok(_) => return Err("plak product path is not a directory".to_owned()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir_all(dir)
                        .map_err(|_| "could not create plak product directories".to_owned())?;
                }
                Err(_) => return Err("could not inspect plak product directories".to_owned()),
            }
        }
        Ok(())
    }
}

/// The nearest ancestor of `cwd` whose `.plak` is a configuration home: a
/// real directory holding `config.toml` or an `agents` directory. A `.plak`
/// that is a symlink, a plain file, an empty directory, or the product home
/// itself is not a project home — discovery fails closed rather than
/// trusting a redirected path or reading the global home as an override.
fn discover_project_root(
    cwd: &Path,
    product_homes: &[&Path],
    stop_at: Option<&Path>,
) -> Option<PathBuf> {
    let cwd = fs::canonicalize(cwd).ok()?;
    let product_homes: Vec<PathBuf> = product_homes
        .iter()
        .map(|home| fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()))
        .collect();
    let stop_at = stop_at.and_then(|stop| fs::canonicalize(stop).ok());
    for ancestor in cwd.ancestors() {
        let home = ancestor.join(".plak");
        let is_home = fs::symlink_metadata(&home)
            .map(|metadata| metadata.file_type().is_dir())
            .unwrap_or(false)
            && (home.join("config.toml").is_file() || home.join("agents").is_dir());
        if is_home && !product_homes.contains(&fs::canonicalize(&home).unwrap_or(home.clone())) {
            return Some(ancestor.to_path_buf());
        }
        if stop_at.as_deref() == Some(ancestor) {
            return None;
        }
    }
    None
}

fn require_private_directory(path: &Path, label: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| format!("could not inspect {label}"))?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "{label} {} is not a private directory",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(format!(
                "{label} {} is owned by another user; use a PLAK_HOME you own",
                path.display()
            ));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(format!(
                "{label} {} is not private to the current user (mode {mode:o}); run `chmod 700 {}`",
                path.display(),
                path.display()
            ));
        }
    }
    Ok(())
}

/// Makes an empty directory the current user owns private. Anything else is
/// left exactly as it is for `require_private_directory` to judge.
fn adopt_empty_directory(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return;
        };
        let empty = fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none());
        if metadata.file_type().is_dir()
            && metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.permissions().mode() & 0o777 != 0o700
            && empty
        {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Resolve Plakous native-subscription state home from XDG conventions.
pub fn application_state_home() -> Result<PathBuf, String> {
    resolve_xdg_state_home(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Where `tools/build/build-plak.sh` keeps installed snapshots, each with a
/// `state-key` manifest: `$HOME/.local/lib/plak/maintainer` for maintainer
/// builds and `$HOME/.local/lib/plak/release` for `just install`.
#[must_use]
pub fn installed_snapshot_roots() -> Vec<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| {
            let lib = PathBuf::from(home).join(".local/lib/plak");
            vec![lib.join("maintainer"), lib.join("release")]
        })
        .unwrap_or_default()
}

fn resolve_xdg_state_home(
    explicit: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, String> {
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(explicit);
        if path.is_absolute() {
            return Ok(path);
        }
        return Err("XDG_STATE_HOME must be absolute".to_owned());
    }
    let home = home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "XDG_STATE_HOME is unset and HOME is unavailable".to_owned())?;
    if !home.is_absolute() {
        return Err("HOME must be absolute".to_owned());
    }
    Ok(home.join(".local/state"))
}

#[cfg(any(test, feature = "test-support"))]
#[allow(clippy::expect_used, reason = "test home isolation helpers")]
impl PlakPaths {
    /// Uses the gate's temp root and resolves symlinks before SQLite `NOFOLLOW` opens.
    #[must_use]
    pub fn isolated_tempdir() -> tempfile::TempDir {
        let base = std::env::var_os("PLAK_TEST_TMPDIR")
            .filter(|value| !value.is_empty())
            .map_or_else(std::env::temp_dir, PathBuf::from);
        Self::isolated_tempdir_in(&base)
    }

    fn isolated_tempdir_in(base: &Path) -> tempfile::TempDir {
        fs::create_dir_all(base).expect("plak test home base");
        let physical = fs::canonicalize(base).expect("physical plak test home base");
        tempfile::tempdir_in(physical).expect("isolated tempdir")
    }

    #[must_use]
    pub fn isolated_test_home() -> (tempfile::TempDir, Self) {
        let temporary = Self::isolated_tempdir();
        let paths = Self::explicit(temporary.path().join("home"));
        create_private_directory(paths.home()).expect("private isolated test home");
        (temporary, paths)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn isolated_test_homes_resolve_symlinked_roots_and_remain_distinct() {
        let root = tempfile::tempdir().unwrap();
        let physical = root.path().join("physical");
        fs::create_dir(&physical).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&physical, &alias).unwrap();
        let first = PlakPaths::isolated_tempdir_in(&alias);
        let second = PlakPaths::isolated_tempdir_in(&alias);
        assert_eq!(
            first.path().parent().unwrap(),
            physical.canonicalize().unwrap()
        );
        assert_ne!(first.path(), second.path());
        assert_eq!(first.path(), first.path().canonicalize().unwrap());
    }

    #[test]
    fn resolve_xdg_state_home_matrix_rejects_relative_roots_and_handles_empty_values() {
        assert_eq!(
            resolve_xdg_state_home(Some(OsStr::new("/state")), None).unwrap(),
            PathBuf::from("/state")
        );
        assert_eq!(
            resolve_xdg_state_home(None, Some(OsStr::new("/home/user"))).unwrap(),
            PathBuf::from("/home/user/.local/state")
        );
        assert_eq!(
            resolve_xdg_state_home(Some(OsStr::new("")), Some(OsStr::new("/home/user"))).unwrap(),
            PathBuf::from("/home/user/.local/state")
        );
        assert!(resolve_xdg_state_home(Some(OsStr::new("relative-state")), None).is_err());
        assert!(resolve_xdg_state_home(None, Some(OsStr::new("relative-home"))).is_err());
        assert!(
            resolve_xdg_state_home(Some(OsStr::new("")), Some(OsStr::new("relative-home")))
                .is_err()
        );
        assert!(resolve_xdg_state_home(Some(OsStr::new("")), Some(OsStr::new(""))).is_err());
        assert!(resolve_xdg_state_home(None, None).is_err());
    }

    #[test]
    fn project_discovery_finds_the_nearest_configuration_home() {
        let root = tempfile::tempdir().unwrap();
        let stop = Some(root.path());
        let product_home = root.path().join("elsewhere/home");
        let outer = root.path().join("outer");
        let nested = outer.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        // No `.plak` anywhere: nothing is discovered.
        assert_eq!(
            discover_project_root(&nested, &[product_home.as_path()], stop),
            None
        );
        // An empty `.plak` is not a configuration home.
        fs::create_dir(outer.join(".plak")).unwrap();
        assert_eq!(
            discover_project_root(&nested, &[product_home.as_path()], stop),
            None
        );
        // `agents/` alone makes it a home, and it answers from a deep cwd.
        fs::create_dir(outer.join(".plak/agents")).unwrap();
        assert_eq!(
            discover_project_root(&nested, &[product_home.as_path()], stop),
            Some(outer.canonicalize().unwrap())
        );
        // A closer home wins over an outer one.
        let inner = outer.join("a");
        fs::create_dir_all(inner.join(".plak")).unwrap();
        fs::write(inner.join(".plak/config.toml"), "").unwrap();
        assert_eq!(
            discover_project_root(&nested, &[product_home.as_path()], stop),
            Some(inner.canonicalize().unwrap())
        );
        // From the home directory itself the same root answers.
        assert_eq!(
            discover_project_root(&inner, &[product_home.as_path()], stop),
            Some(inner.canonicalize().unwrap())
        );
    }

    #[test]
    fn workspace_constructor_discovers_parent_overlay_and_excludes_product_home() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".plak");
        fs::create_dir_all(home.join("agents")).unwrap();
        let nested = root.path().join("project/nested");
        fs::create_dir_all(&nested).unwrap();
        // Discovery keeps walking past the temporary root, so a real home in a
        // developer's ancestors may answer; nothing inside the root may.
        let canonical_root = root.path().canonicalize().unwrap();
        let outside_root = |paths: PlakPaths| {
            paths
                .project_root()
                .is_none_or(|found| !found.starts_with(&canonical_root))
        };
        assert!(outside_root(PlakPaths::for_workspace(
            home.clone(),
            root.path()
        )));
        assert!(outside_root(PlakPaths::for_workspace(
            home.clone(),
            &nested
        )));
        let project = root.path().join("project");
        fs::create_dir_all(project.join(".plak/agents")).unwrap();
        assert_eq!(
            PlakPaths::for_workspace(home, &nested).project_root(),
            Some(project.canonicalize().unwrap().as_path())
        );
    }

    #[test]
    fn launch_discovery_never_takes_the_default_home_for_a_project() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let work = user.join("work/repo");
        fs::create_dir_all(user.join(".plak/agents")).unwrap();
        fs::write(user.join(".plak/config.toml"), "").unwrap();
        fs::create_dir_all(&work).unwrap();
        let private = root.path().join("private-home");
        let paths =
            PlakPaths::explicit(private.clone()).discovered_from(Some(user.as_os_str()), &work);
        assert_eq!(paths.project_root(), None);
        // A real project below `$HOME` is still discovered.
        fs::create_dir_all(work.join(".plak/agents")).unwrap();
        let paths = PlakPaths::explicit(private).discovered_from(Some(user.as_os_str()), &work);
        assert_eq!(
            paths.project_root(),
            Some(work.canonicalize().unwrap().as_path())
        );
    }

    #[test]
    fn project_discovery_never_returns_the_product_home() {
        let root = tempfile::tempdir().unwrap();
        let product_home = root.path().join(".plak");
        fs::create_dir_all(product_home.join("agents")).unwrap();
        // From the home's own parent, the product home is excluded even though
        // it is a real configuration home.
        assert_eq!(
            discover_project_root(root.path(), &[product_home.as_path()], Some(root.path())),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_discovery_fails_closed_on_a_symlinked_home() {
        let root = tempfile::tempdir().unwrap();
        let product_home = root.path().join("elsewhere/home");
        let real = root.path().join("real");
        let work = root.path().join("work");
        fs::create_dir_all(real.join(".plak/agents")).unwrap();
        fs::create_dir_all(&work).unwrap();
        std::os::unix::fs::symlink(real.join(".plak"), work.join(".plak")).unwrap();
        // `.plak` itself is a symlink: not a configuration home.
        assert_eq!(
            discover_project_root(&work, &[product_home.as_path()], Some(root.path())),
            None
        );
    }

    #[test]
    fn resolves_home_dot_plak() {
        let paths = PlakPaths::resolve(None, Some(OsStr::new("/home/user"))).unwrap();
        assert_eq!(paths.home(), Path::new("/home/user/.plak"));
        assert_eq!(
            paths.config_file(),
            PathBuf::from("/home/user/.plak/config.toml")
        );
    }

    #[test]
    fn plak_home_overrides() {
        let paths = PlakPaths::resolve(
            Some(OsStr::new("/tmp/plak-home")),
            Some(OsStr::new("/home/user")),
        )
        .unwrap();
        assert_eq!(paths.home(), Path::new("/tmp/plak-home"));
    }

    #[cfg(unix)]
    #[test]
    fn private_state_home_refuses_symlink_and_permissive_home_without_mutation() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let temporary = tempfile::tempdir().unwrap();
        let external = temporary.path().join("external");
        fs::create_dir(&external).unwrap();
        fs::set_permissions(&external, fs::Permissions::from_mode(0o755)).unwrap();
        let sentinel = external.join("sentinel");
        fs::write(&sentinel, "outside").unwrap();
        let linked = temporary.path().join("linked-home");
        symlink(&external, &linked).unwrap();

        assert!(
            PlakPaths::explicit(linked)
                .ensure_private_state_home()
                .is_err()
        );
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "outside");
        assert_eq!(
            fs::metadata(&external).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let permissive = temporary.path().join("permissive-home");
        fs::create_dir(&permissive).unwrap();
        fs::write(permissive.join("config.toml"), "").unwrap();
        fs::set_permissions(&permissive, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            PlakPaths::explicit(permissive.clone())
                .ensure_private_state_home()
                .is_err()
        );
        assert_eq!(
            fs::metadata(permissive).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_state_home_is_created_private_and_layout_refuses_symlinked_child() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let temporary = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(temporary.path().join("home"));
        paths.ensure_private_state_home().unwrap();
        assert_eq!(
            fs::metadata(paths.home()).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let external = temporary.path().join("external");
        fs::create_dir(&external).unwrap();
        let sentinel = external.join("sentinel");
        fs::write(&sentinel, "outside").unwrap();
        symlink(&external, paths.secrets_dir()).unwrap();
        assert!(paths.ensure_layout().is_err());
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "outside");
    }

    #[cfg(unix)]
    #[test]
    fn private_state_home_rejects_symlinked_ancestor() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        fs::set_permissions(external.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let external_home = external.path().join("plak");
        fs::create_dir(&external_home).unwrap();
        fs::set_permissions(&external_home, fs::Permissions::from_mode(0o700)).unwrap();
        let sentinel = external_home.join("sentinel");
        fs::write(&sentinel, b"untouched").unwrap();
        let redirected_parent = root.path().join("redirected");
        symlink(external.path(), &redirected_parent).unwrap();

        let paths = PlakPaths::explicit(redirected_parent.join("plak"));
        assert!(paths.ensure_private_state_home().is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
    }
}
