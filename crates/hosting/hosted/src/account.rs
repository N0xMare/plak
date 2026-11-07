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
}

impl AccountRef {
    pub const fn default_for(vertical: Vertical) -> Self {
        Self {
            vertical,
            name: None,
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
        Ok(Self { vertical, name })
    }
}

impl fmt::Display for AccountRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.vertical.as_str())?;
        if let Some(name) = &self.name {
            write!(formatter, "/{}", name.as_str())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_preserve_verticals_and_canonicalize_only_explicit_aliases() {
        for vertical in Vertical::ALL {
            let default: AccountRef = vertical.as_str().parse().unwrap();
            let named: AccountRef = format!("{}/work", vertical.as_str()).parse().unwrap();
            assert_eq!(default.vertical(), named.vertical());
            assert_eq!(default.vertical().require(), named.vertical().require());
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
