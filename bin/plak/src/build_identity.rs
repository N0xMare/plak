//! Product build identity, independent of the working directory's Git state.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest as _, Sha256};

mod interactive;
#[cfg(any(test, feature = "tui"))]
pub(crate) use interactive::installed_executable;
#[cfg(feature = "tui")]
pub(crate) use interactive::invocation_argv0;
pub(crate) use interactive::newer_build_installed;

pub(crate) const JJ_ENABLED: bool = cfg!(feature = "jj");

pub(crate) const FLAVOR: &str = match (cfg!(feature = "maintainer"), cfg!(debug_assertions)) {
    (true, true) => "maintainer debug",
    (true, false) => "maintainer release",
    (false, true) => "debug",
    (false, false) => "release",
};

/// The source revision this build was made from, as the build recipe set
/// `PLAK_SOURCE_REVISION`; `unknown` otherwise. A runner binds only a host of
/// the same revision, and an `unknown` build pairs with no remote runner.
pub(crate) const SOURCE_REVISION: &str = match option_env!("PLAK_SOURCE_REVISION") {
    Some(revision) => revision,
    None => "unknown",
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
    Name(std::ffi::OsString),
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

/// The canonical file this process runs. Sidecars are found beside it, so a
/// session started from a snapshot keeps that snapshot's sidecars even after
/// the `PATH` symlinks move to a newer build.
pub(crate) fn running_executable() -> Option<&'static Path> {
    launch()
        .executable
        .as_ref()
        .map(|identity| identity.path.as_path())
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

/// This executable's id when the install manifest or this home's identity
/// cache already names the running image, or when this process has computed
/// it. It never hashes, so a bounded probe answers at once; `None` means the
/// id is not known yet, never that it is something else.
pub(crate) fn known_binary_id() -> Option<String> {
    if let Some(id) = BINARY_ID.get() {
        return id.clone();
    }
    let mut file = open_running_image()?;
    let home = identity_cache_home();
    let cache = home.as_deref().map(|home| IdentityCache {
        home,
        writable: false,
    });
    identify_image(&mut file, running_executable(), cache, |_| None)
}

fn open_running_image() -> Option<std::fs::File> {
    // /proc names the mapped image even after its launch path is replaced.
    if cfg!(target_os = "linux") {
        std::fs::File::open("/proc/self/exe").ok()
    } else {
        std::fs::File::open(running_executable()?).ok()
    }
}

/// A delay every image hash waits out first. Only the offline test hooks set
/// it, so a test can make a manifest-less build's identity slow.
pub(crate) static HASH_DELAY: OnceLock<std::time::Duration> = OnceLock::new();

/// Only test hooks install this simulated executable-launch latency.
pub(crate) static RELOAD_PROBE_LAUNCH_DELAY: OnceLock<std::time::Duration> = OnceLock::new();

fn hash_image(file: &mut std::fs::File) -> Option<String> {
    if let Some(delay) = HASH_DELAY.get() {
        std::thread::sleep(*delay);
    }
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
mod tests;
