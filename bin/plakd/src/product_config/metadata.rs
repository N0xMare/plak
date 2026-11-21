use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// File identity detects atomic replacement; change time also catches an edit
/// that preserves the original size and restores its modification time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileStamp {
    identity: (u64, u64),
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl From<&Metadata> for FileStamp {
    fn from(file: &Metadata) -> Self {
        Self {
            identity: (file.dev(), file.ino()),
            size: file.len(),
            modified: (file.mtime(), file.mtime_nsec()),
            changed: (file.ctime(), file.ctime_nsec()),
        }
    }
}

pub(crate) fn file_stamp(path: &Path) -> io::Result<Option<FileStamp>> {
    match fs::metadata(path) {
        Ok(file) => Ok(Some(FileStamp::from(&file))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
