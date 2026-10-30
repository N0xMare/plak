//! Account-admitted model summaries, selectors and native tool modes.

use super::{Error, ModelId, NativeProvider, ReasoningEffort, fmt};

/// Credential-free summary of one exact account-admitted native model.
///
/// The capability bits are limited to Plak's portable request surface. A
/// false bit means the current authenticated catalog and pinned route did not
/// affirmatively admit that capability; it is not a claim about every
/// provider-native API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeModelSummary {
    pub(super) unavailable_reason: Option<String>,
    pub(super) exact_id: String,
    pub(super) visibility: NativeModelVisibility,
    pub(super) native_tool_mode: Option<NativeToolMode>,
    pub(super) streaming: bool,
    pub(super) tools: bool,
    pub(super) reasoning_efforts: Vec<ReasoningEffort>,
    pub(super) default_reasoning_effort: Option<ReasoningEffort>,
    pub(super) service_tiers: Vec<NativeServiceTierSummary>,
    pub(super) default_service_tier: Option<String>,
    pub(super) reported_context_length: Option<u64>,
}

/// Exact provider-authored presentation for one native service tier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeServiceTierSummary {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) description: String,
}

impl NativeServiceTierSummary {
    /// Returns the exact provider tier ID.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the exact provider-authored tier name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the exact provider-authored tier description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
}

impl NativeModelSummary {
    /// A live contract refusal for this row; never stored in the catalog cache.
    #[must_use]
    pub fn unavailable_reason(&self) -> Option<&str> {
        self.unavailable_reason.as_deref()
    }

    #[cfg(feature = "codex")]
    pub(super) fn unavailable(
        exact_id: String,
        visibility: NativeModelVisibility,
        reason: String,
    ) -> Self {
        Self {
            exact_id,
            unavailable_reason: Some(reason),
            visibility,
            native_tool_mode: None,
            streaming: false,
            tools: false,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            service_tiers: Vec::new(),
            default_service_tier: None,
            reported_context_length: None,
        }
    }
    /// Returns the exact authenticated catalog identifier, never an alias.
    #[must_use]
    pub fn exact_id(&self) -> &str {
        &self.exact_id
    }

    /// Returns the provider-authored catalog visibility for this exact row.
    #[must_use]
    pub const fn visibility(&self) -> NativeModelVisibility {
        self.visibility
    }

    /// Returns the exact authenticated model-specific tool mode when the
    /// provider catalog has such a concept and this release recognizes it.
    #[must_use]
    pub const fn native_tool_mode(&self) -> Option<NativeToolMode> {
        self.native_tool_mode
    }

    /// Reports portable streaming support admitted by current route evidence.
    #[must_use]
    pub const fn supports_streaming(&self) -> bool {
        self.streaming
    }

    /// Reports portable client-side function-tool support admitted by current route evidence.
    #[must_use]
    pub const fn supports_tools(&self) -> bool {
        self.tools
    }

    /// Reports portable reasoning controls admitted by current route evidence.
    #[must_use]
    pub const fn supports_reasoning(&self) -> bool {
        !self.reasoning_efforts.is_empty()
    }

    /// Returns exact portable effort options admitted for this catalog row.
    ///
    /// Ordering is provider-authored for Codex and fixed from the reviewed
    /// Grok Build row for each exact SuperGrok model (`Low` through `High` for
    /// 4.5; `Low` through `XHigh` for 4.6). Minimal, unknown, aliased, default,
    /// and token-budget values are omitted.
    #[must_use]
    pub fn reasoning_efforts(&self) -> &[ReasoningEffort] {
        &self.reasoning_efforts
    }

    /// Returns the exact provider-authored default reasoning effort, if any.
    #[must_use]
    pub const fn default_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.default_reasoning_effort
    }

    /// Returns exact provider-authored service tiers in catalog order.
    #[must_use]
    pub fn service_tiers(&self) -> &[NativeServiceTierSummary] {
        &self.service_tiers
    }

    /// Returns the exact provider-authored default service-tier ID, if any.
    #[must_use]
    pub fn default_service_tier(&self) -> Option<&str> {
        self.default_service_tier.as_deref()
    }

    /// Returns an authenticated provider-reported context length for operator
    /// diagnosis. This is not itself a context-admission certificate.
    #[must_use]
    pub const fn reported_context_length(&self) -> Option<u64> {
        self.reported_context_length
    }
}

/// Fresh credential-free model catalog for one native integration.
///
/// Entries retain authenticated provider order. The catalog does not contain
/// aliases or designate a default model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeModelCatalog {
    pub(super) provider: NativeProvider,
    pub(super) models: Vec<NativeModelSummary>,
}

/// How to select one account-admitted model during gated catalog resolution.
///
/// Resolution consumes it through `NativeSubscriptions::resolve_gated_native_model`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeModelSelector {
    /// Resolve exactly the supplied model identifier.
    Exact(ModelId),
    /// Select the first backend-order Codex entry with text, tools, and reasoning.
    CodexFirstCompatible,
}

/// Fail-closed constructor error for [`NativeModelSelector`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeModelSelectorError;

impl fmt::Display for NativeModelSelectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("native model selector is invalid")
    }
}

impl Error for NativeModelSelectorError {}

impl NativeModelSelector {
    /// Creates an exact selector from one validated model identifier.
    ///
    /// # Errors
    ///
    /// Rejects malformed exact model identifiers.
    pub fn exact(model: impl Into<String>) -> Result<Self, NativeModelSelectorError> {
        ModelId::new(model.into())
            .map(Self::Exact)
            .map_err(|_| NativeModelSelectorError)
    }

    /// Creates the named deterministic Codex catalog selector.
    #[must_use]
    pub const fn codex_first_compatible() -> Self {
        Self::CodexFirstCompatible
    }

    /// Returns the exact identifier carried by an exact selector.
    #[must_use]
    pub const fn exact_model(&self) -> Option<&ModelId> {
        match self {
            Self::Exact(model) => Some(model),
            Self::CodexFirstCompatible => None,
        }
    }

    /// Returns whether this selector accepts `selected` after catalog resolution.
    #[must_use]
    pub fn admits_selected(&self, selected: &ModelId) -> bool {
        match self {
            Self::Exact(exact) => exact == selected,
            Self::CodexFirstCompatible => true,
        }
    }

    /// Rejects selectors that are undefined for `provider`.
    ///
    /// # Errors
    ///
    /// Returns [`NativeModelSelectorError`] when the selector is not defined for
    /// the provider.
    pub fn validate_for_provider(
        &self,
        provider: NativeProvider,
    ) -> Result<(), NativeModelSelectorError> {
        if matches!(self, Self::CodexFirstCompatible) && provider != NativeProvider::Codex {
            return Err(NativeModelSelectorError);
        }
        Ok(())
    }
}

/// Exact provider-advertised native tool presentation admitted for a resolved model.
///
/// This facade-owned type deliberately does not expose provider catalog types. A
/// value is present only when fresh, authenticated evidence for the exact
/// resolved model supplied one of the closed modes understood by this release.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NativeToolMode {
    /// Expose the ordinary qualified direct tool catalog.
    Direct,
    /// Expose both the ordinary direct catalog and local Code Mode controls.
    CodeMode,
    /// Expose only the local Code Mode controls.
    CodeModeOnly,
}

/// Provider-authored visibility of an authenticated native model row.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NativeModelVisibility {
    /// Present in the ordinary product model picker.
    List,
    /// Addressable by an exact provider workflow but hidden from the picker.
    Hide,
    /// Not exposed through picker or public model-list UI.
    None,
}

/// Failure to satisfy an exact native tool-mode requirement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeToolModeRequirementError {
    pub(super) required: NativeToolMode,
    available: Option<NativeToolMode>,
}

impl NativeToolModeRequirementError {
    /// Returns the exact mode required by the caller.
    #[must_use]
    pub const fn required(self) -> NativeToolMode {
        self.required
    }

    /// Returns the exact admitted mode, or `None` when evidence did not admit one.
    #[must_use]
    pub const fn available(self) -> Option<NativeToolMode> {
        self.available
    }
}

impl fmt::Display for NativeToolModeRequirementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "required native tool mode {:?} is not available; resolved mode is {:?}",
            self.required, self.available
        )
    }
}

impl Error for NativeToolModeRequirementError {}

pub(super) const fn require_exact_native_tool_mode(
    available: Option<NativeToolMode>,
    required: NativeToolMode,
) -> Result<(), NativeToolModeRequirementError> {
    if matches!(
        (available, required),
        (Some(NativeToolMode::Direct), NativeToolMode::Direct)
            | (Some(NativeToolMode::CodeMode), NativeToolMode::CodeMode)
            | (
                Some(NativeToolMode::CodeModeOnly),
                NativeToolMode::CodeModeOnly
            )
    ) {
        Ok(())
    } else {
        Err(NativeToolModeRequirementError {
            required,
            available,
        })
    }
}

impl NativeModelCatalog {
    /// Returns the integration whose authenticated catalog was queried.
    #[must_use]
    pub const fn provider(&self) -> NativeProvider {
        self.provider
    }

    /// Returns exact model summaries in authenticated catalog order.
    #[must_use]
    pub fn models(&self) -> &[NativeModelSummary] {
        &self.models
    }
}
