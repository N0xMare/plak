use serde::{Deserialize, Deserializer, Serialize};

use super::{
    ContentError, MAX_CANONICAL_BLOCK_BYTES, ReplayEnvelope, ensure_maximum, ensure_nonempty,
};

/// A semantic phase for text output.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum TextPhase {
    /// Intermediate commentary intended for display.
    Commentary,
    /// The final answer projection.
    Final,
}

/// A validated text content block.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TextBlock {
    text: String,
    phase: Option<TextPhase>,
}

#[derive(Deserialize)]
struct TextBlockWire {
    text: String,
    phase: Option<TextPhase>,
}

impl<'de> Deserialize<'de> for TextBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = TextBlockWire::deserialize(deserializer)?;
        Self::new(wire.text, wire.phase).map_err(serde::de::Error::custom)
    }
}

impl TextBlock {
    /// Creates a non-empty text block.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when `text` is empty.
    pub fn new(text: impl Into<String>, phase: Option<TextPhase>) -> Result<Self, ContentError> {
        let text = text.into();
        ensure_nonempty(&text, "text")?;
        ensure_maximum(text.len(), "text", MAX_CANONICAL_BLOCK_BYTES)?;
        Ok(Self { text, phase })
    }

    /// Returns the text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the optional semantic phase.
    #[must_use]
    pub const fn phase(&self) -> Option<TextPhase> {
        self.phase
    }
}

/// The visibility and replay policy of reasoning content.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ReasoningVisibility {
    /// A displayable semantic summary.
    Summary,
    /// Visible reasoning explicitly authorized for display.
    Visible,
    /// Reasoning that was omitted by the source.
    Omitted,
    /// Reasoning whose content was redacted.
    Redacted,
    /// Opaque replay-only reasoning material.
    OpaqueReplay,
}

/// A canonical reasoning block without hidden chain-of-thought interpretation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReasoningBlock {
    visibility: ReasoningVisibility,
    text: Option<String>,
    replay: Option<ReplayEnvelope>,
}

#[derive(Deserialize)]
struct ReasoningBlockWire {
    visibility: ReasoningVisibility,
    text: Option<String>,
    replay: Option<ReplayEnvelope>,
}

impl<'de> Deserialize<'de> for ReasoningBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ReasoningBlockWire::deserialize(deserializer)?;
        Self::new(wire.visibility, wire.text, wire.replay).map_err(serde::de::Error::custom)
    }
}

impl ReasoningBlock {
    /// Creates a validated reasoning block.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::InvalidReasoning`] when visible states lack text,
    /// hidden states contain text, or replay-only state lacks an envelope.
    pub fn new(
        visibility: ReasoningVisibility,
        text: Option<String>,
        replay: Option<ReplayEnvelope>,
    ) -> Result<Self, ContentError> {
        let valid = match visibility {
            ReasoningVisibility::Summary | ReasoningVisibility::Visible => {
                text.as_deref().is_some_and(|value| !value.is_empty())
            }
            ReasoningVisibility::Omitted | ReasoningVisibility::Redacted => text.is_none(),
            ReasoningVisibility::OpaqueReplay => text.is_none() && replay.is_some(),
        };
        if !valid {
            return Err(ContentError::InvalidReasoning);
        }
        if let Some(text) = &text {
            ensure_maximum(text.len(), "reasoning text", MAX_CANONICAL_BLOCK_BYTES)?;
        }
        Ok(Self {
            visibility,
            text,
            replay,
        })
    }

    /// Returns the visibility state.
    #[must_use]
    pub const fn visibility(&self) -> ReasoningVisibility {
        self.visibility
    }

    /// Returns displayable text when present.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Returns opaque replay material when present.
    #[must_use]
    pub const fn replay(&self) -> Option<&ReplayEnvelope> {
        self.replay.as_ref()
    }
}

/// An explicit refusal block.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RefusalBlock {
    text: String,
}

#[derive(Deserialize)]
struct RefusalBlockWire {
    text: String,
}

impl<'de> Deserialize<'de> for RefusalBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = RefusalBlockWire::deserialize(deserializer)?;
        Self::new(wire.text).map_err(serde::de::Error::custom)
    }
}

impl RefusalBlock {
    /// Creates a non-empty refusal.
    ///
    /// # Errors
    ///
    /// Returns [`ContentError::Empty`] when refusal text is empty.
    pub fn new(text: impl Into<String>) -> Result<Self, ContentError> {
        let text = text.into();
        ensure_nonempty(&text, "refusal")?;
        ensure_maximum(text.len(), "refusal", MAX_CANONICAL_BLOCK_BYTES)?;
        Ok(Self { text })
    }

    /// Returns refusal text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}
