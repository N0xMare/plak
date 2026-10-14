//! One pinning digest for every model-visible string Plakous owns.
//!
//! Prompt text used to be pinned three incompatible ways: `.len()` equality on
//! a concatenation, a byte length plus a hand-rolled `sha256` borrowed from a
//! VCS module, and nothing at all. [`PromptDigest`] is the single scheme: a
//! domain-separated [`Digest`] over the exact UTF-8 bytes of one string, with a
//! `const` constructor so a pin table is a literal and the walker test is the
//! only thing that hashes.

use plakous_model::{Digest, DigestDomain, hex_lower};

/// Format version of the prompt-digest preimage.
pub const PROMPT_DIGEST_VERSION: u32 = 1;

/// A pinned SHA-256 identity for one model-visible string.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PromptDigest([u8; 32]);

impl std::fmt::Debug for PromptDigest {
    /// Prints the pin literal, so an assertion failure is the re-pin.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl PromptDigest {
    /// Digests the exact UTF-8 bytes of one model-visible string.
    #[must_use]
    pub fn of(text: &str) -> Self {
        // `PROMPT_DIGEST_VERSION` is nonzero, so the error arm is unreachable;
        // an all-zero digest can never equal a pin, so it fails the walker
        // closed rather than silently agreeing with one.
        let Ok(digest) = Digest::sha256(
            DigestDomain::Prompt,
            PROMPT_DIGEST_VERSION,
            &[text.as_bytes()],
        ) else {
            return Self([0; 32]);
        };
        Self(*digest.bytes())
    }

    /// Parses a 64-character lowercase-hex pin at compile time.
    ///
    /// # Panics
    ///
    /// Panics — at compile time in a `const` context — when the literal is not
    /// exactly 64 lowercase hexadecimal characters.
    #[must_use]
    pub const fn from_hex(hex: &str) -> Self {
        let encoded = hex.as_bytes();
        assert!(
            encoded.len() == 64,
            "a prompt digest pin is 64 lowercase hex characters"
        );
        let mut bytes = [0_u8; 32];
        let mut index = 0;
        while index < 32 {
            bytes[index] =
                (hex_nibble(encoded[index * 2]) << 4) | hex_nibble(encoded[index * 2 + 1]);
            index += 1;
        }
        Self(bytes)
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns the lowercase-hex pin literal for this digest.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex_lower(&self.0)
    }
}

const fn hex_nibble(character: u8) -> u8 {
    match character {
        b'0'..=b'9' => character - b'0',
        b'a'..=b'f' => character - b'a' + 10,
        _ => panic!("a prompt digest pin is lowercase hexadecimal"),
    }
}

/// One pinned model-visible string: its stable id, the text itself, and the
/// digest that text must have.
///
/// The pin carries its own text so the walker needs no lookup table and a
/// string cannot be pinned twice under two ids by accident.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptPin {
    id: &'static str,
    text: &'static str,
    digest: PromptDigest,
}

impl PromptPin {
    /// Declares one pin from a 64-character lowercase-hex literal.
    #[must_use]
    pub const fn new(id: &'static str, text: &'static str, digest: &'static str) -> Self {
        Self {
            id,
            text,
            digest: PromptDigest::from_hex(digest),
        }
    }

    /// Returns the stable pin id.
    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    /// Returns the pinned text.
    #[must_use]
    pub const fn text(&self) -> &'static str {
        self.text
    }

    /// Returns the pinned digest.
    #[must_use]
    pub const fn digest(&self) -> PromptDigest {
        self.digest
    }
}

/// Fails closed when any pinned string no longer digests to its pin.
///
/// Returns the ids that drifted, each with the digest the text has now, so a
/// deliberate text change can be re-pinned from the failure message.
#[must_use]
pub fn drifted_pins(pins: &[PromptPin]) -> Vec<(&'static str, String)> {
    pins.iter()
        .filter_map(|pin| {
            let actual = PromptDigest::of(pin.text());
            (actual != pin.digest()).then(|| (pin.id(), actual.to_hex()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_digest_round_trips_through_its_hex_pin() {
        let digest = PromptDigest::of("Workspace root:");
        assert_eq!(PromptDigest::from_hex(&digest.to_hex()), digest);
        assert_eq!(digest.to_hex().len(), 64);
    }

    #[test]
    fn prompt_digest_separates_text_and_domain() {
        assert_ne!(PromptDigest::of("a"), PromptDigest::of("b"));
        let plain = plakous_model::sha256_hex(&[b"a"]);
        assert_ne!(PromptDigest::of("a").to_hex(), plain);
    }

    #[test]
    fn drifted_pins_reports_only_the_changed_string() {
        const FIRST: &str = "first";
        const SECOND: &str = "second";
        static PINS: &[PromptPin] = &[
            PromptPin::new(
                "first",
                FIRST,
                "3f4a2c25e1a97d0a6a3c1b2e0e0a2b7f9a0b4d5c6e7f8091a2b3c4d5e6f70819",
            ),
            PromptPin::new(
                "second",
                SECOND,
                "0000000000000000000000000000000000000000000000000000000000000000",
            ),
        ];
        let drifted = drifted_pins(PINS);
        assert_eq!(drifted.len(), 2);
        assert_eq!(drifted[0].0, "first");
        assert_eq!(drifted[0].1, PromptDigest::of(FIRST).to_hex());
        assert_eq!(drifted[1].0, "second");
        assert_eq!(drifted[1].1, PromptDigest::of(SECOND).to_hex());
    }
}
