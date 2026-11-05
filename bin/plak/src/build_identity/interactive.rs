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
/// `PATH` (or as recorded), with symlinks followed to the installed file.
pub(crate) fn installed_executable() -> Option<PathBuf> {
    match &launch().invocation {
        Invocation::Path(path) => std::fs::canonicalize(path).ok(),
        Invocation::Name(name) => resolve_installed_name(name, std::env::var_os("PATH").as_deref()),
    }
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
