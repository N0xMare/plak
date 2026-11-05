use super::{FileIdentity, Invocation, launch};
use std::path::{Path, PathBuf};

/// `argv[0]` to hand a re-exec, so the successor records the same invocation
/// (a `PATH` name stays a name) and keeps detecting newer installs.
#[cfg(feature = "tui")]
pub(crate) fn invocation_argv0() -> std::ffi::OsString {
    match &launch().invocation {
        Invocation::Name(name) => name.clone(),
        Invocation::Path(path) => path.clone().into_os_string(),
    }
}

/// The executable the same invocation would start now: `argv[0]` resolved on
/// `PATH` (or as recorded), with symlinks followed to the installed file. A
/// launch by an installer snapshot's own path follows that flavor's current
/// install ([`resolve_installed_path`]).
pub(crate) fn installed_executable() -> Option<PathBuf> {
    let search = std::env::var_os("PATH");
    match &launch().invocation {
        Invocation::Path(path) => resolve_installed_path(path, search.as_deref()),
        Invocation::Name(name) => resolve_installed_name(name, search.as_deref()),
    }
}

/// Resolve a path invocation to the file it names, following symlinks. A path
/// inside an installer snapshot (`<prefix>/lib/plak/{maintainer,release}/<64
/// hex>/<name>`) instead resolves to what `<name>` on `path` resolves to when
/// that is a snapshot of the same flavor directory: snapshots are immutable,
/// so the snapshot's own path never changes, and a newer install of its flavor
/// is what that launch reloads onto. A `PATH` name that resolves elsewhere
/// (another flavor, a development build, nothing) leaves the snapshot itself.
pub(super) fn resolve_installed_path(
    path: &Path,
    search: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let resolved = std::fs::canonicalize(path).ok();
    let launched = resolved.as_deref().unwrap_or(path);
    if let Some(flavor) = snapshot_flavor_dir(launched)
        && let Some(name) = launched.file_name()
        && let Some(current) = resolve_installed_name(name, search)
        && snapshot_flavor_dir(&current) == Some(flavor)
    {
        return Some(current);
    }
    resolved
}

/// The `<prefix>/lib/plak/<flavor>` directory of an installer snapshot's
/// executable, or `None` outside that layout.
fn snapshot_flavor_dir(executable: &Path) -> Option<&Path> {
    let snapshot = executable.parent()?;
    let id = snapshot.file_name()?.to_str()?;
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let flavor = snapshot.parent()?;
    if !matches!(flavor.file_name()?.to_str()?, "maintainer" | "release") {
        return None;
    }
    let product = flavor.parent()?;
    (product.file_name()? == "plak" && product.parent()?.file_name()? == "lib").then_some(flavor)
}

/// Resolve a command name on `path` (`PATH`'s shape) to the installed file,
/// following symlinks.
pub(super) fn resolve_installed_name(
    name: &std::ffi::OsStr,
    path: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let path = path?;
    let candidate = std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))?;
    std::fs::canonicalize(candidate).ok()
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    metadata.is_file()
}

/// Whether `installed` is a different file from the one this process runs.
///
/// A missing launch record or a missing install is not a newer build: the
/// banner stays off until both identities exist and differ.
pub(super) fn installed_build_is_newer(
    running: Option<&FileIdentity>,
    installed: Option<&FileIdentity>,
) -> bool {
    match (running, installed) {
        (Some(running), Some(installed)) => installed != running,
        _ => false,
    }
}

/// Whether the invocation now resolves to a different file than the one this
/// process is running: a new snapshot, or a rebuild in place.
pub(crate) fn newer_build_installed() -> bool {
    let installed = installed_executable().and_then(|path| FileIdentity::of(&path));
    installed_build_is_newer(launch().executable.as_ref(), installed.as_ref())
}
