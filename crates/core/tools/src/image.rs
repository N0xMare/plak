//! Tool-result image values. Sniffing is `plakous-image`'s, re-exported here.

pub use plakous_image::{ImageHeader, ImageSniffError, sniff_image};

/// A secret-free image refusal, retaining the decoder's original error.
#[derive(Debug)]
pub struct ImageNormalizationError {
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl ImageNormalizationError {
    /// A policy refusal with no underlying decoder failure.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    /// A safe display message with the original decoder failure as its source.
    #[must_use]
    pub fn with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl std::fmt::Display for ImageNormalizationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ImageNormalizationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

impl From<String> for ImageNormalizationError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ImageNormalizationError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

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
    fn normalize(&self, bytes: &[u8]) -> Result<PreparedImage, ImageNormalizationError>;
}
