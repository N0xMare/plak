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

fn dimensions(image: &NormalizedImage) -> (u32, u32) {
    sniff_image(&image.bytes).unwrap().dimensions().unwrap()
}

#[test]
fn an_image_within_both_bounds_is_kept_byte_identical() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Gif] {
        let bytes = encoded(gradient(2048, 600), format);
        let image = normalize(bytes.clone()).unwrap();
        assert_eq!(image.bytes, bytes, "{format:?}");
        assert_eq!(image.source_dimensions, None);
    }
}

#[test]
fn a_long_edge_over_the_bound_is_downscaled_keeping_png_and_the_aspect_ratio() {
    let image = normalize(encoded(gradient(3456, 2234), ImageFormat::Png)).unwrap();
    assert_eq!(image.media_type, "image/png");
    assert_eq!(dimensions(&image), (2048, 1324));
    assert_eq!(image.source_dimensions, Some((3456, 2234)));
}

#[test]
fn a_lossy_source_over_the_edge_bound_becomes_a_jpeg() {
    let image = normalize(encoded(gradient(1000, 4000), ImageFormat::Jpeg)).unwrap();
    assert_eq!(image.media_type, "image/jpeg");
    assert_eq!(dimensions(&image), (512, 2048));
    assert_eq!(image.source_dimensions, Some((1000, 4000)));
}

#[test]
fn bytes_over_the_bound_fall_back_to_jpeg_then_shrink_until_they_fit() {
    // Incompressible pixels: no PNG within the edge bound fits.
    let bytes = encoded(noise(1500, 1500), ImageFormat::Png);
    assert!(bytes.len() > MAX_IMAGE_BYTES);
    let image = normalize(bytes).unwrap();
    assert_eq!(image.media_type, "image/jpeg");
    assert!(image.bytes.len() <= MAX_IMAGE_BYTES);
    let (width, height) = dimensions(&image);
    assert_eq!(width, height);
    assert!(width <= 1500);

    let rgba = noise(2600, 2600).into_raw();
    let image = normalize_rgba(2600, 2600, rgba).unwrap();
    assert!(image.bytes.len() <= MAX_IMAGE_BYTES);
    assert!(dimensions(&image).0 <= MAX_IMAGE_EDGE);
    assert_eq!(image.source_dimensions, Some((2600, 2600)));
}

#[test]
fn clipboard_pixels_are_png_and_malformed_or_foreign_bytes_are_refused() {
    let image = normalize_rgba(3, 2, vec![200; 24]).unwrap();
    assert_eq!(image.media_type, "image/png");
    assert_eq!(dimensions(&image), (3, 2));
    assert_eq!(image.source_dimensions, None);

    assert!(matches!(
        normalize_rgba(2, 2, vec![0; 3]),
        Err(NormalizeError::Malformed { .. })
    ));
    assert!(matches!(
        normalize_rgba(0, 0, Vec::new()),
        Err(NormalizeError::Malformed { .. })
    ));
    assert!(matches!(
        normalize(b"not an image".to_vec()),
        Err(NormalizeError::NotAnImage(_))
    ));
    // A valid header over the bound whose body does not decode.
    assert!(matches!(
        normalize(png_header(4000, 2, 16)),
        Err(NormalizeError::Undecodable(_))
    ));
}

#[test]
fn transparency_is_composed_over_white_for_jpeg() {
    let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 0])));
    assert_eq!(opaque(&image).get_pixel(0, 0).0, [255, 255, 255]);
}

fn animated_gif() -> Vec<u8> {
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame};
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = GifEncoder::new(&mut bytes);
        encoder.set_repeat(Repeat::Infinite).unwrap();
        encoder
            .encode_frame(Frame::from_parts(
                noise(4, 4),
                0,
                0,
                Delay::from_numer_denom_ms(100, 1),
            ))
            .unwrap();
        encoder
            .encode_frame(Frame::from_parts(
                gradient(4, 4),
                0,
                0,
                Delay::from_numer_denom_ms(100, 1),
            ))
            .unwrap();
    }
    bytes.into_inner()
}

#[test]
fn tool_images_always_decode_pass_through_only_clean_fitting_still_frames() {
    for (format, media) in [
        (ImageFormat::Png, "image/png"),
        (ImageFormat::Jpeg, "image/jpeg"),
        (ImageFormat::WebP, "image/webp"),
    ] {
        let bytes = encoded(noise(4, 4), format);
        let image = normalize_tool(&bytes).unwrap();
        assert_eq!(image.media_type, media);
        assert_eq!(image.bytes, bytes);
    }

    let gif = animated_gif();
    let NormalizedImage {
        media_type: media,
        bytes,
        ..
    } = normalize_tool(&gif).unwrap();
    assert_eq!(media, "image/png");
    assert!(bytes.starts_with(b"\x89PNG"));
    assert_ne!(bytes, gif);

    let png = encoded(noise(4, 4), ImageFormat::Png);
    let mut truncated = png.clone();
    truncated.truncate(40);
    assert!(matches!(
        normalize_tool(&truncated),
        Err(NormalizeError::Undecodable(_))
    ));
    let mut garbage = png[..24].to_vec();
    garbage.extend_from_slice(&[0; 32]);
    assert!(matches!(
        normalize_tool(&garbage),
        Err(NormalizeError::Undecodable(_))
    ));

    let wide = encoded(noise(2050, 2), ImageFormat::Png);
    let NormalizedImage {
        media_type: media,
        bytes,
        ..
    } = normalize_tool(&wide).unwrap();
    assert_eq!(media, "image/png");
    let (width, height) = sniff_image(&bytes).unwrap().dimensions().unwrap();
    assert!(width <= MAX_IMAGE_EDGE && height <= MAX_IMAGE_EDGE);
    assert_ne!(bytes, wide);
}

fn png_header(width: u32, height: u32, extra: usize) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0, 0, 0, 13]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&width.to_be_bytes());
    png.extend_from_slice(&height.to_be_bytes());
    png.resize(png.len() + extra, 0);
    png
}

/// Exact encodings of images the normalizer passes through or encodes
/// without resizing: media type, source dimensions and the sha256 of the
/// bytes. Resizing uses floating-point filters whose results may differ by
/// platform, so resized outputs are held to their bounds below instead.
const UNRESIZED_OUTPUTS: [&str; 5] = [
    "passthrough-png image/png None 54c21ec5bb0afbc278d72cfedde4b5f4c7383c87d390f7e97caccadb6114b440",
    "passthrough-jpeg image/jpeg None 370c95687fb91fc7d40e07739d82a69ca137905094c2bb5f60541b0fd4a6bed8",
    "small-rgba image/png None 6024e1e59a9e37d54aec68273c10ad2f1d239cadfa991ffc5809dbfcc9b7409d",
    "photo-rgba image/png None 40ea3e66fca84e02d1fc73b5edea1e42f96d7251fd3a684c2a769ebccf983064",
    "tool-gif image/png None 539c7cfbbe985e9879365f7e9eda5fffc706447f40447eaed8d6fbd15fe06cb1",
];

#[test]
fn unresized_images_keep_their_pinned_encodings() {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    let line = |name: &str, image: NormalizedImage| {
        let hex = sha2::Sha256::digest(&image.bytes)
            .iter()
            .fold(String::new(), |mut hex, byte| {
                write!(hex, "{byte:02x}").unwrap();
                hex
            });
        format!(
            "{name} {} {:?} {hex}",
            image.media_type, image.source_dimensions
        )
    };
    let actual = [
        line(
            "passthrough-png",
            normalize(encoded(gradient(2048, 600), ImageFormat::Png)).unwrap(),
        ),
        line(
            "passthrough-jpeg",
            normalize(encoded(gradient(2048, 600), ImageFormat::Jpeg)).unwrap(),
        ),
        line("small-rgba", normalize_rgba(3, 2, vec![200; 24]).unwrap()),
        line(
            "photo-rgba",
            normalize_rgba(1920, 1080, gradient(1920, 1080).into_raw()).unwrap(),
        ),
        line("tool-gif", normalize_tool(&animated_gif()).unwrap()),
    ];
    assert_eq!(actual, UNRESIZED_OUTPUTS);
}

#[test]
fn resized_outputs_keep_their_media_type_source_dimensions_and_bounds() {
    let within = |image: &NormalizedImage, max_bytes: usize| {
        let (width, height) = dimensions(image);
        assert!(width.max(height) <= MAX_IMAGE_EDGE, "{width}x{height}");
        assert!(image.bytes.len() <= max_bytes, "{}", image.bytes.len());
        (width, height)
    };
    let image = normalize(encoded(gradient(2600, 300), ImageFormat::Gif)).unwrap();
    assert_eq!(image.media_type, "image/png");
    assert_eq!(image.source_dimensions, Some((2600, 300)));
    let (width, height) = within(&image, MAX_IMAGE_BYTES);
    assert_eq!(width, 2048);
    assert!((236..=237).contains(&height), "{height}");

    let image = normalize_rgba(2600, 2600, noise(2600, 2600).into_raw()).unwrap();
    assert_eq!(image.media_type, "image/jpeg");
    assert_eq!(image.source_dimensions, Some((2600, 2600)));
    let (width, height) = within(&image, MAX_IMAGE_BYTES);
    assert_eq!(width, height);

    let image = normalize_tool(&encoded(noise(1200, 1200), ImageFormat::Png)).unwrap();
    assert_eq!(image.media_type, "image/jpeg");
    assert_eq!(image.source_dimensions, Some((1200, 1200)));
    let (width, height) = within(&image, TOOL_IMAGE_RAW_BYTES);
    assert_eq!(width, height);
    assert!(width < 1200);
}
