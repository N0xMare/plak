//! An image on the system clipboard (Ctrl+V, or the empty paste a terminal
//! sends for an image-only clipboard) as a PNG attachment.

use plakous_host_protocol::DraftImage;

use crate::config::image_attachments::MAX_IMAGE_BYTES;

/// Name the chip shows for a pasted clipboard image.
const CLIPBOARD_IMAGE_NAME: &str = "clipboard.png";

/// Reads the clipboard's image and encodes it as PNG.
pub(super) fn read_image() -> Result<DraftImage, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("clipboard unavailable: {error}"))?;
    let image = clipboard.get_image().map_err(|error| match error {
        arboard::Error::ContentNotAvailable => "the clipboard holds no image".to_owned(),
        other => format!("clipboard image unreadable: {other}"),
    })?;
    encode_png(image.width, image.height, image.bytes.into_owned())
}

/// Encodes RGBA pixels as PNG, halving the image until it fits the
/// per-image bound: a full Retina screenshot may not.
pub(super) fn encode_png(
    mut width: usize,
    mut height: usize,
    mut rgba: Vec<u8>,
) -> Result<DraftImage, String> {
    if width == 0
        || height == 0
        || width.checked_mul(height).and_then(|n| n.checked_mul(4)) != Some(rgba.len())
    {
        return Err(format!("clipboard image is malformed ({width}x{height})"));
    }
    loop {
        let bytes = png_bytes(width, height, &rgba)?;
        if bytes.len() <= MAX_IMAGE_BYTES {
            return Ok(DraftImage {
                name: CLIPBOARD_IMAGE_NAME.to_owned(),
                media_type: "image/png".to_owned(),
                bytes,
            });
        }
        if width < 2 || height < 2 {
            return Err("clipboard image does not fit 3.75 MiB as PNG".to_owned());
        }
        (width, height, rgba) = halve(width, height, &rgba);
    }
}

fn png_bytes(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let dimension = |value: usize| {
        u32::try_from(value).map_err(|_| format!("clipboard image is too large ({width}x{height})"))
    };
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, dimension(width)?, dimension(height)?);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let failed =
        |error: png::EncodingError| format!("clipboard image could not be encoded: {error}");
    let mut writer = encoder.write_header().map_err(failed)?;
    writer.write_image_data(rgba).map_err(failed)?;
    writer.finish().map_err(failed)?;
    Ok(bytes)
}

/// Half the size: each output pixel averages a 2x2 block.
fn halve(width: usize, height: usize, rgba: &[u8]) -> (usize, usize, Vec<u8>) {
    let (half_width, half_height) = (width / 2, height / 2);
    let mut out = Vec::with_capacity(half_width * half_height * 4);
    for y in 0..half_height {
        for x in 0..half_width {
            for channel in 0..4 {
                let at = |dx: usize, dy: usize| {
                    u32::from(rgba[((2 * y + dy) * width + 2 * x + dx) * 4 + channel])
                };
                let sum = at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1);
                out.push(u8::try_from(sum / 4).unwrap_or(u8::MAX));
            }
        }
    }
    (half_width, half_height, out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests may assert with unwrap")]
mod tests {
    use super::*;

    #[test]
    fn clipboard_pixels_become_a_png_attachment_that_fits_the_bound() {
        let image = encode_png(3, 2, vec![200; 3 * 2 * 4]).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.name, "clipboard.png");
        assert!(
            crate::config::image_attachments::chip(&image)
                .starts_with("[image: clipboard.png 3x2 ")
        );

        // Incompressible pixels over the bound are halved until they fit.
        let (width, height) = (1100, 900);
        let mut state = 0x2545_f491_u32;
        let noise = (0..width * height * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            })
            .collect();
        let image = encode_png(width, height, noise).unwrap();
        assert!(image.bytes.len() <= MAX_IMAGE_BYTES);
        assert!(
            crate::config::image_attachments::chip(&image)
                .starts_with("[image: clipboard.png 550x450 ")
        );

        assert!(encode_png(2, 2, vec![0; 3]).is_err());
        assert!(encode_png(0, 0, Vec::new()).is_err());
    }
}
