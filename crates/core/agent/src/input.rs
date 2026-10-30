//! Multi-part user input accepted by root, follow-up and steering commands.

use std::fmt;

use plakous_model::{ContentBlock, ImageBlock, MediaSource, TextBlock, sha256_bytes};

use crate::control::{ControlError, ControlErrorKind};

/// Most images one user message may carry.
pub const MAX_USER_IMAGES: usize = 8;

/// Largest raw image one user message part may carry: 3.75 MiB, so the
/// base64 form stays within the most restrictive provider bound of 5 MB.
pub const MAX_USER_IMAGE_BYTES: usize = 3_932_160;

/// Per-message budget: the largest canonical encoding of one user message's
/// content, 16 MiB. Images encode as base64, so three maximum-size images
/// fit and four do not.
pub const MAX_USER_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

// The message, its item and segment identity and the binary framing fit one
// runtime checkpoint.
const _: () = assert!(MAX_USER_MESSAGE_BYTES + 1024 <= crate::MAX_RUNTIME_CHECKPOINT_BYTES);

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
        let encoded_bytes = encoded.len();
        if encoded_bytes > MAX_USER_MESSAGE_BYTES {
            return Err(invalid(&format!(
                "{label} encoded message is {encoded_bytes} bytes; the per-message budget is \
                 {MAX_USER_MESSAGE_BYTES} bytes (16 MiB; three maximum-size images fit); \
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
mod tests;
