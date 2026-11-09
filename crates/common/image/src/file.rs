//! Image files named by the caller: the refusals of reading one, and the
//! header check a dropped path passes.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::{ImageSniffError, MAX_SOURCE_BYTES, NormalizeError, sniff_image};

/// Why an image file could not be attached. Every message names the file.
#[derive(Debug)]
#[non_exhaustive]
pub enum ImageFileError {
    /// The file could not be read.
    Unreadable {
        /// The file named.
        path: PathBuf,
        /// The read failure.
        source: io::Error,
    },
    /// The path is not a regular file.
    NotAFile {
        /// The path named.
        path: PathBuf,
    },
    /// The file exceeds [`MAX_SOURCE_BYTES`]; it is refused unread.
    TooLarge {
        /// The file named.
        path: PathBuf,
        /// Its size.
        bytes: u64,
    },
    /// The file is not an image within reach of the bounds.
    Unusable {
        /// The file named.
        path: PathBuf,
        /// Why it could not be normalized.
        source: NormalizeError,
    },
}

impl fmt::Display for ImageFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { path, source } => {
                write!(
                    formatter,
                    "image {}: cannot be read: {source}",
                    path.display()
                )
            }
            Self::NotAFile { path } => write!(formatter, "image {}: not a file", path.display()),
            Self::TooLarge { path, bytes } => write!(
                formatter,
                "image {}: {bytes} bytes exceeds the {MAX_SOURCE_BYTES}-byte (64 MiB) \
                 limit for an image file",
                path.display()
            ),
            Self::Unusable { path, source } => {
                write!(formatter, "image {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ImageFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            Self::Unusable { source, .. } => Some(source),
            Self::NotAFile { .. } | Self::TooLarge { .. } => None,
        }
    }
}

/// Whether `path` is an absolute path to an existing file whose leading
/// bytes are a png, jpeg, gif or webp header: what a terminal pastes when an
/// image file is dropped on it.
#[must_use]
pub fn is_image_file(path: &Path) -> bool {
    use std::io::Read as _;
    if !path.is_absolute() || !path.is_file() {
        return false;
    }
    let mut head = Vec::new();
    let read = std::fs::File::open(path).and_then(|file| file.take(4096).read_to_end(&mut head));
    // A header too short to state dimensions still names the type.
    read.is_ok() && matches!(sniff_image(&head), Ok(_) | Err(ImageSniffError::Dimensions))
}
