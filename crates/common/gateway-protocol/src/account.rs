//! Account references: `<kind>` or `<kind>/<name>`.

use std::fmt;

use crate::text::{TextError, is_segment};

/// One gateway account: a provider kind and an optional local name, such as
/// `codex` or `codex/work`.
///
/// Both segments match `[a-z][a-z0-9-]{0,31}`, and the name `default` is
/// reserved. The protocol checks only this shape; which kinds exist is the
/// product's vocabulary, so a provider name never enters this crate.
#[derive(
    Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct GatewayAccount(String);

impl GatewayAccount {
    /// Checks the reference grammar.
    ///
    /// # Errors
    /// Refuses a malformed kind or name and the reserved name `default`.
    pub fn new(value: impl Into<String>) -> Result<Self, TextError> {
        let value = value.into();
        let (kind, name) = match value.split_once('/') {
            Some((kind, name)) => (kind, Some(name)),
            None => (value.as_str(), None),
        };
        if !is_segment(kind) {
            return Err(TextError::new("account kind is malformed"));
        }
        match name {
            Some("default") => Err(TextError::new("account name `default` is reserved")),
            Some(name) if !is_segment(name) => Err(TextError::new("account name is malformed")),
            _ => Ok(Self(value)),
        }
    }

    /// The full reference.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The provider kind segment.
    #[must_use]
    pub fn kind(&self) -> &str {
        self.0
            .split_once('/')
            .map_or(self.0.as_str(), |(kind, _)| kind)
    }

    /// The local name, or `None` for the kind's default account.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.0.split_once('/').map(|(_, name)| name)
    }
}

impl TryFrom<String> for GatewayAccount {
    type Error = TextError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<GatewayAccount> for String {
    fn from(value: GatewayAccount) -> Self {
        value.0
    }
}

impl fmt::Display for GatewayAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
