use super::*;

fn png(extra: usize) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0, 0, 0, 13]);
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&2_u32.to_be_bytes());
    png.extend_from_slice(&3_u32.to_be_bytes());
    png.resize(png.len() + extra, 0);
    png
}

fn image(bytes: Vec<u8>) -> UserPart {
    UserPart::Image {
        media_type: "image/png".into(),
        bytes,
    }
}

/// A maximum-size PNG of incompressible bytes, so its base64 form is as
/// large as a real photograph's.
fn random_maximum_png(seed: u64) -> UserPart {
    let mut state = seed | 1;
    let mut bytes = png(0);
    while bytes.len() < MAX_USER_IMAGE_BYTES {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(MAX_USER_IMAGE_BYTES);
    image(bytes)
}

#[test]
fn three_maximum_random_images_fit_one_message_and_four_exceed_its_budget()
-> Result<(), Box<dyn std::error::Error>> {
    let images = |count: u64| {
        let mut parts = vec![UserPart::Text("what is in these?".into())];
        parts.extend((0..count).map(|seed| random_maximum_png(0x9e37_79b9_7f4a_7c15 ^ seed)));
        UserInput::new(parts)
    };
    let three = images(3);
    three.validate("prompt")?;
    assert_eq!(three.content_blocks()?.len(), 4);

    let error = images(4)
        .validate("prompt")
        .err()
        .ok_or("four maximum-size images must exceed the per-message budget")?;
    assert_eq!(error.kind(), ControlErrorKind::InvalidInput);
    let message = error.to_string();
    assert!(message.contains("per-message budget"), "{message}");
    assert!(
        message.contains(&MAX_USER_MESSAGE_BYTES.to_string()),
        "{message}"
    );
    assert!(message.contains("three maximum-size images"), "{message}");
    Ok(())
}

#[test]
fn admission_bounds_count_size_type_and_emptiness() {
    let ok = UserInput::new(vec![UserPart::Text("look".into()), image(png(0))]);
    assert!(ok.validate("prompt").is_ok());
    assert!(
        UserInput::new(vec![image(png(0))])
            .validate("prompt")
            .is_ok()
    );

    let at_limit = png(0).len();
    let largest = png(MAX_USER_IMAGE_BYTES - at_limit);
    assert!(
        UserInput::new(vec![image(largest)])
            .validate("prompt")
            .is_ok()
    );
    let oversize = UserInput::new(vec![image(png(MAX_USER_IMAGE_BYTES - at_limit + 1))]);
    let error = oversize
        .validate("prompt")
        .err()
        .map(|error| error.to_string());
    assert!(error.is_some_and(|message| message.contains("downscale")));

    let eight = UserInput::new((0..MAX_USER_IMAGES).map(|_| image(png(0))).collect());
    assert!(eight.validate("prompt").is_ok());
    let nine = UserInput::new((0..=MAX_USER_IMAGES).map(|_| image(png(0))).collect());
    assert!(nine.validate("prompt").is_err());

    let mislabeled = UserInput::new(vec![UserPart::Image {
        media_type: "image/jpeg".into(),
        bytes: png(0),
    }]);
    assert!(mislabeled.validate("prompt").is_err());
    assert!(
        UserInput::new(vec![image(b"not an image".to_vec())])
            .validate("prompt")
            .is_err()
    );
    assert!(UserInput::default().validate("prompt").is_err());
    assert!(UserInput::from("").validate("prompt").is_err());
}

#[test]
fn content_blocks_keep_part_order_with_inline_image_bytes() {
    let input = UserInput::new(vec![UserPart::Text("look".into()), image(png(0))]);
    let blocks = input.content_blocks().ok().unwrap_or_default();
    assert!(
        matches!(&blocks[..], [ContentBlock::Text(text), ContentBlock::Image(image)]
        if text.text() == "look"
            && image.media_type() == "image/png"
            && image.source().as_bytes() == Some(png(0).as_slice()))
    );
    assert_eq!(UserInput::from("a").text(), "a");
    assert_eq!(input.text(), "look");
}

#[test]
fn oversized_encoded_message_is_refused_before_admission() -> Result<(), Box<dyn std::error::Error>>
{
    let input = UserInput::new(vec![UserPart::Text("x".repeat(MAX_USER_MESSAGE_BYTES))]);
    let error = input
        .validate("prompt")
        .err()
        .ok_or("message must exceed framing bound")?;
    assert_eq!(error.kind(), ControlErrorKind::InvalidInput);
    assert!(error.to_string().contains("per-message budget"));
    assert!(error.to_string().contains("reduce the text"));
    Ok(())
}

#[test]
fn framing_binds_text_and_image_digests_in_order() {
    let frames = |input: &UserInput| {
        let mut frames = Vec::new();
        input.frame_into(&mut |frame: &[u8]| frames.push(frame.to_vec()));
        frames
    };
    let one = UserInput::new(vec![UserPart::Text("a".into()), image(png(0))]);
    let other = UserInput::new(vec![UserPart::Text("a".into()), image(png(1))]);
    let swapped = UserInput::new(vec![image(png(0)), UserPart::Text("a".into())]);
    assert_ne!(frames(&one), frames(&other));
    assert_ne!(frames(&one), frames(&swapped));
    assert_eq!(frames(&one)[4].len(), 32);
    assert!(!format!("{one:?}").contains("137"));
}
