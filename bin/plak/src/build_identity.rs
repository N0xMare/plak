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

/// Resolve the binary identity off the interactive launch path.
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
            let mut file = open_running_image()?;
            identify_image(&mut file, running_executable(), hash_image)
        })
        .as_deref()
        .unwrap_or("unavailable")
}

fn open_running_image() -> Option<std::fs::File> {
    // /proc names the mapped image even after its launch path is replaced.
    if cfg!(target_os = "linux") {
        std::fs::File::open("/proc/self/exe").ok()
    } else {
        std::fs::File::open(running_executable()?).ok()
    }
}

fn hash_image(file: &mut std::fs::File) -> Option<String> {
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
}

fn image_fields(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt as _;
    let mtime_ns = i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec());
    format!(
        "{} {} {} {mtime_ns}",
        metadata.dev(),
        metadata.ino(),
        metadata.len()
    )
}

fn identify_image(
    file: &mut std::fs::File,
    executable: Option<&Path>,
    hash: impl FnOnce(&mut std::fs::File) -> Option<String>,
) -> Option<String> {
    executable
        .and_then(|path| read_manifest_id(file, path))
        .or_else(|| hash(file))
}

fn read_manifest_id(file: &std::fs::File, executable: &Path) -> Option<String> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::MetadataExt as _;
    let directory = installed_snapshot_directory(executable)?;
    let parent = std::fs::metadata(directory).ok()?;
    if !parent.is_dir() || parent.mode() & 0o022 != 0 {
        return None;
    }
    let manifest = std::fs::File::from(
        rustix::fs::open(
            directory.join("binary-id"),
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .ok()?,
    );
    let metadata = manifest.metadata().ok()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return None;
    }
    let fields = image_fields(&file.metadata().ok()?);
    let mut contents = String::new();
    manifest.take(257).read_to_string(&mut contents).ok()?;
    if contents.len() > 256 {
        return None;
    }
    let words: Vec<_> = contents.split_whitespace().collect();
    if words.len() != 5
        || words[0].len() != 12
        || !words[0]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || words[1..].join(" ") != fields
    {
        return None;
    }
    Some(words[0].to_owned())
}

fn installed_snapshot_directory(executable: &Path) -> Option<&Path> {
    let directory = executable.parent()?;
    let id = directory.file_name()?.to_str()?;
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let flavor = directory.parent()?;
    if !matches!(flavor.file_name()?.to_str()?, "maintainer" | "release") {
        return None;
    }
    let plak = flavor.parent()?;
    let lib = plak.parent()?;
    let local = lib.parent()?;
    (plak.file_name()? == "plak" && lib.file_name()? == "lib" && local.file_name()? == ".local")
        .then_some(directory)
}

/// Install-time only: hash and stat the same open image before snapshot rename.
pub(crate) fn write_binary_manifest(directory: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut file =
        open_running_image().ok_or_else(|| std::io::Error::other("running image unavailable"))?;
    let id = hash_image(&mut file)
        .ok_or_else(|| std::io::Error::other("could not hash running image"))?;
    let contents = format!("{id} {}\n", image_fields(&file.metadata()?));
    let path = directory.join("binary-id");
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::io::Write::write_all(&mut output, contents.as_bytes())?;
    output.set_permissions(std::fs::Permissions::from_mode(0o444))
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
    fn manifest_requires_installer_layout_and_safe_file_and_directory() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let root = tempfile::tempdir().unwrap();
        let snapshot = root
            .path()
            .join("target/.local/lib/plak/release")
            .join("b".repeat(64));
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o755)).unwrap();
        let executable = snapshot.join("plak");
        std::fs::write(&executable, "image").unwrap();
        let mut image = std::fs::File::open(&executable).unwrap();
        let manifest = snapshot.join("binary-id");
        let contents = format!("abcdef012345 {}", image_fields(&image.metadata().unwrap()));
        std::fs::write(&manifest, &contents).unwrap();
        assert_eq!(
            read_manifest_id(&image, &executable).as_deref(),
            Some("abcdef012345")
        );
        for mode in [0o664, 0o646] {
            std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(read_manifest_id(&image, &executable), None);
        }
        std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(read_manifest_id(&image, &executable), None);
        std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(0o755)).unwrap();
        let moved = snapshot.join("other");
        std::fs::rename(&manifest, &moved).unwrap();
        symlink(&moved, &manifest).unwrap();
        assert_eq!(read_manifest_id(&image, &executable), None);
        std::fs::remove_file(&manifest).unwrap();
        // rustix has no mkfifo on Apple targets; mkfifo(1) exists on both.
        let made = std::process::Command::new("mkfifo")
            .args(["-m", "600"])
            .arg(&manifest)
            .status()
            .unwrap();
        assert!(made.success());
        assert_eq!(read_manifest_id(&image, &executable), None);
        assert_eq!(
            identify_image(&mut image, None, hash_image).as_deref(),
            Some("6105d6cc76af")
        );
        for path in [
            "/tmp/plak",
            "/home/me/target/debug/plak",
            "/home/me/.local/lib/plak/other/aaaaaaaa/plak",
        ] {
            assert_eq!(installed_snapshot_directory(Path::new(path)), None);
        }
    }

    #[test]
    fn manifest_match_skips_hash_and_all_other_cases_hash() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = directory
            .path()
            .join(".local/lib/plak/maintainer")
            .join("a".repeat(64));
        std::fs::create_dir_all(&snapshot).unwrap();
        let executable = snapshot.join("plak");
        std::fs::write(&executable, "image").unwrap();
        let metadata = std::fs::metadata(&executable).unwrap();
        let fields = image_fields(&metadata);
        let valid = format!("abcdef012345 {fields}\n");
        let mut cases = vec![
            (None, true),
            (Some("malformed".to_owned()), true),
            (Some(format!("zzzzzzzzzzzz {fields}")), true),
            (Some(format!("{valid}extra")), true),
            (Some(format!("{valid}{}", " ".repeat(257))), true),
            (Some(valid.clone()), false),
        ];
        for index in 1..5 {
            let mut words: Vec<_> = valid.split_whitespace().map(str::to_owned).collect();
            words[index] = "0".to_owned();
            cases.push((Some(words.join(" ")), true));
        }
        for (manifest, should_hash) in cases {
            let path = snapshot.join("binary-id");
            if let Some(contents) = manifest {
                std::fs::write(&path, contents).unwrap();
            } else {
                let _ = std::fs::remove_file(&path);
            }
            let calls = std::cell::Cell::new(0);
            let mut image = std::fs::File::open(&executable).unwrap();
            let id = identify_image(&mut image, Some(&executable), |file| {
                calls.set(calls.get() + 1);
                hash_image(file)
            })
            .unwrap();
            assert_eq!(calls.get(), usize::from(should_hash));
            assert_eq!(
                id,
                if should_hash {
                    "6105d6cc76af"
                } else {
                    "abcdef012345"
                }
            );
        }
        let target = directory.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("binary-id"), valid).unwrap();
        let mut image = std::fs::File::open(&executable).unwrap();
        let calls = std::cell::Cell::new(0);
        let _ = identify_image(&mut image, Some(&target.join("plak")), |file| {
            calls.set(calls.get() + 1);
            hash_image(file)
        });
        assert_eq!(calls.get(), 1);
    }

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
