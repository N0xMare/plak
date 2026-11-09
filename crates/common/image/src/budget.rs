//! The image count and per-message byte budget a frontend checks before a
//! message is queued; runtime admission checks the exact encoding.

use std::fmt;

/// Most images one message carries.
pub const MAX_IMAGES: usize = 8;

/// Per-message budget for the encoded message, 16 MiB. Images encode as
/// base64, so three maximum-size images fit and four do not.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Encoding overhead charged per image beside its base64 text.
const IMAGE_FRAMING_BYTES: usize = 128;

/// Encoding overhead charged for the text part beside its escaped text.
const TEXT_FRAMING_BYTES: usize = 64;

/// A message's text and images encode past the per-message budget,
/// [`MAX_MESSAGE_BYTES`] (16 MiB).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageBudgetExceeded {
    encoded_bytes: usize,
}

impl fmt::Display for MessageBudgetExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[expect(
            clippy::cast_precision_loss,
            reason = "message sizes stay far below 2^53 and the rendering is display-only"
        )]
        let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
        write!(
            formatter,
            "the message encodes to {:.1} MiB, over the {:.0} MiB per-message budget (three \
             maximum-size images fit); attach fewer or smaller images",
            mib(self.encoded_bytes),
            mib(MAX_MESSAGE_BYTES)
        )
    }
}

impl std::error::Error for MessageBudgetExceeded {}

/// Checks a message's text and the byte lengths of its images against the
/// per-message budget, counting the text as its JSON string and each image
/// as its base64 text, each with framing.
///
/// # Errors
///
/// Returns [`MessageBudgetExceeded`] when the message would not fit.
pub fn check_message_budget(
    text: &str,
    image_lengths: impl IntoIterator<Item = usize>,
) -> Result<(), MessageBudgetExceeded> {
    let text_bytes = if text.is_empty() {
        0
    } else {
        json_string_bytes(text).saturating_add(TEXT_FRAMING_BYTES)
    };
    let encoded_bytes = image_lengths.into_iter().fold(text_bytes, |total, length| {
        total
            .saturating_add(length.div_ceil(3).saturating_mul(4))
            .saturating_add(IMAGE_FRAMING_BYTES)
    });
    if encoded_bytes > MAX_MESSAGE_BYTES {
        return Err(MessageBudgetExceeded { encoded_bytes });
    }
    Ok(())
}

/// The length of `text` as a JSON string, quotes included: `"` and `\\`
/// and the short-form controls take two bytes, other controls six.
fn json_string_bytes(text: &str) -> usize {
    text.bytes().fold(2_usize, |total, byte| {
        total.saturating_add(match byte {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::MAX_IMAGE_BYTES;

    #[test]
    fn three_maximum_images_fit_the_message_budget_and_four_do_not() {
        assert_eq!(
            check_message_budget("look at these", [MAX_IMAGE_BYTES; 3]),
            Ok(())
        );
        assert_eq!(
            check_message_budget("", [1_400 * 1024 + 24; MAX_IMAGES]),
            Ok(())
        );
        let message = check_message_budget("", [MAX_IMAGE_BYTES; 4])
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("20.0 MiB, over the 16 MiB per-message budget"),
            "{message}"
        );
        assert!(
            message.contains("three maximum-size images fit"),
            "{message}"
        );
    }

    #[test]
    fn text_counts_as_its_json_string() {
        for text in [
            "plain",
            "a \"quoted\" \\ path\n\ttab",
            "\u{1}\u{1f}\u{8}\u{c}\r",
            "é ✓",
        ] {
            assert_eq!(
                json_string_bytes(text),
                serde_json::to_string(text).unwrap().len(),
                "{text:?}"
            );
        }
        // Escaping, not raw length, decides whether text beside three
        // maximum-size images fits.
        let three = [MAX_IMAGE_BYTES; 3];
        assert_eq!(check_message_budget(&"x".repeat(600_000), three), Ok(()));
        assert!(check_message_budget(&"\"\n".repeat(300_000), three).is_err());
    }
}
