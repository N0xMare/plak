//! Image files an operator attaches to a prompt (`plak run --image`, JSONL
//! `prompt.images`, the TUI's `/image` and a dropped path) and clipboard
//! pixels: reading, sniffing, and normalizing into the bounds.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use plakous::agent::{MAX_USER_IMAGE_BYTES, MAX_USER_MESSAGE_BYTES};
use plakous_host_protocol::DraftImage;

/// Most images one message carries.
pub const MAX_IMAGES: usize = plakous::agent::MAX_USER_IMAGES;

/// Largest raw image one message part carries (3.75 MiB).
pub const MAX_IMAGE_BYTES: usize = MAX_USER_IMAGE_BYTES;

/// Largest image file read for normalizing (64 MiB).
pub const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

/// Encoding overhead charged per image beside its base64 text.
const IMAGE_FRAMING_BYTES: usize = 128;

/// Encoding overhead charged for the text part beside its escaped text.
const TEXT_FRAMING_BYTES: usize = 64;

/// A message's text and images encode past the per-message budget,
/// [`MAX_USER_MESSAGE_BYTES`] (16 MiB).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageBudgetExceeded {
    encoded_bytes: usize,
}

impl fmt::Display for MessageBudgetExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[expect(
            clippy::cast_precision_loss,
            reason = "message sizes stay far below 2^53 and the rendering is display-only"
        )]
        let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        write!(
            formatter,
            "the message encodes to {:.1} MiB, over the {:.0} MiB per-message budget (three \
             maximum-size images fit); attach fewer or smaller images",
            mib(self.encoded_bytes),
            mib(MAX_USER_MESSAGE_BYTES)
        )
    }
}

impl std::error::Error for MessageBudgetExceeded {}

/// Checks a message's text and images against the per-message budget,
/// counting the text as its JSON string and each image as its base64 text,
/// each with framing. Runtime admission checks the exact encoding; this lets
/// a frontend refuse an attachment before it is queued.
///
/// # Errors
///
/// Returns [`MessageBudgetExceeded`] when the message would not fit.
pub fn check_message_budget<'a>(
    text: &str,
    images: impl IntoIterator<Item = &'a DraftImage>,
) -> Result<(), MessageBudgetExceeded> {
    let text_bytes = if text.is_empty() {
        0
    } else {
        json_string_bytes(text).saturating_add(TEXT_FRAMING_BYTES)
    };
    let encoded_bytes = images.into_iter().fold(text_bytes, |total, image| {
        total
            .saturating_add(image.bytes.len().div_ceil(3).saturating_mul(4))
            .saturating_add(IMAGE_FRAMING_BYTES)
    });
    if encoded_bytes > MAX_USER_MESSAGE_BYTES {
        return Err(MessageBudgetExceeded { encoded_bytes });
    }
    Ok(())
}

/// The length of `text` as a `serde_json` string, quotes included: `"` and
/// `\\` and the short-form controls take two bytes, other controls six.
fn json_string_bytes(text: &str) -> usize {
    text.bytes().fold(2_usize, |total, byte| {
        total.saturating_add(match byte {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
    })
}

mod normalize;

pub use normalize::{MAX_IMAGE_EDGE, NormalizeError, normalize, normalize_rgba};

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
    Unusable {
        path: PathBuf,
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

/// Reads one image file, sniffs its type from its bytes (the extension is
/// never trusted) and [`normalize`]s it into the bounds.
///
/// # Errors
///
/// Returns [`ImageFileError`] naming the file when it cannot be read, is not
/// a file, exceeds [`MAX_SOURCE_BYTES`], or is not an image within reach of
/// the bounds.
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
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err(too_large(metadata.len()));
    }
    let bytes = std::fs::read(path).map_err(unreadable)?;
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(too_large(bytes.len() as u64));
    }
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    normalize(name, bytes).map_err(|source| ImageFileError::Unusable {
        path: path.to_owned(),
        source,
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

/// The chip of an attached image under its type, as the transcript shows
/// it: `[image: png 2048x1324 1.9 MB, downscaled from 3456x2234]`.
#[must_use]
pub fn unnamed_chip(image: &DraftImage) -> String {
    crate::sessions::draft_image_chip(image)
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
        assert!(matches!(error, ImageFileError::Unusable { .. }));
        assert!(error.to_string().contains("notes.png"));

        // Over the bound, so decoded to be downscaled: a header alone fails.
        let big = dir.path().join("big.png");
        std::fs::write(&big, png(2, 2, MAX_USER_IMAGE_BYTES)).unwrap();
        let error = read_image(&big).unwrap_err();
        assert!(matches!(error, ImageFileError::Unusable { .. }));
        assert!(error.to_string().contains("big.png") && error.to_string().contains("decoded"));
    }

    #[test]
    fn three_maximum_images_fit_the_message_budget_and_four_do_not() {
        let maximum = || DraftImage {
            bytes: png(2, 3, MAX_IMAGE_BYTES - png(2, 3, 0).len()),
            ..draft_image()
        };
        let three = vec![maximum(); 3];
        assert_eq!(check_message_budget("look at these", &three), Ok(()));
        let typical = vec![
            DraftImage {
                bytes: png(1280, 720, 1_400 * 1024),
                ..draft_image()
            };
            MAX_IMAGES
        ];
        assert_eq!(check_message_budget("", &typical), Ok(()));

        let error = check_message_budget("", &vec![maximum(); 4]).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("20.0 MiB, over the 16 MiB per-message budget"),
            "{message}"
        );
        assert!(
            message.contains("three maximum-size images fit"),
            "{message}"
        );
    }

    #[test]
    fn text_counts_as_its_json_string() {
        for text in [
            "plain",
            "a \"quoted\" \\ path\n\ttab",
            "\u{1}\u{1f}\u{8}\u{c}\r",
            "é ✓",
        ] {
            assert_eq!(
                json_string_bytes(text),
                serde_json::to_string(text).unwrap().len(),
                "{text:?}"
            );
        }
        // Escaping, not raw length, decides whether text beside three
        // maximum-size images fits.
        let maximum = || DraftImage {
            bytes: png(2, 3, MAX_IMAGE_BYTES - png(2, 3, 0).len()),
            ..draft_image()
        };
        let three = vec![maximum(); 3];
        assert_eq!(check_message_budget(&"x".repeat(600_000), &three), Ok(()));
        assert!(check_message_budget(&"\"\n".repeat(300_000), &three).is_err());
    }

    fn draft_image() -> DraftImage {
        DraftImage {
            name: "shot.png".into(),
            media_type: "image/png".into(),
            bytes: png(2, 3, 0),
            source_dimensions: None,
        }
    }
}
