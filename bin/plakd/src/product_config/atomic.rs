//! Atomic publication of one config or template file.

use super::load::ConfigSaveError;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsFd;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AtomicStage {
    Write,
    FileSync,
    Publish,
    ParentSync,
}

/// One file publication primitive. New template names use an exclusive link
/// instead of replacing an existing file; the config pointer uses rename.
pub(super) fn atomic_file(
    target: &Path,
    bytes: &[u8],
    create_new: bool,
    fault: &mut impl FnMut(&Path, AtomicStage) -> Result<(), String>,
) -> Result<(), ConfigSaveError> {
    let parent = target.parent().ok_or_else(|| {
        ConfigSaveError::BeforePublication("publication target has no parent".into())
    })?;
    let directory = rustix::fs::open(
        parent,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| {
        ConfigSaveError::BeforePublication("could not open publication directory".into())
    })?;
    atomic_file_at(&directory, target, bytes, create_new, fault)
}

/// Same publication stages through a retained, validated directory handle.
pub(super) fn atomic_file_at(
    directory: &impl AsFd,
    target: &Path,
    bytes: &[u8],
    create_new: bool,
    fault: &mut impl FnMut(&Path, AtomicStage) -> Result<(), String>,
) -> Result<(), ConfigSaveError> {
    let before = |message: &str| ConfigSaveError::BeforePublication(message.to_owned());
    let name = target
        .file_name()
        .ok_or_else(|| before("publication target has no name"))?;
    let temporary = format!(".publication.{}.tmp", uuid::Uuid::new_v4());
    let mut file = rustix::fs::openat(
        directory,
        temporary.as_str(),
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map(File::from)
    .map_err(|_| before("could not create publication temporary file"))?;
    let publish = (|| -> Result<(), String> {
        fault(target, AtomicStage::Write)?;
        file.write_all(bytes)
            .map_err(|_| "could not write publication".to_owned())?;
        fault(target, AtomicStage::FileSync)?;
        file.sync_all()
            .map_err(|_| "could not sync publication".to_owned())?;
        fault(target, AtomicStage::Publish)?;
        if create_new {
            rustix::fs::linkat(
                directory,
                temporary.as_str(),
                directory,
                name,
                rustix::fs::AtFlags::empty(),
            )
        } else {
            rustix::fs::renameat(directory, temporary.as_str(), directory, name)
        }
        .map_err(|_| "could not publish file".to_owned())?;
        Ok(())
    })();
    let _ = rustix::fs::unlinkat(directory, temporary.as_str(), rustix::fs::AtFlags::empty());
    publish.map_err(ConfigSaveError::BeforePublication)?;
    fault(target, AtomicStage::ParentSync)
        .and_then(|()| {
            rustix::fs::fsync(directory).map_err(|_| "could not sync publication directory".into())
        })
        .map_err(ConfigSaveError::PublishedDurabilityUncertain)
}

pub(super) fn sync_parent_directory(parent: &Path) -> Result<(), String> {
    OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "could not sync publication directory".to_owned())
}
