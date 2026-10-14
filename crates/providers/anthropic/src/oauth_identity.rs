//! Validated Claude wire identities, separate from local credential authority.

use crate::{AnthropicError, AnthropicErrorKind, AnthropicErrorPhase};
use serde::{Deserialize, Serialize};

/// Provider-issued account UUID authenticated by OAuth grant or profile.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClaudeAccountUuid(String);

impl ClaudeAccountUuid {
    /// Validates canonical lowercase UUID syntax; never derives an account identity.
    pub fn new(value: impl Into<String>) -> Result<Self, AnthropicError> {
        let value = value.into();
        let valid = value.len() == 36
            && value.bytes().enumerate().all(|(index, byte)| {
                if matches!(index, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                }
            })
            && value != "00000000-0000-0000-0000-000000000000";
        if !valid {
            return Err(invalid_identity());
        }
        Ok(Self(value))
    }
    /// Returns the provider-issued canonical UUID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for ClaudeAccountUuid {
    type Error = AnthropicError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<ClaudeAccountUuid> for String {
    fn from(value: ClaudeAccountUuid) -> Self {
        value.0
    }
}
impl std::fmt::Debug for ClaudeAccountUuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClaudeAccountUuid(<redacted>)")
    }
}

/// Independent installation identity: 32 random bytes encoded as lowercase hex.
/// The embedding application generates and persists this value across accounts
/// and sessions. It must not derive it from credentials or copy another client.
#[derive(Clone, Eq, PartialEq)]
pub struct ClaudeClientDeviceId(String);
impl ClaudeClientDeviceId {
    /// Validates an explicitly supplied installation identity.
    pub fn new(value: impl Into<String>) -> Result<Self, AnthropicError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid_identity());
        }
        Ok(Self(value))
    }
    /// Returns the lowercase hex installation identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for ClaudeClientDeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClaudeClientDeviceId(<redacted>)")
    }
}

/// Authenticated account and application-owned installation identity for lowering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeOAuthRequestIdentity {
    account_uuid: ClaudeAccountUuid,
    device_id: ClaudeClientDeviceId,
}
impl ClaudeOAuthRequestIdentity {
    /// Binds independently supplied account and installation identities.
    pub const fn new(account_uuid: ClaudeAccountUuid, device_id: ClaudeClientDeviceId) -> Self {
        Self {
            account_uuid,
            device_id,
        }
    }
    /// Returns the authenticated provider account UUID.
    pub const fn account_uuid(&self) -> &ClaudeAccountUuid {
        &self.account_uuid
    }
    /// Returns the embedding application's persisted installation identity.
    pub const fn device_id(&self) -> &ClaudeClientDeviceId {
        &self.device_id
    }
}
fn invalid_identity() -> AnthropicError {
    AnthropicError::new(
        AnthropicErrorKind::InvalidConfiguration,
        AnthropicErrorPhase::Prepare,
        "Claude OAuth identity has invalid syntax",
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "validated fixture assertions")]
mod tests {
    use super::*;

    #[test]
    fn provider_uuid_and_device_hex_have_independent_strict_grammars() {
        let account = "12345678-1234-4234-8234-123456789abc";
        assert!(ClaudeAccountUuid::new(account).is_ok());
        for invalid in [
            "",
            "00000000-0000-0000-0000-000000000000",
            "12345678123442348234123456789abc",
            "12345678-1234-4234-8234-123456789ABC",
        ] {
            assert!(ClaudeAccountUuid::new(invalid).is_err());
        }
        assert!(ClaudeClientDeviceId::new("ab".repeat(32)).is_ok());
        assert!(ClaudeClientDeviceId::new("AB".repeat(32)).is_err());
        assert!(ClaudeClientDeviceId::new(account).is_err());
        assert!(ClaudeAccountUuid::new("ab".repeat(32)).is_err());
    }

    #[test]
    fn token_and_profile_keep_authenticated_uuid_and_reject_invalid_uuid() {
        let account = "12345678-1234-4234-8234-123456789abc";
        let body = serde_json::json!({"access_token":"fixture-access","expires_in":3600,"account":{"uuid":account}});
        let token =
            crate::parse_claude_oauth_token_grant(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(token.account_uuid().unwrap().as_str(), account);
        let profile =
            crate::decode_claude_oauth_profile(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(profile.account_uuid.unwrap().as_str(), account);
        let mut invalid = body;
        invalid["account"]["uuid"] = "not-a-provider-uuid".into();
        let bytes = serde_json::to_vec(&invalid).unwrap();
        assert!(crate::parse_claude_oauth_token_grant(&bytes).is_err());
        assert!(crate::decode_claude_oauth_profile(&bytes).is_err());
    }
}
