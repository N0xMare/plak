//! Local account identity, distinct from the vertical's model behavior.

use std::{fmt, str::FromStr};

use crate::capabilities::{self, AccountAdmissionError, RouteCapability};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Vertical {
    Claude,
    Codex,
    Grok,
    Anthropic,
    OpenAi,
    XaiApi,
    OpenRouter,
}

impl Vertical {
    pub const ALL: [Self; 7] = [
        Self::Claude,
        Self::Codex,
        Self::Grok,
        Self::Anthropic,
        Self::OpenAi,
        Self::XaiApi,
        Self::OpenRouter,
    ];

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

    pub const fn as_str(self) -> &'static str {
        self.capability().account
    }

    pub const fn is_api(self) -> bool {
        matches!(
            self,
            Self::Anthropic | Self::OpenAi | Self::XaiApi | Self::OpenRouter
        )
    }

    pub fn require(self) -> Result<(), AccountAdmissionError> {
        capabilities::require(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AccountName(String);

impl AccountName {
    pub fn as_str(&self) -> &str {
        &self.0
    }

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

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AccountRef {
    vertical: Vertical,
    name: Option<AccountName>,
    reference: String,
}

impl AccountRef {
    pub fn default_for(vertical: Vertical) -> Self {
        Self {
            vertical,
            name: None,
            reference: vertical.as_str().to_owned(),
        }
    }
    pub const fn vertical(&self) -> Vertical {
        self.vertical
    }
    pub fn name(&self) -> Option<&AccountName> {
        self.name.as_ref()
    }
    pub fn is_named(&self) -> bool {
        self.name.is_some()
    }

    pub fn as_str(&self) -> &str {
        &self.reference
    }

    pub fn source_authority(&self, default: &str) -> String {
        if self.is_named() {
            format!("{default}/plak.account-scope.v1/{self}")
        } else {
            default.to_owned()
        }
    }

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
