//! Multi-part user input accepted by root, follow-up and steering commands.

use std::fmt;

use plakous_model::{ContentBlock, ImageBlock, MediaSource, TextBlock, sha256_bytes};

use crate::control::{ControlError, ControlErrorKind};

/// Most images one user message may carry.
pub const MAX_USER_IMAGES: usize = 8;

/// Largest raw image one user message part may carry: 3.75 MiB, so the
/// base64 form stays within the most restrictive provider bound of 5 MB.
pub const MAX_USER_IMAGE_BYTES: usize = 3_932_160;

/// One ordered part of a user message.
#[derive(Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum UserPart {
    /// Canonical text.
    Text(String),
    /// Raw image bytes with their declared media type.
    Image {
        /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`; must match
        /// the type sniffed from `bytes`.
        media_type: String,
        /// The unmodified encoded image.
        bytes: Vec<u8>,
    },
}

impl fmt::Debug for UserPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => formatter
                .debug_struct("Text")
                .field("bytes", &text.len())
                .finish(),
            Self::Image { media_type, bytes } => formatter
                .debug_struct("Image")
                .field("media_type", media_type)
                .field("bytes", &bytes.len())
                .finish(),
        }
    }
}

/// Ordered parts of one user message. A string converts to one text part.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UserInput {
    parts: Vec<UserPart>,
}

impl UserInput {
    /// Creates an input from ordered parts.
    #[must_use]
    pub const fn new(parts: Vec<UserPart>) -> Self {
        Self { parts }
    }

    /// Returns the ordered parts.
    #[must_use]
    pub fn parts(&self) -> &[UserPart] {
        &self.parts
    }

    /// Counts image parts.
    #[must_use]
    pub fn image_count(&self) -> usize {
        self.parts
            .iter()
            .filter(|part| matches!(part, UserPart::Image { .. }))
            .count()
    }

    /// Joins the text parts with blank lines, ignoring images.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = String::new();
        for part in &self.parts {
            if let UserPart::Text(part) = part {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(part);
            }
        }
        text
    }

    /// Lowers the parts into canonical content blocks in order.
    pub(crate) fn content_blocks(&self) -> Result<Vec<ContentBlock>, ControlError> {
        self.parts
            .iter()
            .map(|part| match part {
                UserPart::Text(text) => TextBlock::new(text.clone(), None)
                    .map(ContentBlock::Text)
                    .map_err(|_| invalid("text part is not valid canonical text")),
                UserPart::Image { media_type, bytes } => MediaSource::bytes(bytes.clone())
                    .and_then(|source| ImageBlock::new(media_type.clone(), source))
                    .map(ContentBlock::Image)
                    .map_err(|_| invalid("image part is not a valid canonical image")),
            })
            .collect()
    }

    /// Validates provider-neutral bounds: at least one part, canonical text,
    /// at most [`MAX_USER_IMAGES`] images of at most [`MAX_USER_IMAGE_BYTES`]
    /// each, and a declared media type equal to the sniffed one.
    pub(crate) fn validate(&self, label: &str) -> Result<(), ControlError> {
        if self.parts.is_empty() {
            return Err(invalid(&format!("{label} has no content")));
        }
        let images = self.image_count();
        if images > MAX_USER_IMAGES {
            return Err(invalid(&format!(
                "{label} carries {images} images; at most {MAX_USER_IMAGES} are accepted"
            )));
        }
        for part in &self.parts {
            match part {
                UserPart::Text(text) => {
                    TextBlock::new(text.clone(), None).map_err(|_| {
                        invalid(&format!("{label} text must be valid canonical text"))
                    })?;
                }
                UserPart::Image { media_type, bytes } => {
                    if bytes.len() > MAX_USER_IMAGE_BYTES {
                        return Err(invalid(&format!(
                            "{label} image is {} bytes; the limit is {MAX_USER_IMAGE_BYTES} \
                             bytes (3.75 MiB); downscale it and attach it again",
                            bytes.len()
                        )));
                    }
                    let header = plakous_tools::sniff_image(bytes)
                        .map_err(|error| invalid(&format!("{label} image is {error}")))?;
                    if header.media_type() != media_type {
                        return Err(invalid(&format!(
                            "{label} image is declared {media_type} but its bytes are {}",
                            header.media_type()
                        )));
                    }
                }
            }
        }
        self.validate_encoded_size(label)?;
        Ok(())
    }

    fn validate_encoded_size(&self, label: &str) -> Result<(), ControlError> {
        let encoded = serde_json::to_vec(&self.content_blocks()?)
            .map_err(|_| invalid("message cannot be encoded"))?;
        // Reserve space for the item, segment identity and binary framing.
        let limit = crate::MAX_RUNTIME_CHECKPOINT_BYTES - 1024;
        let encoded_bytes = encoded.len();
        if encoded_bytes > limit {
            return Err(invalid(&format!(
                "{label} encoded message is {encoded_bytes} bytes; the limit is {limit} bytes; \
                 reduce the text or attach fewer or smaller images"
            )));
        }
        Ok(())
    }

    /// Appends the identity-bearing framing of every part: text verbatim,
    /// images as media type and SHA-256 of the bytes.
    pub(crate) fn frame_into(&self, frame: &mut impl FnMut(&[u8])) {
        for part in &self.parts {
            match part {
                UserPart::Text(text) => {
                    frame(b"text");
                    frame(text.as_bytes());
                }
                UserPart::Image { media_type, bytes } => {
                    frame(b"image");
                    frame(media_type.as_bytes());
                    frame(&sha256_bytes(&[bytes]));
                }
            }
        }
    }
}

fn invalid(message: &str) -> ControlError {
    ControlError::new(ControlErrorKind::InvalidInput, message)
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self::new(vec![UserPart::Text(text)])
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl From<&String> for UserInput {
    fn from(text: &String) -> Self {
        Self::from(text.clone())
    }
}

impl From<Vec<UserPart>> for UserInput {
    fn from(parts: Vec<UserPart>) -> Self {
        Self::new(parts)
    }
}

#[cfg(test)]
mod tests {
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
    fn oversized_encoded_message_is_refused_before_admission()
    -> Result<(), Box<dyn std::error::Error>> {
        let input = UserInput::new(vec![UserPart::Text(
            "x".repeat(crate::MAX_RUNTIME_CHECKPOINT_BYTES - 1024),
        )]);
        let error = input
            .validate("prompt")
            .err()
            .ok_or("message must exceed framing bound")?;
        assert_eq!(error.kind(), ControlErrorKind::InvalidInput);
        assert!(error.to_string().contains("encoded message"));
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
}
