//! Image files an operator attaches to a prompt (`plak run --image`, JSONL
//! `prompt.images`, the TUI's `/image` and a dropped path) and clipboard
//! pixels, as drafts: `plakous-image` reads, sniffs, budgets and normalizes;
//! this module names the draft and renders its chip.

use std::fmt;
use std::path::Path;

use plakous::agent::{MAX_USER_IMAGE_BYTES, MAX_USER_IMAGES, MAX_USER_MESSAGE_BYTES};
use plakous_host_protocol::DraftImage;

pub use plakous_image::{
    MAX_IMAGE_BYTES, MAX_IMAGE_EDGE, MAX_IMAGES, MAX_SOURCE_BYTES, MessageBudgetExceeded,
    is_image_file,
};

// The attachment bounds are the runtime's admission bounds.
const _: () = assert!(MAX_IMAGE_BYTES == MAX_USER_IMAGE_BYTES);
const _: () = assert!(MAX_IMAGES == MAX_USER_IMAGES);
const _: () = assert!(plakous_image::MAX_MESSAGE_BYTES == MAX_USER_MESSAGE_BYTES);

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
    plakous_image::check_message_budget(text, images.into_iter().map(|image| image.bytes.len()))
}

#[cfg(feature = "images")]
mod normalize;
#[cfg(not(feature = "images"))]
#[path = "attachments/unavailable.rs"]
mod normalize;

pub use normalize::{normalize, normalize_rgba, normalize_tool};

/// Why an image could not be brought within the bounds.
#[derive(Debug)]
#[non_exhaustive]
pub enum NormalizeError {
    /// Image decoding was omitted from this build.
    Unavailable(crate::capabilities::CapabilityUnavailable),
    /// The normalizer refused the image; displayed as its own message.
    Image(plakous_image::NormalizeError),
}

impl fmt::Display for NormalizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(source) => write!(formatter, "{source}"),
            Self::Image(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for NormalizeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unavailable(source) => Some(source),
            // Transparent: the chain continues at the normalizer's source.
            Self::Image(error) => std::error::Error::source(error),
        }
    }
}

/// Why an image file could not be attached. Every file refusal names the
/// file.
#[derive(Debug)]
#[non_exhaustive]
pub enum ImageFileError {
    /// Image decoding was omitted from this build.
    Unavailable(crate::capabilities::CapabilityUnavailable),
    /// The file could not be read or normalized; displayed as its own message.
    File(plakous_image::ImageFileError),
}

impl fmt::Display for ImageFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(error) => write!(formatter, "{error}"),
            Self::File(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ImageFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unavailable(error) => Some(error),
            // Transparent: the chain continues at the reader's source.
            Self::File(error) => std::error::Error::source(error),
        }
    }
}

/// Reads one image file, sniffs its type from its bytes (the extension is
/// never trusted) and normalizes it into the bounds, as a draft named after
/// the file.
///
/// # Errors
///
/// Returns [`ImageFileError::Unavailable`] in a build without image decoding,
/// and otherwise [`ImageFileError::File`] naming the file when it cannot be
/// read, is not a file, exceeds [`MAX_SOURCE_BYTES`], or is not an image
/// within reach of the bounds.
pub fn read_image(path: &Path) -> Result<DraftImage, ImageFileError> {
    crate::capabilities::require_feature("images").map_err(ImageFileError::Unavailable)?;
    normalize::read_image(path)
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
mod tests;
