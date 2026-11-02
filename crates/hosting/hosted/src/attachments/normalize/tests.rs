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
        let (got_media, got_bytes) = normalize_tool(&bytes).unwrap();
        assert_eq!(got_media, media);
        assert_eq!(got_bytes, bytes);
    }

    let gif = animated_gif();
    let (media, bytes) = normalize_tool(&gif).unwrap();
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
    let (media, bytes) = normalize_tool(&wide).unwrap();
    assert_eq!(media, "image/png");
    let (width, height) = plakous::tools::sniff_image(&bytes)
        .unwrap()
        .dimensions()
        .unwrap();
    assert!(width <= MAX_IMAGE_EDGE && height <= MAX_IMAGE_EDGE);
    assert_ne!(bytes, wide);
}
