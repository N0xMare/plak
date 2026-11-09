use super::*;

#[cfg(feature = "images")]
#[test]
fn a_read_file_is_a_draft_named_after_it_and_refusals_name_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let shot = dir.path().join("shot.dat");
    std::fs::write(&shot, png(1280, 720, 412 * 1024)).unwrap();
    let image = read_image(&shot).unwrap();
    assert_eq!(image.media_type, "image/png");
    assert_eq!(image.name, "shot.dat");
    assert_eq!(image.source_dimensions, None);
    assert_eq!(chip(&image), "[image: shot.dat 1280x720 413 KB]");

    let missing = dir.path().join("missing.png");
    let error = read_image(&missing).unwrap_err();
    assert!(matches!(
        error,
        ImageFileError::File(plakous_image::ImageFileError::Unreadable { .. })
    ));
    assert!(
        error
            .to_string()
            .starts_with(&format!("image {}: cannot be read: ", missing.display()))
    );
}

#[cfg(not(feature = "images"))]
#[test]
fn a_build_without_image_decoding_refuses_every_file() {
    let error = read_image(Path::new("/nonexistent.png")).unwrap_err();
    assert!(matches!(error, ImageFileError::Unavailable(_)));
}

#[cfg(feature = "images")]
#[test]
fn downscaled_drafts_keep_their_name_and_source_dimensions() {
    use std::io::Cursor;
    let pixels = |width: u32, height: u32| {
        image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([
                u8::try_from(x * 255 / width).unwrap(),
                u8::try_from(y * 255 / height).unwrap(),
                128,
                255,
            ])
        })
    };
    let mut encoded = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(pixels(3456, 2234))
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let image = normalize("shot.png".to_owned(), encoded.into_inner()).unwrap();
    assert_eq!(image.name, "shot.png");
    assert_eq!(image.media_type, "image/png");
    assert_eq!(image.source_dimensions, Some((3456, 2234)));
    let chip = unnamed_chip(&image);
    assert!(chip.starts_with("[image: png 2048x1324 "), "{chip}");
    assert!(chip.ends_with(", downscaled from 3456x2234]"), "{chip}");

    let rgba = pixels(2600, 1300).into_raw();
    let image = normalize_rgba("clipboard.png".to_owned(), 2600, 1300, rgba).unwrap();
    assert_eq!(image.name, "clipboard.png");
    assert_eq!(image.source_dimensions, Some((2600, 1300)));
    let chip = super::chip(&image);
    assert!(
        chip.starts_with("[image: clipboard.png 2048x1024 "),
        "{chip}"
    );
    assert!(chip.ends_with(", downscaled from 2600x1300]"), "{chip}");
}

#[test]
fn the_message_budget_counts_each_draft_image_by_its_bytes() {
    let image = |extra: usize| DraftImage {
        name: "shot.png".into(),
        media_type: "image/png".into(),
        bytes: png(2, 3, extra),
        source_dimensions: None,
    };
    let maximum = image(MAX_IMAGE_BYTES - png(2, 3, 0).len());
    assert_eq!(
        check_message_budget("look", &vec![maximum.clone(); 3]),
        Ok(())
    );
    let error = check_message_budget("", &vec![maximum; 4]).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("20.0 MiB, over the 16 MiB per-message budget")
    );
}
