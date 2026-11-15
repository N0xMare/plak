//! Local account identity, distinct from the vertical's model behavior.

use std::{fmt, str::FromStr};

use crate::capabilities::{self, AccountAdmissionError, RouteCapability};

/// A known credential route, whether or not this build can execute it.
/// Identity parsing never admits a route or resolves credentials.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Vertical {
    /// Claude subscription authentication and native model behavior.
    Claude,
    /// ChatGPT subscription authentication and Codex model behavior.
    Codex,
    /// Grok subscription authentication and native model behavior.
    Grok,
    /// Anthropic's direct API-key route, not a Claude subscription.
    Anthropic,
    /// OpenAI's direct API-key route, not a ChatGPT subscription.
    OpenAi,
    /// xAI's direct API-key route, not a Grok subscription.
    XaiApi,
    /// OpenRouter's catalog-backed API-key route.
    OpenRouter,
}

impl Vertical {
    /// Every known identity, including routes omitted from this build.
    pub const ALL: [Self; 7] = [
        Self::Claude,
        Self::Codex,
        Self::Grok,
        Self::Anthropic,
        Self::OpenAi,
        Self::XaiApi,
        Self::OpenRouter,
    ];

    /// Static route facts; reading these does not perform capability admission.
    pub const fn capability(self) -> &'static RouteCapability {
        match self {
            Self::Claude => &capabilities::CLAUDE,
            Self::Codex => &capabilities::CODEX,
            Self::Grok => &capabilities::GROK,
            Self::Anthropic => &capabilities::ANTHROPIC,
            Self::OpenAi => &capabilities::OPENAI,
            Self::XaiApi => &capabilities::XAI,
            Self::OpenRouter => &capabilities::OPENROUTER,
        }
    }

    /// Canonical account prefix, shared by persistence and credential lookup.
    pub const fn as_str(self) -> &'static str {
        self.capability().account
    }

    /// Whether this identity uses an API key rather than subscription OAuth.
    pub const fn is_api(self) -> bool {
        matches!(
            self,
            Self::Anthropic | Self::OpenAi | Self::XaiApi | Self::OpenRouter
        )
    }

    /// Admit execution on this route without looking up an account's credentials.
    ///
    /// # Errors
    /// Returns a typed capability refusal if this build omits the route.
    pub fn require(self) -> Result<(), AccountAdmissionError> {
        capabilities::require(self.as_str())
    }
}

/// A validated named-account suffix, excluding the implicit default account.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AccountName(String);

impl AccountName {
    /// The validated suffix, without the route prefix or slash.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Accept 1–32 ASCII bytes beginning with a lowercase letter, followed by
    /// lowercase letters, digits or hyphens. `default` is reserved and refused.
    pub fn parse(name: &str) -> Option<Self> {
        ((1..=32).contains(&name.len())
            && name != "default"
            && name.as_bytes()[0].is_ascii_lowercase()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
        .then(|| Self(name.to_owned()))
    }
}

/// Exact local credential identity: a route and an optional validated name.
/// Named accounts never fall back to the route's default credentials.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AccountRef {
    vertical: Vertical,
    name: Option<AccountName>,
    reference: String,
}

impl AccountRef {
    /// The route's implicit default account; no credentials are resolved.
    pub fn default_for(vertical: Vertical) -> Self {
        Self {
            vertical,
            name: None,
            reference: vertical.as_str().to_owned(),
        }
    }
    /// The route whose authentication rules govern this account.
    pub const fn vertical(&self) -> Vertical {
        self.vertical
    }
    /// The named suffix, or `None` for the implicit default.
    pub fn name(&self) -> Option<&AccountName> {
        self.name.as_ref()
    }
    /// Whether this identity requires an isolated named-account source.
    pub fn is_named(&self) -> bool {
        self.name.is_some()
    }

    /// Canonical `route` or `route/name`, with accepted route aliases normalized.
    pub fn as_str(&self) -> &str {
        &self.reference
    }

    /// Preserve the default source's identity, or domain-separate it by the exact
    /// named account. This is an authority label, not a credential or endpoint.
    pub fn source_authority(&self, default: &str) -> String {
        if self.is_named() {
            format!("{default}/plak.account-scope.v1/{self}")
        } else {
            default.to_owned()
        }
    }

    /// Application credential reference for this exact account, never a secret.
    pub fn credential_reference(&self) -> String {
        format!("plak/{self}")
    }
}

impl FromStr for AccountRef {
    type Err = AccountAdmissionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let unknown = || AccountAdmissionError::UnknownAccount {
            account: value.to_owned(),
        };
        let (vertical, name) = value
            .split_once('/')
            .map_or((value, None), |(vertical, name)| (vertical, Some(name)));
        let vertical = match vertical {
            "claude" => Vertical::Claude,
            "codex" | "chatgpt" => Vertical::Codex,
            "grok" | "supergrok" | "xai-oauth" => Vertical::Grok,
            "anthropic" => Vertical::Anthropic,
            "openai" => Vertical::OpenAi,
            "xai-api" => Vertical::XaiApi,
            "openrouter" => Vertical::OpenRouter,
            _ => return Err(unknown()),
        };
        let name = name
            .map(|name| AccountName::parse(name).ok_or_else(unknown))
            .transpose()?;
        let reference = name.as_ref().map_or_else(
            || vertical.as_str().to_owned(),
            |name| format!("{}/{}", vertical.as_str(), name.as_str()),
        );
        Ok(Self {
            vertical,
            name,
            reference,
        })
    }
}

impl fmt::Display for AccountRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reference)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_scopes_preserve_default_bytes_and_distinguish_named_accounts() {
        for vertical in Vertical::ALL {
            let default = AccountRef::default_for(vertical);
            let named: AccountRef = format!("{}/work", vertical.as_str()).parse().unwrap();
            assert_eq!(
                default.source_authority("existing-authority"),
                "existing-authority"
            );
            assert_eq!(
                default.credential_reference(),
                format!("plak/{}", vertical.as_str())
            );
            assert_eq!(
                named.source_authority("existing-authority"),
                format!("existing-authority/plak.account-scope.v1/{named}")
            );
            assert_ne!(
                named.source_authority("existing-authority"),
                format!("{}/other", vertical.as_str())
                    .parse::<AccountRef>()
                    .unwrap()
                    .source_authority("existing-authority")
            );
        }
    }

    #[test]
    fn names_preserve_verticals_and_canonicalize_only_explicit_aliases() {
        for vertical in Vertical::ALL {
            let default: AccountRef = vertical.as_str().parse().unwrap();
            let named: AccountRef = format!("{}/work", vertical.as_str()).parse().unwrap();
            assert_eq!(default.vertical(), named.vertical());
            assert_eq!(default.vertical().require(), named.vertical().require());
            assert!(std::ptr::eq(
                capabilities::for_account(default.as_str()).unwrap(),
                capabilities::for_account(named.as_str()).unwrap()
            ));
            assert_eq!(
                capabilities::require(default.as_str()),
                capabilities::require(named.as_str())
            );
            assert_eq!(named.name().unwrap().as_str(), "work");
            assert_eq!(named.to_string(), format!("{}/work", vertical.as_str()));
        }
        for (alias, canonical) in [
            ("chatgpt/work", "codex/work"),
            ("supergrok/work", "grok/work"),
            ("xai-oauth/work", "grok/work"),
        ] {
            assert_eq!(alias.parse::<AccountRef>().unwrap().to_string(), canonical);
        }
        for invalid in [
            "unknown/work",
            "xai",
            "xai/work",
            "openai/default",
            "openai/",
            "openai/a/b",
            "openai/../b",
            "openai/Work",
            "openai/a@b",
            "openai/a%2fb",
            "openai/ work",
            " openai",
            "OPENAI",
        ] {
            assert!(invalid.parse::<AccountRef>().is_err(), "{invalid}");
        }
        assert!(
            format!("openai/a{}", "b".repeat(31))
                .parse::<AccountRef>()
                .is_ok()
        );
        assert!(
            format!("openai/a{}", "b".repeat(32))
                .parse::<AccountRef>()
                .is_err()
        );
    }
}
