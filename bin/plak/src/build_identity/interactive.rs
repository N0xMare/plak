use super::{FileIdentity, Invocation, launch};
#[cfg(feature = "tui")]
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// `argv[0]` to hand a re-exec, so the successor records the same invocation
/// (a `PATH` name stays a name) and keeps detecting newer installs.
#[cfg(feature = "tui")]
pub(crate) fn invocation_argv0() -> OsString {
    match &launch().invocation {
        Invocation::Name(name) => name.clone(),
        Invocation::Path(path) => path.clone().into_os_string(),
    }
}

/// The executable the same invocation would start now: `argv[0]` resolved on
/// `PATH` (or as recorded), with symlinks followed to the installed file.
pub(crate) fn installed_executable() -> Option<PathBuf> {
    let candidate = match &launch().invocation {
        Invocation::Path(path) => path.clone(),
        Invocation::Name(name) => std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(name))
            .find(|path| is_executable_file(path))?,
    };
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

/// Whether the invocation now resolves to a different file than the one this
/// process is running: a new snapshot, or a rebuild in place.
pub(crate) fn newer_build_installed() -> bool {
    let Some(running) = &launch().executable else {
        return false;
    };
    installed_executable()
        .and_then(|path| FileIdentity::of(&path))
        .is_some_and(|installed| installed != *running)
}
