//! Image attachment rules without a runtime: recognize png, jpeg, gif and
//! webp from their leading bytes, hold a message to its image and byte
//! budgets and, with the `normalize` feature, read a caller-named file and
//! bring an image within the attachment or tool-result bounds.

use std::fmt;

mod budget;
mod file;
mod sniff;

pub use budget::{MAX_IMAGES, MAX_MESSAGE_BYTES, MessageBudgetExceeded, check_message_budget};
pub use file::{ImageFileError, is_image_file};
pub use sniff::{ImageHeader, ImageSniffError, MAX_IMAGE_PIXELS, sniff_image};

#[cfg(feature = "normalize")]
pub mod normalize;

/// Largest raw image one attachment carries (3.75 MiB), so its base64 form
/// stays within the most restrictive provider bound of 5 MB.
pub const MAX_IMAGE_BYTES: usize = 3_932_160;

/// Longest edge, in pixels, of an image that is re-encoded.
pub const MAX_IMAGE_EDGE: u32 = 2048;

/// Largest image file read for normalizing (64 MiB).
pub const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

/// An image within the bounds: the source bytes unchanged, or a re-encoding.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct NormalizedImage {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub media_type: &'static str,
    /// The encoded image.
    pub bytes: Vec<u8>,
    /// The source's width and height when the image was downscaled.
    pub source_dimensions: Option<(u32, u32)>,
}

/// Why an image could not be brought within the bounds.
#[derive(Debug)]
#[non_exhaustive]
pub enum NormalizeError {
    /// The bytes are not a png, jpeg, gif or webp image.
    NotAnImage(ImageSniffError),
    /// The image could not be decoded, or its decoded size exceeds the
    /// decode limits.
    Undecodable(Box<dyn std::error::Error + Send + Sync>),
    /// Raw pixels whose length does not match their dimensions.
    Malformed {
        /// Stated width.
        width: u32,
        /// Stated height.
        height: u32,
    },
    /// Even a small JPEG exceeds the byte bound.
    Unfit,
}

impl fmt::Display for NormalizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnImage(source) => write!(formatter, "{source}"),
            Self::Undecodable(source) => write!(formatter, "cannot be decoded: {source}"),
            Self::Malformed { width, height } => {
                write!(formatter, "image data is malformed ({width}x{height})")
            }
            Self::Unfit => write!(
                formatter,
                "does not fit {MAX_IMAGE_BYTES} bytes (3.75 MiB) even downscaled"
            ),
        }
    }
}

impl std::error::Error for NormalizeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotAnImage(source) => Some(source),
            Self::Undecodable(source) => Some(source.as_ref()),
            Self::Malformed { .. } | Self::Unfit => None,
        }
    }
}
