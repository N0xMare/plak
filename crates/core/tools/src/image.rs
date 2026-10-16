//! Byte-level image sniffing shared by every image-accepting surface.

use std::fmt;

/// Upper bound on `width * height` an image header may claim.
pub const MAX_IMAGE_PIXELS: u64 = 100_000_000;

/// The media type and, when the header carries them, the dimensions of an
/// image recognized from its leading bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageHeader {
    media_type: &'static str,
    dimensions: Option<(u32, u32)>,
}

impl ImageHeader {
    /// The sniffed media type: `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    #[must_use]
    pub const fn media_type(&self) -> &'static str {
        self.media_type
    }

    /// Width and height in pixels, when the header states them.
    #[must_use]
    pub const fn dimensions(&self) -> Option<(u32, u32)> {
        self.dimensions
    }
}

/// Why bytes were not accepted as an image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ImageSniffError {
    /// The bytes are not PNG, JPEG, GIF or WebP.
    Unsupported,
    /// The header claims a zero dimension or more than [`MAX_IMAGE_PIXELS`].
    Dimensions,
}

impl fmt::Display for ImageSniffError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unsupported => "not a supported image (png, jpeg, gif, webp)",
            Self::Dimensions => "image dimensions exceed the supported limit",
        })
    }
}

impl std::error::Error for ImageSniffError {}

/// Recognizes PNG, GIF, JPEG and WebP from their leading bytes.
///
/// # Errors
///
/// Returns [`ImageSniffError::Unsupported`] for any other content and
/// [`ImageSniffError::Dimensions`] for a header with a zero or oversized area.
pub fn sniff_image(bytes: &[u8]) -> Result<ImageHeader, ImageSniffError> {
    let header = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.len() >= 24 {
        ImageHeader {
            media_type: "image/png",
            dimensions: Some((
                u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
                u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
            )),
        }
    } else if (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) && bytes.len() >= 10 {
        ImageHeader {
            media_type: "image/gif",
            dimensions: Some((
                u32::from(u16::from_le_bytes([bytes[6], bytes[7]])),
                u32::from(u16::from_le_bytes([bytes[8], bytes[9]])),
            )),
        }
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        ImageHeader {
            media_type: "image/jpeg",
            dimensions: jpeg_dimensions(bytes),
        }
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        ImageHeader {
            media_type: "image/webp",
            dimensions: webp_dimensions(bytes),
        }
    } else {
        return Err(ImageSniffError::Unsupported);
    };
    if header.dimensions.is_some_and(|(width, height)| {
        width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    }) {
        return Err(ImageSniffError::Dimensions);
    }
    Ok(header)
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut offset = 2_usize;
    while offset.saturating_add(9) <= bytes.len() {
        if bytes[offset] != 0xff {
            offset = offset.saturating_add(1);
            continue;
        }
        let marker = bytes[offset.saturating_add(1)];
        offset = offset.saturating_add(2);
        if matches!(marker, 0xd8 | 0xd9) {
            continue;
        }
        if offset.saturating_add(2) > bytes.len() {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([bytes[offset], bytes[offset + 1]]));
        if length < 2 || offset.saturating_add(length) > bytes.len() {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) && length >= 7 {
            return Some((
                u32::from(u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]])),
                u32::from(u16::from_be_bytes([bytes[offset + 3], bytes[offset + 4]])),
            ));
        }
        offset = offset.saturating_add(length);
    }
    None
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 30 && &bytes[12..16] == b"VP8X" {
        let width = 1_u32
            .saturating_add(u32::from(bytes[24]))
            .saturating_add(u32::from(bytes[25]) << 8)
            .saturating_add(u32::from(bytes[26]) << 16);
        let height = 1_u32
            .saturating_add(u32::from(bytes[27]))
            .saturating_add(u32::from(bytes[28]) << 8)
            .saturating_add(u32::from(bytes[29]) << 16);
        Some((width, height))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&width.to_be_bytes());
        png.extend_from_slice(&height.to_be_bytes());
        png
    }

    #[test]
    fn every_supported_format_is_recognized_with_its_dimensions() {
        assert_eq!(
            sniff_image(&png(1280, 720)),
            Ok(ImageHeader {
                media_type: "image/png",
                dimensions: Some((1280, 720)),
            })
        );
        let gif = b"GIF89a\x40\x01\xf0\x00".to_vec();
        assert_eq!(
            sniff_image(&gif).map(|header| (header.media_type(), header.dimensions())),
            Ok(("image/gif", Some((320, 240))))
        );
        let mut jpeg = vec![
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x20, 0x00, 0x40,
        ];
        jpeg.extend_from_slice(&[0; 12]);
        assert_eq!(
            sniff_image(&jpeg).map(|header| (header.media_type(), header.dimensions())),
            Ok(("image/jpeg", Some((64, 32))))
        );
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend_from_slice(&[0; 8]);
        webp.extend_from_slice(&[9, 0, 0, 4, 0, 0]);
        assert_eq!(
            sniff_image(&webp).map(|header| (header.media_type(), header.dimensions())),
            Ok(("image/webp", Some((10, 5))))
        );
    }

    #[test]
    fn unsupported_bytes_and_degenerate_dimensions_are_refused() {
        assert_eq!(
            sniff_image(b"not-an-image"),
            Err(ImageSniffError::Unsupported)
        );
        assert_eq!(sniff_image(b""), Err(ImageSniffError::Unsupported));
        assert_eq!(sniff_image(&png(0, 10)), Err(ImageSniffError::Dimensions));
        assert_eq!(
            sniff_image(&png(100_000, 100_000)),
            Err(ImageSniffError::Dimensions)
        );
    }
}
