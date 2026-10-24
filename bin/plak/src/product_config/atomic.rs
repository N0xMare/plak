//! Atomic publication of one config or template file.

use super::load::ConfigSaveError;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
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
    let temporary = parent.join(format!(".publication.{}.tmp", uuid::Uuid::new_v4()));
    let before = |message: &str| ConfigSaveError::BeforePublication(message.to_owned());
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
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
            fs::hard_link(&temporary, target)
        } else {
            fs::rename(&temporary, target)
        }
        .map_err(|_| "could not publish file".to_owned())?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    publish.map_err(ConfigSaveError::BeforePublication)?;
    fault(target, AtomicStage::ParentSync)
        .and_then(|()| sync_parent_directory(parent))
        .map_err(ConfigSaveError::PublishedDurabilityUncertain)
}

pub(super) fn sync_parent_directory(parent: &Path) -> Result<(), String> {
    OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "could not sync publication directory".to_owned())
}
