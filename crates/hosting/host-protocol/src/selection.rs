//! Closed selections an operator makes about how a session is bound: how the
//! model may compose tools, and which `OpenRouter` wire carries it.

use serde::{Deserialize, Serialize};

/// How the model may compose the product's tools.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompositionSelection {
    /// Ordinary directly exposed product tools.
    Direct,
    /// The versioned Plakous-recommended tool presentation for the bound
    /// provider/model, resolved once at attach and pinned for the session.
    #[default]
    #[serde(rename = "plakous-default")]
    FollowCatalog,
    /// Explicit Codex-native Code Mode where admitted.
    NativePtc,
    /// Plak-owned JavaScript PTC profile.
    JsPtc,
    /// Plak-owned Python PTC profile.
    PyPtc,
}

impl CompositionSelection {
    /// Every selection, in settings-menu order.
    pub const ALL: [Self; 5] = [
        Self::Direct,
        Self::FollowCatalog,
        Self::NativePtc,
        Self::JsPtc,
        Self::PyPtc,
    ];

    /// Stable configuration label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::FollowCatalog => "plakous-default",
            Self::NativePtc => "native-ptc",
            Self::JsPtc => "js-ptc",
            Self::PyPtc => "py-ptc",
        }
    }

    /// Parses a canonical label.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == value)
    }
}

/// `OpenRouter` wire selected by the operator. `Auto` is resolved once by the
/// host; it is not a fallback list.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolSelection {
    /// Product default, resolved once to Chat.
    #[default]
    Auto,
    /// Chat Completions.
    Chat,
    /// Responses.
    Responses,
    /// Messages.
    Messages,
}

impl ProtocolSelection {
    /// Every selection, in picker order.
    pub const ALL: [Self; 4] = [Self::Auto, Self::Chat, Self::Responses, Self::Messages];

    /// Stable product label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Chat => "chat",
            Self::Responses => "responses",
            Self::Messages => "messages",
        }
    }

    /// Parses a canonical label.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == value)
    }
}
