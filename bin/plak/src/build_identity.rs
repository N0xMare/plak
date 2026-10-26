//! Product build identity, independent of the working directory's Git state.

use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest as _, Sha256};

pub(crate) const JJ_ENABLED: bool = cfg!(feature = "jj");

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
    #[cfg_attr(
        not(feature = "tui"),
        allow(
            dead_code,
            reason = "launch identity records invocation even without interactive reload"
        )
    )]
    invocation: Invocation,
    executable: Option<FileIdentity>,
}

#[cfg_attr(
    not(feature = "tui"),
    allow(
        dead_code,
        reason = "launch identity records invocation even without interactive reload"
    )
)]
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
                // The banner shows the identity once it is ready.
                crate::ui_wake::wake();
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
#[cfg(feature = "tui")]
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
#[cfg(any(test, feature = "tui"))]
pub(crate) fn installed_executable() -> Option<PathBuf> {
    let candidate = match &launch().invocation {
        Invocation::Path(path) => path.clone(),
        Invocation::Name(name) => std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(name))
            .find(|path| is_executable_file(path))?,
    };
    std::fs::canonicalize(candidate).ok()
}

#[cfg(any(test, feature = "tui"))]
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
#[cfg(any(test, feature = "tui"))]
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
            let home = identity_cache_home();
            let cache = home.as_deref().map(|home| IdentityCache {
                home,
                writable: IDENTITY_CACHE_WRITES.load(std::sync::atomic::Ordering::Relaxed),
            });
            identify_image(&mut file, running_executable(), cache, hash_image)
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

/// The cache key: the manifest's `dev ino size mtime_ns` plus `ctime_ns`,
/// which userspace cannot set, so a copy that preserves the modification time
/// misses rather than inheriting another image's id.
fn cache_fields(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt as _;
    let ctime_ns = i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec());
    format!("{} {ctime_ns}", image_fields(metadata))
}

/// One product home's identity cache, and whether this command may write it.
#[derive(Clone, Copy)]
struct IdentityCache<'a> {
    home: &'a Path,
    writable: bool,
}

/// The installer manifest, then this home's identity cache, then a hash of
/// the open image. A fresh hash is cached only by a command that may write
/// the cache, and only if the image's fields did not change while it was read.
fn identify_image(
    file: &mut std::fs::File,
    executable: Option<&Path>,
    cache: Option<IdentityCache<'_>>,
    hash: impl FnOnce(&mut std::fs::File) -> Option<String>,
) -> Option<String> {
    if let Some(id) = executable.and_then(|path| read_manifest_id(file, path)) {
        return Some(id);
    }
    let fields = file.metadata().ok().map(|metadata| cache_fields(&metadata));
    let keyed = cache.zip(fields.as_deref());
    if let Some(id) = keyed.and_then(|(cache, fields)| read_cached_id(cache.home, fields)) {
        return Some(id);
    }
    let id = hash(file)?;
    let unchanged = file.metadata().ok().map(|metadata| cache_fields(&metadata)) == fields;
    if let Some((cache, fields)) = keyed.filter(|(cache, _)| cache.writable && unchanged) {
        write_cached_id(cache.home, &id, fields);
    }
    Some(id)
}

fn read_manifest_id(file: &std::fs::File, executable: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt as _;
    let directory = installed_snapshot_directory(executable)?;
    let parent = std::fs::metadata(directory).ok()?;
    if !parent.is_dir() || parent.mode() & 0o022 != 0 {
        return None;
    }
    let manifest = open_owned_regular_file(&directory.join("binary-id"), 0o022)?;
    let fields = image_fields(&file.metadata().ok()?);
    let mut contents = String::new();
    manifest.take(257).read_to_string(&mut contents).ok()?;
    if contents.len() > 256 {
        return None;
    }
    let words: Vec<_> = contents.split_whitespace().collect();
    if words.len() != 5 || !valid_binary_id(words[0]) || words[1..].join(" ") != fields {
        return None;
    }
    Some(words[0].to_owned())
}

/// A regular file the effective user owns with none of `denied` mode bits,
/// opened without following a link or blocking on a FIFO.
fn open_owned_regular_file(path: &Path, denied: u32) -> Option<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::MetadataExt as _;
    let file = std::fs::File::from(
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .ok()?,
    );
    let metadata = file.metadata().ok()?;
    (metadata.is_file()
        && metadata.uid() == rustix::process::geteuid().as_raw()
        && metadata.mode() & denied == 0)
        .then_some(file)
}

fn valid_binary_id(id: &str) -> bool {
    id.len() == 12
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Per-home identity cache for executables outside an installed snapshot (a
/// development or test build), keyed by the image's
/// `dev ino size mtime_ns ctime_ns`.
const IDENTITY_CACHE_FILE: &str = "binary-ids";
const IDENTITY_CACHE_ENTRIES: usize = 16;
const IDENTITY_CACHE_BYTES: u64 = 4 * 1024;

static IDENTITY_CACHE_HOME: OnceLock<PathBuf> = OnceLock::new();
static IDENTITY_CACHE_WRITES: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Records the product home whose identity cache this process uses. The
/// composition root installs it once, before any command asks for the id; an
/// id computed without it (an embedding caller or a test) just hashes.
pub(crate) fn install_identity_cache_home(home: PathBuf) {
    let _ = IDENTITY_CACHE_HOME.set(home);
}

/// Lets this process record a fresh hash in the cache. Only commands that use
/// product state call it; inspection commands (`version`, help, `session
/// rehearse`, `status`) read a hit and never write.
pub(crate) fn allow_identity_cache_writes() {
    IDENTITY_CACHE_WRITES.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn identity_cache_home() -> Option<PathBuf> {
    IDENTITY_CACHE_HOME.get().cloned()
}

/// The home must already exist as a private directory this user owns; the
/// cache never creates product state.
fn private_home(home: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(home).is_ok_and(|metadata| {
        metadata.is_dir()
            && metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.mode() & 0o777 == 0o700
    })
}

/// Valid `id dev ino size mtime_ns ctime_ns` lines of a private cache file.
fn cached_entries(home: &Path) -> Option<Vec<(String, String)>> {
    if !private_home(home) {
        return None;
    }
    let file = open_owned_regular_file(&home.join(IDENTITY_CACHE_FILE), 0o077)?;
    let mut contents = String::new();
    file.take(IDENTITY_CACHE_BYTES)
        .read_to_string(&mut contents)
        .ok()?;
    Some(
        contents
            .lines()
            .filter_map(|line| {
                let words: Vec<_> = line.split_whitespace().collect();
                (words.len() == 6 && valid_binary_id(words[0]))
                    .then(|| (words[0].to_owned(), words[1..].join(" ")))
            })
            .collect(),
    )
}

fn read_cached_id(home: &Path, fields: &str) -> Option<String> {
    cached_entries(home)?
        .into_iter()
        .find_map(|(id, key)| (key == fields).then_some(id))
}

/// Best effort: newest entry first, bounded, replaced atomically. A home that
/// is missing or not private, or any write failure, leaves no cache.
fn write_cached_id(home: &Path, id: &str, fields: &str) {
    if !private_home(home) {
        return;
    }
    let mut contents = format!("{id} {fields}\n");
    for (other, key) in cached_entries(home)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, key)| key != fields)
        .take(IDENTITY_CACHE_ENTRIES - 1)
    {
        contents.push_str(&format!("{other} {key}\n"));
    }
    // A uniquely named mode-0600 staged file; dropping it on failure removes
    // only this process's own file.
    let Ok(mut staged) = tempfile::Builder::new()
        .prefix(".binary-ids.")
        .tempfile_in(home)
    else {
        return;
    };
    if std::io::Write::write_all(&mut staged, contents.as_bytes()).is_ok() {
        let _ = staged.persist(home.join(IDENTITY_CACHE_FILE));
    }
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
        "{} ({FLAVOR}; binary {}{hooks}{})",
        plakous::VERSION,
        binary_id(),
        crate::build_features::version_suffix(),
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
            identify_image(&mut image, None, None, hash_image).as_deref(),
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
            let id = identify_image(&mut image, Some(&executable), None, |file| {
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
        let _ = identify_image(&mut image, Some(&target.join("plak")), None, |file| {
            calls.set(calls.get() + 1);
            hash_image(file)
        });
        assert_eq!(calls.get(), 1);
    }

    fn private_home() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        home
    }

    fn counted_identify(image: &Path, home: &Path) -> (String, usize) {
        identify_with(image, home, true)
    }

    fn identify_with(image: &Path, home: &Path, writable: bool) -> (String, usize) {
        let calls = std::cell::Cell::new(0);
        let mut file = std::fs::File::open(image).unwrap();
        let cache = Some(IdentityCache { home, writable });
        let id = identify_image(&mut file, Some(image), cache, |file| {
            calls.set(calls.get() + 1);
            hash_image(file)
        })
        .unwrap();
        (id, calls.get())
    }

    #[test]
    fn the_home_cache_skips_the_hash_for_the_same_image_fields() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = private_home();
        let builds = tempfile::tempdir().unwrap();
        let image = builds.path().join("plak");
        std::fs::write(&image, "image").unwrap();
        assert_eq!(
            counted_identify(&image, home.path()),
            ("6105d6cc76af".to_owned(), 1)
        );
        let cache = home.path().join(IDENTITY_CACHE_FILE);
        assert_eq!(
            std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            counted_identify(&image, home.path()),
            ("6105d6cc76af".to_owned(), 0)
        );
        // A rebuild (new size and mtime) is a new key; the old entry stays.
        std::fs::write(&image, "image, rebuilt").unwrap();
        let rebuilt = counted_identify(&image, home.path());
        assert_eq!(rebuilt.1, 1);
        assert_ne!(rebuilt.0, "6105d6cc76af");
        assert_eq!(counted_identify(&image, home.path()), (rebuilt.0, 0));
        assert_eq!(std::fs::read_to_string(&cache).unwrap().lines().count(), 2);
    }

    #[test]
    fn the_home_cache_matches_every_field_and_stays_bounded() {
        let home = private_home();
        let builds = tempfile::tempdir().unwrap();
        let image = builds.path().join("plak");
        std::fs::write(&image, "image").unwrap();
        let fields = cache_fields(&std::fs::metadata(&image).unwrap());
        let cache = home.path().join(IDENTITY_CACHE_FILE);
        for index in 1..6 {
            let mut words: Vec<_> = format!("abcdef012345 {fields}")
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            words[index] = "0".to_owned();
            std::fs::write(&cache, words.join(" ")).unwrap();
            assert_eq!(counted_identify(&image, home.path()).1, 1, "field {index}");
        }
        let foreign: String = (0..40)
            .map(|index| format!("{index:012x} 1 {index} 2 3 4\n"))
            .collect();
        std::fs::write(&cache, format!("{foreign}abcdef012345 {fields}\n")).unwrap();
        assert_eq!(
            counted_identify(&image, home.path()),
            ("abcdef012345".to_owned(), 0)
        );
        std::fs::write(&cache, &foreign).unwrap();
        assert_eq!(counted_identify(&image, home.path()).1, 1);
        let kept = std::fs::read_to_string(&cache).unwrap();
        assert_eq!(kept.lines().count(), IDENTITY_CACHE_ENTRIES);
        assert!(kept.starts_with(&format!("6105d6cc76af {fields}\n")));
    }

    #[test]
    fn a_changed_inode_change_time_misses_the_cache() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = private_home();
        let builds = tempfile::tempdir().unwrap();
        let image = builds.path().join("plak");
        std::fs::write(&image, "image").unwrap();
        assert_eq!(counted_identify(&image, home.path()).1, 1);
        let before = std::fs::metadata(&image).unwrap();
        // chmod moves ctime and nothing in the manifest's fields.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o640)).unwrap();
        let after = std::fs::metadata(&image).unwrap();
        assert_eq!(image_fields(&before), image_fields(&after));
        assert_ne!(cache_fields(&before), cache_fields(&after));
        assert_eq!(counted_identify(&image, home.path()).1, 1);
        assert_eq!(counted_identify(&image, home.path()).1, 0);
    }

    #[test]
    fn a_read_only_command_reads_the_cache_and_never_writes_it() {
        let home = private_home();
        let builds = tempfile::tempdir().unwrap();
        let image = builds.path().join("plak");
        std::fs::write(&image, "image").unwrap();
        let cache = home.path().join(IDENTITY_CACHE_FILE);
        assert_eq!(identify_with(&image, home.path(), false).1, 1);
        assert!(
            !cache.exists(),
            "an inspection command created product state"
        );
        assert_eq!(identify_with(&image, home.path(), true).1, 1);
        assert_eq!(
            identify_with(&image, home.path(), false),
            ("6105d6cc76af".to_owned(), 0)
        );
        let leftovers: Vec<_> = std::fs::read_dir(home.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, [OsString::from(IDENTITY_CACHE_FILE)]);
    }

    #[test]
    fn an_unsafe_home_or_cache_file_falls_back_to_hashing() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let builds = tempfile::tempdir().unwrap();
        let image = builds.path().join("plak");
        std::fs::write(&image, "image").unwrap();
        let fields = cache_fields(&std::fs::metadata(&image).unwrap());
        let entry = format!("abcdef012345 {fields}\n");

        let shared = private_home();
        std::fs::set_permissions(shared.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(shared.path().join(IDENTITY_CACHE_FILE), &entry).unwrap();
        assert_eq!(counted_identify(&image, shared.path()).1, 1);
        assert_eq!(
            std::fs::read_to_string(shared.path().join(IDENTITY_CACHE_FILE)).unwrap(),
            entry,
            "a home that is not private is neither read nor written"
        );

        let missing = private_home();
        let absent = missing.path().join("absent");
        assert_eq!(counted_identify(&image, &absent).1, 1);
        assert!(!absent.exists(), "the cache never creates a home");

        let home = private_home();
        let cache = home.path().join(IDENTITY_CACHE_FILE);
        std::fs::write(&cache, &entry).unwrap();
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(counted_identify(&image, home.path()).0, "6105d6cc76af");
        std::fs::remove_file(&cache).unwrap();
        let elsewhere = builds.path().join("forged");
        std::fs::write(&elsewhere, &entry).unwrap();
        symlink(&elsewhere, &cache).unwrap();
        assert_eq!(counted_identify(&image, home.path()).0, "6105d6cc76af");
        // The fresh hash replaced the link with this home's own private file.
        assert!(std::fs::symlink_metadata(&cache).unwrap().is_file());
        assert_eq!(
            counted_identify(&image, home.path()),
            ("6105d6cc76af".to_owned(), 0)
        );
    }

    #[test]
    fn a_matching_manifest_is_used_before_the_home_cache() {
        let home = private_home();
        let directory = tempfile::tempdir().unwrap();
        let snapshot = directory
            .path()
            .join(".local/lib/plak/release")
            .join("c".repeat(64));
        std::fs::create_dir_all(&snapshot).unwrap();
        let executable = snapshot.join("plak");
        std::fs::write(&executable, "image").unwrap();
        let fields = image_fields(&std::fs::metadata(&executable).unwrap());
        std::fs::write(
            snapshot.join("binary-id"),
            format!("abcdef012345 {fields}\n"),
        )
        .unwrap();
        assert_eq!(
            counted_identify(&executable, home.path()),
            ("abcdef012345".to_owned(), 0)
        );
        assert!(!home.path().join(IDENTITY_CACHE_FILE).exists());
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
