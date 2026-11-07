//! Wire-only draft identity; shared drafts and persisted rows carry none.

use serde::{Deserialize, Serialize};

/// A canonical lowercase, hyphenated version-4 UUID chosen for one submission.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ClientDraftId(uuid::Uuid);

impl ClientDraftId {
    /// Mint a fresh identity for a new send, never for an automatic retry.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for ClientDraftId {
    fn default() -> Self {
        Self::new()
    }
}

impl schemars::JsonSchema for ClientDraftId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ClientDraftId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        serde_json::Map::from_iter([
            (
                "type".to_owned(),
                serde_json::Value::String("string".to_owned()),
            ),
            (
                "pattern".to_owned(),
                serde_json::Value::String(
                    "^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
                        .to_owned(),
                ),
            ),
        ])
        .into()
    }
}

impl std::str::FromStr for ClientDraftId {
    type Err = InvalidClientDraftId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parsed = uuid::Uuid::parse_str(value).map_err(|_| InvalidClientDraftId)?;
        if parsed.get_version() != Some(uuid::Version::Random)
            || parsed.get_variant() != uuid::Variant::RFC4122
            || parsed.hyphenated().to_string() != value
        {
            return Err(InvalidClientDraftId);
        }
        Ok(Self(parsed))
    }
}

impl<'de> Deserialize<'de> for ClientDraftId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for ClientDraftId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

/// A client draft id did not have its one permitted spelling/version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidClientDraftId;

impl std::fmt::Display for InvalidClientDraftId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("client_id must be a canonical lowercase hyphenated UUID v4")
    }
}

impl std::error::Error for InvalidClientDraftId {}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn client_identity_has_one_spelling_and_requires_random_uuid_variant() {
        let canonical = "4e46c5a8-7664-4f8a-b75b-a9b102bd0b03";
        let id: ClientDraftId = canonical.parse().unwrap();
        assert_eq!(id.to_string(), canonical);
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            format!("\"{canonical}\"")
        );
        for invalid in [
            "4E46C5A8-7664-4F8A-B75B-A9B102BD0B03",
            "4e46c5a876644f8ab75ba9b102bd0b03",
            "{4e46c5a8-7664-4f8a-b75b-a9b102bd0b03}",
            "urn:uuid:4e46c5a8-7664-4f8a-b75b-a9b102bd0b03",
            "4e46c5a8-7664-5f8a-b75b-a9b102bd0b03",
            "4e46c5a8-7664-4f8a-075b-a9b102bd0b03",
            "00000000-0000-0000-0000-000000000000",
        ] {
            assert!(invalid.parse::<ClientDraftId>().is_err(), "{invalid}");
            assert!(serde_json::from_value::<ClientDraftId>(serde_json::json!(invalid)).is_err());
        }
    }
}
