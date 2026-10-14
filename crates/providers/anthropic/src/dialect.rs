//! Versioned Anthropic Messages dialect and declared feature matrix.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The stable Anthropic Messages API version implemented by this codec.
pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Exact caller discriminator documented for the targeted programmatic
/// code-execution wire shape. Other revisions require a dialect update.
pub const ANTHROPIC_CODE_EXECUTION_CALLER: &str = "code_execution_20260120";
/// Second admitted request-side hosted code caller version.
pub const ANTHROPIC_CODE_EXECUTION_CALLER_20260521: &str = "code_execution_20260521";
/// Exact server-tool request version for the first hosted PTC slice.
pub const ANTHROPIC_CODE_EXECUTION_TOOL: &str = "code_execution_20260120";

/// Explicitly selectable Anthropic beta capabilities.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum AnthropicBetaFeature {
    /// Fine-grained tool-input streaming. Parsing does not imply that malformed
    /// partial JSON is executable.
    FineGrainedToolStreaming,
    /// Claude Code OAuth token class on Messages (`oauth-2025-04-20`).
    ClaudeOAuth,
    /// Claude Code client class (`claude-code-20250219`).
    ClaudeCodeClient,
    /// `output_config.effort` (`effort-2025-11-24`).
    Effort,
    /// Interleaved thinking for adaptive-thinking tool turns.
    InterleavedThinking,
}

impl AnthropicBetaFeature {
    /// Returns the exact beta header token.
    #[must_use]
    pub const fn header_token(self) -> &'static str {
        match self {
            Self::FineGrainedToolStreaming => "fine-grained-tool-streaming-2025-05-14",
            Self::ClaudeOAuth => "oauth-2025-04-20",
            Self::ClaudeCodeClient => "claude-code-20250219",
            Self::Effort => "effort-2025-11-24",
            Self::InterleavedThinking => "interleaved-thinking-2025-05-14",
        }
    }
}

/// A fully explicit Messages wire dialect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnthropicMessagesDialect {
    api_version: String,
    betas: BTreeSet<AnthropicBetaFeature>,
    /// Exact Claude OAuth `anthropic-beta` header. Order is the enrollment
    /// row's, not the feature-set order. Absent on every other dialect.
    oauth_betas: Option<&'static str>,
}

impl AnthropicMessagesDialect {
    /// Creates the stable dialect with no beta behavior enabled.
    #[must_use]
    pub fn stable() -> Self {
        Self {
            api_version: ANTHROPIC_API_VERSION.to_owned(),
            betas: BTreeSet::new(),
            oauth_betas: None,
        }
    }

    /// Creates a dialect with an explicit set of beta features.
    ///
    /// The Claude OAuth client class (both OAuth features) carries the
    /// captured baseline header. A per-model header replaces it through
    /// [`Self::claude_oauth_for`].
    #[must_use]
    pub fn with_betas(betas: impl IntoIterator<Item = AnthropicBetaFeature>) -> Self {
        let betas: BTreeSet<_> = betas.into_iter().collect();
        let oauth_betas = (betas.contains(&AnthropicBetaFeature::ClaudeOAuth)
            && betas.contains(&AnthropicBetaFeature::ClaudeCodeClient))
        .then_some(crate::oauth::CLAUDE_OAUTH_BASELINE_BETAS);
        Self {
            api_version: ANTHROPIC_API_VERSION.to_owned(),
            betas,
            oauth_betas,
        }
    }

    /// Returns the API version header value.
    #[must_use]
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// Returns the enabled beta features.
    #[must_use]
    pub const fn betas(&self) -> &BTreeSet<AnthropicBetaFeature> {
        &self.betas
    }

    /// Claude Code OAuth dialect for one exact model.
    ///
    /// The beta header is that model's enrollment row. An unenrolled id keeps
    /// the no-extra-flag baseline, which fixtures use.
    #[must_use]
    pub fn claude_oauth_for(model: &str) -> Self {
        let mut dialect = if crate::claude_oauth_reasoning_effort_options(model).is_empty() {
            Self::claude_oauth()
        } else {
            Self::claude_oauth_with_effort()
        };
        dialect.oauth_betas = Some(crate::claude_oauth_beta_header(model));
        dialect
    }

    /// Claude Code OAuth dialect using the no-extra-flag baseline header.
    ///
    /// Enrolled models use [`Self::claude_oauth_for`]. This constructor is the
    /// fixture path that has no enrollment row.
    #[must_use]
    pub fn claude_oauth() -> Self {
        Self {
            api_version: ANTHROPIC_API_VERSION.to_owned(),
            betas: [
                AnthropicBetaFeature::ClaudeOAuth,
                AnthropicBetaFeature::ClaudeCodeClient,
            ]
            .into_iter()
            .collect(),
            oauth_betas: Some(crate::oauth::CLAUDE_OAUTH_BASELINE_BETAS),
        }
    }

    /// Claude Code OAuth dialect that may emit `output_config.effort`.
    ///
    /// The header is still the no-extra-flag baseline. Enrolled models use
    /// [`Self::claude_oauth_for`], whose header does not depend on effort.
    #[must_use]
    pub fn claude_oauth_with_effort() -> Self {
        Self {
            api_version: ANTHROPIC_API_VERSION.to_owned(),
            betas: [
                AnthropicBetaFeature::ClaudeOAuth,
                AnthropicBetaFeature::ClaudeCodeClient,
                AnthropicBetaFeature::Effort,
                AnthropicBetaFeature::InterleavedThinking,
            ]
            .into_iter()
            .collect(),
            oauth_betas: Some(crate::oauth::CLAUDE_OAUTH_BASELINE_BETAS),
        }
    }

    /// Returns whether this dialect is the Claude OAuth client class.
    ///
    /// The captured header is the source of truth. The feature set does not
    /// admit the class on its own.
    #[must_use]
    pub const fn is_claude_oauth(&self) -> bool {
        self.oauth_betas.is_some()
    }

    /// Returns the deterministic comma-separated beta header, when needed.
    #[must_use]
    pub fn beta_header(&self) -> Option<String> {
        if let Some(header) = self.oauth_betas {
            return Some(header.to_owned());
        }
        (!self.betas.is_empty()).then(|| {
            self.betas
                .iter()
                .map(|feature| feature.header_token())
                .collect::<Vec<_>>()
                .join(",")
        })
    }
}

impl Default for AnthropicMessagesDialect {
    fn default() -> Self {
        Self::stable()
    }
}

/// Conservative evidence status for one codec surface.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum FeatureStatus {
    /// The offline codec parses, preserves, and replays this surface. This is
    /// not a route or model support claim.
    CodecOnly,
    /// The offline codec can parse this beta surface, but dispatch requires an
    /// exact explicit beta binding and separate route/model evidence.
    BetaCodecOnly,
    /// Deliberately rejected rather than transformed or discarded.
    Excluded,
}

/// One entry in the versioned Messages feature matrix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureMatrixEntry {
    /// Stable feature name.
    pub feature: &'static str,
    /// Foundation support status.
    pub status: FeatureStatus,
}

/// The stable feature declaration for adapter revision 1.
pub const MESSAGES_FEATURE_MATRIX_V1: &[FeatureMatrixEntry] = &[
    FeatureMatrixEntry {
        feature: "text",
        status: FeatureStatus::CodecOnly,
    },
    FeatureMatrixEntry {
        feature: "client_tools",
        status: FeatureStatus::CodecOnly,
    },
    FeatureMatrixEntry {
        feature: "thinking_signature_replay",
        status: FeatureStatus::CodecOnly,
    },
    FeatureMatrixEntry {
        feature: "omitted_thinking",
        status: FeatureStatus::CodecOnly,
    },
    FeatureMatrixEntry {
        feature: "redacted_thinking_replay",
        status: FeatureStatus::CodecOnly,
    },
    FeatureMatrixEntry {
        feature: "cumulative_usage",
        status: FeatureStatus::CodecOnly,
    },
    FeatureMatrixEntry {
        feature: "end_turn_stop_sequence_tool_use_and_limits",
        status: FeatureStatus::CodecOnly,
    },
    FeatureMatrixEntry {
        feature: "refusal_output",
        status: FeatureStatus::CodecOnly,
    },
    FeatureMatrixEntry {
        feature: "programmatic_caller_lineage",
        status: FeatureStatus::CodecOnly,
    },
    FeatureMatrixEntry {
        feature: "pause_turn_continuation",
        status: FeatureStatus::CodecOnly,
    },
    FeatureMatrixEntry {
        feature: "model_fallback",
        status: FeatureStatus::Excluded,
    },
    FeatureMatrixEntry {
        feature: "fine_grained_tool_streaming",
        status: FeatureStatus::BetaCodecOnly,
    },
    FeatureMatrixEntry {
        feature: "citations",
        status: FeatureStatus::Excluded,
    },
    FeatureMatrixEntry {
        feature: "multimodal",
        status: FeatureStatus::Excluded,
    },
    FeatureMatrixEntry {
        feature: "prompt_caching",
        status: FeatureStatus::CodecOnly,
    },
    FeatureMatrixEntry {
        feature: "structured_output",
        status: FeatureStatus::Excluded,
    },
    FeatureMatrixEntry {
        feature: "server_tools",
        status: FeatureStatus::Excluded,
    },
];
