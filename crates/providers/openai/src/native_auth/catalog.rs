//! Authenticated Codex model catalog shapes, parsing and release enrollment.

use super::{
    BTreeMap, BTreeSet, CODEX_REFERENCE_CAPACITY_REVISION, CODEX_REFERENCE_MEASUREMENT_REVISION,
    CapacityCertificate, CapacityRule, ContextContract, ContextEvidenceSource,
    CountCertificateKind, CredentialFence, Deserialize, Digest, DigestDomain, EvidenceFreshness,
    MAX_SERVICE_TIER_DESCRIPTION_BYTES, MAX_SERVICE_TIER_ID_BYTES, MAX_SERVICE_TIER_NAME_BYTES,
    MeasurementCertificate, NativeCodexAuthError, NativeCodexCatalogWitness, NonZeroU8, NonZeroU32,
    OPENAI_CODEX_SOURCE_REVISION, ProtocolId, RESPONSES_CANONICAL_PROJECTION_REVISION,
    RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION, RESPONSES_FAMILY_ID, RawValue, ReasoningEffort,
    ResolvedModelDescriptor, Serialize, Timestamp, validate_identifier, validate_oauth_value,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CodexResponsesWireMode {
    Standard,
    Lite,
}

/// Credential-free exact catalog entry admitted by the pinned profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexCatalogEntry {
    pub(super) model_id: String,
    pub(super) visibility: Option<CodexCatalogVisibility>,
    pub(super) priority: i32,
    pub(super) api_support: CodexCatalogApiSupport,
    pub(super) text: bool,
    pub(super) tools: bool,
    pub(super) parallel_tools: bool,
    pub(super) wire_mode: CodexResponsesWireMode,
    pub(super) tool_mode: Option<CodexCatalogToolMode>,
    pub(super) reasoning_effort_options: Vec<ReasoningEffort>,
    pub(super) default_reasoning_effort: Option<ReasoningEffort>,
    pub(super) reasoning_efforts: CodexReasoningEffortSet,
    pub(super) service_tiers: Vec<CodexCatalogServiceTier>,
    pub(super) default_service_tier: Option<String>,
    pub(super) capacity_facts: CodexCatalogCapacityFacts,
    pub(super) observed_at: Timestamp,
    pub(super) expires_at: Timestamp,
}

impl CodexCatalogEntry {
    /// Returns the exact backend slug.
    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Returns the exact recognized picker visibility advertised by this row.
    /// Unknown future values are retained in identity but return `None`.
    #[must_use]
    pub const fn visibility(&self) -> Option<CodexCatalogVisibility> {
        self.visibility
    }

    /// Returns the provider-authored picker priority (lower is earlier).
    #[must_use]
    pub const fn priority(&self) -> i32 {
        self.priority
    }

    /// Reports API-key Platform eligibility. ChatGPT OAuth eligibility is
    /// independent, so a false value does not remove this row from Codex OAuth.
    #[must_use]
    pub const fn supported_in_api(&self) -> bool {
        matches!(self.api_support, CodexCatalogApiSupport::PlatformEligible)
    }

    /// Reports explicitly advertised text input.
    #[must_use]
    pub const fn supports_text(&self) -> bool {
        self.text
    }

    /// Reports tool support conservatively derived from a non-disabled shell type.
    #[must_use]
    pub const fn supports_tools(&self) -> bool {
        self.tools
    }

    /// Reports parallel tools when the pinned reference-client release enables
    /// them for a catalog row that independently advertises tools.
    #[must_use]
    pub const fn supports_parallel_tools(&self) -> bool {
        self.parallel_tools
    }

    /// Whether this authenticated row uses Codex Responses Lite.
    #[must_use]
    pub const fn uses_responses_lite(&self) -> bool {
        matches!(self.wire_mode, CodexResponsesWireMode::Lite)
    }

    /// Returns the exact recognized `tool_mode` advertised by this row.
    ///
    /// The pinned Codex 0.162.1 reference client defaults an omitted value to
    /// Direct. Unknown future values return `None`; callers must not infer a
    /// mode from the model slug, shell type, or neighboring rows.
    #[must_use]
    pub const fn tool_mode(&self) -> Option<CodexCatalogToolMode> {
        self.tool_mode
    }

    /// Reports exact authenticated Code Mode advertisement for this row.
    #[must_use]
    pub const fn supports_code_mode(&self) -> bool {
        matches!(
            self.tool_mode,
            Some(CodexCatalogToolMode::CodeMode | CodexCatalogToolMode::CodeModeOnly)
        )
    }

    /// Reports the coarse capability when at least one named neutral effort
    /// was advertised. Exact effort admission uses the full witnessed set.
    #[must_use]
    pub const fn supports_reasoning(&self) -> bool {
        self.reasoning_efforts.supports_named_reasoning()
    }

    /// Returns recognized portable efforts in authenticated catalog order.
    ///
    /// Provider-only `minimal`, unknown future labels, and token budgets are
    /// never projected into this portable option list.
    #[must_use]
    pub fn reasoning_effort_options(&self) -> &[ReasoningEffort] {
        &self.reasoning_effort_options
    }

    /// Returns the exact provider-authored default reasoning effort when the
    /// authenticated row supplies one understood by this pinned release.
    #[must_use]
    pub const fn default_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.default_reasoning_effort
    }

    /// Returns service tiers in exact authenticated provider order.
    #[must_use]
    pub fn service_tiers(&self) -> &[CodexCatalogServiceTier] {
        &self.service_tiers
    }

    /// Returns the exact authenticated default service-tier ID, if present.
    #[must_use]
    pub fn default_service_tier(&self) -> Option<&str> {
        self.default_service_tier.as_deref()
    }

    /// Reports whether this exact catalog row advertised the named effort.
    ///
    /// Provider-neutral token budgets and efforts unknown to this pinned
    /// integration fail closed. This query performs no credential or network
    /// access.
    #[must_use]
    pub const fn supports_reasoning_effort(&self, effort: ReasoningEffort) -> bool {
        self.reasoning_efforts.supports(effort)
    }

    /// Returns exact authenticated catalog capacity facts for this entry.
    ///
    /// These facts are discovery evidence only. They do not prove complete
    /// request measurement or an enforced output reservation and therefore do
    /// not constitute a neutral context-admission contract.
    #[must_use]
    pub const fn capacity_facts(&self) -> CodexCatalogCapacityFacts {
        self.capacity_facts
    }

    /// Returns the exact route-observation time.
    #[must_use]
    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    /// Returns the hard evidence expiry.
    #[must_use]
    pub const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }
}

/// Exact provider-authored service-tier presentation from one Codex row.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CodexCatalogServiceTier {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) description: String,
}

impl CodexCatalogServiceTier {
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

/// Exact credential-free context and compaction thresholds from one catalog row.
///
/// Construction is private so every public value has passed the provider-wire
/// integer bounds and cross-field coherence checks. The value is intentionally
/// not a [`plakous_model::ContextContract`]: the catalog supplies no certified
/// counter for the complete prepared request and no enforced output maximum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CodexCatalogCapacityFacts {
    pub(super) context_window: Option<NonZeroU32>,
    pub(super) max_context_window: Option<NonZeroU32>,
    pub(super) effective_context_window_percent: NonZeroU8,
    pub(super) auto_compact_token_limit: Option<NonZeroU32>,
}

impl CodexCatalogCapacityFacts {
    pub(super) fn from_wire(model: &CatalogModelWire) -> Result<Self, NativeCodexAuthError> {
        let context_window = model
            .context_window
            .map(positive_catalog_tokens)
            .transpose()?;
        let max_context_window = model
            .max_context_window
            .map(positive_catalog_tokens)
            .transpose()?;
        let resolved_context_window = context_window
            .or(max_context_window)
            .ok_or(NativeCodexAuthError::Catalog)?;
        let effective_context_window_percent = u8::try_from(model.effective_context_window_percent)
            .ok()
            .filter(|percent| (1..=100).contains(percent))
            .and_then(NonZeroU8::new)
            .ok_or(NativeCodexAuthError::Catalog)?;
        let auto_compact_token_limit = model
            .auto_compact_token_limit
            .map(positive_catalog_tokens)
            .transpose()?;
        if context_window
            .zip(max_context_window)
            .is_some_and(|(context, maximum)| context > maximum)
        {
            return Err(NativeCodexAuthError::Catalog);
        }
        let _ = resolved_context_window;
        Ok(Self {
            context_window,
            max_context_window,
            effective_context_window_percent,
            auto_compact_token_limit,
        })
    }

    /// Returns the exact catalog context window in tokens.
    #[must_use]
    pub const fn context_window(self) -> Option<NonZeroU32> {
        self.context_window
    }

    /// Returns the exact catalog maximum context-window override in tokens.
    #[must_use]
    pub const fn max_context_window(self) -> Option<NonZeroU32> {
        self.max_context_window
    }

    /// Returns Codex's operative context window (`context_window`, otherwise
    /// `max_context_window`). Admitted facts always contain at least one.
    #[must_use]
    pub const fn resolved_context_window(self) -> NonZeroU32 {
        match self.context_window {
            Some(window) => window,
            None => match self.max_context_window {
                Some(window) => window,
                None => unreachable!(),
            },
        }
    }

    /// Returns the exact catalog usable-input percentage.
    #[must_use]
    pub const fn effective_context_window_percent(self) -> NonZeroU8 {
        self.effective_context_window_percent
    }

    /// Returns the exact optional catalog auto-compaction threshold in tokens.
    #[must_use]
    pub const fn auto_compact_token_limit(self) -> Option<NonZeroU32> {
        self.auto_compact_token_limit
    }

    /// Returns Codex's effective 90%-derived, explicitly clamped threshold.
    #[must_use]
    pub fn effective_auto_compact_token_limit(self) -> u32 {
        let derived = self.resolved_context_window().get().saturating_mul(9) / 10;
        self.auto_compact_token_limit
            .map_or(derived, |explicit| explicit.get().min(derived))
    }

    pub(super) fn provider_reserved_input(self) -> Result<u64, NativeCodexAuthError> {
        u64::from(self.resolved_context_window().get())
            .checked_mul(u64::from(self.effective_context_window_percent.get()))
            .map(|scaled| scaled / 100)
            .filter(|tokens| *tokens != 0)
            .ok_or(NativeCodexAuthError::Catalog)
    }
}

pub(super) fn reference_client_envelope_context_contract(
    authority_binding: Digest,
    capacity_facts: CodexCatalogCapacityFacts,
    observed: Timestamp,
    expires: Timestamp,
    catalog_receipt: Digest,
) -> Result<ContextContract, NativeCodexAuthError> {
    let freshness = EvidenceFreshness::new(observed, Some(expires))
        .map_err(|_| NativeCodexAuthError::Catalog)?;
    let capacity = CapacityCertificate::reference_client_envelope(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        "codex-authenticated-catalog-input-envelope",
        CODEX_REFERENCE_CAPACITY_REVISION,
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(u64::from(
                capacity_facts.resolved_context_window().get(),
            )),
            maximum_projected_input: capacity_facts.provider_reserved_input()?,
            auto_compact_input: Some(u64::from(
                capacity_facts.effective_auto_compact_token_limit(),
            )),
        },
        authority_binding,
        freshness,
        catalog_receipt,
        OPENAI_CODEX_SOURCE_REVISION,
    )
    .map_err(|_| NativeCodexAuthError::Catalog)?;
    let measurement = MeasurementCertificate::standard(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        "codex-complete-input-token-upper-bound",
        CODEX_REFERENCE_MEASUREMENT_REVISION,
        "ceil-utf8-wire-bytes-div-4-compaction-reasoning-length",
        CODEX_REFERENCE_MEASUREMENT_REVISION,
        ContextEvidenceSource::AuthenticatedDiscovery,
        CountCertificateKind::ReferenceClientEnvelopeUpperBound { catalog_receipt },
        Some(authority_binding),
        freshness,
    )
    .map_err(|_| NativeCodexAuthError::Catalog)?;
    ContextContract::new(
        ProtocolId::new(RESPONSES_FAMILY_ID).map_err(|_| NativeCodexAuthError::Catalog)?,
        RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION,
        RESPONSES_CANONICAL_PROJECTION_REVISION,
        "reference-client-envelope-no-output-authority-v1",
        capacity,
        vec![measurement],
    )
    .map_err(|_| NativeCodexAuthError::Catalog)
}

pub(super) fn positive_catalog_tokens(value: i64) -> Result<NonZeroU32, NativeCodexAuthError> {
    u32::try_from(value)
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or(NativeCodexAuthError::Catalog)
}

/// A fresh credential-free catalog snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexModelCatalog {
    pub(super) entries: Vec<CodexCatalogEntry>,
    pub(super) profile_id: &'static str,
    pub(super) source_revision: &'static str,
}

impl CodexModelCatalog {
    /// Returns catalog entries in backend order.
    #[must_use]
    pub fn entries(&self) -> &[CodexCatalogEntry] {
        &self.entries
    }

    /// Returns the immutable profile identity.
    #[must_use]
    pub const fn profile_id(&self) -> &'static str {
        self.profile_id
    }

    /// Returns the immutable official source revision.
    #[must_use]
    pub const fn source_revision(&self) -> &'static str {
        self.source_revision
    }
}

#[derive(Deserialize)]
pub(super) struct CatalogDocument {
    pub(super) models: Vec<Box<RawValue>>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum CatalogReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    /// Provider-published maximum reasoning tier.
    Max,
    /// Provider-published ultra reasoning tier.
    Ultra,
    /// Preserve forward compatibility for newly published efforts.
    Other(String),
}

impl CatalogReasoningEffort {
    pub(super) fn label(&self) -> &str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
            Self::Other(label) => label,
        }
    }

    pub(super) const fn portable(&self) -> Option<ReasoningEffort> {
        match self {
            Self::Low => Some(ReasoningEffort::Low),
            Self::Medium => Some(ReasoningEffort::Medium),
            Self::High => Some(ReasoningEffort::High),
            Self::Xhigh => Some(ReasoningEffort::XHigh),
            Self::Max => Some(ReasoningEffort::Max),
            Self::Ultra => Some(ReasoningEffort::Ultra),
            Self::Minimal | Self::Other(_) => None,
        }
    }
}

impl<'de> Deserialize<'de> for CatalogReasoningEffort {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            "max" => Self::Max,
            "ultra" => Self::Ultra,
            other => Self::Other(other.to_owned()),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(super) struct CodexReasoningEffortSet(pub(super) u8);

impl CodexReasoningEffortSet {
    const MINIMAL: u8 = 1 << 0;
    pub(super) const LOW: u8 = 1 << 1;
    pub(super) const MEDIUM: u8 = 1 << 2;
    pub(super) const HIGH: u8 = 1 << 3;
    const XHIGH: u8 = 1 << 4;
    const MAX: u8 = 1 << 5;
    const ULTRA: u8 = 1 << 6;
    const OTHER: u8 = 1 << 7;

    pub(super) fn from_catalog(levels: &[CatalogReasoningLevel]) -> Self {
        Self(levels.iter().fold(0, |bits, level| {
            bits | match &level.effort {
                CatalogReasoningEffort::Minimal => Self::MINIMAL,
                CatalogReasoningEffort::Low => Self::LOW,
                CatalogReasoningEffort::Medium => Self::MEDIUM,
                CatalogReasoningEffort::High => Self::HIGH,
                CatalogReasoningEffort::Xhigh => Self::XHIGH,
                CatalogReasoningEffort::Max => Self::MAX,
                CatalogReasoningEffort::Ultra => Self::ULTRA,
                CatalogReasoningEffort::Other(_) => Self::OTHER,
            }
        }))
    }

    pub(super) const fn supports(self, effort: ReasoningEffort) -> bool {
        let required = match effort {
            ReasoningEffort::Low => Self::LOW,
            ReasoningEffort::Medium => Self::MEDIUM,
            ReasoningEffort::High => Self::HIGH,
            ReasoningEffort::XHigh => Self::XHIGH,
            ReasoningEffort::Max => Self::MAX,
            ReasoningEffort::Ultra => Self::ULTRA,
            ReasoningEffort::TokenBudget(_) => return false,
            _ => return false,
        };
        self.0 & required != 0
    }

    pub(super) const fn supports_named_reasoning(self) -> bool {
        self.0 & (Self::LOW | Self::MEDIUM | Self::HIGH | Self::XHIGH | Self::MAX | Self::ULTRA)
            != 0
    }

    pub(super) const fn identity_byte(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub(super) struct CatalogReasoningLevel {
    pub(super) effort: CatalogReasoningEffort,
    /// Provider-facing description text; ignored by Plak capability admission.
    #[serde(default)]
    pub(super) description: Option<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum CatalogShellType {
    Default,
    Disabled,
    Local,
    UnifiedExec,
    ShellCommand,
    /// Preserve unrelated future catalog rows without assigning capability.
    Other(String),
}

impl<'de> Deserialize<'de> for CatalogShellType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "default" => Self::Default,
            "disabled" => Self::Disabled,
            "local" => Self::Local,
            "unified_exec" => Self::UnifiedExec,
            "shell_command" => Self::ShellCommand,
            other => Self::Other(other.to_owned()),
        })
    }
}

impl CatalogShellType {
    pub(super) fn label(&self) -> &str {
        match self {
            Self::Default => "default",
            Self::Disabled => "disabled",
            Self::Local => "local",
            Self::UnifiedExec => "unified_exec",
            Self::ShellCommand => "shell_command",
            Self::Other(label) => label,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum CatalogInputModality {
    Text,
    Image,
    Audio,
    /// Preserve unrelated future catalog rows without assigning capability.
    Other(String),
}

/// Closed authenticated Codex catalog tool-mode facts understood by this
/// integration revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexCatalogToolMode {
    /// The provider's `direct` catalog value.
    Direct,
    /// The provider's `code_mode` catalog value.
    CodeMode,
    /// The provider's `code_mode_only` catalog value.
    CodeModeOnly,
}

/// Closed authenticated Codex picker-visibility facts understood by this
/// integration revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexCatalogVisibility {
    /// Present in an ordinary model picker.
    List,
    /// Retained for exact internal selection but hidden from the picker.
    Hide,
    /// Not presented through picker or public model-list surfaces.
    None,
}

/// Route-scoped meaning of Codex's `supported_in_api` catalog flag.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexCatalogApiSupport {
    /// The row may also be presented on the separately authenticated API route.
    PlatformEligible,
    /// The row remains valid for ChatGPT OAuth but is not an API-key row.
    SubscriptionOnly,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum CatalogVisibility {
    List,
    Hide,
    None,
    #[default]
    Missing,
    Other(String),
}

impl<'de> Deserialize<'de> for CatalogVisibility {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "list" => Self::List,
            "hide" => Self::Hide,
            "none" => Self::None,
            other => Self::Other(other.to_owned()),
        })
    }
}

impl CatalogVisibility {
    pub(super) fn label(&self) -> &str {
        match self {
            Self::List => "list",
            Self::Hide => "hide",
            Self::None => "none",
            Self::Missing => "<missing>",
            Self::Other(label) => label,
        }
    }

    pub(super) const fn recognized(&self) -> Option<CodexCatalogVisibility> {
        match self {
            Self::List => Some(CodexCatalogVisibility::List),
            Self::Hide => Some(CodexCatalogVisibility::Hide),
            Self::None => Some(CodexCatalogVisibility::None),
            Self::Missing | Self::Other(_) => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum CatalogToolMode {
    Direct,
    CodeMode,
    CodeModeOnly,
    #[default]
    Missing,
    Other(String),
}

impl<'de> Deserialize<'de> for CatalogToolMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let Some(value) = Option::<String>::deserialize(deserializer)? else {
            return Ok(Self::Missing);
        };
        Ok(match value.as_str() {
            "direct" => Self::Direct,
            "code_mode" => Self::CodeMode,
            "code_mode_only" => Self::CodeModeOnly,
            other => Self::Other(other.to_owned()),
        })
    }
}

impl CatalogToolMode {
    pub(super) fn label(&self) -> &str {
        match self {
            Self::Direct => "direct",
            Self::CodeMode => "code_mode",
            Self::CodeModeOnly => "code_mode_only",
            Self::Missing => "<missing>",
            Self::Other(label) => label,
        }
    }

    pub(super) const fn recognized(&self) -> Option<CodexCatalogToolMode> {
        match self {
            Self::Direct => Some(CodexCatalogToolMode::Direct),
            Self::CodeMode => Some(CodexCatalogToolMode::CodeMode),
            Self::CodeModeOnly => Some(CodexCatalogToolMode::CodeModeOnly),
            // Exact Codex 0.162.1 reference-client semantics.
            Self::Missing => Some(CodexCatalogToolMode::Direct),
            Self::Other(_) => None,
        }
    }
}

impl<'de> Deserialize<'de> for CatalogInputModality {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "text" => Self::Text,
            "image" => Self::Image,
            "audio" => Self::Audio,
            other => Self::Other(other.to_owned()),
        })
    }
}

impl CatalogInputModality {
    pub(super) fn label(&self) -> &str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Other(label) => label,
        }
    }
}

pub(super) fn default_catalog_input_modalities() -> Vec<CatalogInputModality> {
    // This is the provider's documented legacy wire default, not a Plakous
    // capability guess.
    vec![CatalogInputModality::Text, CatalogInputModality::Image]
}

pub(super) const fn default_catalog_effective_context_window_percent() -> i64 {
    // Kept in lock-step with the official Codex protocol default.
    95
}

#[derive(Deserialize)]
pub(super) struct CatalogModelWire {
    pub(super) slug: String,
    pub(super) visibility: CatalogVisibility,
    pub(super) priority: i32,
    pub(super) supported_in_api: bool,
    pub(super) supported_reasoning_levels: Vec<CatalogReasoningLevel>,
    #[serde(default)]
    pub(super) default_reasoning_level: Option<CatalogReasoningEffort>,
    #[serde(default)]
    pub(super) service_tiers: Vec<CatalogServiceTierWire>,
    #[serde(default)]
    pub(super) default_service_tier: Option<String>,
    pub(super) shell_type: CatalogShellType,
    #[serde(default)]
    pub(super) tool_mode: CatalogToolMode,
    #[serde(default = "default_catalog_input_modalities")]
    pub(super) input_modalities: Vec<CatalogInputModality>,
    #[serde(default)]
    pub(super) context_window: Option<i64>,
    #[serde(default)]
    pub(super) max_context_window: Option<i64>,
    #[serde(default = "default_catalog_effective_context_window_percent")]
    pub(super) effective_context_window_percent: i64,
    #[serde(default)]
    pub(super) auto_compact_token_limit: Option<i64>,
    #[serde(default)]
    pub(super) use_responses_lite: bool,
    #[serde(default)]
    pub(super) model_messages: Option<CatalogModelMessagesWire>,
}

#[derive(Deserialize)]
pub(super) struct CatalogModelMessagesWire {
    #[serde(default)]
    pub(super) content_filter_guidance: Option<String>,
}

pub(super) fn catalog_content_filter_guidance(model: &CatalogModelWire) -> Option<&str> {
    model
        .model_messages
        .as_ref()?
        .content_filter_guidance
        .as_deref()
        .filter(|text| !text.trim().is_empty() && text.len() <= 512)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct CatalogServiceTierWire {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) description: String,
}

pub(super) fn catalog_service_tiers(
    model: &CatalogModelWire,
) -> Result<(Vec<CodexCatalogServiceTier>, Option<String>), NativeCodexAuthError> {
    let mut ids = BTreeSet::new();
    let mut tiers = Vec::with_capacity(model.service_tiers.len());
    for tier in &model.service_tiers {
        validate_identifier(&tier.id, MAX_SERVICE_TIER_ID_BYTES)
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        validate_oauth_value(&tier.name, MAX_SERVICE_TIER_NAME_BYTES)
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        validate_oauth_value(&tier.description, MAX_SERVICE_TIER_DESCRIPTION_BYTES)
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        if tier.id != "priority" || !ids.insert(tier.id.as_str()) {
            return Err(NativeCodexAuthError::Catalog);
        }
        tiers.push(CodexCatalogServiceTier {
            id: tier.id.clone(),
            name: tier.name.clone(),
            description: tier.description.clone(),
        });
    }
    if model
        .default_service_tier
        .as_ref()
        .is_some_and(|default| !ids.contains(default.as_str()))
    {
        return Err(NativeCodexAuthError::Catalog);
    }
    Ok((tiers, model.default_service_tier.clone()))
}

pub(super) fn catalog_enum_fingerprint(
    model: &CatalogModelWire,
) -> Result<Digest, NativeCodexAuthError> {
    #[derive(Serialize)]
    struct EnumLabels<'a> {
        visibility: &'a str,
        priority: i32,
        supported_in_api: bool,
        shell_type: &'a str,
        input_modalities: Vec<&'a str>,
        supported_reasoning_levels: Vec<&'a str>,
        default_reasoning_level: Option<&'a str>,
        service_tiers: &'a [CatalogServiceTierWire],
        default_service_tier: Option<&'a str>,
        tool_mode: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        content_filter_guidance: Option<&'a str>,
    }
    let bytes = serde_json::to_vec(&EnumLabels {
        visibility: model.visibility.label(),
        priority: model.priority,
        supported_in_api: model.supported_in_api,
        shell_type: model.shell_type.label(),
        input_modalities: model
            .input_modalities
            .iter()
            .map(CatalogInputModality::label)
            .collect(),
        supported_reasoning_levels: model
            .supported_reasoning_levels
            .iter()
            .map(|level| level.effort.label())
            .collect(),
        default_reasoning_level: model
            .default_reasoning_level
            .as_ref()
            .map(CatalogReasoningEffort::label),
        service_tiers: &model.service_tiers,
        default_service_tier: model.default_service_tier.as_deref(),
        tool_mode: model.tool_mode.label(),
        content_filter_guidance: catalog_content_filter_guidance(model),
    })
    .map_err(|_| NativeCodexAuthError::Catalog)?;
    Digest::sha256(
        DigestDomain::ModelBinding,
        4,
        &[b"plak-native-codex-catalog-row-facts-v4", &bytes],
    )
    .map_err(|_| NativeCodexAuthError::Catalog)
}

#[derive(Clone)]
pub(super) struct WitnessedDescriptor {
    pub(super) descriptor: ResolvedModelDescriptor,
    pub(super) witness: NativeCodexCatalogWitness,
    pub(super) capacity_facts: CodexCatalogCapacityFacts,
    pub(super) reasoning_efforts: CodexReasoningEffortSet,
    pub(super) tool_mode: Option<CodexCatalogToolMode>,
    pub(super) responses_lite: bool,
    pub(super) content_filter_guidance: Option<String>,
    pub(super) catalog_enum_fingerprint: Digest,
    pub(super) catalog_receipt: Digest,
    pub(super) observed_at: Timestamp,
    pub(super) expires_at: Timestamp,
}

pub(super) struct ResolvedCatalog {
    pub(super) public: CodexModelCatalog,
    pub(super) descriptors: BTreeMap<String, WitnessedDescriptor>,
    pub(super) ambiguous_slugs: BTreeSet<String>,
    pub(super) credential_fence: CredentialFence,
}

/// Exact Codex 0.162.1 model rows accepted for executable catalog intersection.
///
/// This list is bound to the reviewed reference-client/catalog contract.
/// Authenticated catalog observation remains independently required, including for server-delivered rows that
/// are not baked into the reference client's fallback catalog.
///
/// Each row pairs the literal with its source-backed tool mode. It is the only
/// place a Codex model is enrolled; presentation, Code Mode eligibility and
/// catalog admission all read it. Window and effort facts come from the
/// authenticated catalog, never from here.
pub const RELEASE_ENROLLED_CODEX_MODELS: &[(&str, CodexCatalogToolMode)] = &[
    ("gpt-6-astra", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-6.1-sol", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-6-sol", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-6-luna", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-5.6-sol", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-5.6-terra", CodexCatalogToolMode::CodeModeOnly),
    ("gpt-5.6-luna", CodexCatalogToolMode::CodeModeOnly),
];

/// Returns whether an exact model literal is enrolled by the reviewed release.
#[must_use]
pub fn release_enrolled_codex_model(model: &str) -> bool {
    release_enrolled_codex_tool_mode(model).is_some()
}

/// Returns the exact source-backed tool mode for an enrolled model literal.
#[must_use]
pub fn release_enrolled_codex_tool_mode(model: &str) -> Option<CodexCatalogToolMode> {
    RELEASE_ENROLLED_CODEX_MODELS
        .iter()
        .find_map(|(enrolled, mode)| (*enrolled == model).then_some(*mode))
}

pub(super) fn executable_codex_catalog_model(model: &str) -> bool {
    release_enrolled_codex_model(model)
}
