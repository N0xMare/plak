//! Attachment and tool-result normalization through `plakous-image`, the one
//! normalizer; this module only names the draft.

use std::path::Path;

use plakous_host_protocol::DraftImage;
use plakous_image::normalize as normalizer;

use super::{ImageFileError, NormalizeError};

/// Normalizes encoded image bytes attached under `name`.
///
/// # Errors
///
/// Returns [`NormalizeError::Image`] when the bytes are not a supported
/// image, cannot be decoded, or cannot be brought within the bounds.
pub fn normalize(name: String, bytes: Vec<u8>) -> Result<DraftImage, NormalizeError> {
    normalizer::normalize(bytes)
        .map(|image| draft(name, image))
        .map_err(NormalizeError::Image)
}

/// Normalizes one workspace image for a tool result, returning its media
/// type and bytes.
///
/// # Errors
///
/// Returns [`NormalizeError::Image`] when the bytes are not a supported image
/// or cannot be brought within the tool-result budget.
pub fn normalize_tool(bytes: &[u8]) -> Result<(String, Vec<u8>), NormalizeError> {
    normalizer::normalize_tool(bytes)
        .map(|image| (image.media_type.to_owned(), image.bytes))
        .map_err(NormalizeError::Image)
}

/// Normalizes raw RGBA pixels (a clipboard image) into a PNG attachment,
/// or a JPEG when no PNG within the edge bound fits.
///
/// # Errors
///
/// Returns [`NormalizeError::Image`] when the pixel buffer does not match the
/// dimensions or cannot be brought within the bounds.
pub fn normalize_rgba(
    name: String,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> Result<DraftImage, NormalizeError> {
    normalizer::normalize_rgba(width, height, rgba)
        .map(|image| draft(name, image))
        .map_err(NormalizeError::Image)
}

pub(super) fn read_image(path: &Path) -> Result<DraftImage, ImageFileError> {
    normalizer::read_image(path)
        .map(|(name, image)| draft(name, image))
        .map_err(ImageFileError::File)
}

fn draft(name: String, image: plakous_image::NormalizedImage) -> DraftImage {
    DraftImage {
        name,
        media_type: image.media_type.to_owned(),
        bytes: image.bytes,
        source_dimensions: image.source_dimensions,
    }
}
