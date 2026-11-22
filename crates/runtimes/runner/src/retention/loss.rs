use std::fs::File;
use std::os::unix::fs::MetadataExt as _;

use super::{Origin, Path, io};

const NATIVE_LOCK: &str = "retention-native.lock";

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    pub(super) fn observe(builds: &Path, origin: &Origin) -> io::Result<Self> {
        let path = builds
            .join(origin.build.as_str())
            .join(format!("retention-v{}.sock", origin.version));
        Self::at(&path)
    }

    pub(super) fn at(path: &Path) -> io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        super::check_socket(&metadata)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[derive(Debug)]
struct ConfirmedLoss;
impl std::fmt::Display for ConfirmedLoss {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("original drainer instance was lost")
    }
}
impl std::error::Error for ConfirmedLoss {}

pub(super) fn error() -> io::Error {
    io::Error::other(ConfirmedLoss)
}
pub(crate) fn confirmed(error: &io::Error) -> bool {
    let Some(cause) = error.get_ref() else {
        return false;
    };
    cause.is::<ConfirmedLoss>()
}

pub(super) fn positive(builds: &Path, origin: &Origin, previous: Option<SocketIdentity>) -> bool {
    match SocketIdentity::observe(builds, origin) {
        Ok(current) => {
            previous.is_some_and(|old| old.device != current.device || old.inode != current.inode)
                || previous.is_some() && native_lock_free(builds, origin.build.as_str())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            native_lock_free(builds, origin.build.as_str())
        }
        Err(_) => false,
    }
}

pub(super) fn native_lock_free(builds: &Path, build: &str) -> bool {
    native_lock(builds, build).is_some_and(|(file, _)| file.try_lock().is_ok())
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct NativeHolder {
    device: u64,
    inode: u64,
    pub(super) pid: u32,
}

pub(super) fn native_holder(builds: &Path, build: &str) -> Option<NativeHolder> {
    use std::io::Read as _;
    let (file, metadata) = native_lock(builds, build)?;
    if !matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
        return None;
    }
    let mut text = String::new();
    file.take(16).read_to_string(&mut text).ok()?;
    Some(NativeHolder {
        device: metadata.dev(),
        inode: metadata.ino(),
        pid: text.parse().ok()?,
    })
}

fn native_lock(builds: &Path, build: &str) -> Option<(File, std::fs::Metadata)> {
    let path = builds.join(build).join(NATIVE_LOCK);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !private_file(&metadata) {
        return None;
    }
    let lock = File::options().read(true).write(true).open(path).ok()?;
    lock.metadata()
        .is_ok_and(|now| now.dev() == metadata.dev() && now.ino() == metadata.ino())
        .then_some((lock, metadata))
}

fn private_file(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
        && metadata.uid() == rustix::process::geteuid().as_raw()
        // No group/other permissions: the low six mode bits are clear.
        && metadata.mode().trailing_zeros() >= 6
}

pub(crate) fn link_native_lock(directory: &Path, native: &Path) -> io::Result<()> {
    let source = std::fs::symlink_metadata(native)?;
    if !private_file(&source) {
        return Err(super::invalid("native lock must be an owned private file"));
    }
    let target = directory.join(NATIVE_LOCK);
    match std::fs::symlink_metadata(&target) {
        Ok(existing) if existing.dev() == source.dev() && existing.ino() == source.ino() => {
            return Ok(());
        }
        Ok(existing) if private_file(&existing) => std::fs::remove_file(&target)?,
        Ok(_) => {
            return Err(super::invalid(
                "retention native-lock witness is not private",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::hard_link(native, target)?;
    File::open(directory)?.sync_all()
}
