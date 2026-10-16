//! Image files an operator attaches to a prompt (`plak run --image`, the
//! TUI's `/image` and a dropped path): reading, sniffing and the bounds.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use plakous::agent::MAX_USER_IMAGE_BYTES;
use plakous_host_protocol::DraftImage;

/// Most images one message carries.
pub const MAX_IMAGES: usize = plakous::agent::MAX_USER_IMAGES;

/// Why an image file could not be attached. Every message names the file.
#[derive(Debug)]
#[non_exhaustive]
pub enum ImageFileError {
    Unreadable {
        path: PathBuf,
        source: io::Error,
    },
    NotAFile {
        path: PathBuf,
    },
    TooLarge {
        path: PathBuf,
        bytes: u64,
    },
    NotAnImage {
        path: PathBuf,
        source: plakous::tools::ImageSniffError,
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
                "image {}: {bytes} bytes exceeds the {MAX_USER_IMAGE_BYTES}-byte (3.75 MiB) \
                 limit; downscale it and attach it again",
                path.display()
            ),
            Self::NotAnImage { path, source } => {
                write!(formatter, "image {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ImageFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable { source, .. } => Some(source),
            Self::NotAnImage { source, .. } => Some(source),
            Self::NotAFile { .. } | Self::TooLarge { .. } => None,
        }
    }
}

/// Reads one image file within the admission bound and sniffs its type from
/// its bytes; the extension is never trusted.
///
/// # Errors
///
/// Returns [`ImageFileError`] naming the file when it cannot be read, is not
/// a file, exceeds the bound, or is not an image.
pub fn read_image(path: &Path) -> Result<DraftImage, ImageFileError> {
    let unreadable = |source| ImageFileError::Unreadable {
        path: path.to_owned(),
        source,
    };
    let metadata = std::fs::metadata(path).map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(ImageFileError::NotAFile {
            path: path.to_owned(),
        });
    }
    let too_large = |bytes| ImageFileError::TooLarge {
        path: path.to_owned(),
        bytes,
    };
    if metadata.len() > MAX_USER_IMAGE_BYTES as u64 {
        return Err(too_large(metadata.len()));
    }
    let bytes = std::fs::read(path).map_err(unreadable)?;
    if bytes.len() > MAX_USER_IMAGE_BYTES {
        return Err(too_large(bytes.len() as u64));
    }
    let header =
        plakous::tools::sniff_image(&bytes).map_err(|source| ImageFileError::NotAnImage {
            path: path.to_owned(),
            source,
        })?;
    Ok(DraftImage {
        name: path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        ),
        media_type: header.media_type().to_owned(),
        bytes,
    })
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
    read.is_ok()
        && matches!(
            plakous::tools::sniff_image(&head),
            Ok(_) | Err(plakous::tools::ImageSniffError::Dimensions)
        )
}

/// The chip of an attached image under its file name:
/// `[image: shot.png 1280x720 412 KB]`.
#[must_use]
pub fn chip(image: &DraftImage) -> String {
    crate::sessions::named_image_chip(image)
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
#[must_use]
pub fn png(width: u32, height: u32, extra: usize) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0, 0, 0, 13]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&width.to_be_bytes());
    png.extend_from_slice(&height.to_be_bytes());
    png.resize(png.len() + extra, 0);
    png
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sniffs_and_names_the_file_in_every_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("shot.dat");
        std::fs::write(&shot, png(1280, 720, 412 * 1024)).unwrap();
        let image = read_image(&shot).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.name, "shot.dat");
        assert_eq!(chip(&image), "[image: shot.dat 1280x720 413 KB]");

        let missing = dir.path().join("missing.png");
        let error = read_image(&missing).unwrap_err();
        assert!(matches!(error, ImageFileError::Unreadable { .. }));
        assert!(error.to_string().contains("missing.png"));

        let error = read_image(dir.path()).unwrap_err();
        assert!(matches!(error, ImageFileError::NotAFile { .. }));

        let text = dir.path().join("notes.png");
        std::fs::write(&text, "not an image").unwrap();
        let error = read_image(&text).unwrap_err();
        assert!(matches!(error, ImageFileError::NotAnImage { .. }));
        assert!(error.to_string().contains("notes.png"));

        let big = dir.path().join("big.png");
        std::fs::write(&big, png(2, 2, MAX_USER_IMAGE_BYTES)).unwrap();
        let error = read_image(&big).unwrap_err();
        assert!(matches!(error, ImageFileError::TooLarge { .. }));
        assert!(error.to_string().contains("big.png") && error.to_string().contains("downscale"));
    }
}
