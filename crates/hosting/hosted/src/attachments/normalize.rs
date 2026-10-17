//! The one normalizer every attached image passes: an image within both
//! bounds is kept byte-identical; any other is decoded, downscaled with a
//! Lanczos3 filter so its long edge is at most [`MAX_IMAGE_EDGE`], and
//! re-encoded (PNG for a lossless source when it fits, otherwise JPEG),
//! shrinking further only while the encoding is still over the byte bound.

use std::borrow::Cow;
use std::fmt;
use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder as _, ImageFormat, Limits, RgbImage, RgbaImage};
use plakous_host_protocol::DraftImage;

use super::MAX_IMAGE_BYTES;

/// Longest edge, in pixels, of an image Plak re-encodes.
pub const MAX_IMAGE_EDGE: u32 = 2048;

const JPEG_QUALITY: u8 = 90;

/// Below this long edge a JPEG that still does not fit is refused.
const MIN_EDGE: u32 = 64;

/// Decoding one attached image may allocate at most this much, and neither
/// edge may exceed [`MAX_SOURCE_EDGE`].
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
const MAX_SOURCE_EDGE: u32 = 16_384;

/// Why an image could not be brought within the bounds.
#[derive(Debug)]
#[non_exhaustive]
pub enum NormalizeError {
    /// The bytes are not a png, jpeg, gif or webp image.
    NotAnImage(plakous::tools::ImageSniffError),
    /// The image could not be decoded, or its decoded size exceeds what Plak
    /// decodes.
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

/// Normalizes encoded image bytes attached under `name`.
///
/// # Errors
///
/// Returns [`NormalizeError`] when the bytes are not a supported image,
/// cannot be decoded, or cannot be brought within the bounds.
pub fn normalize(name: String, bytes: Vec<u8>) -> Result<DraftImage, NormalizeError> {
    let header = plakous::tools::sniff_image(&bytes).map_err(NormalizeError::NotAnImage)?;
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
        None => Ok(DraftImage {
            name,
            media_type: media_type.to_owned(),
            bytes,
            source_dimensions: None,
        }),
        Some(image) => fit(
            name,
            &image,
            matches!(media_type, "image/png" | "image/gif"),
        ),
    }
}

/// Normalizes raw RGBA pixels (a clipboard image) into a PNG attachment,
/// or a JPEG when no PNG within the edge bound fits.
///
/// # Errors
///
/// Returns [`NormalizeError::Malformed`] when the pixel buffer does not
/// match the dimensions.
pub fn normalize_rgba(
    name: String,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> Result<DraftImage, NormalizeError> {
    let image = RgbaImage::from_raw(width, height, rgba)
        .filter(|_| width > 0 && height > 0)
        .ok_or(NormalizeError::Malformed { width, height })?;
    fit(name, &DynamicImage::ImageRgba8(image), true)
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
fn fit(name: String, image: &DynamicImage, lossless: bool) -> Result<DraftImage, NormalizeError> {
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
                .filter(|png| png.len() <= MAX_IMAGE_BYTES)
                .map(|png| ("image/png", png))
        } else {
            None
        };
        let encoded = match encoded {
            Some(encoded) => Some(encoded),
            None => Some(encode_jpeg(&scaled)?)
                .filter(|jpeg| jpeg.len() <= MAX_IMAGE_BYTES)
                .map(|jpeg| ("image/jpeg", jpeg)),
        };
        if let Some((media_type, bytes)) = encoded {
            let dimensions = (scaled.width(), scaled.height());
            return Ok(DraftImage {
                name,
                media_type: media_type.to_owned(),
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
        .write_with_encoder(PngEncoder::new(&mut bytes))
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
mod tests {
    use super::*;

    fn noise(width: u32, height: u32) -> RgbaImage {
        let mut state = 0x2545_f491_u32;
        RgbaImage::from_fn(width, height, |_, _| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let [red, green, blue, _] = state.to_le_bytes();
            image::Rgba([red, green, blue, 255])
        })
    }

    fn gradient(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            let red = u8::try_from(x * 255 / width).unwrap();
            let green = u8::try_from(y * 255 / height).unwrap();
            image::Rgba([red, green, 128, 255])
        })
    }

    fn encoded(image: RgbaImage, format: ImageFormat) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        let image = DynamicImage::ImageRgba8(image);
        let image = if format == ImageFormat::Jpeg {
            DynamicImage::ImageRgb8(image.to_rgb8())
        } else {
            image
        };
        image.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    fn dimensions(image: &DraftImage) -> (u32, u32) {
        plakous::tools::sniff_image(&image.bytes)
            .unwrap()
            .dimensions()
            .unwrap()
    }

    #[test]
    fn an_image_within_both_bounds_is_kept_byte_identical() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Gif] {
            let bytes = encoded(gradient(2048, 600), format);
            let image = normalize("shot".to_owned(), bytes.clone()).unwrap();
            assert_eq!(image.bytes, bytes, "{format:?}");
            assert_eq!(image.source_dimensions, None);
            assert_eq!(image.name, "shot");
        }
    }

    #[test]
    fn a_long_edge_over_the_bound_is_downscaled_keeping_png_and_the_aspect_ratio() {
        let image = normalize(
            "shot.png".to_owned(),
            encoded(gradient(3456, 2234), ImageFormat::Png),
        )
        .unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(dimensions(&image), (2048, 1324));
        assert_eq!(image.source_dimensions, Some((3456, 2234)));
        assert_eq!(image.name, "shot.png");
    }

    #[test]
    fn a_lossy_source_over_the_edge_bound_becomes_a_jpeg() {
        let image = normalize(
            "photo.jpg".to_owned(),
            encoded(gradient(1000, 4000), ImageFormat::Jpeg),
        )
        .unwrap();
        assert_eq!(image.media_type, "image/jpeg");
        assert_eq!(dimensions(&image), (512, 2048));
        assert_eq!(image.source_dimensions, Some((1000, 4000)));
    }

    #[test]
    fn bytes_over_the_bound_fall_back_to_jpeg_then_shrink_until_they_fit() {
        // Incompressible pixels: no PNG within the edge bound fits.
        let bytes = encoded(noise(1500, 1500), ImageFormat::Png);
        assert!(bytes.len() > MAX_IMAGE_BYTES);
        let image = normalize("noise.png".to_owned(), bytes).unwrap();
        assert_eq!(image.media_type, "image/jpeg");
        assert!(image.bytes.len() <= MAX_IMAGE_BYTES);
        let (width, height) = dimensions(&image);
        assert_eq!(width, height);
        assert!(width <= 1500);

        let rgba = noise(2600, 2600).into_raw();
        let image = normalize_rgba("clipboard.png".to_owned(), 2600, 2600, rgba).unwrap();
        assert!(image.bytes.len() <= MAX_IMAGE_BYTES);
        assert!(dimensions(&image).0 <= MAX_IMAGE_EDGE);
        assert_eq!(image.source_dimensions, Some((2600, 2600)));
    }

    #[test]
    fn clipboard_pixels_are_png_and_malformed_or_foreign_bytes_are_refused() {
        let image = normalize_rgba("clipboard.png".to_owned(), 3, 2, vec![200; 24]).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(dimensions(&image), (3, 2));
        assert_eq!(image.source_dimensions, None);

        assert!(matches!(
            normalize_rgba("c".to_owned(), 2, 2, vec![0; 3]),
            Err(NormalizeError::Malformed { .. })
        ));
        assert!(matches!(
            normalize_rgba("c".to_owned(), 0, 0, Vec::new()),
            Err(NormalizeError::Malformed { .. })
        ));
        assert!(matches!(
            normalize("notes".to_owned(), b"not an image".to_vec()),
            Err(NormalizeError::NotAnImage(_))
        ));
        // A valid header over the bound whose body does not decode.
        assert!(matches!(
            normalize("big.png".to_owned(), super::super::png(4000, 2, 16)),
            Err(NormalizeError::Undecodable(_))
        ));
    }

    #[test]
    fn transparency_is_composed_over_white_for_jpeg() {
        let image =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 0])));
        assert_eq!(opaque(&image).get_pixel(0, 0).0, [255, 255, 255]);
    }
}
