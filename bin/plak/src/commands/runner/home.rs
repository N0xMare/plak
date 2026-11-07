//! The runner home on a runner machine (spec 02 §2.1): `$PLAK_HOME/runner/`
//! with its `runner.toml`, its stable runner id, the owner floors, the
//! private socket directory and the detached daemon's log.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use plakous_hosted::error::{CliFailure, CliResult};

use crate::home::paths::PlakPaths;

/// How long a runner with nothing to keep stays up after its last host.
const DEFAULT_IDLE_EXIT_SECONDS: u64 = 300;

/// `runner.toml`: what this machine's runner may serve and how.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunnerToml {
    /// The workspace roots hosts may name. Consent, not containment: a
    /// workspace outside every root is refused.
    #[serde(default)]
    pub(crate) roots: Vec<PathBuf>,
    /// Where copies carried from another runner are kept, one mirror per
    /// tree; unset, this runner takes none.
    pub(crate) copies: Option<PathBuf>,
    /// The `PATH` the runner's processes start with; unset, a login shell
    /// is asked once when the daemon starts.
    pub(crate) path: Option<String>,
    /// The `HOME` the runner's processes start with; unset, the user's.
    pub(crate) home: Option<String>,
    /// The host identities paired with this runner (`plak runner allow`).
    #[serde(default)]
    pub(crate) hosts: Vec<String>,
    /// How long the daemon stays up with nothing to keep, in seconds.
    pub(crate) idle_exit_seconds: Option<u64>,
}

impl RunnerToml {
    pub(crate) fn idle_exit(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_exit_seconds.unwrap_or(DEFAULT_IDLE_EXIT_SECONDS))
    }
}

/// The runner home of one product home.
#[derive(Clone, Debug)]
pub(crate) struct RunnerHome {
    root: PathBuf,
}

impl RunnerHome {
    pub(crate) fn of(paths: &PlakPaths) -> Self {
        Self {
            root: paths.home().join("runner"),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn config_path(&self) -> PathBuf {
        self.root.join("runner.toml")
    }

    pub(crate) fn run_directory(&self) -> PathBuf {
        self.build_directory().join("run")
    }

    pub(crate) fn floors_path(&self) -> PathBuf {
        self.build_directory().join("owners.json")
    }

    pub(crate) fn log_path(&self) -> PathBuf {
        self.build_directory().join("serve.log")
    }

    pub(crate) fn build_directory(&self) -> PathBuf {
        self.root
            .join("builds")
            .join(plakous::runner::daemon::triple_key(&super::version()))
    }

    /// Pins the full triple before opening any mutable state. A shortened
    /// filesystem-key collision fails closed rather than sharing owner floors.
    pub(crate) fn prepare_build(&self) -> CliResult<()> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let directory = self.build_directory();
        private_directory(&directory)?;
        let path = directory.join("version.json");
        let version = super::version();
        let identity_lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(directory.join("version.lock"))
            .map_err(|error| CliFailure::new(error.to_string()))?;
        identity_lock
            .lock()
            .map_err(|error| CliFailure::new(error.to_string()))?;
        match std::fs::read(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let bytes = serde_json::to_vec(&version)
                    .map_err(|error| CliFailure::new(error.to_string()))?;
                write_private(&path, &bytes)?;
            }
            Ok(bytes) => {
                let recorded =
                    serde_json::from_slice::<plakous::runner::protocol::VersionTriple>(&bytes).ok();
                if recorded.as_ref() != Some(&version) {
                    return Err(CliFailure::new(
                        "runner build directory belongs to another triple or has incomplete identity; inspect and drain that build before repairing its state",
                    ));
                }
            }
            Err(error) => {
                return Err(CliFailure::new(format!(
                    "runner build identity could not be recorded: {error}"
                )));
            }
        }
        Ok(())
    }

    /// `runner.toml`, or the empty configuration when there is none.
    pub(crate) fn config(&self) -> CliResult<RunnerToml> {
        let path = self.config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RunnerToml::default());
            }
            Err(error) => {
                return Err(CliFailure::new(format!(
                    "{} could not be read: {error}",
                    path.display()
                )));
            }
        };
        let config: RunnerToml = toml::from_str(&text).map_err(|error| {
            CliFailure::new(format!(
                "{} is invalid: {}",
                path.display(),
                error.message()
            ))
        })?;
        for root in &config.roots {
            if !root.is_absolute() {
                return Err(CliFailure::new(format!(
                    "{}: root {} is not an absolute path",
                    path.display(),
                    root.display()
                )));
            }
        }
        for host in &config.hosts {
            validate_host_id(host)?;
        }
        Ok(config)
    }

    /// This runner's stable identity, created on first use.
    pub(crate) fn runner_id(&self) -> CliResult<String> {
        read_or_create_id(&self.root, "runner-id", "r-")
    }

    /// Adds `host_id` to the allowlist in `runner.toml`, keeping the rest of
    /// the file as written. Returns whether it was new.
    pub(crate) fn allow(&self, host_id: &str) -> CliResult<bool> {
        validate_host_id(host_id)?;
        private_directory(&self.root)?;
        let path = self.config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(CliFailure::new(format!(
                    "{} could not be read: {error}",
                    path.display()
                )));
            }
        };
        let mut document: toml_edit::DocumentMut = text
            .parse()
            .map_err(|error| CliFailure::new(format!("{} is invalid: {error}", path.display())))?;
        let hosts = document
            .entry("hosts")
            .or_insert_with(|| toml_edit::value(toml_edit::Array::new()))
            .as_array_mut()
            .ok_or_else(|| CliFailure::new(format!("{}: hosts is not an array", path.display())))?;
        if hosts.iter().any(|value| value.as_str() == Some(host_id)) {
            return Ok(false);
        }
        hosts.push(host_id);
        write_private(&path, document.to_string().as_bytes())?;
        // The file must still load.
        self.config()?;
        Ok(true)
    }
}

/// This home's home-local runner (spec 02 §2.1), apart from the machine's
/// runner for paired hosts: `$PLAK_HOME/host/runner/v<N>/`, one per runner
/// protocol version, so its owner floors never meet a build of another
/// protocol. It holds the runner id, the owner floors, the private socket
/// directory and an empty default workspace.
#[derive(Clone, Debug)]
pub(crate) struct LocalRunnerHome {
    root: PathBuf,
}

impl LocalRunnerHome {
    pub(crate) fn of(paths: &PlakPaths) -> Self {
        Self {
            root: paths
                .home()
                .join("host")
                .join("runner")
                .join(format!("v{}", super::version().protocol)),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The socket, named by the directory's protocol version alone: every
    /// build of that protocol reaches the same daemon.
    pub(crate) fn socket_path(&self) -> PathBuf {
        self.root.join("run").join("runner.sock")
    }

    pub(crate) fn floors_path(&self) -> PathBuf {
        self.root.join("owners.json")
    }

    /// Where a request that names no workspace is answered: an empty
    /// directory.
    pub(crate) fn empty_directory(&self) -> PathBuf {
        self.root.join("empty")
    }

    /// The home-local runner's stable identity, created on first use.
    pub(crate) fn runner_id(&self) -> CliResult<String> {
        read_or_create_id(&self.root, "runner-id", "r-")
    }
}

/// This home's stable host identity, created on first use: a runner pairs
/// with it, and every process of the home presents it. Owner leases are per
/// process instance ([`super::bind`]).
pub(crate) fn host_id(paths: &PlakPaths) -> CliResult<String> {
    let host_id = read_or_create_id(paths.home(), "host-id", "")?;
    // It is sent to runners and interpolated into a remote command line.
    validate_host_id(&host_id)?;
    Ok(host_id)
}

/// A host identity as `plak runner allow` takes it: a lowercase uuid.
pub(crate) fn validate_host_id(host_id: &str) -> CliResult<()> {
    match uuid::Uuid::parse_str(host_id) {
        Ok(parsed) if parsed.hyphenated().to_string() == host_id => Ok(()),
        _ => Err(CliFailure::new(format!(
            "host id {host_id:?} is not a lowercase uuid"
        ))),
    }
}

fn read_or_create_id(directory: &Path, name: &str, prefix: &str) -> CliResult<String> {
    let path = directory.join(name);
    if let Some(id) = read_id(&path)? {
        return Ok(id);
    }
    private_directory(directory)?;
    let id = format!("{prefix}{}", uuid::Uuid::new_v4().hyphenated());
    // Written whole and synced under a name no other first use shares, then
    // linked into place, which fails when a concurrent first use won: a crash
    // leaves no partial id, and every racer returns the id the file holds.
    let staged = directory.join(format!(".{name}.{}", uuid::Uuid::new_v4().simple()));
    let linked = stage_and_link(&staged, &path, format!("{id}\n").as_bytes());
    let _ = std::fs::remove_file(&staged);
    let created = |error: std::io::Error| {
        CliFailure::new(format!("{} could not be created: {error}", path.display()))
    };
    match linked {
        Ok(()) => {
            std::fs::File::open(directory)
                .and_then(|directory| directory.sync_all())
                .map_err(created)?;
            Ok(id)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            read_id(&path)?.ok_or_else(|| CliFailure::new(format!("{} vanished", path.display())))
        }
        Err(error) => Err(created(error)),
    }
}

/// The id `path` holds, or `None` when there is no file yet.
fn read_id(path: &Path) -> CliResult<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let id = text.trim().to_owned();
            if id.is_empty() {
                return Err(CliFailure::new(format!("{} is empty", path.display())));
            }
            Ok(Some(id))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CliFailure::new(format!(
            "{} could not be read: {error}",
            path.display()
        ))),
    }
}

/// Creates `staged` (mode 0600, never an existing file), writes and syncs
/// `bytes`, then links it as `path`.
fn stage_and_link(staged: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(staged)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::hard_link(staged, path)
}

/// Creates `directory` with mode 0700, or closes an existing one of this
/// user's to everyone else.
pub(crate) fn private_directory(directory: &Path) -> CliResult<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    let failed = |error: std::io::Error| {
        CliFailure::new(format!(
            "{} could not be made private: {error}",
            directory.display()
        ))
    };
    builder.create(directory).map_err(failed)?;
    let metadata = std::fs::symlink_metadata(directory).map_err(failed)?;
    if !metadata.is_dir() {
        return Err(CliFailure::new(format!(
            "{} is not a directory",
            directory.display()
        )));
    }
    // The owner is who may speak on a socket there: never another user.
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(CliFailure::new(format!(
            "{} belongs to another user",
            directory.display()
        )));
    }
    if metadata.mode() & 0o077 != 0 {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .map_err(failed)?;
    }
    Ok(())
}

/// Writes `bytes` to `path` (mode 0600) through a temporary file in the
/// same directory.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> CliResult<()> {
    let temporary = path.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()
    };
    write().map_err(|error| {
        CliFailure::new(format!("{} could not be written: {error}", path.display()))
    })
}

/// Whether `root` lies inside `home` or holds it, after resolving symlinks
/// as far as the paths exist.
pub(crate) fn overlaps_home(root: &Path, home: &Path) -> bool {
    let (root, home) = (resolved(root), resolved(home));
    root.starts_with(&home) || home.starts_with(&root)
}

/// Whether a workspace probe may answer about `candidate`: an absolute path
/// without `..` that, resolved as far as it exists, lies under one of
/// `roots` and outside `excluded` (the product home).
pub(crate) fn admitted_probe_path(roots: &[PathBuf], excluded: &Path, candidate: &Path) -> bool {
    if !candidate.is_absolute()
        || candidate
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }
    let candidate = resolved(candidate);
    !candidate.starts_with(resolved(excluded))
        && roots
            .iter()
            .any(|root| candidate.starts_with(resolved(root)))
}

/// `path` with its longest existing ancestor resolved and the rest kept.
fn resolved(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            return rest
                .iter()
                .rev()
                .fold(canonical, |path, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Whether `candidate` lies under one of `roots` and neither inside nor
/// around `excluded` (the product home), after resolving symlinks.
pub(crate) fn admitted_workspace(roots: &[PathBuf], excluded: &Path, candidate: &Path) -> bool {
    let Ok(candidate) = candidate.canonicalize() else {
        return false;
    };
    if let Ok(excluded) = excluded.canonicalize()
        && (candidate.starts_with(&excluded) || excluded.starts_with(&candidate))
    {
        return false;
    }
    roots.iter().any(|root| {
        root.canonicalize()
            .is_ok_and(|root| candidate.starts_with(root))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
