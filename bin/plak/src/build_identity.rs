//! Product build identity, independent of the working directory's Git state.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest as _, Sha256};

pub(crate) const FLAVOR: &str = match (cfg!(feature = "maintainer"), cfg!(debug_assertions)) {
    (true, true) => "maintainer debug",
    (true, false) => "maintainer release",
    (false, true) => "debug",
    (false, false) => "release",
};

/// How this process was invoked and which file it is running, recorded once at
/// startup so a later rebuild cannot change the answer.
struct Launch {
    /// `argv[0]` as a command name searched on `PATH`, or as a path made
    /// absolute against the launch directory.
    invocation: Invocation,
    executable: Option<FileIdentity>,
}

enum Invocation {
    Name(OsString),
    Path(PathBuf),
}

#[derive(Clone, PartialEq, Eq)]
struct FileIdentity {
    path: PathBuf,
    #[cfg(unix)]
    inode: (u64, u64),
    modified: Option<std::time::SystemTime>,
}

impl FileIdentity {
    fn of(path: &Path) -> Option<Self> {
        let path = std::fs::canonicalize(path).ok()?;
        let metadata = std::fs::metadata(&path).ok()?;
        Some(Self {
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt as _;
                (metadata.dev(), metadata.ino())
            },
            modified: metadata.modified().ok(),
            path,
        })
    }
}

static LAUNCH: OnceLock<Launch> = OnceLock::new();
static BINARY_ID: OnceLock<Option<String>> = OnceLock::new();
#[cfg(test)]
// Hold background work so cache assertions distinguish caller-side hashing.
static BACKGROUND_IDENTITY_TEST_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hash the executable off the interactive launch path.
pub(crate) fn prepare_binary_id(startup_command: Option<&'static str>) {
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        if std::thread::Builder::new()
            .name("plak-binary-id".to_owned())
            .spawn(move || {
                #[cfg(test)]
                let _gate = BACKGROUND_IDENTITY_TEST_GATE.lock().ok();
                let binary = binary_id();
                if let Some(command) = startup_command {
                    crate::log_start(binary, command);
                }
            })
            .is_err()
        {
            let _ = BINARY_ID.set(None);
        }
    });
}

/// Read the completed identity without waiting for an in-progress hash.
pub(crate) fn ready_binary_id() -> Option<&'static str> {
    BINARY_ID
        .get()
        .map(|id| id.as_deref().unwrap_or("unavailable"))
}

/// Record the invocation and the running executable. Call once, first thing.
pub(crate) fn record_launch(argv0: Option<&std::ffi::OsStr>) {
    LAUNCH.get_or_init(|| {
        let invocation = match argv0 {
            Some(arg) if Path::new(arg).components().count() > 1 => Invocation::Path(
                std::env::current_dir()
                    .map(|cwd| cwd.join(arg))
                    .unwrap_or_else(|_| PathBuf::from(arg)),
            ),
            Some(arg) if !arg.is_empty() => Invocation::Name(arg.to_owned()),
            _ => Invocation::Name("plak".into()),
        };
        let executable = std::env::current_exe()
            .ok()
            .and_then(|path| FileIdentity::of(&path));
        Launch {
            invocation,
            executable,
        }
    });
}

/// Without a recorded `argv[0]` (an embedding caller or a test) the
/// invocation is the running executable's own path, so only a rebuild in place
/// of that file reads as a newer build.
fn launch() -> &'static Launch {
    LAUNCH.get_or_init(|| {
        let current = std::env::current_exe().ok();
        Launch {
            executable: current.as_deref().and_then(FileIdentity::of),
            invocation: Invocation::Path(current.unwrap_or_else(|| PathBuf::from("plak"))),
        }
    })
}

/// `argv[0]` to hand a re-exec, so the successor records the same invocation
/// (a `PATH` name stays a name) and keeps detecting newer installs.
pub(crate) fn invocation_argv0() -> OsString {
    match &launch().invocation {
        Invocation::Name(name) => name.clone(),
        Invocation::Path(path) => path.clone().into_os_string(),
    }
}

/// The canonical file this process runs. Sidecars are found beside it, so a
/// session started from a snapshot keeps that snapshot's sidecars even after
/// the `PATH` symlinks move to a newer build.
pub(crate) fn running_executable() -> Option<&'static Path> {
    launch()
        .executable
        .as_ref()
        .map(|identity| identity.path.as_path())
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

/// The first twelve SHA-256 digits of this executable, not a source revision.
/// Installed maintainer snapshots are immutable; later checkout/build changes
/// therefore cannot make an old running process claim a newer build identity.
pub(crate) fn binary_id() -> &'static str {
    BINARY_ID
        .get_or_init(|| {
            // Linux names the mapped image itself, which a rebuild in place cannot
            // replace; elsewhere the canonical path recorded at launch is used.
            let mut file = if cfg!(target_os = "linux") {
                std::fs::File::open("/proc/self/exe").ok()
            } else {
                None
            }
            .or_else(|| std::fs::File::open(&launch().executable.as_ref()?.path).ok())?;
            let mut hash = Sha256::new();
            let mut bytes = [0u8; 8 * 1024];
            loop {
                let count = file.read(&mut bytes).ok()?;
                if count == 0 {
                    break;
                }
                hash.update(&bytes[..count]);
            }
            Some(
                hash.finalize()
                    .iter()
                    .take(6)
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            )
        })
        .as_deref()
        .unwrap_or("unavailable")
}

pub(crate) fn version_label() -> String {
    let hooks = if cfg!(feature = "test-hooks") {
        "; test-hooks"
    } else {
        ""
    };
    format!(
        "{} ({FLAVOR}; binary {}{hooks})",
        plakous::VERSION,
        binary_id()
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn run_and_agent_startup_leave_the_identity_cache_cold_without_waiting_for_the_worker() {
        let gate = BACKGROUND_IDENTITY_TEST_GATE.lock().unwrap();
        assert_eq!(ready_binary_id(), None);
        for arguments in [
            vec!["run", "--account", "grok", "--model", "m", "--", "hello"],
            vec![
                "run",
                "--session",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "--",
                "hello",
            ],
            vec!["agent"],
        ] {
            let command = crate::parse(arguments.into_iter().map(OsString::from)).unwrap();
            crate::log_command_start(&command);
            assert_eq!(
                ready_binary_id(),
                None,
                "startup must not hash on the calling thread"
            );
        }
        drop(gate);
    }

    #[test]
    fn an_unrecorded_launch_is_its_own_installed_build() {
        assert!(!newer_build_installed());
        assert_eq!(
            installed_executable(),
            std::env::current_exe()
                .ok()
                .and_then(|path| std::fs::canonicalize(path).ok())
        );
    }

    #[test]
    fn a_replaced_file_is_a_different_build() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plak");
        std::fs::write(&path, b"old").unwrap();
        let old = FileIdentity::of(&path).unwrap();
        assert!(FileIdentity::of(&path) == Some(old.clone()));
        let staged = dir.path().join("plak.new");
        std::fs::write(&staged, b"new").unwrap();
        std::fs::rename(&staged, &path).unwrap();
        assert!(FileIdentity::of(&path) != Some(old));
    }
}
