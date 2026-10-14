use super::{MAX_RECEIPT_BYTES, NativeSubscriptionError};
#[cfg(unix)]
use rustix::fs::{AtFlags, Mode, OFlags};
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs::File;
#[cfg(unix)]
use std::io::{Read as _, Seek as _, Write as _};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

pub(super) fn validate_state_root(path: &Path) -> Result<(), NativeSubscriptionError> {
    if !path.is_absolute() || path.parent().is_none() || path == Path::new("/") {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    for component in path.components() {
        if matches!(component, Component::CurDir | Component::ParentDir) {
            return Err(NativeSubscriptionError::UnsafeStorage);
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn ensure_state_root(path: &Path) -> Result<File, NativeSubscriptionError> {
    validate_state_root(path)?;
    let mut directory = open_filesystem_root()?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let created =
                    match rustix::fs::mkdirat(&directory, name, Mode::from_raw_mode(0o700)) {
                        Ok(()) => true,
                        Err(rustix::io::Errno::EXIST) => false,
                        Err(_) => return Err(NativeSubscriptionError::UnsafeStorage),
                    };
                directory = open_directory_child(&directory, name)?;
                if created {
                    validate_private_directory(&directory)?;
                }
            }
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                return Err(NativeSubscriptionError::UnsafeStorage);
            }
        }
    }
    validate_external_state_root(&directory)?;
    Ok(directory)
}

#[cfg(not(unix))]
pub(super) fn ensure_state_root(_path: &Path) -> Result<File, NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}

#[cfg(unix)]
fn open_filesystem_root() -> Result<File, NativeSubscriptionError> {
    rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| NativeSubscriptionError::UnsafeStorage)
}

#[cfg(unix)]
fn open_directory_child(parent: &File, name: &OsStr) -> Result<File, NativeSubscriptionError> {
    rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| NativeSubscriptionError::UnsafeStorage)
}

#[cfg(unix)]
fn open_absolute_directory(path: &Path) -> Result<File, NativeSubscriptionError> {
    validate_state_root(path)?;
    let mut directory = open_filesystem_root()?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_directory_child(&directory, name)?;
            }
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                return Err(NativeSubscriptionError::UnsafeStorage);
            }
        }
    }
    validate_private_directory(&directory)?;
    Ok(directory)
}

#[cfg(not(unix))]
fn open_absolute_directory(_path: &Path) -> Result<File, NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}

/// Reports whether the fixed Plak layout under `state_root` exists, without
/// creating anything; present directories are still validated.
#[cfg(unix)]
pub(super) fn existing_layout(state_root: &Path) -> Result<bool, NativeSubscriptionError> {
    validate_state_root(state_root)?;
    let mut directory = open_filesystem_root()?;
    for component in state_root.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let Some(child) = open_existing_child(&directory, name)? else {
                    return Ok(false);
                };
                directory = child;
            }
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => {
                return Err(NativeSubscriptionError::UnsafeStorage);
            }
        }
    }
    validate_external_state_root(&directory)?;
    for name in ["plak", "subscriptions"] {
        let Some(child) = open_existing_child(&directory, OsStr::new(name))? else {
            return Ok(false);
        };
        validate_private_directory(&child)?;
        directory = child;
    }
    Ok(true)
}

#[cfg(not(unix))]
pub(super) fn existing_layout(_state_root: &Path) -> Result<bool, NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}

#[cfg(unix)]
fn open_existing_child(
    parent: &File,
    name: &OsStr,
) -> Result<Option<File>, NativeSubscriptionError> {
    match rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => Ok(Some(File::from(directory))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(_) => Err(NativeSubscriptionError::UnsafeStorage),
    }
}

#[cfg(unix)]
pub(super) fn open_or_create_private_child(
    parent: &File,
    name: &OsStr,
) -> Result<File, NativeSubscriptionError> {
    match rustix::fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(_) => return Err(NativeSubscriptionError::UnsafeStorage),
    }
    let directory = open_directory_child(parent, name)?;
    validate_private_directory(&directory)?;
    Ok(directory)
}

#[cfg(not(unix))]
pub(super) fn open_or_create_private_child(
    _parent: &File,
    _name: &OsStr,
) -> Result<File, NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}

#[cfg(unix)]
pub(super) fn validate_private_directory(directory: &File) -> Result<(), NativeSubscriptionError> {
    let metadata = directory
        .metadata()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_external_state_root(directory: &File) -> Result<(), NativeSubscriptionError> {
    let metadata = directory
        .metadata()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    let permissions = metadata.mode() & 0o7777;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || permissions & 0o700 != 0o700
        || permissions & 0o022 != 0
        || permissions & !0o777 != 0
    {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn open_private_parent(
    path: &Path,
) -> Result<(File, OsString), NativeSubscriptionError> {
    let parent = path
        .parent()
        .ok_or(NativeSubscriptionError::UnsafeStorage)?;
    let name = path
        .file_name()
        .ok_or(NativeSubscriptionError::UnsafeStorage)?
        .to_owned();
    Ok((open_absolute_directory(parent)?, name))
}

#[cfg(unix)]
fn validate_existing_private_file(
    directory: &File,
    name: &OsStr,
) -> Result<(), NativeSubscriptionError> {
    match rustix::fs::openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => validate_private_file(&File::from(descriptor)),
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(_) => Err(NativeSubscriptionError::UnsafeStorage),
    }
}

#[cfg(unix)]
pub(super) fn read_private_bounded(path: &Path) -> Result<Vec<u8>, NativeSubscriptionError> {
    read_private_bounded_with_hook_and_limit(path, MAX_RECEIPT_BYTES, || {})
}

#[cfg(all(unix, test))]
pub(super) fn read_private_bounded_with_hook(
    path: &Path,
    after_read: impl FnOnce(),
) -> Result<Vec<u8>, NativeSubscriptionError> {
    read_private_bounded_with_hook_and_limit(path, MAX_RECEIPT_BYTES, after_read)
}

#[cfg(unix)]
pub(super) fn read_private_bounded_with_hook_and_limit(
    path: &Path,
    max_bytes: u64,
    after_read: impl FnOnce(),
) -> Result<Vec<u8>, NativeSubscriptionError> {
    let (parent, name) = open_private_parent(path)?;
    let parent_identity = private_file_identity(
        &parent
            .metadata()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?,
    );
    let descriptor = match rustix::fs::openat(
        &parent,
        &name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => {
            return Err(NativeSubscriptionError::AcceptanceRequired);
        }
        Err(_) => return Err(NativeSubscriptionError::UnsafeStorage),
    };
    let mut file = File::from(descriptor);
    let before = file
        .metadata()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    validate_private_file_metadata(&before)?;
    if before.len() == 0 || before.len() > max_bytes {
        return Err(NativeSubscriptionError::InvalidReceipt);
    }
    let retained_identity = private_file_stability(&before);
    let capacity =
        usize::try_from(before.len()).map_err(|_| NativeSubscriptionError::InvalidReceipt)?;
    let mut bytes = Vec::with_capacity(capacity);
    std::io::Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| NativeSubscriptionError::InvalidReceipt)?;
    if bytes.len() as u64 != before.len() {
        return Err(NativeSubscriptionError::InvalidReceipt);
    }
    after_read();
    let after = file
        .metadata()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    validate_private_file_metadata(&after)?;
    if private_file_stability(&after) != retained_identity {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }

    let (current_parent, current_name) = open_private_parent(path)?;
    if current_name != name
        || private_file_identity(
            &current_parent
                .metadata()
                .map_err(|_| NativeSubscriptionError::UnsafeStorage)?,
        ) != parent_identity
    {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    let current = rustix::fs::openat(
        &current_parent,
        &current_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    let current_metadata = current
        .metadata()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    validate_private_file_metadata(&current_metadata)?;
    if private_file_stability(&current_metadata) != retained_identity {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    Ok(bytes)
}

#[cfg(not(unix))]
pub(super) fn read_private_bounded(_path: &Path) -> Result<Vec<u8>, NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}

#[cfg(unix)]
pub(super) fn remove_private_file_durable(path: &Path) -> Result<(), NativeSubscriptionError> {
    let (directory, name) = open_private_parent(path)?;
    validate_existing_private_file(&directory, &name)?;
    match rustix::fs::unlinkat(&directory, &name, AtFlags::empty()) {
        Ok(()) => directory
            .sync_all()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage),
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(_) => Err(NativeSubscriptionError::UnsafeStorage),
    }
}

#[cfg(not(unix))]
pub(super) fn remove_private_file_durable(_path: &Path) -> Result<(), NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}

#[cfg(unix)]
pub(super) fn write_private_atomic(
    path: &Path,
    bytes: &[u8],
) -> Result<(), NativeSubscriptionError> {
    write_private_atomic_with_hook(path, bytes, |_| Ok(()))
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    clippy::enum_variant_names,
    reason = "test fault stages intentionally identify the exact pre-operation boundary"
)]
pub(crate) enum AtomicWriteStage {
    BeforeFileSync,
    BeforeDirectorySync,
    BeforeInitialReadback,
}

#[cfg(unix)]
pub(super) fn write_private_atomic_with_hook(
    path: &Path,
    bytes: &[u8],
    hook: impl FnMut(AtomicWriteStage) -> Result<(), NativeSubscriptionError>,
) -> Result<(), NativeSubscriptionError> {
    write_private_atomic_retained_with_hook(path, bytes, hook).map(drop)
}

#[cfg(unix)]
pub(super) fn write_private_atomic_retained_with_hook(
    path: &Path,
    bytes: &[u8],
    mut hook: impl FnMut(AtomicWriteStage) -> Result<(), NativeSubscriptionError>,
) -> Result<RetainedPrivateFile, NativeSubscriptionError> {
    let (directory, name) = open_private_parent(path)?;
    let directory_identity = private_file_identity(
        &directory
            .metadata()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?,
    );
    validate_existing_private_file(&directory, &name)?;
    let temporary = OsString::from(format!(
        ".{}.tmp-{}-{}",
        name.to_string_lossy(),
        std::process::id(),
        NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let descriptor = rustix::fs::openat(
            &directory,
            &temporary,
            OFlags::RDWR
                | OFlags::CREATE
                | OFlags::EXCL
                | OFlags::NOFOLLOW
                | OFlags::CLOEXEC
                | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        let mut file = File::from(descriptor);
        file.write_all(bytes)
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        hook(AtomicWriteStage::BeforeFileSync)?;
        file.sync_all()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        validate_private_file(&file)?;
        validate_existing_private_file(&directory, &name)?;
        rustix::fs::renameat(&directory, &temporary, &directory, &name)
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        let mut retained = RetainedPrivateFile::from_created(
            path,
            directory
                .try_clone()
                .map_err(|_| NativeSubscriptionError::UnsafeStorage)?,
            name.clone(),
            directory_identity,
            file,
            MAX_RECEIPT_BYTES,
        )?;
        hook(AtomicWriteStage::BeforeDirectorySync)?;
        directory
            .sync_all()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        retained.verify_path_binding()?;
        hook(AtomicWriteStage::BeforeInitialReadback)?;
        retained.verify_exact_bytes(bytes)?;
        Ok(retained)
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&directory, &temporary, AtFlags::empty());
    }
    result
}

#[cfg(unix)]
pub(super) struct RetainedPrivateFile {
    path: PathBuf,
    directory: File,
    name: OsString,
    directory_identity: PrivateFileIdentity,
    file: File,
    stability: PrivateFileStability,
    max_bytes: u64,
}

#[cfg(unix)]
impl RetainedPrivateFile {
    fn from_created(
        path: &Path,
        directory: File,
        name: OsString,
        directory_identity: PrivateFileIdentity,
        file: File,
        max_bytes: u64,
    ) -> Result<Self, NativeSubscriptionError> {
        let metadata = file
            .metadata()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        validate_private_file_metadata(&metadata)?;
        if metadata.len() == 0 || metadata.len() > max_bytes {
            return Err(NativeSubscriptionError::InvalidReceipt);
        }
        let retained = Self {
            path: path.to_owned(),
            directory,
            name,
            directory_identity,
            file,
            stability: private_file_stability(&metadata),
            max_bytes,
        };
        retained.verify_path_binding()?;
        Ok(retained)
    }

    fn verify_retained(&self) -> Result<(), NativeSubscriptionError> {
        validate_private_directory(&self.directory)?;
        let directory = self
            .directory
            .metadata()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        if private_file_identity(&directory) != self.directory_identity {
            return Err(NativeSubscriptionError::UnsafeStorage);
        }
        let file = self
            .file
            .metadata()
            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
        validate_private_file_metadata(&file)?;
        if private_file_stability(&file) != self.stability {
            return Err(NativeSubscriptionError::UnsafeStorage);
        }
        Ok(())
    }

    fn verify_path_binding(&self) -> Result<(), NativeSubscriptionError> {
        self.verify_retained()?;
        verify_private_path_identity(
            &self.path,
            &self.name,
            self.directory_identity,
            self.stability,
        )
    }

    fn read_bytes(&mut self) -> Result<Vec<u8>, NativeSubscriptionError> {
        self.verify_retained()?;
        let capacity = usize::try_from(self.stability.size)
            .map_err(|_| NativeSubscriptionError::InvalidReceipt)?;
        self.file
            .seek(std::io::SeekFrom::Start(0))
            .map_err(|_| NativeSubscriptionError::InvalidReceipt)?;
        let mut bytes = Vec::with_capacity(capacity);
        std::io::Read::by_ref(&mut self.file)
            .take(self.max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| NativeSubscriptionError::InvalidReceipt)?;
        if bytes.len() as u64 != self.stability.size {
            return Err(NativeSubscriptionError::InvalidReceipt);
        }
        self.verify_path_binding()?;
        Ok(bytes)
    }

    pub(super) fn verify_exact_bytes(
        &mut self,
        expected: &[u8],
    ) -> Result<(), NativeSubscriptionError> {
        if self.read_bytes()? != expected {
            return Err(NativeSubscriptionError::InvalidReceipt);
        }
        Ok(())
    }
}

#[cfg(not(unix))]
pub(super) struct RetainedPrivateFile;

#[cfg(not(unix))]
impl RetainedPrivateFile {
    pub(super) fn verify_exact_bytes(
        &mut self,
        _expected: &[u8],
    ) -> Result<(), NativeSubscriptionError> {
        Err(NativeSubscriptionError::UnsupportedPlatform)
    }
}

#[cfg(unix)]
fn verify_private_path_identity(
    path: &Path,
    expected_name: &OsStr,
    expected_parent: PrivateFileIdentity,
    expected_file: PrivateFileStability,
) -> Result<(), NativeSubscriptionError> {
    let (parent, name) = open_private_parent(path)?;
    if name != expected_name
        || private_file_identity(
            &parent
                .metadata()
                .map_err(|_| NativeSubscriptionError::UnsafeStorage)?,
        ) != expected_parent
    {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    let current = rustix::fs::openat(
        &parent,
        &name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    let metadata = current
        .metadata()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    validate_private_file_metadata(&metadata)?;
    if private_file_stability(&metadata) != expected_file {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn write_private_atomic(
    _path: &Path,
    _bytes: &[u8],
) -> Result<(), NativeSubscriptionError> {
    Err(NativeSubscriptionError::UnsupportedPlatform)
}

#[cfg(unix)]
pub(super) fn validate_private_file(file: &File) -> Result<(), NativeSubscriptionError> {
    let metadata = file
        .metadata()
        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
    validate_private_file_metadata(&metadata)
}

#[cfg(unix)]
pub(super) fn validate_private_file_metadata(
    metadata: &std::fs::Metadata,
) -> Result<(), NativeSubscriptionError> {
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(NativeSubscriptionError::UnsafeStorage);
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct PrivateFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
#[derive(Clone, Copy, Eq, PartialEq)]
struct PrivateFileStability {
    identity: PrivateFileIdentity,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(unix)]
pub(super) fn private_file_identity(metadata: &std::fs::Metadata) -> PrivateFileIdentity {
    PrivateFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(unix)]
fn private_file_stability(metadata: &std::fs::Metadata) -> PrivateFileStability {
    PrivateFileStability {
        identity: private_file_identity(metadata),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}
