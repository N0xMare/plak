//! Inbound write-only input. Cloning an envelope shares its zeroizing buffer.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use zeroize::Zeroizing;

#[derive(Clone, Eq, PartialEq)]
pub struct SecretInput(Arc<Zeroizing<String>>);

impl SecretInput {
    pub const MAX_BYTES: usize = 64 * 1024;

    /// # Errors
    /// Refuses input above the envelope's byte limit without reflecting it.
    pub fn new(value: Zeroizing<String>) -> Result<Self, &'static str> {
        if value.len() > Self::MAX_BYTES {
            return Err("secret input exceeds its byte limit");
        }
        Ok(Self(Arc::new(value)))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }

    #[must_use]
    pub fn into_zeroizing(self) -> Zeroizing<String> {
        Arc::try_unwrap(self.0).unwrap_or_else(|shared| Zeroizing::new(shared.as_str().to_owned()))
    }
}

impl std::fmt::Debug for SecretInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretInput(<redacted>)")
    }
}

impl Serialize for SecretInput {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.expose())
    }
}

impl<'de> Deserialize<'de> for SecretInput {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Input;
        impl serde::de::Visitor<'_> for Input {
            type Value = SecretInput;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a bounded secret string")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value.len() > SecretInput::MAX_BYTES {
                    return Err(E::custom("secret input exceeds its byte limit"));
                }
                SecretInput::new(Zeroizing::new(value.to_owned())).map_err(E::custom)
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                SecretInput::new(Zeroizing::new(value)).map_err(E::custom)
            }
        }
        deserializer.deserialize_string(Input)
    }
}

impl schemars::JsonSchema for SecretInput {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SecretInput".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type":"string", "maxLength":65536})
    }
}
