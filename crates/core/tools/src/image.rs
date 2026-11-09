//! Tool-result image values. Sniffing is `plakous-image`'s, re-exported here.

pub use plakous_image::{ImageHeader, ImageSniffError, sniff_image};

/// Pixels a tool-result image carries after normalization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedImage {
    media_type: String,
    bytes: Vec<u8>,
}

impl PreparedImage {
    /// One normalized image.
    #[must_use]
    pub fn new(media_type: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            media_type: media_type.into(),
            bytes,
        }
    }

    /// The sniffed or re-encoded media type.
    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// The image bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Decodes and downscales a workspace image for a tool result.
///
/// The hosted build installs the shared Lanczos normalizer. A build without
/// image decoding leaves the hook unset, and the tool refuses instead of
/// returning the raw file.
pub trait ImageNormalizer: Send + Sync {
    /// Returns bytes that fit the tool-result image budget.
    ///
    /// # Errors
    ///
    /// Returns a secret-free reason when the bytes are not a supported image
    /// or cannot be brought within the budget.
    fn normalize(&self, bytes: &[u8]) -> Result<PreparedImage, String>;
}
