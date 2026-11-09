//! The one normalizer every attached image passes: an image within both
//! bounds is kept byte-identical; any other is decoded, downscaled with a
//! Lanczos3 filter so its long edge is at most [`MAX_IMAGE_EDGE`], and
//! re-encoded (PNG for a lossless source when it fits, otherwise JPEG),
//! shrinking further only while the encoding is still over the byte bound.

use std::borrow::Cow;
use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder as _, ImageFormat, Limits, RgbImage, RgbaImage};

use crate::{MAX_IMAGE_BYTES, MAX_IMAGE_EDGE, NormalizeError, NormalizedImage, sniff_image};

mod read;

pub use read::read_image;

const JPEG_QUALITY: u8 = 90;

/// Below this long edge a JPEG that still does not fit is refused.
const MIN_EDGE: u32 = 64;

/// Decoding one attached image may allocate at most this much, and neither
/// edge may exceed [`MAX_SOURCE_EDGE`].
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
const MAX_SOURCE_EDGE: u32 = 16_384;

/// Normalizes encoded image bytes for an attachment.
///
/// # Errors
///
/// Returns [`NormalizeError`] when the bytes are not a supported image,
/// cannot be decoded, or cannot be brought within the bounds.
pub fn normalize(bytes: Vec<u8>) -> Result<NormalizedImage, NormalizeError> {
    let header = sniff_image(&bytes).map_err(NormalizeError::NotAnImage)?;
    let media_type = header.media_type();
    let fits = bytes.len() <= MAX_IMAGE_BYTES;
    let image = if fits && header.dimensions().is_some_and(within_edge) {
        None
    } else {
        // A header that states no dimensions is decoded to learn them.
        Some(decode(&bytes, media_type)?)
            .filter(|image| !fits || !within_edge((image.width(), image.height())))
    };
    match image {
        None => Ok(NormalizedImage {
            media_type,
            bytes,
            source_dimensions: None,
        }),
        Some(image) => fit(
            &image,
            matches!(media_type, "image/png" | "image/gif"),
            MAX_IMAGE_BYTES,
        ),
    }
}

/// Normalizes one workspace image for a tool result.
///
/// The long-edge rule is the attachment rule. The byte budget is the
/// tool-result JSON limit, not the 3.75 MiB attachment budget: a tool result
/// is measured as JSON, and image bytes are base64 inside that JSON.
///
/// # Errors
///
/// Returns [`NormalizeError`] when the bytes are not a supported image or
/// cannot be brought within the tool-result budget.
pub fn normalize_tool(bytes: &[u8]) -> Result<NormalizedImage, NormalizeError> {
    let header = sniff_image(bytes).map_err(NormalizeError::NotAnImage)?;
    let media_type = header.media_type();
    // Always decode. A header that does not decode is a refusal, never a
    // pass-through of the original bytes. GIF keeps only its first frame.
    let image = decode(bytes, media_type)?;
    let fits = bytes.len() <= TOOL_IMAGE_RAW_BYTES && within_edge((image.width(), image.height()));
    if fits && matches!(media_type, "image/png" | "image/jpeg" | "image/webp") {
        return Ok(NormalizedImage {
            media_type,
            bytes: bytes.to_vec(),
            source_dimensions: None,
        });
    }
    fit(
        &image,
        matches!(media_type, "image/png" | "image/gif" | "image/webp"),
        TOOL_IMAGE_RAW_BYTES,
    )
}

/// Raw image bytes whose standard base64 fits a 1 MiB tool-result JSON part,
/// leaving room for the part envelope.
const TOOL_IMAGE_RAW_BYTES: usize = 786_048;

/// Normalizes raw RGBA pixels (a clipboard image) into a PNG attachment,
/// or a JPEG when no PNG within the edge bound fits.
///
/// # Errors
///
/// Returns [`NormalizeError::Malformed`] when the pixel buffer does not
/// match the dimensions.
pub fn normalize_rgba(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> Result<NormalizedImage, NormalizeError> {
    let image = RgbaImage::from_raw(width, height, rgba)
        .filter(|_| width > 0 && height > 0)
        .ok_or(NormalizeError::Malformed { width, height })?;
    fit(&DynamicImage::ImageRgba8(image), true, MAX_IMAGE_BYTES)
}

fn within_edge((width, height): (u32, u32)) -> bool {
    width.max(height) <= MAX_IMAGE_EDGE
}

fn decode(bytes: &[u8], media_type: &str) -> Result<DynamicImage, NormalizeError> {
    let format = match media_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        _ => ImageFormat::WebP,
    };
    let undecodable = |error: image::ImageError| NormalizeError::Undecodable(Box::new(error));
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits.max_image_width = Some(MAX_SOURCE_EDGE);
    limits.max_image_height = Some(MAX_SOURCE_EDGE);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(undecodable)?;
    // Re-encoding drops the metadata, so the stored orientation is applied
    // to the pixels first.
    let orientation = decoder.orientation().map_err(undecodable)?;
    // A gif decodes to its first frame.
    let mut image = DynamicImage::from_decoder(decoder).map_err(undecodable)?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// Encodes `image` at the largest long edge, at most [`MAX_IMAGE_EDGE`],
/// whose encoding fits the byte bound.
fn fit(
    image: &DynamicImage,
    lossless: bool,
    max_bytes: usize,
) -> Result<NormalizedImage, NormalizeError> {
    let source = (image.width(), image.height());
    let mut edge = source.0.max(source.1).min(MAX_IMAGE_EDGE);
    loop {
        let scaled = if source.0.max(source.1) > edge {
            Cow::Owned(image.resize(edge, edge, FilterType::Lanczos3))
        } else {
            Cow::Borrowed(image)
        };
        let encoded = if lossless {
            Some(encode_png(&scaled)?)
                .filter(|png| png.len() <= max_bytes)
                .map(|png| ("image/png", png))
        } else {
            None
        };
        let encoded = match encoded {
            Some(encoded) => Some(encoded),
            None => Some(encode_jpeg(&scaled)?)
                .filter(|jpeg| jpeg.len() <= max_bytes)
                .map(|jpeg| ("image/jpeg", jpeg)),
        };
        if let Some((media_type, bytes)) = encoded {
            let dimensions = (scaled.width(), scaled.height());
            return Ok(NormalizedImage {
                media_type,
                bytes,
                source_dimensions: (dimensions != source).then_some(source),
            });
        }
        if edge <= MIN_EDGE {
            return Err(NormalizeError::Unfit);
        }
        edge = (edge * 3 / 4).max(MIN_EDGE);
    }
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, NormalizeError> {
    let mut bytes = Vec::new();
    image
        // Fast compression: an interactive paste waits on this encode.
        .write_with_encoder(PngEncoder::new_with_quality(
            &mut bytes,
            CompressionType::Fast,
            PngFilter::Adaptive,
        ))
        .map_err(|error| NormalizeError::Undecodable(Box::new(error)))?;
    Ok(bytes)
}

fn encode_jpeg(image: &DynamicImage) -> Result<Vec<u8>, NormalizeError> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
        .encode_image(&opaque(image))
        .map_err(|error| NormalizeError::Undecodable(Box::new(error)))?;
    Ok(bytes)
}

/// JPEG has no alpha: transparent pixels are composed over white.
fn opaque(image: &DynamicImage) -> RgbImage {
    if !image.color().has_alpha() {
        return image.to_rgb8();
    }
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [red, green, blue, alpha] = rgba.get_pixel(x, y).0;
        let over_white = |channel: u8| {
            let alpha = u16::from(alpha);
            let mixed = (u16::from(channel) * alpha + 255 * (255 - alpha)) / 255;
            u8::try_from(mixed).unwrap_or(u8::MAX)
        };
        image::Rgb([over_white(red), over_white(green), over_white(blue)])
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests;
