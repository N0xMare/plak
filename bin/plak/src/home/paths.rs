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
/// root whose `.plak/agents/` may override user templates: the product
/// context of one workspace. The workspace is always explicit. The CLI edge
/// resolves the launch directory once (`shell::home`); below it nothing reads
/// the process working directory, so one process can hold contexts for many
/// workspaces. Every context of a home is derived from it through one
/// discovery ([`Self::for_workspace`]), so a workspace's project root never
/// depends on which path built its context. A home the environment resolved
/// ([`Self::resolve`]) knows the default `$HOME/.plak` too; an
/// [`Self::explicit`] home (a test's, doctor's file listing) knows only
/// itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlakPaths {
    home: PathBuf,
    /// The default `$HOME/.plak` when the environment named a `HOME`: a
    /// product home even when `PLAK_HOME` points elsewhere, so never a
    /// project overlay.
    default_home: Option<PathBuf>,
    project_root: Option<PathBuf>,
    /// The workspace this context was resolved for, when it has one: the
    /// launch directory at the CLI edge, or a session's workspace.
    workspace: Option<PathBuf>,
}

impl PlakPaths {
    /// The context of `home` alone, outside the environment: it knows no
    /// default `$HOME/.plak`, so its [`Self::for_workspace`] excludes only
    /// `home` from discovery.
    #[must_use]
    pub fn explicit(home: PathBuf) -> Self {
        Self {
            home,
            default_home: None,
            project_root: None,
            workspace: None,
        }
    }

    /// The same home's context for `workspace`: the nearest ancestor carrying
    /// a project `.plak` home is its project override root. An ancestor is
    /// not a project home merely because `.plak` exists: it must hold
    /// `config.toml` or an `agents` directory, and this product home is
    /// never one, nor, for a home the environment resolved, the default
    /// `$HOME/.plak`.
    #[must_use]
    pub fn for_workspace(&self, workspace: &Path) -> Self {
        let mut product_homes = vec![self.home.as_path()];
        product_homes.extend(self.default_home.as_deref());
        Self {
            home: self.home.clone(),
            default_home: self.default_home.clone(),
            project_root: discover_project_root(workspace, &product_homes, None),
            workspace: Some(workspace.to_path_buf()),
        }
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
        let user_home = home.filter(|v| !v.is_empty()).map(PathBuf::from);
        let default_home = user_home
            .as_deref()
            .filter(|home| home.is_absolute())
            .map(|home| home.join(".plak"));
        if let Some(explicit) = plak_home.filter(|v| !v.is_empty()) {
            let path = PathBuf::from(explicit);
            if !path.is_absolute() {
                return Err("PLAK_HOME must be an absolute path".to_owned());
            }
            return Ok(Self {
                home: path,
                default_home,
                project_root: None,
                workspace: None,
            });
        }
        let home_path = user_home.ok_or_else(|| "HOME is unavailable".to_owned())?;
        if !home_path.is_absolute() {
            return Err("HOME must be an absolute path".to_owned());
        }
        Ok(Self {
            home: home_path.join(".plak"),
            default_home,
            project_root: None,
            workspace: None,
        })
    }

    /// `$PLAK_HOME` or `$HOME/.plak`, with no workspace: for the commands
    /// that touch only the home (keys, secrets).
    pub fn home_from_env() -> Result<Self, String> {
        Self::from_env_in(None)
    }

    /// `$PLAK_HOME` or `$HOME/.plak`, as the context of the workspace
    /// `launch` (the CLI edge's launch directory; `None` when it is
    /// unavailable gives a context with no workspace).
    pub fn from_env_in(launch: Option<&Path>) -> Result<Self, String> {
        let plak_home = std::env::var_os("PLAK_HOME");
        let home = std::env::var_os("HOME");
        let paths = Self::resolve(plak_home.as_deref(), home.as_deref())?;
        Ok(match launch {
            Some(cwd) => paths.for_workspace(cwd),
            None => paths,
        })
    }

    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    #[must_use]
    pub fn from_home(home: PathBuf) -> Self {
        Self::explicit(home)
    }

    /// The workspace this context was resolved for.
    ///
    /// # Errors
    ///
    /// `cwd unavailable` when it has none: the launch directory could not
    /// be read, or the context names only the home.
    pub fn workspace(&self) -> Result<&Path, String> {
        self.workspace
            .as_deref()
            .ok_or_else(|| "cwd unavailable".to_owned())
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
        self.secret_shelf_dir(SecretShelf::Named)
    }

    /// The directory of one shelf of named secrets.
    #[must_use]
    pub fn secret_shelf_dir(&self, kind: SecretShelf) -> PathBuf {
        self.secrets_dir().join(kind.directory())
    }

    /// `secrets/mcp-oauth/<id>/credential.json`: the OAuth login of MCP
    /// server `<id>`, a private credential record in a directory of its own.
    /// The caller has validated `server`.
    #[must_use]
    pub fn mcp_login_file(&self, server: &str) -> PathBuf {
        self.secrets_dir()
            .join("mcp-oauth")
            .join(server)
            .join("credential.json")
    }

    /// One named secret's file. The caller has validated `name`.
    #[must_use]
    pub fn named_secret_file(&self, name: &str) -> PathBuf {
        self.secret_shelf_file(SecretShelf::Named, name)
    }

    /// One secret's file on `shelf`. The caller has validated `name`.
    #[must_use]
    pub fn secret_shelf_file(&self, kind: SecretShelf, name: &str) -> PathBuf {
        self.secret_shelf_dir(kind).join(format!("{name}.secret"))
    }

    /// Creates the shelf's directory 0700 under the private layout, and
    /// refuses one that exists but is not private, exactly as the layout
    /// refuses the secrets directory itself.
    pub fn ensure_secret_shelf_dir(&self, kind: SecretShelf) -> Result<(), String> {
        self.ensure_layout()?;
        ensure_private_child(&self.secret_shelf_dir(kind), kind.label())
    }

    /// Creates the directory of MCP server `server`'s login record,
    /// `secrets/mcp-oauth/<server>/`, and its parent 0700 under the private
    /// layout, refusing either when it exists but is not private. The caller
    /// has validated `server`.
    pub fn ensure_mcp_login_dir(&self, server: &str) -> Result<(), String> {
        self.ensure_layout()?;
        let login = self.mcp_login_file(server);
        let dir = login
            .parent()
            .ok_or_else(|| "the MCP login directory has no parent".to_owned())?;
        let parent = dir
            .parent()
            .ok_or_else(|| "the MCP login directory has no parent".to_owned())?;
        ensure_private_child(parent, "MCP logins directory")?;
        ensure_private_child(dir, "MCP login directory")
    }

    /// Whether the shelf's directory exists, verified private along with the
    /// secrets directory above it. Creates nothing.
    pub fn verify_secret_shelf_dir(&self, kind: SecretShelf) -> Result<bool, String> {
        let dir = self.secret_shelf_dir(kind);
        match fs::symlink_metadata(&dir) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err(format!("could not inspect the {}", kind.label())),
        }
        require_private_directory(&self.secrets_dir(), "Plak secrets directory")?;
        require_private_directory(&dir, &format!("Plak {}", kind.label()))?;
        Ok(true)
    }

    /// Transient process state: announcements and requests between live
    /// processes of this home. Never session authority, never a build's state,
    /// so legacy-state reports and prunes never see it.
    #[must_use]
    pub fn run_dir(&self) -> PathBuf {
        self.home.join("run")
    }

    /// `plak host`'s control sockets, one per state key (spec 04 §4.7):
    /// a 0700 directory under a 0700 `host/`. Not state.
    #[must_use]
    pub fn host_run_dir(&self) -> PathBuf {
        self.home.join("host").join("run")
    }

    /// The control socket of this build's `plak host`.
    #[must_use]
    pub fn host_socket(&self) -> PathBuf {
        self.host_run_dir()
            .join(format!("{}.sock", crate::home::state::state_key()))
    }

    /// The lock this build's `plak host` holds for its whole life: one host
    /// per home and state key.
    #[must_use]
    pub fn host_lock(&self) -> PathBuf {
        self.state_dir().join("host.lock")
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

/// Creates `dir` 0700 when it is missing, then refuses it unless private.
/// One shelf of named secrets: the host's, which `[mcp_servers.<id>]` names
/// for a server on the host, and the runner's, which a runner reads for a
/// server placed on it. A name on one shelf never resolves from the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretShelf {
    /// `secrets/named/` (`plak secret set`).
    Named,
    /// `secrets/runner/` (`plak runner secret set`).
    Runner,
    /// `secrets/web/`: the web endpoint's cookie key, which it makes itself.
    Web,
}

impl SecretShelf {
    const fn directory(self) -> &'static str {
        match self {
            Self::Named => "named",
            Self::Runner => "runner",
            Self::Web => "web",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Named => "named secrets directory",
            Self::Runner => "runner secrets directory",
            Self::Web => "web secrets directory",
        }
    }
}

fn ensure_private_child(dir: &Path, label: &str) -> Result<(), String> {
    match fs::symlink_metadata(dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_directory(dir)
                .or_else(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        Ok(())
                    } else {
                        Err(error)
                    }
                })
                .map_err(|_| format!("could not create the private {label}"))?;
        }
        Err(_) => return Err(format!("could not inspect the {label}")),
    }
    require_private_directory(dir, &format!("Plak {label}"))
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
/// builds and `$HOME/.local/lib/plak/release` for `just install release`.
#[must_use]
pub fn installed_snapshot_roots() -> Vec<PathBuf> {
    user_home()
        .map(|home| {
            let lib = home.join(".local/lib/plak");
            vec![lib.join("maintainer"), lib.join("release")]
        })
        .unwrap_or_default()
}

/// The operator's login home: display-path shortening and the user skills
/// root (`~/.agents/skills`); never a product-state default.
pub(crate) fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
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
mod tests;
