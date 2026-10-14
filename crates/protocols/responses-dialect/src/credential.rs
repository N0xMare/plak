//! Non-secret credential-generation identity shared by Responses dialects.

use std::fmt;

use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

// Immutable legacy hash-domain bytes: changing the historical `paid-platform`
// literal would rotate every retained credential-generation fingerprint.
const CREDENTIAL_GENERATION_DOMAIN: &[u8] = b"plak.paid-platform.credential-generation.v1";
const MAX_CREDENTIAL_GENERATION_BINDING_BYTES: usize = 64 * 1024;

/// Closed authority for the exact provider tag in a credential-generation hash.
///
/// The variants preserve the legacy tag bytes while preventing callers from
/// supplying an arbitrary string that impersonates an integration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum CredentialProviderTag {
    /// OpenAI Platform and Codex credential authority.
    OpenAiPlatform,
    /// OpenRouter Platform credential authority.
    OpenRouterPlatform,
    /// xAI Platform and SuperGrok credential authority.
    XaiPlatform,
}

impl CredentialProviderTag {
    const fn legacy_bytes(self) -> &'static [u8] {
        match self {
            Self::OpenAiPlatform => b"openai-platform",
            Self::OpenRouterPlatform => b"openrouter-platform",
            Self::XaiPlatform => b"xai-platform",
        }
    }
}

/// A public fingerprint for the exact credential generation retained by a route.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CredentialGenerationFingerprint([u8; 32]);

impl CredentialGenerationFingerprint {
    /// Mints an exact provider-tagged fingerprint without retaining the binding.
    ///
    /// The closed tag authority preserves the former dispatch encoding and
    /// prevents arbitrary provider-tag strings from crossing this boundary.
    ///
    /// # Errors
    ///
    /// Rejects an empty or over-64-KiB binding.
    pub fn mint_exact(
        provider: CredentialProviderTag,
        generation_binding: &[u8],
    ) -> Result<Self, CredentialGenerationFingerprintError> {
        if generation_binding.is_empty()
            || generation_binding.len() > MAX_CREDENTIAL_GENERATION_BINDING_BYTES
        {
            return Err(CredentialGenerationFingerprintError);
        }
        let provider_tag = provider.legacy_bytes();
        let mut hasher = Sha256::new();
        hasher.update(CREDENTIAL_GENERATION_DOMAIN);
        hasher.update((provider_tag.len() as u64).to_be_bytes());
        hasher.update(provider_tag);
        hasher.update((generation_binding.len() as u64).to_be_bytes());
        hasher.update(generation_binding);
        Ok(Self(hasher.finalize().into()))
    }

    /// Returns the fingerprint bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns canonical lowercase hexadecimal.
    #[must_use]
    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(64);
        for byte in self.0 {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }
}

/// A credential-generation binding was not bounded and nonempty.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("invalid credential generation binding")]
pub struct CredentialGenerationFingerprintError;

impl fmt::Debug for CredentialGenerationFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CredentialGenerationFingerprint")
            .field(&self.to_hex())
            .finish()
    }
}

impl Serialize for CredentialGenerationFingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for CredentialGenerationFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.len() != 64
            || !value
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return Err(de::Error::custom(
                "invalid credential-generation fingerprint",
            ));
        }
        let mut decoded = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            decoded[index] = (decode_nibble(pair[0]) << 4) | decode_nibble(pair[1]);
        }
        Ok(Self(decoded))
    }
}

fn decode_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("lowercase hexadecimal validated before decoding"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_transcripts_and_lowercase_codec_are_pinned() -> Result<(), Box<dyn std::error::Error>>
    {
        for (tag, expected) in [
            (
                CredentialProviderTag::OpenAiPlatform,
                "532aed710d0dcad07dcae7e186c0c1325963c429ba1890afccbffe84bd7b9420",
            ),
            (
                CredentialProviderTag::OpenRouterPlatform,
                "3e0058be745e52e380006aaac6377cbfa839da4016f25ef5107d0757a12606a0",
            ),
            (
                CredentialProviderTag::XaiPlatform,
                "34e1b5a5e7d156278086a6562021237068dc8623bfd8d4fa4bff98f53cb0aaf0",
            ),
        ] {
            let fingerprint =
                CredentialGenerationFingerprint::mint_exact(tag, b"exact credential generation")?;
            assert_eq!(fingerprint.to_hex(), expected);
            let json = serde_json::to_string(&fingerprint)?;
            assert_eq!(
                serde_json::from_str::<CredentialGenerationFingerprint>(&json)?,
                fingerprint
            );
            assert!(
                serde_json::from_str::<CredentialGenerationFingerprint>(&format!(
                    "\"{}\"",
                    expected.to_uppercase()
                ))
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn invalid_bindings_fail_without_retention() {
        assert!(
            CredentialGenerationFingerprint::mint_exact(CredentialProviderTag::OpenAiPlatform, b"")
                .is_err()
        );
        assert!(
            CredentialGenerationFingerprint::mint_exact(
                CredentialProviderTag::OpenAiPlatform,
                &vec![0_u8; MAX_CREDENTIAL_GENERATION_BINDING_BYTES + 1],
            )
            .is_err()
        );
    }

    #[test]
    fn closed_provider_tags_are_unique_and_cannot_share_an_identity()
    -> Result<(), CredentialGenerationFingerprintError> {
        let binding = b"same generation binding";
        let fingerprints = [
            CredentialGenerationFingerprint::mint_exact(
                CredentialProviderTag::OpenAiPlatform,
                binding,
            )?,
            CredentialGenerationFingerprint::mint_exact(
                CredentialProviderTag::OpenRouterPlatform,
                binding,
            )?,
            CredentialGenerationFingerprint::mint_exact(
                CredentialProviderTag::XaiPlatform,
                binding,
            )?,
        ];
        for left in 0..fingerprints.len() {
            for right in left + 1..fingerprints.len() {
                assert_ne!(fingerprints[left], fingerprints[right]);
            }
        }
        Ok(())
    }
}
