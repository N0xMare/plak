//! Canonical lowering into the Messages request grammar.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use plakous_attempt::ReplaySafety;
use plakous_model::{
    ActiveFamilyArtifact, Actor, CachePolicy, CallId, CapabilityRequirement, ContentBlock,
    DataClassification, Digest, DigestDomain, EffectiveProviderConfigurationId, HostedTool,
    InstructionSource, ModelDiagnostic, ModelId, OpaqueFallback, OutputFormat, ProtocolId,
    ReasoningEffort, ReasoningPolicy, ReasoningVisibility, ReplayEnvelope, ReplayScope,
    SelectedProviderConfigurationId, SemanticJsonV1, ToolCallBlock, ToolCallerDisposition,
    ToolCallerLineage, ToolChoice, ToolExecutionOwner, ToolInputContract, ToolName,
    ToolOwnerProjectionError, ToolResultKind, ToolResultPart, ToolResultUpdateBlock,
    ValidatedRequestPlan,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

/// Family-stable opaque replay kind for a thinking signature.
pub const SIGNATURE_REPLAY_KIND: &str = "messages.thinking_signature";
/// Family-stable opaque replay kind for a redacted thinking block.
pub const REDACTED_REPLAY_KIND: &str = "messages.redacted_thinking";
/// Exact native replay kind for Anthropic hosted code execution results.
pub const CODE_EXECUTION_RESULT_REPLAY_KIND: &str = "messages.code_execution_tool_result";
/// Canonical schema revision for Messages item-level opaque replay.
pub const MESSAGES_REPLAY_SCHEMA_REVISION: u32 = 4;
const MAX_ROUTER_DATA_COLLECTION_BYTES: usize = 32;

/// Closed failure classification from pre-dispatch Messages lowering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessagesRequestErrorKind {
    /// The canonical request asks for a deliberately unsupported semantic.
    UnsupportedSemantic,
    /// The canonical request violates Messages grammar or exact binding.
    InvalidRequest,
    /// History contains native material that cannot be replayed on this kernel.
    ProtectedHistory,
}

/// Pre-dispatch Messages lowering failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagesRequestError {
    kind: MessagesRequestErrorKind,
    message: String,
}

impl MessagesRequestError {
    /// Returns the closed failure kind.
    pub const fn kind(&self) -> MessagesRequestErrorKind {
        self.kind
    }
    /// Returns the actionable non-secret diagnostic.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for MessagesRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MessagesRequestError {}

fn unsupported(message: impl Into<String>) -> MessagesRequestError {
    MessagesRequestError {
        kind: MessagesRequestErrorKind::UnsupportedSemantic,
        message: message.into(),
    }
}

fn invalid(message: impl Into<String>) -> MessagesRequestError {
    MessagesRequestError {
        kind: MessagesRequestErrorKind::InvalidRequest,
        message: message.into(),
    }
}

fn native_compaction_refusal() -> MessagesRequestError {
    MessagesRequestError {
        kind: MessagesRequestErrorKind::ProtectedHistory,
        message: "native compaction cannot be replayed on Messages".into(),
    }
}

/// Exact provider-supplied facts required by neutral Messages lowering.
#[allow(
    clippy::struct_excessive_bools,
    reason = "dialect-selected wire omission knobs are independent policy axes"
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagesRequestPolicy {
    protocol: ProtocolId,
    requested_model: ModelId,
    expected_reported_model: Option<ModelId>,
    descriptor_binding: Digest,
    semantic_descriptor_binding: Digest,
    account_binding: Digest,
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    maximum_output_tokens: u32,
    minimum_thinking_budget: Option<u32>,
    api_version: String,
    beta_header: String,
    system_prefixes: Vec<String>,
    metadata_user_id: Option<String>,
    prompt_cache: bool,
    top_level_prompt_cache: bool,
    prompt_cache_ttl: Option<&'static str>,
    omit_disabled_thinking: bool,
    strict_thinking_replay: bool,
    completed_history_prefix: Option<plakous_model::ConversationPrefixIdentity>,
    thinking_display: Option<MessagesThinkingDisplay>,
    programmatic_tool_caller: Option<ProgrammaticToolCallerPolicy>,
    router: Option<MessagesRouterPolicy>,
    plugins: Option<&'static [MessagesRequestPlugin]>,
    session_id: Option<String>,
    /// Optional in-place transform of the serialized JSON, applied after
    /// `serde_json::to_vec` and before the operation digest. Claude OAuth uses
    /// this to attest `cch` on the exact bytes that go on the wire.
    serialized_body_transform: Option<SerializedBodyTransform>,
    /// When set, Messages `max_tokens` is the minimum of the configured bound
    /// and this cap. Claude OAuth uses the model's request default here so the
    /// product output ceiling can stay larger than the wire value.
    request_output_cap: Option<u32>,
    /// Emits the Claude OAuth `context_management` edit on this request.
    claude_oauth_context_management: bool,
    /// Emits Claude OAuth `diagnostics`.
    claude_oauth_diagnostics: bool,
    /// The `diagnostics.previous_message_id`; `None` is an explicit `null`.
    claude_oauth_previous_message_id: Option<PreviousMessageId>,
}

/// A provider message id; debug output never prints it.
#[derive(Clone, Eq, PartialEq)]
struct PreviousMessageId(String);

impl fmt::Debug for PreviousMessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreviousMessageId(..)")
    }
}

/// Equality is presence-only: the function pointer itself is not compared.
#[derive(Clone, Copy, Debug)]
struct SerializedBodyTransform(fn(&mut [u8]) -> Result<(), &'static str>);

impl PartialEq for SerializedBodyTransform {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for SerializedBodyTransform {}

/// One OpenRouter request plugin declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct MessagesRequestPlugin {
    id: &'static str,
    enabled: bool,
}

/// Disables OpenRouter middle-out truncation on every product Messages request.
pub const OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS: &[MessagesRequestPlugin] =
    &[MessagesRequestPlugin {
        id: "context-compression",
        enabled: false,
    }];

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProgrammaticToolCallerPolicy {
    request_callers: Vec<String>,
    response_caller: String,
    namespace: plakous_model::NamespaceId,
}

/// Closed provider-neutral routing policy for a Messages gateway.
///
/// It always disables fallback and requires requested parameters. Retention
/// fields are emitted only when the dialect explicitly selected their exact
/// values. Endpoint pins and arbitrary provider JSON are outside the
/// Messages-family surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagesRouterPolicy {
    data_collection: Option<String>,
    zdr: Option<bool>,
}

impl MessagesRouterPolicy {
    /// Selects gateway-managed, no-fallback routing with optional exact privacy values.
    pub fn gateway_managed(
        data_collection: Option<String>,
        zdr: Option<bool>,
    ) -> Result<Self, MessagesRequestError> {
        if data_collection.as_deref().is_some_and(|value| {
            value.is_empty()
                || value.len() > MAX_ROUTER_DATA_COLLECTION_BYTES
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }) {
            return Err(invalid(
                "Messages router data collection policy is not bounded",
            ));
        }
        Ok(Self {
            data_collection,
            zdr,
        })
    }
}

impl MessagesRequestPolicy {
    /// Creates the closed request policy supplied by one exact dialect target.
    ///
    /// `descriptor_binding` is the full freshness-sensitive plan binding;
    /// `semantic_descriptor_binding` is the descriptor's stable behavior binding
    /// used only to validate opaque replay across refreshed observations.
    pub fn new(
        protocol: ProtocolId,
        requested_model: ModelId,
        expected_reported_model: Option<ModelId>,
        descriptor_binding: Digest,
        semantic_descriptor_binding: Digest,
        account_binding: Digest,
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
        maximum_output_tokens: u32,
        minimum_thinking_budget: Option<u32>,
        api_version: impl Into<String>,
        beta_header: impl Into<String>,
    ) -> Self {
        Self {
            protocol,
            requested_model,
            expected_reported_model,
            descriptor_binding,
            semantic_descriptor_binding,
            account_binding,
            selected_provider_configuration,
            effective_provider_configuration,
            maximum_output_tokens,
            minimum_thinking_budget,
            api_version: api_version.into(),
            beta_header: beta_header.into(),
            system_prefixes: Vec::new(),
            metadata_user_id: None,
            prompt_cache: false,
            top_level_prompt_cache: true,
            prompt_cache_ttl: None,
            omit_disabled_thinking: false,
            strict_thinking_replay: false,
            completed_history_prefix: None,
            thinking_display: None,
            programmatic_tool_caller: None,
            router: None,
            plugins: None,
            session_id: None,
            serialized_body_transform: None,
            request_output_cap: None,
            claude_oauth_context_management: false,
            claude_oauth_diagnostics: false,
            claude_oauth_previous_message_id: None,
        }
    }

    /// Caps Messages `max_tokens` at `tokens` without lowering the model ceiling.
    ///
    /// Zero is ignored. A cap above the configured bound does not raise it.
    #[must_use]
    pub const fn with_request_output_cap(mut self, tokens: u32) -> Self {
        if tokens > 0 {
            self.request_output_cap = Some(tokens);
        }
        self
    }

    /// Emits `context_management` `{edits:[{type:clear_thinking_20251015,keep:all}]}`.
    ///
    /// This is a request edit. It does not rewrite replayed thinking blocks,
    /// and it is not the response `applied_edits` shape.
    #[must_use]
    pub const fn with_claude_oauth_context_management(mut self) -> Self {
        self.claude_oauth_context_management = true;
        self
    }

    /// Emits `diagnostics` `{previous_message_id: <id or null>}`.
    ///
    /// The id names the provider response whose assistant output this request
    /// replays last; `None` sends `null`.
    #[must_use]
    pub fn with_claude_oauth_previous_message_id(mut self, message_id: Option<String>) -> Self {
        self.claude_oauth_diagnostics = true;
        self.claude_oauth_previous_message_id = message_id.map(PreviousMessageId);
        self
    }

    /// Refuses incompatible opaque thinking instead of converting it to portable text.
    #[must_use]
    pub const fn with_strict_thinking_replay(mut self, enabled: bool) -> Self {
        self.strict_thinking_replay = enabled;
        self
    }

    /// Supplies the runtime-proven closed-turn prefix for this exact request.
    #[must_use]
    pub const fn with_completed_history_prefix(
        mut self,
        prefix: Option<plakous_model::ConversationPrefixIdentity>,
    ) -> Self {
        self.completed_history_prefix = prefix;
        self
    }

    /// Runs `transform` on the serialized request JSON before digesting it.
    #[must_use]
    pub const fn with_serialized_body_transform(
        mut self,
        transform: fn(&mut [u8]) -> Result<(), &'static str>,
    ) -> Self {
        self.serialized_body_transform = Some(SerializedBodyTransform(transform));
        self
    }

    /// Attaches OpenRouter plugin declarations to the lowered request body.
    #[must_use]
    pub const fn with_plugins(mut self, plugins: &'static [MessagesRequestPlugin]) -> Self {
        self.plugins = Some(plugins);
        self
    }

    /// Emits OpenRouter `session_id` from branch cache-affinity identity.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        self.session_id = (!session_id.is_empty()).then_some(session_id);
        self
    }

    /// Appends one exact gateway-required system prefix block.
    #[must_use]
    pub fn with_system_prefix(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        if !text.is_empty() {
            self.system_prefixes.push(text);
        }
        self
    }

    /// Attaches the compatibility profile's opaque request attribution.
    #[must_use]
    pub fn with_metadata_user_id(mut self, user_id: impl Into<String>) -> Self {
        let user_id = user_id.into();
        self.metadata_user_id = (!user_id.is_empty()).then_some(user_id);
        self
    }

    /// Enables Messages `cache_control` emission for this exact policy.
    #[must_use]
    pub const fn with_prompt_cache(mut self, enabled: bool) -> Self {
        self.prompt_cache = enabled;
        self
    }

    /// Selects whether enabled prompt caching also emits the top-level marker.
    #[must_use]
    pub const fn with_top_level_prompt_cache(mut self, enabled: bool) -> Self {
        self.top_level_prompt_cache = enabled;
        self
    }

    /// Sets the exact `cache_control.ttl` for enabled prompt-cache writes.
    ///
    /// `None` omits `ttl` (provider default, 5 minutes on Anthropic).
    #[must_use]
    pub const fn with_prompt_cache_ttl(mut self, ttl: Option<&'static str>) -> Self {
        self.prompt_cache_ttl = ttl;
        self
    }

    fn cache_control_param(&self) -> CacheControlParam {
        match self.prompt_cache_ttl {
            Some(ttl) => CacheControlParam::ephemeral_with_ttl(ttl),
            None => CacheControlParam::ephemeral(),
        }
    }

    /// Selects omission as the exact wire encoding for disabled reasoning.
    #[must_use]
    pub const fn with_omitted_disabled_thinking(mut self, enabled: bool) -> Self {
        self.omit_disabled_thinking = enabled;
        self
    }

    /// Selects a route-owned display mode for enabled or adaptive thinking.
    /// Disabled reasoning never carries a display field.
    #[must_use]
    pub const fn with_thinking_display(mut self, display: MessagesThinkingDisplay) -> Self {
        self.thinking_display = Some(display);
        self
    }

    /// Enables exact Messages hosted-program request callers and response discriminator.
    #[must_use]
    pub fn with_programmatic_tool_calling(
        mut self,
        request_callers: Vec<String>,
        response_caller: impl Into<String>,
        namespace: plakous_model::NamespaceId,
    ) -> Self {
        self.programmatic_tool_caller = Some(ProgrammaticToolCallerPolicy {
            request_callers,
            response_caller: response_caller.into(),
            namespace,
        });
        self
    }

    /// Enables the exact closed gateway-managed router block.
    #[must_use]
    pub fn with_router_policy(mut self, router: MessagesRouterPolicy) -> Self {
        self.router = Some(router);
        self
    }

    /// Returns the exact Messages protocol identity.
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the provider-reported model required by the exact target, when pinned.
    pub const fn expected_reported_model(&self) -> Option<&ModelId> {
        self.expected_reported_model.as_ref()
    }

    /// Reports whether this exact request policy admits provider-hosted execution.
    pub const fn hosted_effect_capable(&self) -> bool {
        self.programmatic_tool_caller.is_some()
    }
}

/// Explicit owner mapping for model-facing client tools.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ToolOwnership {
    owners: BTreeMap<ToolName, ToolExecutionOwner>,
    callers: BTreeMap<ToolName, ToolCallerDisposition>,
}

impl ToolOwnership {
    /// Creates an exact mapping. Absent tools default to local ownership.
    pub fn new(owners: BTreeMap<ToolName, ToolExecutionOwner>) -> Self {
        Self {
            owners,
            callers: BTreeMap::new(),
        }
    }
    /// Returns the declared owner or local ownership.
    pub fn owner(&self, tool: &ToolName) -> ToolExecutionOwner {
        self.owners
            .get(tool)
            .cloned()
            .unwrap_or(ToolExecutionOwner::PlakLocal)
    }
    /// Reports whether the exact tool was included.
    pub fn contains(&self, tool: &ToolName) -> bool {
        self.owners.contains_key(tool)
    }
    /// Returns the caller disposition frozen for an advertised customer tool.
    pub fn caller(&self, tool: &ToolName) -> Option<ToolCallerDisposition> {
        self.callers.get(tool).copied()
    }
}

/// Credential-free output of the neutral Messages request kernel.
#[derive(Clone, Debug, PartialEq)]
pub struct LoweredMessagesRequest {
    body: Vec<u8>,
    /// Messages `max_tokens` written into `body`.
    max_tokens: u32,
    operation_digest: Digest,
    tool_ownership: ToolOwnership,
    active_artifact_lineage: Option<crate::artifact::MessagesArtifactLineage>,
    active_artifact_container: Option<String>,
    replay_diagnostics: Vec<String>,
    historical_projection_diagnostics: Vec<ModelDiagnostic>,
    replay_safety: ReplaySafety,
}

impl LoweredMessagesRequest {
    /// Returns what replaying the exact body can cost, classified from the
    /// serialized bytes.
    pub const fn replay_safety(&self) -> ReplaySafety {
        self.replay_safety
    }
    /// Returns deterministic Messages JSON.
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    /// Returns the `max_tokens` value serialized into [`Self::body`].
    pub const fn max_tokens(&self) -> u32 {
        self.max_tokens
    }
    /// Returns the family operation identity.
    pub const fn operation_digest(&self) -> Digest {
        self.operation_digest
    }
    /// Returns exact execution ownership for emitted tools.
    pub const fn tool_ownership(&self) -> &ToolOwnership {
        &self.tool_ownership
    }
    /// Returns bounded non-secret diagnostics for stripped native replay.
    pub fn replay_diagnostics(&self) -> &[String] {
        &self.replay_diagnostics
    }
    /// Returns typed notices for completed thinking omitted across configuration transitions.
    pub fn historical_projection_diagnostics(&self) -> &[ModelDiagnostic] {
        &self.historical_projection_diagnostics
    }
    /// Transfers the already validated active artifact lineage into stream parsing.
    pub fn bind_active_artifact_to_stream(
        &self,
        binding: crate::PreparedMessagesStreamBinding,
    ) -> crate::PreparedMessagesStreamBinding {
        binding.with_active_artifact_lineage(
            self.active_artifact_container.clone(),
            self.active_artifact_lineage.clone(),
        )
    }
}

/// Lowers one already provider-validated canonical plan into Messages wire bytes.
#[allow(
    clippy::too_many_lines,
    reason = "one fail-closed lowering boundary keeps semantic checks ordered"
)]
pub fn lower_request(
    plan: &ValidatedRequestPlan,
    policy: &MessagesRequestPolicy,
    ownership: ToolOwnership,
) -> Result<LoweredMessagesRequest, MessagesRequestError> {
    lower_request_with_active_artifact(plan, policy, ownership, None)
}

/// Lowers one request while validating and consuming the sole active Messages artifact.
#[allow(
    clippy::too_many_lines,
    reason = "one fail-closed lowering boundary keeps semantic and artifact checks ordered"
)]
pub fn lower_request_with_active_artifact(
    plan: &ValidatedRequestPlan,
    policy: &MessagesRequestPolicy,
    mut ownership: ToolOwnership,
    active_artifact: Option<&ActiveFamilyArtifact>,
) -> Result<LoweredMessagesRequest, MessagesRequestError> {
    let request = plan.request();
    if plan.binding_digest() != policy.descriptor_binding
        || request.selected_provider_configuration_id() != policy.selected_provider_configuration
        || request.effective_provider_configuration_id() != policy.effective_provider_configuration
    {
        return Err(invalid(
            "Messages request authority does not match the exact admitted target",
        ));
    }
    if !request.native().entries().is_empty() {
        return Err(unsupported(
            "native options are not enabled by this Messages profile",
        ));
    }
    if !matches!(request.output().format(), OutputFormat::Text) {
        return Err(unsupported(
            "structured output is outside the admitted Messages profile",
        ));
    }
    let prompt_cache = match request.cache() {
        CachePolicy::Disabled | CachePolicy::Enabled { .. } if policy.prompt_cache => true,
        CachePolicy::Disabled => false,
        CachePolicy::Enabled { requirement } => {
            return Err(match requirement {
                CapabilityRequirement::Required => {
                    unsupported("Messages prompt-cache writes are not admitted by this profile")
                }
                _ => unsupported(
                    "preferred Messages prompt cache cannot be silently downgraded during lowering",
                ),
            });
        }
        _ => {
            return Err(unsupported(
                "unknown Messages cache policy cannot be translated",
            ));
        }
    };
    let mut system = Vec::new();
    for identity in &policy.system_prefixes {
        system.push(SystemBlock {
            kind: "text",
            text: identity.clone(),
            cache_control: None,
        });
    }
    for item in request.instructions() {
        system.push(SystemBlock {
            kind: "text",
            text: item.text().to_owned(),
            cache_control: None,
        });
    }
    if prompt_cache {
        let skip_identity_only =
            !policy.system_prefixes.is_empty() && system.len() == policy.system_prefixes.len();
        let cache_index = if skip_identity_only || system.is_empty() {
            None
        } else {
            Some(system.len() - 1)
        };
        if let Some(index) = cache_index {
            system[index].cache_control = Some(policy.cache_control_param());
        }
    }
    let programmatic = request
        .hosted_tools()
        .contains(&HostedTool::ProgrammaticComposition);
    if request
        .hosted_tools()
        .contains(&HostedTool::CodeInterpreter)
    {
        return Err(unsupported(
            "Messages does not admit code-interpreter hosted tools",
        ));
    }
    let caller_policy = match (programmatic, policy.programmatic_tool_caller.as_ref()) {
        (true, Some(policy)) => Some(policy),
        (false, None) => None,
        _ => {
            return Err(invalid(
                "Messages hosted-tool selection does not match the exact policy",
            ));
        }
    };
    if programmatic && request.tool_projection().is_some() {
        return Err(unsupported(
            "Messages hosted programmatic tools require exact configured tool names",
        ));
    }
    let mut tools = if let Some(projection) = request.tool_projection() {
        projection
            .definitions()
            .iter()
            .map(|entry| {
                let Some(input_schema) = entry.input_schema() else {
                    return Err(unsupported(
                        "Messages does not admit projected free-form local tools",
                    ));
                };
                Ok(ToolParam::Customer(ToolDefinitionParam {
                    name: entry.exposed_name().as_str().to_owned(),
                    description: entry.description().to_owned(),
                    input_schema: input_schema.wire_document(),
                    cache_control: None,
                    allowed_callers: None,
                }))
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        request
            .tools()
            .iter()
            .map(|tool| {
                let definition = tool.definition();
                let ToolInputContract::Structured(schema) = definition.input() else {
                    return Err(unsupported("Messages does not admit free-form local tools"));
                };
                let allowed_callers = caller_policy.and_then(|policy| match tool.caller() {
                    ToolCallerDisposition::DirectOnly => None,
                    ToolCallerDisposition::ProviderProgrammaticOnly => {
                        Some(policy.request_callers.clone())
                    }
                    ToolCallerDisposition::DirectAndProviderProgrammatic => {
                        let mut callers = Vec::with_capacity(policy.request_callers.len() + 1);
                        callers.push("direct".into());
                        callers.extend(policy.request_callers.iter().cloned());
                        Some(callers)
                    }
                });
                Ok(ToolParam::Customer(ToolDefinitionParam {
                    name: definition.name().as_str().to_owned(),
                    description: definition.description().to_owned(),
                    input_schema: schema.wire_document(),
                    cache_control: None,
                    allowed_callers,
                }))
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    if prompt_cache && let Some(ToolParam::Customer(tool)) = tools.last_mut() {
        tool.cache_control = Some(policy.cache_control_param());
    }
    if let Some(policy) = caller_policy {
        tools.push(ToolParam::Server(ServerToolParam {
            kind: &policy.response_caller,
            name: "code_execution",
        }));
    }
    let advertised: BTreeSet<ToolName> = if let Some(projection) = request.tool_projection() {
        projection
            .definitions()
            .iter()
            .map(|entry| entry.exposed_name().clone())
            .collect()
    } else {
        request
            .tools()
            .iter()
            .map(|tool| tool.definition().name().clone())
            .collect()
    };
    if ownership
        .owners
        .keys()
        .any(|name| !advertised.contains(name))
    {
        return Err(invalid(
            "tool ownership named a tool absent from the validated request",
        ));
    }
    for name in advertised {
        ownership
            .owners
            .entry(name.clone())
            .or_insert(ToolExecutionOwner::PlakLocal);
        let caller = request
            .tools()
            .iter()
            .find(|tool| tool.definition().name() == &name)
            .map_or(
                ToolCallerDisposition::DirectOnly,
                plakous_model::ConfiguredTool::caller,
            );
        ownership.callers.insert(name, caller);
    }
    for tool in request.tools() {
        let owner = ownership.owner(tool.definition().name());
        tool.validate_owner_projection(&owner)
            .map_err(|error| match error {
                ToolOwnerProjectionError::OwnerMismatch => invalid(error.to_string()),
            })?;
    }
    let mut replay_diagnostics = Vec::new();
    // Semantic compact (and any other Disabled-reasoning request) must not
    // replay thinking blocks: Anthropic 400s when `thinking` is disabled.
    let include_native_thinking = !matches!(request.reasoning(), ReasoningPolicy::Disabled);
    let (mut messages, mut container, historical_projection_diagnostics) = translate_history(
        request.input(),
        policy,
        &ownership,
        programmatic,
        include_native_thinking,
        &mut replay_diagnostics,
    )?;
    // Semantic compact's sealed prefix ends on a complete assistant group.
    // Anthropic Messages requires the last role to be user (prefill is not used).
    if messages
        .last()
        .is_some_and(|message| message.role == MessageRole::Assistant)
    {
        let trailing = request
            .instructions()
            .iter()
            .rev()
            .find(|item| item.source() == InstructionSource::Harness)
            .map(plakous_model::InstructionBlock::text)
            .filter(|text| !text.is_empty())
            .map_or_else(
                || plakous_model::SEMANTIC_COMPACTION_INSTRUCTION.to_owned(),
                str::to_owned,
            );
        push_message(&mut messages, MessageRole::User, vec![text_block(trailing)]);
    }
    if prompt_cache {
        mark_last_message_cache_breakpoint(&mut messages, policy.cache_control_param());
    }
    let tool_set = messages_tool_set_identity(request, policy, &ownership)?;
    let configuration = messages_artifact_configuration_identity(policy, plan.digest())?;
    let artifact_binding = messages_artifact_binding(policy, tool_set, configuration)?;
    let mut active_artifact_lineage = None;
    if let Some(active) = active_artifact {
        if !programmatic {
            return Err(invalid(
                "active Messages artifact requires hosted programmatic composition",
            ));
        }
        let artifact =
            crate::artifact::MessagesFamilyArtifact::from_active(active, artifact_binding)
                .map_err(|error| invalid(error.to_string()))?;
        let covered_index = usize::try_from(artifact.covered_items)
            .map_err(|_| invalid("Messages artifact covered-item count overflow"))?;
        let assistant = covered_index
            .checked_sub(1)
            .and_then(|index| request.input().items().get(index))
            .filter(|item| item.actor() == Actor::Assistant)
            .ok_or_else(|| invalid("Messages artifact does not cover one assistant output"))?;
        let mut replay_container = None;
        let mut lineage = artifact.prior_lineage();
        let (native, _) = translate_assistant_bound(
            assistant.blocks(),
            policy,
            &ownership,
            programmatic,
            include_native_thinking,
            &mut replay_container,
            &mut lineage,
            &mut replay_diagnostics,
        )?;
        artifact
            .validate_reuse(
                plan,
                tool_set,
                configuration,
                &native,
                replay_container
                    .as_deref()
                    .or(Some(artifact.container.as_str())),
            )
            .map_err(|error| invalid(error.to_string()))?;
        let replay_message = messages
            .iter_mut()
            .rfind(|message| {
                message.role == MessageRole::Assistant
                    && crate::artifact::native_blocks_match_canonical(
                        artifact.native_blocks(),
                        &message.content,
                    )
            })
            .ok_or_else(|| invalid("Messages artifact assistant replay was not found"))?;
        replay_message.content = artifact.native_blocks().to_vec();
        if container
            .as_deref()
            .is_some_and(|value| value != artifact.container)
        {
            return Err(invalid(
                "Messages artifact container drifted during complete lowering",
            ));
        }
        container = Some(artifact.container.clone());
        active_artifact_lineage = Some(artifact.active_lineage());
    }
    let configured_max_tokens = request
        .limits()
        .max_output_tokens()
        .map_or(policy.maximum_output_tokens, std::num::NonZeroU32::get);
    if configured_max_tokens > policy.maximum_output_tokens {
        return Err(invalid(
            "requested max output tokens exceed the exact model limit",
        ));
    }
    let max_tokens = policy
        .request_output_cap
        .map_or(configured_max_tokens, |cap| configured_max_tokens.min(cap));
    if max_tokens == 0 {
        return Err(invalid("Messages max_tokens must be non-zero"));
    }
    let (thinking, output_config) = translate_reasoning(
        request.reasoning(),
        policy.minimum_thinking_budget,
        max_tokens,
        policy.omit_disabled_thinking,
        policy.thinking_display,
    )?;
    if matches!(&thinking, Some(ThinkingParam::Enabled { .. }))
        && matches!(
            request.tool_choice(),
            ToolChoice::Required | ToolChoice::Specific(_)
        )
    {
        return Err(invalid(
            "manual Messages thinking is incompatible with forced or specific tool choice",
        ));
    }
    let active_artifact_container = active_artifact_lineage
        .as_ref()
        .and_then(|_| container.clone());
    let has_tools = !tools.is_empty();
    let body = MessagesRequestBody {
        model: policy.requested_model.as_str(),
        max_tokens,
        cache_control: (prompt_cache && policy.top_level_prompt_cache)
            .then(|| policy.cache_control_param()),
        system,
        messages,
        container,
        tools,
        tool_choice: translate_tool_choice(request.tool_choice(), has_tools)?,
        thinking,
        context_management: policy
            .claude_oauth_context_management
            .then_some(CLAUDE_OAUTH_CONTEXT_MANAGEMENT),
        output_config,
        provider: policy.router.as_ref().map(RouterParam::gateway_managed),
        metadata: policy
            .metadata_user_id
            .as_deref()
            .map(|user_id| MetadataParam { user_id }),
        stop_sequences: request.limits().stop_sequences(),
        plugins: policy.plugins,
        session_id: policy.session_id.as_deref(),
        diagnostics: policy.claude_oauth_diagnostics.then_some(DiagnosticsParam {
            previous_message_id: policy
                .claude_oauth_previous_message_id
                .as_ref()
                .map(|id| id.0.as_str()),
        }),
        stream: true,
    };
    let mut body = serde_json::to_vec(&body)
        .map_err(|e| invalid(format!("failed to serialize Messages request: {e}")))?;
    if let Some(SerializedBodyTransform(transform)) = policy.serialized_body_transform {
        transform(&mut body).map_err(invalid)?;
    }
    let owner_bytes = serde_json::to_vec(&ownership)
        .map_err(|e| invalid(format!("failed to bind tool ownership: {e}")))?;
    let router_identity = messages_router_identity(policy)?;
    let operation_digest = Digest::sha256(
        DigestDomain::RequestPlan,
        3,
        &[
            &plan.digest().identity_bytes(),
            &plan.binding_digest().identity_bytes(),
            policy.api_version.as_bytes(),
            policy.beta_header.as_bytes(),
            &policy.account_binding.identity_bytes(),
            &policy
                .selected_provider_configuration
                .digest()
                .identity_bytes(),
            &policy
                .effective_provider_configuration
                .digest()
                .identity_bytes(),
            &MESSAGES_REPLAY_SCHEMA_REVISION.to_be_bytes(),
            &router_identity,
            &owner_bytes,
            &body,
        ],
    )
    .map_err(|e| invalid(format!("failed to digest Messages request: {e}")))?;
    replay_diagnostics.sort_unstable();
    replay_diagnostics.dedup();
    let replay_safety = classify_replay_safety(&body);
    Ok(LoweredMessagesRequest {
        replay_safety,
        body,
        max_tokens,
        operation_digest,
        tool_ownership: ownership,
        active_artifact_lineage,
        active_artifact_container,
        replay_diagnostics,
        historical_projection_diagnostics,
    })
}

/// Top-level Messages fields whose replay can only cost duplicate inference.
/// `context_management` edits transform what the request itself sent.
const COST_ONLY_FIELDS: &[&str] = &[
    "cache_control",
    "context_management",
    "diagnostics",
    "max_tokens",
    "messages",
    "metadata",
    "model",
    "output_config",
    "plugins",
    "provider",
    "session_id",
    "stop_sequences",
    "stream",
    "system",
    "temperature",
    "thinking",
    "tool_choice",
    "tools",
    "top_k",
    "top_p",
];

/// Classifies the exact serialized body by allowlist: only client tools
/// (no `type`, or `custom`) without programmatic callers, disabled gateway
/// plugins, and the fields above are cost-only. A server tool, a container,
/// an enabled plugin, or any field nobody classified is a server effect.
fn classify_replay_safety(body: &[u8]) -> ReplaySafety {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return ReplaySafety::ServerEffect;
    };
    let pure = fields.iter().all(|(name, value)| match name.as_str() {
        "tools" => value.as_array().is_some_and(|tools| {
            tools.iter().all(|tool| {
                tool.as_object().is_some_and(|tool| {
                    !tool.contains_key("allowed_callers")
                        && tool
                            .get("type")
                            .is_none_or(|kind| kind.as_str() == Some("custom"))
                })
            })
        }),
        "plugins" => value.as_array().is_some_and(|plugins| {
            plugins
                .iter()
                .all(|plugin| plugin.get("enabled") == Some(&Value::Bool(false)))
        }),
        name => COST_ONLY_FIELDS.contains(&name),
    });
    if pure {
        ReplaySafety::CostOnly
    } else {
        ReplaySafety::ServerEffect
    }
}

pub(crate) fn messages_tool_set_identity(
    request: &plakous_model::ModelRequest,
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
) -> Result<Digest, MessagesRequestError> {
    let tools = serde_json::to_vec(&(request.tools(), request.hosted_tools(), ownership))
        .map_err(|_| invalid("Messages tool set could not be sealed"))?;
    let caller_policy =
        serde_json::to_vec(&policy.programmatic_tool_caller.as_ref().map(|caller| {
            (
                &caller.request_callers,
                &caller.response_caller,
                &caller.namespace,
            )
        }))
        .map_err(|_| invalid("Messages caller policy could not be sealed"))?;
    Digest::sha256(
        DigestDomain::Blob,
        1,
        &[
            b"plakous-messages/hosted-tool-set-v1",
            &tools,
            &caller_policy,
        ],
    )
    .map_err(|_| invalid("Messages tool set identity failed"))
}

pub(crate) fn messages_artifact_configuration_identity(
    policy: &MessagesRequestPolicy,
    _plan: Digest,
) -> Result<Digest, MessagesRequestError> {
    let router_identity = messages_router_identity(policy)?;
    Digest::sha256(
        DigestDomain::Blob,
        1,
        &[
            b"plakous-messages/hosted-configuration-v1",
            &policy
                .selected_provider_configuration
                .digest()
                .identity_bytes(),
            &policy
                .effective_provider_configuration
                .digest()
                .identity_bytes(),
            policy.api_version.as_bytes(),
            policy.beta_header.as_bytes(),
            &router_identity,
        ],
    )
    .map_err(|_| invalid("Messages configuration identity failed"))
}

fn messages_router_identity(
    policy: &MessagesRequestPolicy,
) -> Result<Vec<u8>, MessagesRequestError> {
    policy.router.as_ref().map_or_else(
        || Ok(Vec::new()),
        |router| {
            serde_json::to_vec(&RouterParam::gateway_managed(router))
                .map_err(|_| invalid("Messages router identity failed"))
        },
    )
}

pub(crate) fn messages_artifact_binding(
    policy: &MessagesRequestPolicy,
    tool_set: Digest,
    configuration: Digest,
) -> Result<Digest, MessagesRequestError> {
    Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[
            b"plakous-messages/hosted-artifact-binding-v1",
            &policy.descriptor_binding.identity_bytes(),
            &policy.account_binding.identity_bytes(),
            &tool_set.identity_bytes(),
            &configuration.identity_bytes(),
            policy.requested_model.as_str().as_bytes(),
        ],
    )
    .map_err(|_| invalid("Messages artifact binding identity failed"))
}

#[derive(Serialize)]
struct MessagesRequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControlParam>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<SystemBlock>,
    messages: Vec<MessageParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolParam<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoiceParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_management: Option<ContextManagementParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfigParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<RouterParam<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<MetadataParam<'a>>,
    #[serde(skip_serializing_if = "slice_is_empty")]
    stop_sequences: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    plugins: Option<&'a [MessagesRequestPlugin]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<DiagnosticsParam<'a>>,
    stream: bool,
}

#[derive(Serialize)]
struct DiagnosticsParam<'a> {
    previous_message_id: Option<&'a str>,
}

/// Claude Code's request edit: keep every thinking block. This is not the
/// response `context_management.applied_edits` object.
const CLAUDE_OAUTH_CONTEXT_MANAGEMENT: ContextManagementParam = ContextManagementParam {
    edits: [ClearThinkingEdit {
        edit_type: "clear_thinking_20251015",
        keep: "all",
    }],
};

#[derive(Clone, Copy, Serialize)]
struct ContextManagementParam {
    edits: [ClearThinkingEdit; 1],
}

#[derive(Clone, Copy, Serialize)]
struct ClearThinkingEdit {
    #[serde(rename = "type")]
    edit_type: &'static str,
    keep: &'static str,
}

#[derive(Clone, Copy, Serialize)]
struct MetadataParam<'a> {
    user_id: &'a str,
}

#[derive(Clone, Copy, Serialize)]
struct RouterParam<'a> {
    allow_fallbacks: bool,
    require_parameters: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_collection: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    zdr: Option<bool>,
}

impl<'a> RouterParam<'a> {
    fn gateway_managed(policy: &'a MessagesRouterPolicy) -> Self {
        Self {
            allow_fallbacks: false,
            require_parameters: true,
            data_collection: policy.data_collection.as_deref(),
            zdr: policy.zdr,
        }
    }
}
fn slice_is_empty<T>(value: &[T]) -> bool {
    value.is_empty()
}
#[derive(Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControlParam>,
}
#[derive(Serialize)]
struct ToolDefinitionParam {
    name: String,
    description: String,
    input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControlParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_callers: Option<Vec<String>>,
}
#[derive(Serialize)]
#[serde(untagged)]
enum ToolParam<'a> {
    Customer(ToolDefinitionParam),
    Server(ServerToolParam<'a>),
}
#[derive(Serialize)]
struct ServerToolParam<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    name: &'static str,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CacheControlParam {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ttl: Option<String>,
}
impl CacheControlParam {
    fn ephemeral() -> Self {
        Self {
            kind: "ephemeral".to_owned(),
            ttl: None,
        }
    }

    fn ephemeral_with_ttl(ttl: &'static str) -> Self {
        Self {
            kind: "ephemeral".to_owned(),
            ttl: Some(ttl.to_owned()),
        }
    }
}
#[derive(Serialize)]
struct OutputConfigParam {
    effort: &'static str,
}
#[derive(Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MessageRole {
    User,
    Assistant,
}
#[derive(Serialize)]
struct MessageParam {
    role: MessageRole,
    content: Vec<MessageBlock>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum MessageBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlParam>,
    },
    Thinking {
        thinking: String,
        signature: Box<RawValue>,
    },
    RedactedThinking {
        data: Box<RawValue>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<ToolCallerParam>,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    CodeExecutionToolResult {
        tool_use_id: String,
        content: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ToolResultContent>,
        #[serde(skip_serializing_if = "is_false")]
        is_error: bool,
    },
}
impl PartialEq for MessageBlock {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_vec(self).ok() == serde_json::to_vec(other).ok()
    }
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolCallerParam {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_id: Option<String>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ToolResultContent {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlParam>,
    },
}
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip predicate receives a reference"
)]
const fn is_false(value: &bool) -> bool {
    !*value
}
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToolChoiceParam {
    Auto,
    None,
    Any,
    Tool { name: String },
}
/// Exact Messages thinking presentation selected by the owning route.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagesThinkingDisplay {
    /// Emit progress updates while omitting the remaining thinking text.
    Updates,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ThinkingParam {
    Disabled,
    Enabled {
        budget_tokens: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<MessagesThinkingDisplay>,
    },
    Adaptive {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<MessagesThinkingDisplay>,
    },
}
#[derive(Clone)]
pub(crate) struct PendingTool {
    call_id: CallId,
    remote_id: String,
    owner: ToolExecutionOwner,
    correlation: plakous_model::ToolCorrelation,
}

type TranslatedHistory = (Vec<MessageParam>, Option<String>, Vec<ModelDiagnostic>);

fn translate_history(
    input: &plakous_model::ConversationView,
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
    programmatic: bool,
    include_native_thinking: bool,
    replay_diagnostics: &mut Vec<String>,
) -> Result<TranslatedHistory, MessagesRequestError> {
    let mut messages: Vec<MessageParam> = Vec::new();
    let mut historical_projection_diagnostics = Vec::new();
    let items = input.items();
    let completed_items = match policy.completed_history_prefix {
        Some(prefix)
            if input.semantic_prefix_identity(prefix.item_count()).ok() == Some(prefix) =>
        {
            prefix.item_count()
        }
        Some(_) => {
            return Err(invalid(
                "completed history prefix does not match this request",
            ));
        }
        None => 0,
    };
    let mut index = 0;
    let mut pending = Vec::<PendingTool>::new();
    let mut container = None;
    let mut server_lineage = crate::artifact::MessagesArtifactLineage::default();
    while index < items.len() {
        let item = &items[index];
        match item.actor() {
            Actor::Assistant => {
                let extending = !pending.is_empty();
                let (blocks, calls) = translate_assistant_with_history(
                    item.blocks(),
                    policy,
                    ownership,
                    programmatic,
                    include_native_thinking,
                    index < completed_items,
                    &mut container,
                    &mut server_lineage,
                    replay_diagnostics,
                    &mut historical_projection_diagnostics,
                )?;
                if extending {
                    // A cancelled batch records each tool call as its own
                    // assistant item, with the results after all of them.
                    // Only further `tool_use` blocks fold into the open
                    // assistant message: thinking must lead that message, and
                    // text or server-tool output in front of unfinished
                    // results is not a batch.
                    let only_tool_use = blocks
                        .iter()
                        .all(|block| matches!(block, MessageBlock::ToolUse { .. }));
                    if !only_tool_use || calls.is_empty() {
                        return Err(invalid(
                            "assistant output intervened before all tool results",
                        ));
                    }
                    match messages.last_mut() {
                        Some(message) if message.role == MessageRole::Assistant => {
                            message.content.extend(blocks);
                        }
                        _ => {
                            return Err(invalid(
                                "assistant output intervened before all tool results",
                            ));
                        }
                    }
                    pending.extend(calls);
                } else {
                    if !blocks.is_empty() {
                        push_message(&mut messages, MessageRole::Assistant, blocks);
                    }
                    pending = calls;
                }
                index += 1;
            }
            Actor::Tool => {
                let result_blocks = translate_tool_messages(items, &mut index, &mut pending)?;
                push_message(&mut messages, MessageRole::User, result_blocks);
            }
            Actor::User => {
                if !pending.is_empty() {
                    return Err(invalid("user content intervened before tool results"));
                }
                push_message(
                    &mut messages,
                    MessageRole::User,
                    translate_user(item.blocks())?,
                );
                index += 1;
            }
            Actor::System | Actor::Developer | Actor::Harness => {
                return Err(unsupported(
                    "historical instruction actors require an admitted native capability",
                ));
            }
            _ => {
                return Err(unsupported(
                    "unknown canonical actor cannot be translated safely",
                ));
            }
        }
    }
    if !pending.is_empty() {
        return Err(invalid("history ends before required tool results"));
    }
    Ok((messages, container, historical_projection_diagnostics))
}

fn translate_tool_messages(
    items: plakous_model::ConversationItems<'_>,
    index: &mut usize,
    pending: &mut Vec<PendingTool>,
) -> Result<Vec<MessageBlock>, MessagesRequestError> {
    if pending.is_empty() {
        return Err(invalid("tool result has no immediately preceding tool use"));
    }
    let mut result_blocks = Vec::new();
    let mut updates = BTreeMap::<CallId, Vec<&ToolResultUpdateBlock>>::new();
    let mut terminated = BTreeSet::<CallId>::new();
    while *index < items.len() && matches!(items[*index].actor(), Actor::Tool) {
        for block in items[*index].blocks() {
            match block {
                ContentBlock::ToolResultUpdate(update) => {
                    if !pending.iter().any(|tool| tool.call_id == update.call_id()) {
                        return Err(invalid("tool-result update has no pending tool use"));
                    }
                    if terminated.contains(&update.call_id()) {
                        return Err(invalid("tool-result update followed its terminal result"));
                    }
                    updates.entry(update.call_id()).or_default().push(update);
                }
                ContentBlock::ToolResult(result) => {
                    let expected = pending
                        .get(result_blocks.len())
                        .ok_or_else(|| invalid("too many tool results"))?;
                    if result.call_id() != expected.call_id {
                        return Err(invalid("parallel tool results are not in source order"));
                    }
                    let result_updates = updates.remove(&result.call_id()).unwrap_or_default();
                    result_blocks.push(translate_tool_result(result, expected, &result_updates)?);
                    terminated.insert(result.call_id());
                }
                _ => {
                    return Err(invalid(
                        "Tool actor may contain only tool results or updates",
                    ));
                }
            }
        }
        *index += 1;
    }
    if !updates.is_empty() {
        return Err(invalid("tool-result update has no terminal result"));
    }
    if result_blocks.len() != pending.len() {
        return Err(invalid(
            "all parallel tool results must be returned together",
        ));
    }
    if *index < items.len() && matches!(items[*index].actor(), Actor::User) {
        if pending
            .iter()
            .any(|tool| !matches!(tool.correlation.caller_lineage(), ToolCallerLineage::Direct))
        {
            return Err(invalid(
                "programmatic continuation user message must contain only tool results",
            ));
        }
        result_blocks.extend(translate_user(items[*index].blocks())?);
        *index += 1;
    }
    pending.clear();
    Ok(result_blocks)
}

fn text_block(text: impl Into<String>) -> MessageBlock {
    MessageBlock::Text {
        text: text.into(),
        cache_control: None,
    }
}

fn tool_result_text(text: impl Into<String>) -> ToolResultContent {
    ToolResultContent::Text {
        text: text.into(),
        cache_control: None,
    }
}

fn mark_last_message_cache_breakpoint(messages: &mut [MessageParam], cache: CacheControlParam) {
    for message in messages.iter_mut().rev() {
        for block in message.content.iter_mut().rev() {
            match block {
                MessageBlock::Text { cache_control, .. } => {
                    *cache_control = Some(cache);
                    return;
                }
                MessageBlock::ToolResult { content, .. } => {
                    if let Some(ToolResultContent::Text { cache_control, .. }) = content.last_mut()
                    {
                        *cache_control = Some(cache);
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}

fn push_message(
    messages: &mut Vec<MessageParam>,
    role: MessageRole,
    mut content: Vec<MessageBlock>,
) {
    if let Some(last) = messages.last_mut().filter(|message| message.role == role) {
        last.content.append(&mut content);
    } else {
        messages.push(MessageParam { role, content });
    }
}

fn portable_reasoning_text(reasoning: &plakous_model::ReasoningBlock) -> Option<MessageBlock> {
    reasoning
        .text()
        .filter(|text| !text.is_empty())
        .map(text_block)
}

/// Reasoning that cannot be replayed natively, under a policy that forbids
/// sending it as text.
///
/// Anthropic replays a thinking block exactly with its signature and a redacted
/// block opaque. Reasoning has no third form on this wire: handing the model's
/// own chain of thought back as assistant text is what a resumed Claude Opus 5
/// turn read as `reasoning_extraction` and refused. When the signature is not
/// there to send, the block is dropped, not translated.
fn unreplayable_thinking(
    reasoning: &plakous_model::ReasoningBlock,
    policy: &MessagesRequestPolicy,
    reason: &str,
    diagnose_when_lenient: bool,
    replay_diagnostics: &mut Vec<String>,
) -> Option<MessageBlock> {
    if policy.strict_thinking_replay {
        replay_diagnostics.push(format!(
            "{reason}; the block was dropped, never sent as text"
        ));
        return None;
    }
    if diagnose_when_lenient {
        replay_diagnostics.push(reason.to_owned());
    }
    portable_reasoning_text(reasoning)
}

fn native_or_portable_thinking(
    reasoning: &plakous_model::ReasoningBlock,
    policy: &MessagesRequestPolicy,
    kind: &str,
    visible: bool,
    replay_diagnostics: &mut Vec<String>,
) -> Option<MessageBlock> {
    let Some(replay) = reasoning.replay() else {
        return unreplayable_thinking(
            reasoning,
            policy,
            "Messages thinking carried no replay signature",
            false,
            replay_diagnostics,
        );
    };
    if replay.protocol() != &policy.protocol {
        return unreplayable_thinking(
            reasoning,
            policy,
            "incompatible Messages thinking replay was stripped",
            true,
            replay_diagnostics,
        );
    }
    let Ok(payload) = replay_payload(replay, policy, kind) else {
        return unreplayable_thinking(
            reasoning,
            policy,
            "incompatible Messages thinking replay was stripped",
            true,
            replay_diagnostics,
        );
    };
    if matches!(
        serde_json::from_str::<String>(payload.get()).as_deref(),
        Ok("")
    ) {
        return unreplayable_thinking(
            reasoning,
            policy,
            "empty Messages thinking replay was stripped",
            true,
            replay_diagnostics,
        );
    }
    Some(if visible {
        MessageBlock::Thinking {
            thinking: reasoning.text().unwrap_or_default().to_owned(),
            signature: payload,
        }
    } else {
        MessageBlock::RedactedThinking { data: payload }
    })
}

fn translate_reasoning_block(
    reasoning: &plakous_model::ReasoningBlock,
    policy: &MessagesRequestPolicy,
    replay_diagnostics: &mut Vec<String>,
) -> Option<MessageBlock> {
    match reasoning.visibility() {
        ReasoningVisibility::Visible | ReasoningVisibility::Omitted => native_or_portable_thinking(
            reasoning,
            policy,
            SIGNATURE_REPLAY_KIND,
            true,
            replay_diagnostics,
        ),
        ReasoningVisibility::Redacted => native_or_portable_thinking(
            reasoning,
            policy,
            REDACTED_REPLAY_KIND,
            false,
            replay_diagnostics,
        ),
        ReasoningVisibility::OpaqueReplay => None,
        // A summary, or any visibility this wire has no native form for, is
        // still reasoning. It is dropped rather than flattened into text.
        _ => unreplayable_thinking(
            reasoning,
            policy,
            "Messages has no native form for this reasoning visibility",
            false,
            replay_diagnostics,
        ),
    }
}

pub(crate) fn translate_assistant_bound(
    blocks: &[ContentBlock],
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
    programmatic: bool,
    include_native_thinking: bool,
    container: &mut Option<String>,
    server_lineage: &mut crate::artifact::MessagesArtifactLineage,
    replay_diagnostics: &mut Vec<String>,
) -> Result<(Vec<MessageBlock>, Vec<PendingTool>), MessagesRequestError> {
    translate_assistant_with_history(
        blocks,
        policy,
        ownership,
        programmatic,
        include_native_thinking,
        false,
        container,
        server_lineage,
        replay_diagnostics,
        &mut Vec::new(),
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "historical projection adds one turn-boundary fact"
)]
fn translate_assistant_with_history(
    blocks: &[ContentBlock],
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
    programmatic: bool,
    include_native_thinking: bool,
    completed_turn: bool,
    container: &mut Option<String>,
    server_lineage: &mut crate::artifact::MessagesArtifactLineage,
    replay_diagnostics: &mut Vec<String>,
    historical_projection_diagnostics: &mut Vec<ModelDiagnostic>,
) -> Result<(Vec<MessageBlock>, Vec<PendingTool>), MessagesRequestError> {
    let mut output = Vec::with_capacity(blocks.len());
    let mut pending = Vec::new();
    let mut remote_ids = BTreeSet::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => output.push(text_block(text.text().to_owned())),
            ContentBlock::Reasoning(reasoning) => {
                if policy.strict_thinking_replay
                    && let Some(replay) = reasoning.replay()
                {
                    let kind = if reasoning.visibility() == ReasoningVisibility::Redacted {
                        REDACTED_REPLAY_KIND
                    } else {
                        SIGNATURE_REPLAY_KIND
                    };
                    if completed_turn && projectable_historical_thinking(replay, policy, kind) {
                        let identity = historical_projection_identity(replay, policy)?;
                        record_historical_projection(identity, historical_projection_diagnostics);
                        continue;
                    }
                    replay_payload(replay, policy, kind)?;
                }
                if include_native_thinking
                    && let Some(block) =
                        translate_reasoning_block(reasoning, policy, replay_diagnostics)
                {
                    output.push(block);
                }
            }
            ContentBlock::ToolCall(call) => {
                let (block, tool) = translate_assistant_tool_call(
                    call,
                    policy,
                    ownership,
                    programmatic,
                    container,
                    server_lineage,
                    &mut remote_ids,
                )?;
                output.push(block);
                pending.extend(tool);
            }
            ContentBlock::Opaque(opaque)
                if opaque.replay().protocol() == &policy.protocol
                    && opaque.replay().kind().as_str() == CODE_EXECUTION_RESULT_REPLAY_KIND =>
            {
                if !programmatic {
                    return Err(invalid("hosted result replay is not admitted"));
                }
                let raw =
                    replay_payload(opaque.replay(), policy, CODE_EXECUTION_RESULT_REPLAY_KIND)?;
                let value: Value = serde_json::from_str(raw.get())
                    .map_err(|_| invalid("invalid hosted result replay"))?;
                let object = value
                    .as_object()
                    .ok_or_else(|| invalid("hosted result replay must be an object"))?;
                let tool_use_id = object
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .filter(|id| server_lineage.is_unresolved(id))
                    .ok_or_else(|| invalid("hosted result does not match a server tool"))?;
                let content = object
                    .get("content")
                    .cloned()
                    .ok_or_else(|| invalid("hosted result requires content"))?;
                output.push(MessageBlock::CodeExecutionToolResult {
                    tool_use_id: tool_use_id.into(),
                    content,
                });
                server_lineage.complete_next(tool_use_id).map_err(|_| {
                    invalid("hosted results did not follow exact server-tool order")
                })?;
            }
            ContentBlock::Opaque(opaque) => match opaque.fallback() {
                OpaqueFallback::SemanticSummary(summary) => {
                    output.push(text_block(summary.clone()));
                }
                OpaqueFallback::Omit | OpaqueFallback::CompatibleReplay => {}
                _ => {
                    return Err(unsupported(
                        "assistant content block is outside the Messages profile",
                    ));
                }
            },
            ContentBlock::Citation(_) => {}
            ContentBlock::Refusal(refusal) => output.push(text_block(refusal.text().to_owned())),
            ContentBlock::Compaction(compaction) if compaction.native_item().is_some() => {
                return Err(native_compaction_refusal());
            }
            ContentBlock::Compaction(compaction) => {
                output.push(text_block(compaction.summary().to_owned()));
            }
            _ => {
                return Err(unsupported(
                    "assistant content block is outside the Messages profile",
                ));
            }
        }
    }
    Ok((output, pending))
}

fn translate_assistant_tool_call(
    call: &ToolCallBlock,
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
    programmatic: bool,
    container: &mut Option<String>,
    server_lineage: &mut crate::artifact::MessagesArtifactLineage,
    remote_ids: &mut BTreeSet<String>,
) -> Result<(MessageBlock, Option<PendingTool>), MessagesRequestError> {
    // Messages `tool_use.input` is always an object. A structured call is one
    // already. A UTF-8 cell (Codex Code Mode `exec`) is canonical as a JSON
    // string literal, and a call whose text never parsed as JSON has no
    // parsed value at all; both replay truthfully as `{"input": <text>}` so a
    // rebind onto Messages re-projects the prefix instead of
    // refusing it.
    let input = match call.arguments() {
        Some(Value::Object(_)) => call
            .arguments()
            .cloned()
            .unwrap_or_else(|| unreachable!("matched a parsed object")),
        Some(other) => serde_json::json!({ "input": other }),
        None => serde_json::json!({ "input": call.raw_arguments() }),
    };
    let remote_id = call
        .correlation()
        .remote_call_id()
        .ok_or_else(|| invalid("tool call requires exact remote correlation"))?
        .to_owned();
    if !remote_ids.insert(remote_id.clone()) {
        return Err(invalid("duplicate provider tool-use ID"));
    }
    if matches!(call.owner(), ToolExecutionOwner::ProviderHosted { .. }) {
        if !programmatic
            || call.exposed_name().as_str() != "code_execution"
            || !matches!(
                call.correlation().caller_lineage(),
                ToolCallerLineage::Direct
            )
            || call.correlation().container_id().is_some()
        {
            return Err(invalid("incompatible provider-hosted Messages tool call"));
        }
        server_lineage
            .push_server(&remote_id)
            .map_err(|_| invalid("duplicate provider server-tool ID"))?;
        return Ok((
            MessageBlock::ServerToolUse {
                id: remote_id,
                name: "code_execution".into(),
                input,
            },
            None,
        ));
    }
    let caller = translate_tool_caller(
        call,
        policy,
        ownership,
        programmatic,
        container,
        server_lineage,
    )?;
    Ok((
        MessageBlock::ToolUse {
            id: remote_id.clone(),
            name: call.exposed_name().as_str().to_owned(),
            input,
            caller,
        },
        Some(PendingTool {
            call_id: call.call_id(),
            remote_id,
            owner: call.owner().clone(),
            correlation: call.correlation().clone(),
        }),
    ))
}

fn translate_tool_caller(
    call: &ToolCallBlock,
    policy: &MessagesRequestPolicy,
    ownership: &ToolOwnership,
    programmatic: bool,
    container: &mut Option<String>,
    server_lineage: &crate::artifact::MessagesArtifactLineage,
) -> Result<Option<ToolCallerParam>, MessagesRequestError> {
    Ok(match call.correlation().caller_lineage() {
        ToolCallerLineage::Direct => {
            if call.correlation().container_id().is_some() {
                return Err(invalid("direct tool replay cannot invent a container"));
            }
            None
        }
        ToolCallerLineage::ProviderProgrammatic { kind, execution_id } => {
            let Some(caller_policy) = policy.programmatic_tool_caller.as_ref() else {
                return Err(invalid(
                    "programmatic caller is not admitted by this policy",
                ));
            };
            if !programmatic
                || kind != &caller_policy.namespace
                || !server_lineage.is_unresolved(execution_id)
            {
                return Err(invalid(
                    "programmatic caller does not match a preceding server tool",
                ));
            }
            if !matches!(
                ownership.caller(call.exposed_name()),
                Some(
                    ToolCallerDisposition::ProviderProgrammaticOnly
                        | ToolCallerDisposition::DirectAndProviderProgrammatic
                )
            ) {
                return Err(invalid("tool is not eligible for programmatic callers"));
            }
            let call_container = call
                .correlation()
                .container_id()
                .ok_or_else(|| invalid("programmatic tool call requires a container"))?;
            match container {
                Some(existing) if existing != call_container => {
                    return Err(invalid(
                        "programmatic tool calls used mismatched containers",
                    ));
                }
                Some(_) => {}
                None => *container = Some(call_container.to_owned()),
            }
            Some(ToolCallerParam {
                kind: caller_policy.response_caller.clone(),
                tool_id: Some(execution_id.clone()),
            })
        }
        ToolCallerLineage::LocalProgrammatic { .. } => {
            return Err(invalid(
                "local programmatic caller cannot be translated to the Messages provider wire format",
            ));
        }
    })
}

#[cfg(test)]
fn translate_assistant(
    blocks: &[ContentBlock],
    policy: &MessagesRequestPolicy,
) -> Result<(Vec<MessageBlock>, Vec<PendingTool>), MessagesRequestError> {
    translate_assistant_bound(
        blocks,
        policy,
        &ToolOwnership::default(),
        false,
        true,
        &mut None,
        &mut crate::artifact::MessagesArtifactLineage::default(),
        &mut Vec::new(),
    )
}

fn translate_user(blocks: &[ContentBlock]) -> Result<Vec<MessageBlock>, MessagesRequestError> {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => Ok(text_block(text.text().to_owned())),
            _ => Err(unsupported(
                "user content block is outside the Messages profile",
            )),
        })
        .collect()
}

fn translate_tool_result(
    result: &plakous_model::ToolResultBlock,
    expected: &PendingTool,
    updates: &[&ToolResultUpdateBlock],
) -> Result<MessageBlock, MessagesRequestError> {
    if matches!(result.owner(), ToolExecutionOwner::ProviderHosted { .. }) {
        return Err(unsupported(
            "provider-hosted results cannot be sent as client tool results",
        ));
    }
    if result.owner() != &expected.owner {
        return Err(invalid(
            "tool-result execution owner does not match the assistant tool call",
        ));
    }
    if result.correlation().remote_call_id() != Some(expected.remote_id.as_str()) {
        return Err(invalid(
            "tool-result remote correlation does not match the assistant tool call",
        ));
    }
    if result.correlation() != &expected.correlation {
        return Err(invalid(
            "tool-result caller/container correlation does not match the tool call",
        ));
    }
    let mut content = updates
        .iter()
        .map(|update| {
            if update.owner() != result.owner() || update.correlation() != result.correlation() {
                return Err(invalid(
                    "tool-result update correlation does not match terminal result",
                ));
            }
            Ok(tool_result_text(update.text().text().to_owned()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    content.extend(
        result
            .parts()
            .iter()
            .map(|part| match part {
                ToolResultPart::Text(text) => Ok(tool_result_text(text.text().to_owned())),
                ToolResultPart::Image(image)
                    if matches!(
                        expected.correlation.caller_lineage(),
                        ToolCallerLineage::Direct
                    ) =>
                {
                    Ok(tool_result_text(format!(
                        "[image result: {}]",
                        image.media_type()
                    )))
                }
                ToolResultPart::Audio(audio)
                    if matches!(
                        expected.correlation.caller_lineage(),
                        ToolCallerLineage::Direct
                    ) =>
                {
                    Ok(tool_result_text(format!(
                        "[audio result: {}]",
                        audio.media_type()
                    )))
                }
                _ => Err(unsupported("tool result is outside the Messages profile")),
            })
            .collect::<Result<Vec<_>, _>>()?,
    );
    Ok(MessageBlock::ToolResult {
        tool_use_id: expected.remote_id.clone(),
        content,
        is_error: matches!(result.kind(), ToolResultKind::Failure),
    })
}

/// A completed assistant turn may lose its native thinking on a rebind. The
/// source envelope still has to be internally well formed for this Messages
/// route; none of its opaque bytes are sent when the target binding changes.
fn projectable_historical_thinking(
    replay: &ReplayEnvelope,
    policy: &MessagesRequestPolicy,
    expected_kind: &str,
) -> bool {
    let ReplayScope::ExactModel(source_model) = replay.scope() else {
        return false;
    };
    if replay.protocol() != &policy.protocol
        || replay.model_family().is_some()
        || replay.kind().as_str() != expected_kind
        || replay.classification() != DataClassification::Sensitive
        || replay.version() != MESSAGES_REPLAY_SCHEMA_REVISION
    {
        return false;
    }
    let Ok(bound) = serde_json::from_str::<BoundReplayPayload>(replay.payload().get()) else {
        return false;
    };
    bound.actual_model == *source_model
        && bound.replay_schema_revision == MESSAGES_REPLAY_SCHEMA_REVISION
        && serde_json::from_str::<String>(bound.value.get()).is_ok()
        && (bound.requested_model != policy.requested_model
            || bound.semantic_descriptor_binding != policy.semantic_descriptor_binding
            || bound.account_binding != policy.account_binding
            || bound.selected_provider_configuration != policy.selected_provider_configuration
            || bound.effective_provider_configuration != policy.effective_provider_configuration
            || bound.api_version != policy.api_version
            || bound.beta_header != policy.beta_header
            || policy
                .expected_reported_model
                .as_ref()
                .is_some_and(|expected| expected != source_model))
}

fn record_historical_projection(identity: Digest, diagnostics: &mut Vec<ModelDiagnostic>) {
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.history_projection_identity() == Some(identity))
    {
        return;
    }
    diagnostics.push(ModelDiagnostic::historical_projection(
        identity,
        "Configuration transition: completed historical thinking was omitted; conversation content was retained. No private thinking or signatures were sent as text.",
    ));
}

// Only compatibility facts identify the transition. In particular, signatures,
// reasoning, freshness bindings and the growing request projection do not.
fn historical_projection_identity(
    replay: &ReplayEnvelope,
    policy: &MessagesRequestPolicy,
) -> Result<Digest, MessagesRequestError> {
    let bound: BoundReplayPayload = serde_json::from_str(replay.payload().get())
        .map_err(|_| invalid("opaque replay binding was malformed"))?;
    let facts = serde_json::to_value((
        (
            replay.protocol(),
            &bound.requested_model,
            &bound.actual_model,
            bound.semantic_descriptor_binding,
            bound.account_binding,
            bound.selected_provider_configuration,
            bound.effective_provider_configuration,
            bound.replay_schema_revision,
            &bound.api_version,
            &bound.beta_header,
        ),
        (
            &policy.protocol,
            &policy.requested_model,
            &policy.expected_reported_model,
            policy.semantic_descriptor_binding,
            policy.account_binding,
            policy.selected_provider_configuration,
            policy.effective_provider_configuration,
            MESSAGES_REPLAY_SCHEMA_REVISION,
            &policy.api_version,
            &policy.beta_header,
        ),
    ))
    .map_err(|_| invalid("failed to encode historical projection transition"))?;
    let canonical = SemanticJsonV1::from_value(facts)
        .map_err(|_| invalid("failed to canonicalize historical projection transition"))?;
    Digest::sha256(
        DigestDomain::HistoryProjection,
        1,
        &[
            b"messages-historical-thinking-transition",
            canonical.canonical_bytes(),
        ],
    )
    .map_err(|_| invalid("failed to digest historical projection transition"))
}

fn replay_payload(
    replay: &ReplayEnvelope,
    policy: &MessagesRequestPolicy,
    expected_kind: &str,
) -> Result<Box<RawValue>, MessagesRequestError> {
    let ReplayScope::ExactModel(source_model) = replay.scope() else {
        return Err(invalid("opaque thinking replay must use exact-model scope"));
    };
    if replay.protocol() != &policy.protocol {
        return Err(invalid(
            "opaque thinking replay protocol does not match Messages",
        ));
    }
    if replay.model_family().is_some()
        || replay.kind().as_str() != expected_kind
        || replay.classification() != DataClassification::Sensitive
    {
        return Err(invalid(
            "opaque thinking replay is incompatible with the exact model",
        ));
    }
    let payload = match replay.version() {
        MESSAGES_REPLAY_SCHEMA_REVISION => {
            let bound: BoundReplayPayload = serde_json::from_str(replay.payload().get())
                .map_err(|_| invalid("opaque replay binding was malformed"))?;
            if bound.requested_model != policy.requested_model
                || &bound.actual_model != source_model
                || bound.semantic_descriptor_binding != policy.semantic_descriptor_binding
                || bound.account_binding != policy.account_binding
                || bound.selected_provider_configuration != policy.selected_provider_configuration
                || bound.effective_provider_configuration != policy.effective_provider_configuration
                || bound.replay_schema_revision != MESSAGES_REPLAY_SCHEMA_REVISION
                || bound.api_version != policy.api_version
                || bound.beta_header != policy.beta_header
                || policy
                    .expected_reported_model
                    .as_ref()
                    .is_some_and(|expected| expected != source_model)
            {
                return Err(invalid(
                    "opaque replay does not match the exact target binding",
                ));
            }
            bound.value
        }
        _ => {
            return Err(invalid(
                "opaque replay used an unsupported envelope version",
            ));
        }
    };
    if matches!(expected_kind, SIGNATURE_REPLAY_KIND | REDACTED_REPLAY_KIND) {
        serde_json::from_str::<String>(payload.get())
            .map_err(|_| invalid("opaque thinking replay payload must be an exact JSON string"))?;
    }
    Ok(payload)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BoundReplayPayload {
    requested_model: ModelId,
    actual_model: ModelId,
    semantic_descriptor_binding: Digest,
    account_binding: Digest,
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
    replay_schema_revision: u32,
    api_version: String,
    beta_header: String,
    value: Box<RawValue>,
}

/// Binds one decoded opaque replay value to the exact Messages target.
pub fn bind_replay_payload(
    policy: &MessagesRequestPolicy,
    actual_model: ModelId,
    kind: &str,
    payload: Box<RawValue>,
) -> Result<ReplayEnvelope, MessagesRequestError> {
    if !matches!(
        kind,
        SIGNATURE_REPLAY_KIND | REDACTED_REPLAY_KIND | CODE_EXECUTION_RESULT_REPLAY_KIND
    ) || policy
        .expected_reported_model
        .as_ref()
        .is_some_and(|expected| expected != &actual_model)
    {
        return Err(invalid(
            "opaque replay does not match the exact target binding",
        ));
    }
    if matches!(kind, SIGNATURE_REPLAY_KIND | REDACTED_REPLAY_KIND) {
        serde_json::from_str::<String>(payload.get())
            .map_err(|_| invalid("opaque replay payload was not an exact JSON string"))?;
    } else if !serde_json::from_str::<Value>(payload.get()).is_ok_and(|value| value.is_object()) {
        return Err(invalid(
            "hosted replay payload was not an exact JSON object",
        ));
    }
    // HistorySegment freeze JCS-canonicalizes nested replay JSON. Hash the
    // envelope over those same bytes or checkpoint decode reports
    // InvalidModelComponent (struct-field order ≠ JCS key order).
    let bound = serde_json::to_value(&BoundReplayPayload {
        requested_model: policy.requested_model.clone(),
        actual_model: actual_model.clone(),
        semantic_descriptor_binding: policy.semantic_descriptor_binding,
        account_binding: policy.account_binding,
        selected_provider_configuration: policy.selected_provider_configuration,
        effective_provider_configuration: policy.effective_provider_configuration,
        replay_schema_revision: MESSAGES_REPLAY_SCHEMA_REVISION,
        api_version: policy.api_version.clone(),
        beta_header: policy.beta_header.clone(),
        value: payload,
    })
    .map_err(|e| invalid(format!("failed to bind replay: {e}")))?;
    let canonical = SemanticJsonV1::from_value(bound)
        .map_err(|e| invalid(format!("failed to canonicalize replay binding: {e}")))?;
    let payload = RawValue::from_string(
        std::str::from_utf8(canonical.canonical_bytes())
            .map_err(|e| invalid(format!("failed to bind replay: {e}")))?
            .to_owned(),
    )
    .map_err(|e| invalid(format!("failed to bind replay: {e}")))?;
    ReplayEnvelope::new(
        policy.protocol.clone(),
        None,
        ReplayScope::ExactModel(actual_model),
        plakous_model::ReplayKindId::new(kind).map_err(|e| invalid(e.to_string()))?,
        MESSAGES_REPLAY_SCHEMA_REVISION,
        DataClassification::Sensitive,
        payload,
    )
    .map_err(|e| invalid(e.to_string()))
}

fn translate_reasoning(
    policy: ReasoningPolicy,
    minimum: Option<u32>,
    max_tokens: u32,
    omit_disabled: bool,
    display: Option<MessagesThinkingDisplay>,
) -> Result<(Option<ThinkingParam>, Option<OutputConfigParam>), MessagesRequestError> {
    match policy {
        ReasoningPolicy::Disabled if omit_disabled => Ok((None, None)),
        ReasoningPolicy::Disabled => Ok((Some(ThinkingParam::Disabled), None)),
        ReasoningPolicy::Enabled {
            effort,
            requirement,
        } => {
            if let Some(ReasoningEffort::TokenBudget(budget)) = effort {
                let Some(minimum) = minimum else {
                    return Err(match requirement {
                        CapabilityRequirement::Required => {
                            unsupported("exact model does not support manual thinking")
                        }
                        _ => unsupported("thinking cannot be silently downgraded during lowering"),
                    });
                };
                let budget = budget.get();
                if budget < minimum || budget >= max_tokens {
                    return Err(invalid(
                        "thinking budget must meet the model minimum and be below max_tokens",
                    ));
                }
                return Ok((
                    Some(ThinkingParam::Enabled {
                        budget_tokens: budget,
                        display,
                    }),
                    None,
                ));
            }
            let effort_name = match effort {
                Some(ReasoningEffort::Low) => Some("low"),
                Some(ReasoningEffort::Medium) => Some("medium"),
                Some(ReasoningEffort::High) => Some("high"),
                Some(ReasoningEffort::XHigh) => Some("xhigh"),
                Some(ReasoningEffort::Max) => Some("max"),
                None => None,
                Some(ReasoningEffort::TokenBudget(_)) => unreachable!(),
                Some(_) => {
                    return Err(unsupported(
                        "Messages does not admit this output_config.effort",
                    ));
                }
            };
            Ok((
                Some(ThinkingParam::Adaptive { display }),
                effort_name.map(|effort| OutputConfigParam { effort }),
            ))
        }
        _ => Err(unsupported("unknown reasoning policy cannot be translated")),
    }
}

fn translate_tool_choice(
    choice: &ToolChoice,
    has_tools: bool,
) -> Result<Option<ToolChoiceParam>, MessagesRequestError> {
    Ok(match choice {
        ToolChoice::Auto if has_tools => Some(ToolChoiceParam::Auto),
        ToolChoice::None if has_tools => Some(ToolChoiceParam::None),
        ToolChoice::Auto | ToolChoice::None => None,
        ToolChoice::Required => Some(ToolChoiceParam::Any),
        ToolChoice::Specific(name) => Some(ToolChoiceParam::Tool {
            name: name.as_str().to_owned(),
        }),
        _ => {
            return Err(unsupported(
                "unknown tool-choice policy cannot be translated",
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use plakous_model::EntryId as TestEntryId;
    use plakous_model::TextBlock as TestTextBlock;
    use plakous_model::{
        AttemptId, BranchId, Capability, CapabilityConstraints, CapabilitySet, ConfiguredTool,
        ConversationItem, ConversationView, DataClassification, FactProvenance, FactSource,
        FeatureSupport, ModelAdapterProfileId, ModelCacheAffinity, ModelFamilyId, ModelRequest,
        Provenance, ReplayKindId, RequestMetadata, ResolvedModelDescriptor, RouteId, RunId,
        SessionId, Support, Timestamp, ToolCallerDisposition, ToolConcurrency, ToolDefinition,
        ToolEffect, ToolExecutionDisposition, ToolInputContract, ToolName, ToolOutputContract,
        TurnId, ValidationMode,
    };
    use uuid::Uuid as TestUuid;

    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn selected_id(label: &[u8]) -> TestResult<SelectedProviderConfigurationId> {
        Ok(serde_json::from_value(serde_json::to_value(
            Digest::sha256(DigestDomain::SelectedProviderConfiguration, 5, &[label])?,
        )?)?)
    }

    fn effective_id(label: &[u8]) -> TestResult<EffectiveProviderConfigurationId> {
        Ok(serde_json::from_value(serde_json::to_value(
            Digest::sha256(DigestDomain::EffectiveProviderConfiguration, 5, &[label])?,
        )?)?)
    }

    fn policy_with_authority(
        account: &[u8],
        selected: &[u8],
        effective: &[u8],
    ) -> TestResult<MessagesRequestPolicy> {
        Ok(MessagesRequestPolicy::new(
            ProtocolId::new("anthropic.messages")?,
            ModelId::new("claude-exact")?,
            Some(ModelId::new("claude-exact")?),
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"descriptor"])?,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"semantic-descriptor"])?,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
            selected_id(selected)?,
            effective_id(effective)?,
            8_192,
            None,
            "2023-06-01",
            "",
        ))
    }

    fn policy() -> TestResult<MessagesRequestPolicy> {
        policy_with_authority(b"account", b"selected", b"effective")
    }

    #[test]
    fn policy_debug_never_prints_the_previous_message_id() -> TestResult {
        let policy =
            policy()?.with_claude_oauth_previous_message_id(Some("msg_secret_123".to_owned()));
        let debug = format!("{policy:?}");
        assert!(!debug.contains("msg_secret_123"), "{debug}");
        assert!(debug.contains("PreviousMessageId(..)"), "{debug}");
        Ok(())
    }

    #[test]
    fn thinking_display_is_opt_in_and_never_applies_to_disabled_reasoning() -> TestResult {
        for display in [None, Some(MessagesThinkingDisplay::Updates)] {
            for (effort, expected_type) in [
                (Some(ReasoningEffort::Low), "adaptive"),
                (
                    Some(ReasoningEffort::TokenBudget(
                        std::num::NonZeroU32::new(1024).ok_or("budget")?,
                    )),
                    "enabled",
                ),
            ] {
                let (thinking, _) = translate_reasoning(
                    ReasoningPolicy::Enabled {
                        effort,
                        requirement: CapabilityRequirement::Required,
                    },
                    Some(1024),
                    4096,
                    false,
                    display,
                )?;
                let body = serde_json::to_value(thinking)?;
                assert_eq!(body["type"], expected_type);
                if display.is_some() {
                    assert_eq!(body["display"], "updates");
                } else {
                    assert!(body.get("display").is_none());
                }
            }
            for omit_disabled in [false, true] {
                let (thinking, _) = translate_reasoning(
                    ReasoningPolicy::Disabled,
                    None,
                    1024,
                    omit_disabled,
                    display,
                )?;
                assert_eq!(
                    serde_json::to_value(thinking)?,
                    if omit_disabled {
                        Value::Null
                    } else {
                        serde_json::json!({"type":"disabled"})
                    }
                );
            }
        }
        Ok(())
    }

    fn lowering_fixture() -> TestResult<(ValidatedRequestPlan, MessagesRequestPolicy)> {
        lowering_fixture_with_history(vec![ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(4))?,
            Actor::User,
            vec![ContentBlock::Text(TestTextBlock::new("hello", None)?)],
            Provenance::UserInput,
        )?])
    }

    fn lowering_fixture_with_history(
        history: Vec<ConversationItem>,
    ) -> TestResult<(ValidatedRequestPlan, MessagesRequestPolicy)> {
        let selected = selected_id(b"selected")?;
        let effective = effective_id(b"effective")?;
        let provenance = FactProvenance::new(
            FactSource::TestFixture,
            Timestamp::new(1_700_000_000, 0)?,
            None,
        );
        let features = [Capability::Streaming]
            .into_iter()
            .map(|capability| {
                (
                    capability,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        provenance,
                    ),
                )
            })
            .collect();
        let descriptor = ResolvedModelDescriptor::new_custom(
            ProtocolId::new("messages.test")?,
            RouteId::new("messages-test")?,
            ModelId::new("model/exact")?,
            None,
            None,
            ModelAdapterProfileId::new("messages-test")?,
            CapabilitySet::new(features, BTreeMap::new()),
        );
        let request = ModelRequest::builder(
            selected,
            effective,
            ConversationView::new(history),
            RequestMetadata::for_turn(
                RunId::try_from(TestUuid::from_u128(1))?,
                TurnId::try_from(TestUuid::from_u128(2))?,
                AttemptId::try_from(TestUuid::from_u128(3))?,
                Timestamp::new(1_700_000_001, 0)?,
            ),
        )
        .build()?;
        let plan = descriptor.validate_request(request, ValidationMode::Strict)?;
        let policy = MessagesRequestPolicy::new(
            ProtocolId::new("messages.test")?,
            ModelId::new("model/exact")?,
            Some(ModelId::new("model/exact")?),
            plan.binding_digest(),
            descriptor.semantic_binding_digest()?,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
            selected,
            effective,
            8_192,
            None,
            "2023-06-01",
            "",
        );
        Ok((plan, policy))
    }

    fn lowering_fixture_with_customer_tool()
    -> TestResult<(ValidatedRequestPlan, MessagesRequestPolicy)> {
        let selected = selected_id(b"selected-tool")?;
        let effective = effective_id(b"effective-tool")?;
        let provenance = FactProvenance::new(
            FactSource::TestFixture,
            Timestamp::new(1_700_000_000, 0)?,
            None,
        );
        let features = [Capability::Streaming, Capability::Tools]
            .into_iter()
            .map(|capability| {
                (
                    capability,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        provenance,
                    ),
                )
            })
            .collect();
        let descriptor = ResolvedModelDescriptor::new_custom(
            ProtocolId::new("messages.test")?,
            RouteId::new("messages-test")?,
            ModelId::new("model/exact")?,
            None,
            None,
            ModelAdapterProfileId::new("messages-test")?,
            CapabilitySet::new(features, BTreeMap::new()),
        );
        let tool = ConfiguredTool::new(
            ToolDefinition::new(
                ToolName::new("lookup")?,
                "1",
                "lookup tool",
                ToolInputContract::structured(serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }))?,
                ToolOutputContract::utf8(1024)?,
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            ToolExecutionDisposition::Local,
            ToolCallerDisposition::DirectOnly,
        );
        let request = ModelRequest::builder(
            selected,
            effective,
            ConversationView::new(vec![ConversationItem::new(
                TestEntryId::try_from(TestUuid::from_u128(4))?,
                Actor::User,
                vec![ContentBlock::Text(TestTextBlock::new("hello", None)?)],
                Provenance::UserInput,
            )?]),
            RequestMetadata::for_turn(
                RunId::try_from(TestUuid::from_u128(1))?,
                TurnId::try_from(TestUuid::from_u128(2))?,
                AttemptId::try_from(TestUuid::from_u128(3))?,
                Timestamp::new(1_700_000_001, 0)?,
            ),
        )
        .tools(vec![tool])
        .build()?;
        let plan = descriptor.validate_request(request, ValidationMode::Strict)?;
        let policy = MessagesRequestPolicy::new(
            ProtocolId::new("messages.test")?,
            ModelId::new("model/exact")?,
            Some(ModelId::new("model/exact")?),
            plan.binding_digest(),
            descriptor.semantic_binding_digest()?,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
            selected,
            effective,
            8_192,
            None,
            "2023-06-01",
            "",
        );
        Ok((plan, policy))
    }

    fn lowering_fixture_with_tool_choice(
        tool_choice: ToolChoice,
    ) -> TestResult<(ValidatedRequestPlan, MessagesRequestPolicy)> {
        let selected = selected_id(b"selected-tool-choice")?;
        let effective = effective_id(b"effective-tool-choice")?;
        let provenance = FactProvenance::new(
            FactSource::TestFixture,
            Timestamp::new(1_700_000_000, 0)?,
            None,
        );
        let features = [Capability::Streaming, Capability::Tools]
            .into_iter()
            .map(|capability| {
                (
                    capability,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        provenance,
                    ),
                )
            })
            .collect();
        let descriptor = ResolvedModelDescriptor::new_custom(
            ProtocolId::new("messages.test")?,
            RouteId::new("messages-test")?,
            ModelId::new("model/exact")?,
            None,
            None,
            ModelAdapterProfileId::new("messages-test")?,
            CapabilitySet::new(features, BTreeMap::new()),
        );
        let tool = ConfiguredTool::new(
            ToolDefinition::new(
                ToolName::new("lookup")?,
                "1",
                "lookup tool",
                ToolInputContract::structured(serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": false
                }))?,
                ToolOutputContract::utf8(1024)?,
                ToolConcurrency::Parallel,
                ToolEffect::ReadOnly,
            )?,
            ToolExecutionDisposition::Local,
            ToolCallerDisposition::DirectOnly,
        );
        let request = ModelRequest::builder(
            selected,
            effective,
            ConversationView::new(vec![ConversationItem::new(
                TestEntryId::try_from(TestUuid::from_u128(4))?,
                Actor::User,
                vec![ContentBlock::Text(TestTextBlock::new("hello", None)?)],
                Provenance::UserInput,
            )?]),
            RequestMetadata::for_turn(
                RunId::try_from(TestUuid::from_u128(1))?,
                TurnId::try_from(TestUuid::from_u128(2))?,
                AttemptId::try_from(TestUuid::from_u128(3))?,
                Timestamp::new(1_700_000_001, 0)?,
            ),
        )
        .tools(vec![tool])
        .tool_choice(tool_choice)
        .build()?;
        let plan = descriptor.validate_request(request, ValidationMode::Strict)?;
        let policy = MessagesRequestPolicy::new(
            ProtocolId::new("messages.test")?,
            ModelId::new("model/exact")?,
            Some(ModelId::new("model/exact")?),
            plan.binding_digest(),
            descriptor.semantic_binding_digest()?,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
            selected,
            effective,
            8_192,
            None,
            "2023-06-01",
            "",
        );
        Ok((plan, policy))
    }

    #[test]
    fn tool_choice_none_with_tools_emits_none() -> TestResult {
        let (plan, policy) = lowering_fixture_with_tool_choice(ToolChoice::None)?;
        let lowered = lower_request(&plan, &policy, ToolOwnership::default())?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["tool_choice"], serde_json::json!({"type": "none"}));
        Ok(())
    }

    #[test]
    fn openrouter_gateway_session_emits_session_id_and_breakpoints_without_ttl() -> TestResult {
        let (plan, policy) = lowering_fixture_with_customer_tool()?;
        let session =
            Digest::sha256(DigestDomain::CacheAffinity, 1, &[b"openrouter-session"])?.to_string();
        let lowered = lower_request(
            &plan,
            &policy
                .with_session_id(session.clone())
                .with_prompt_cache(true)
                .with_top_level_prompt_cache(false),
            ToolOwnership::default(),
        )?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(body["session_id"], session);
        assert!(body.get("cache_control").is_none());
        assert_eq!(
            body["tools"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        assert!(body["tools"][0]["cache_control"].get("ttl").is_none());
        let last_block = body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .and_then(|message| message["content"].as_array())
            .and_then(|blocks| blocks.last())
            .ok_or("last message content")?;
        assert_eq!(
            last_block["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        assert!(last_block["cache_control"].get("ttl").is_none());
        Ok(())
    }

    #[test]
    fn forked_openrouter_session_id_does_not_reuse_source_identity() -> TestResult {
        let (plan, source_policy) = lowering_fixture_with_customer_tool()?;
        let (_, fork_policy) = lowering_fixture_with_customer_tool()?;
        let session = SessionId::try_from(TestUuid::from_u128(92_001))?;
        let source_aff = ModelCacheAffinity::from_runtime_entropy(
            session,
            BranchId::try_from(TestUuid::from_u128(92_002))?,
            0,
            [0x41; 32],
        )?;
        let fork_aff = ModelCacheAffinity::from_runtime_entropy(
            session,
            BranchId::try_from(TestUuid::from_u128(92_003))?,
            0,
            [0x42; 32],
        )?;
        let source = source_aff.identity().to_string();
        let fork = fork_aff.identity().to_string();
        assert_ne!(source, fork);
        let source_body: Value = serde_json::from_slice(
            lower_request(
                &plan,
                &source_policy
                    .with_session_id(source.clone())
                    .with_prompt_cache(true)
                    .with_top_level_prompt_cache(false),
                ToolOwnership::default(),
            )?
            .body(),
        )?;
        let fork_body: Value = serde_json::from_slice(
            lower_request(
                &plan,
                &fork_policy
                    .with_session_id(fork.clone())
                    .with_prompt_cache(true)
                    .with_top_level_prompt_cache(false),
                ToolOwnership::default(),
            )?
            .body(),
        )?;
        assert_eq!(source_body["session_id"], source);
        assert_eq!(fork_body["session_id"], fork);
        assert_ne!(source_body["session_id"], fork_body["session_id"]);
        assert!(source_body.get("cache_control").is_none());
        assert!(fork_body.get("cache_control").is_none());
        Ok(())
    }

    /// A harness-authored note in history reaches the wire as a user message.
    ///
    /// The product writes one such note when it abandons a turn whose model
    /// request was lost, and every later request on that session carries it.
    /// This route refuses to translate a historical instruction actor, so the
    /// note is spelled `User` with `Provenance::Instruction` — the pair the
    /// product reads as harness-authored — rather than `System`. A note this
    /// route cannot carry does not merely go missing: it fails every turn the
    /// recovered session is asked for.
    #[test]
    fn a_harness_authored_note_in_history_lowers_as_a_user_message() -> TestResult {
        let (plan, policy) = lowering_fixture_with_history(vec![
            ConversationItem::new(
                TestEntryId::try_from(TestUuid::from_u128(4))?,
                Actor::User,
                vec![ContentBlock::Text(TestTextBlock::new("hello", None)?)],
                Provenance::UserInput,
            )?,
            ConversationItem::new(
                TestEntryId::try_from(TestUuid::from_u128(5))?,
                Actor::User,
                vec![ContentBlock::Text(TestTextBlock::new(
                    "the model request for this turn was abandoned",
                    None,
                )?)],
                Provenance::Instruction,
            )?,
        ])?;
        let lowered = lower_request(&plan, &policy, ToolOwnership::default())?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        let messages = body["messages"]
            .as_array()
            .ok_or("lowered request has no messages")?;
        assert_eq!(messages.len(), 1, "consecutive user content is one message");
        assert_eq!(messages[0]["role"], "user");
        let text = serde_json::to_string(&messages[0]["content"])?;
        assert!(
            text.contains("was abandoned"),
            "the note did not reach the wire: {text}"
        );
        Ok(())
    }

    #[test]
    fn seeded_content_filter_guidance_lowers_on_messages() -> TestResult {
        let instruction =
            "<content_filter_guidance>\nOffer a permitted alternative.\n</content_filter_guidance>";
        let (plan, policy) = lowering_fixture_with_history(vec![
            ConversationItem::new(
                TestEntryId::try_from(TestUuid::from_u128(4))?,
                Actor::User,
                vec![ContentBlock::Text(TestTextBlock::new(
                    "original request",
                    None,
                )?)],
                Provenance::UserInput,
            )?,
            ConversationItem::new(
                TestEntryId::try_from(TestUuid::from_u128(5))?,
                Actor::User,
                vec![ContentBlock::Text(TestTextBlock::new(instruction, None)?)],
                Provenance::Instruction,
            )?,
        ])?;
        let lowered = lower_request(&plan, &policy, ToolOwnership::default())?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(body["messages"][0]["role"], "user");
        let content = body["messages"][0]["content"]
            .as_array()
            .ok_or("no content")?;
        assert!(content.iter().any(|block| block["text"] == instruction));
        Ok(())
    }

    /// A historical instruction actor is still refused, on purpose.
    ///
    /// This route has one system field and no place for a system turn inside
    /// the conversation, so translating one silently would mean inventing a
    /// position for it. The refusal is what sent the abandonment note to the
    /// harness-authored spelling above.
    #[test]
    fn a_system_actor_in_history_is_refused_rather_than_invented() -> TestResult {
        let fixture = lowering_fixture_with_history(vec![ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(6))?,
            Actor::System,
            vec![ContentBlock::Text(TestTextBlock::new("a note", None)?)],
            Provenance::Instruction,
        )?]);
        let refused = match fixture {
            Err(_) => true,
            Ok((plan, policy)) => lower_request(&plan, &policy, ToolOwnership::default()).is_err(),
        };
        assert!(refused, "a system actor in history must not reach the wire");
        Ok(())
    }

    #[test]
    fn openrouter_context_compression_plugin_is_disabled() -> TestResult {
        let (plan, policy) = lowering_fixture()?;
        let lowered = lower_request(
            &plan,
            &policy.with_plugins(OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS),
            ToolOwnership::default(),
        )?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(
            body["plugins"],
            serde_json::json!([{ "id": "context-compression", "enabled": false }])
        );
        Ok(())
    }

    #[test]
    fn optional_gateway_router_is_exact_and_identity_bearing() -> TestResult {
        let (plan, policy) = lowering_fixture()?;
        let direct = lower_request(&plan, &policy, ToolOwnership::default())?;
        let routed_without_privacy = lower_request(
            &plan,
            &policy
                .clone()
                .with_router_policy(MessagesRouterPolicy::gateway_managed(None, None)?),
            ToolOwnership::default(),
        )?;
        let routed = lower_request(
            &plan,
            &policy
                .clone()
                .with_router_policy(MessagesRouterPolicy::gateway_managed(
                    Some("deny".to_owned()),
                    Some(true),
                )?),
            ToolOwnership::default(),
        )?;
        let direct_body: Value = serde_json::from_slice(direct.body())?;
        let routed_without_privacy_body: Value =
            serde_json::from_slice(routed_without_privacy.body())?;
        let routed_body: Value = serde_json::from_slice(routed.body())?;
        assert!(direct_body.get("provider").is_none());
        assert_eq!(
            routed_without_privacy_body["provider"],
            serde_json::json!({
                "allow_fallbacks": false,
                "require_parameters": true
            })
        );
        assert_eq!(
            routed_body["provider"],
            serde_json::json!({
                "allow_fallbacks": false,
                "require_parameters": true,
                "data_collection": "deny",
                "zdr": true
            })
        );
        assert_eq!(routed_body.as_object().map(serde_json::Map::len), Some(6));
        assert_ne!(
            direct.operation_digest(),
            routed_without_privacy.operation_digest()
        );
        assert_ne!(
            routed_without_privacy.operation_digest(),
            routed.operation_digest()
        );
        assert_ne!(direct.operation_digest(), routed.operation_digest());
        let without_privacy = policy
            .clone()
            .with_router_policy(MessagesRouterPolicy::gateway_managed(None, None)?);
        let with_privacy =
            policy
                .clone()
                .with_router_policy(MessagesRouterPolicy::gateway_managed(
                    Some("deny".to_owned()),
                    Some(true),
                )?);
        let with_privacy_false =
            policy
                .clone()
                .with_router_policy(MessagesRouterPolicy::gateway_managed(
                    Some("deny".to_owned()),
                    Some(false),
                )?);
        assert_ne!(
            messages_artifact_configuration_identity(&policy, plan.digest())?,
            messages_artifact_configuration_identity(&without_privacy, plan.digest())?
        );
        assert_ne!(
            messages_artifact_configuration_identity(&without_privacy, plan.digest())?,
            messages_artifact_configuration_identity(&with_privacy, plan.digest())?
        );
        assert_ne!(
            messages_artifact_configuration_identity(&with_privacy, plan.digest())?,
            messages_artifact_configuration_identity(&with_privacy_false, plan.digest())?
        );
        assert!(MessagesRouterPolicy::gateway_managed(Some("DENY".to_owned()), None).is_err());
        Ok(())
    }

    #[test]
    fn omitted_router_preserves_anthropic_request_bytes() -> TestResult {
        let (plan, policy) = lowering_fixture()?;
        let lowered = lower_request(&plan, &policy, ToolOwnership::default())?;
        assert_eq!(
            lowered.body(),
            br#"{"model":"model/exact","max_tokens":8192,"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}],"thinking":{"type":"disabled"},"stream":true}"#
        );
        Ok(())
    }

    #[test]
    fn disabled_reasoning_omits_thinking_blocks_and_tool_choice_none() -> TestResult {
        let policy = policy()?;
        let thinking = plakous_model::ReasoningBlock::new(
            ReasoningVisibility::Visible,
            Some("hidden thought".to_owned()),
            Some(bind_replay_payload(
                &policy,
                ModelId::new("claude-exact")?,
                SIGNATURE_REPLAY_KIND,
                raw("\"sig\"")?,
            )?),
        )?;
        let (blocks, pending) = translate_assistant_bound(
            &[
                ContentBlock::Reasoning(thinking),
                ContentBlock::Text(plakous_model::TextBlock::new("bravo", None)?),
            ],
            &policy,
            &ToolOwnership::default(),
            false,
            false,
            &mut None,
            &mut crate::artifact::MessagesArtifactLineage::default(),
            &mut Vec::new(),
        )?;
        assert!(pending.is_empty());
        assert!(blocks.iter().all(|block| !matches!(
            block,
            MessageBlock::Thinking { .. } | MessageBlock::RedactedThinking { .. }
        )));
        let texts: Vec<&str> = blocks
            .iter()
            .filter_map(|block| match block {
                MessageBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["bravo"]);
        assert!(translate_tool_choice(&ToolChoice::None, false)?.is_none());
        assert!(matches!(
            translate_tool_choice(&ToolChoice::None, true)?,
            Some(ToolChoiceParam::None)
        ));
        Ok(())
    }

    fn raw(value: &str) -> TestResult<Box<RawValue>> {
        Ok(RawValue::from_string(value.to_owned())?)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "test mutates every closed envelope axis"
    )]
    fn rebound(
        source: &ReplayEnvelope,
        protocol: &str,
        model_family: Option<ModelFamilyId>,
        scope: ReplayScope,
        kind: &str,
        version: u32,
        classification: DataClassification,
    ) -> TestResult<ReplayEnvelope> {
        Ok(ReplayEnvelope::new(
            ProtocolId::new(protocol)?,
            model_family,
            scope,
            ReplayKindId::new(kind)?,
            version,
            classification,
            raw(source.payload().get())?,
        )?)
    }

    #[test]
    fn signed_and_redacted_thinking_use_exact_canonical_replay_envelopes() -> TestResult {
        let policy = policy()?;
        let model = ModelId::new("claude-exact")?;
        for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
            let replay = bind_replay_payload(&policy, model.clone(), kind, raw("\"opaque\"")?)?;
            assert_eq!(replay.protocol(), policy.protocol());
            assert_eq!(replay.scope(), &ReplayScope::ExactModel(model.clone()));
            assert_eq!(replay.kind().as_str(), kind);
            assert_eq!(replay.version(), MESSAGES_REPLAY_SCHEMA_REVISION);
            assert_eq!(replay.classification(), DataClassification::Sensitive);
            assert_eq!(replay_payload(&replay, &policy, kind)?.get(), "\"opaque\"");
            let parsed: Value = serde_json::from_str(replay.payload().get())?;
            let recanonical = SemanticJsonV1::from_value(parsed)?;
            assert_eq!(
                replay.payload().get().as_bytes(),
                recanonical.canonical_bytes().as_ref(),
                "{kind} payload must be JCS so history persist can roundtrip the digest"
            );
        }
        Ok(())
    }

    #[test]
    fn empty_signature_replay_converges_to_portable_visible_reasoning() -> TestResult {
        let policy = policy()?;
        let replay = bind_replay_payload(
            &policy,
            ModelId::new("claude-exact")?,
            SIGNATURE_REPLAY_KIND,
            raw("\"\"")?,
        )?;
        let reasoning = plakous_model::ReasoningBlock::new(
            ReasoningVisibility::Visible,
            Some("portable thought".to_owned()),
            Some(replay),
        )?;
        assert!(matches!(
            native_or_portable_thinking(
                &reasoning,
                &policy,
                SIGNATURE_REPLAY_KIND,
                true,
                &mut Vec::new(),
            ),
            Some(MessageBlock::Text { text, .. }) if text == "portable thought"
        ));
        Ok(())
    }

    #[test]
    fn replay_ignores_freshness_binding_but_requires_semantic_descriptor() -> TestResult {
        let original = policy()?;
        let mut refreshed = original.clone();
        refreshed.descriptor_binding =
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"fresh-observation"])?;
        for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
            let replay = bind_replay_payload(
                &original,
                ModelId::new("claude-exact")?,
                kind,
                raw("\"opaque\"")?,
            )?;
            assert_eq!(
                replay_payload(&replay, &refreshed, kind)?.get(),
                "\"opaque\""
            );
            let mut changed = refreshed.clone();
            changed.semantic_descriptor_binding =
                Digest::sha256(DigestDomain::ModelBinding, 1, &[b"changed-capability"])?;
            assert!(replay_payload(&replay, &changed, kind).is_err());
        }
        Ok(())
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one replay fixture exercises completed and active history boundaries"
    )]
    fn completed_thinking_projects_changed_bindings_but_active_turn_stays_exact() -> TestResult {
        let source = policy()?.with_strict_thinking_replay(true);
        let mut effort_target = source.clone();
        effort_target.effective_provider_configuration = effective_id(b"lower-effort")?;
        let mut model_target = source.clone();
        model_target.requested_model = ModelId::new("claude-successor")?;
        model_target.expected_reported_model = Some(ModelId::new("claude-successor")?);
        model_target.semantic_descriptor_binding =
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"successor-descriptor"])?;
        let mut account_target = source.clone();
        account_target.account_binding =
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"new-account"])?;
        let mut beta_target = source.clone();
        beta_target.beta_header = "new-beta".into();

        let mut transition_sets = Vec::new();
        for (kind, visibility) in [
            (SIGNATURE_REPLAY_KIND, ReasoningVisibility::Visible),
            (REDACTED_REPLAY_KIND, ReasoningVisibility::Redacted),
        ] {
            let reasoning = plakous_model::ReasoningBlock::new(
                visibility,
                (visibility == ReasoningVisibility::Visible).then(|| "private thought".into()),
                Some(bind_replay_payload(
                    &source,
                    ModelId::new("claude-exact")?,
                    kind,
                    raw("\"signature\"")?,
                )?),
            )?;
            let assistant = ConversationItem::new(
                TestEntryId::try_from(TestUuid::from_u128(101))?,
                Actor::Assistant,
                vec![
                    ContentBlock::Reasoning(reasoning),
                    ContentBlock::Text(TestTextBlock::new("answer", None)?),
                ],
                Provenance::ModelOutput,
            )?;
            let user = ConversationItem::new(
                TestEntryId::try_from(TestUuid::from_u128(102))?,
                Actor::User,
                vec![ContentBlock::Text(TestTextBlock::new("next", None)?)],
                Provenance::UserInput,
            )?;
            let initial_user = ConversationItem::new(
                TestEntryId::try_from(TestUuid::from_u128(100))?,
                Actor::User,
                vec![ContentBlock::Text(TestTextBlock::new("first", None)?)],
                Provenance::UserInput,
            )?;
            let mut transition_ids = std::collections::HashSet::new();
            for (target, expected_native_count) in [
                (&source, 1),
                (&effort_target, 0),
                (&model_target, 0),
                (&account_target, 0),
                (&beta_target, 0),
            ] {
                let history = ConversationView::new(vec![
                    initial_user.clone(),
                    assistant.clone(),
                    user.clone(),
                ]);
                let mut diagnostics = Vec::new();
                let (messages, _, projections) = translate_history(
                    &history,
                    &target
                        .clone()
                        .with_completed_history_prefix(Some(history.semantic_prefix_identity(2)?)),
                    &ToolOwnership::default(),
                    false,
                    true,
                    &mut diagnostics,
                )?;
                let native_count = messages[1]
                    .content
                    .iter()
                    .filter(|block| {
                        matches!(
                            block,
                            MessageBlock::Thinking { .. } | MessageBlock::RedactedThinking { .. }
                        )
                    })
                    .count();
                assert_eq!(native_count, expected_native_count);
                assert_eq!(messages[1].content.len(), native_count + 1);
                assert!(
                    matches!(&messages[1].content[native_count], MessageBlock::Text { text, .. } if text == "answer")
                );
                assert_eq!(diagnostics, [] as [String; 0]);
                assert_eq!(projections.len(), usize::from(native_count == 0));
                if native_count == 0 {
                    let identity = projections[0]
                        .history_projection_identity()
                        .ok_or("missing identity")?;
                    assert!(
                        transition_ids.insert(identity),
                        "distinct targets shared an identity"
                    );
                    let wire = serde_json::to_string(&messages)?;
                    assert!(!wire.contains("private thought"));
                    assert!(!wire.contains("signature"));
                    let quiescent =
                        ConversationView::new(vec![initial_user.clone(), assistant.clone()]);
                    assert!(
                        translate_history(
                            &quiescent,
                            &target.clone().with_completed_history_prefix(Some(
                                quiescent.semantic_prefix_identity(2)?
                            )),
                            &ToolOwnership::default(),
                            false,
                            true,
                            &mut Vec::new(),
                        )
                        .is_ok()
                    );
                    // Identical actor shapes can be an assistant continuation
                    // or a live steering boundary. Neither proves a closed turn.
                    for unproven in [&quiescent, &history] {
                        assert!(
                            translate_history(
                                unproven,
                                target,
                                &ToolOwnership::default(),
                                false,
                                true,
                                &mut Vec::new(),
                            )
                            .is_err()
                        );
                    }
                    // A proof for another exact projection is never reusable.
                    let foreign = target
                        .clone()
                        .with_completed_history_prefix(Some(history.semantic_prefix_identity(3)?));
                    assert!(
                        translate_history(
                            &quiescent,
                            &foreign,
                            &ToolOwnership::default(),
                            false,
                            true,
                            &mut Vec::new(),
                        )
                        .is_err()
                    );
                    let tool_call = ToolCallBlock::new(
                        CallId::try_from(TestUuid::from_u128(103))?,
                        ToolName::new("process_session_start")?,
                        ToolExecutionOwner::PlakLocal,
                        plakous_model::ToolCallState::Requested,
                        plakous_model::ToolCorrelation::new(
                            Some("call_current".into()),
                            None,
                            ToolCallerLineage::Direct,
                        )?,
                        r#"{"command":"pwd"}"#,
                    )?;
                    let mut active_blocks = assistant.blocks().to_vec();
                    active_blocks.push(ContentBlock::ToolCall(tool_call.clone()));
                    let active_assistant = ConversationItem::new(
                        TestEntryId::try_from(TestUuid::from_u128(104))?,
                        Actor::Assistant,
                        active_blocks,
                        Provenance::ModelOutput,
                    )?;
                    let active =
                        ConversationView::new(vec![user.clone(), active_assistant.clone()]);
                    assert!(
                        translate_history(
                            &active,
                            target,
                            &ToolOwnership::default(),
                            false,
                            true,
                            &mut Vec::new(),
                        )
                        .is_err()
                    );
                    let cancelled_result = ConversationItem::new(
                        TestEntryId::try_from(TestUuid::from_u128(105))?,
                        Actor::Tool,
                        vec![ContentBlock::ToolResult(
                            plakous_model::ToolResultBlock::new(
                                tool_call.call_id(),
                                tool_call.owner().clone(),
                                ToolResultKind::Failure,
                                vec![ToolResultPart::Text(TestTextBlock::new(
                                    "cancelled after joined cleanup",
                                    None,
                                )?)],
                                tool_call.correlation().clone(),
                            )?,
                        )],
                        Provenance::ToolExecution,
                    )?;
                    let cancelled = ConversationView::new(vec![
                        user.clone(),
                        active_assistant,
                        cancelled_result,
                    ]);
                    let closed = target.clone().with_completed_history_prefix(Some(
                        cancelled.semantic_prefix_identity(3)?,
                    ));
                    let (messages, _, repeated_projections) = translate_history(
                        &cancelled,
                        &closed,
                        &ToolOwnership::default(),
                        false,
                        true,
                        &mut Vec::new(),
                    )?;
                    assert_eq!(messages.len(), 3);
                    assert_eq!(projections, repeated_projections);
                    assert!(!messages[1].content.iter().any(|block| matches!(
                        block,
                        MessageBlock::Thinking { .. } | MessageBlock::RedactedThinking { .. }
                    )));
                }
            }
            transition_sets.push(transition_ids);
        }
        assert_eq!(
            transition_sets[0], transition_sets[1],
            "replay kind is not a transition fact"
        );
        Ok(())
    }

    #[test]
    fn historical_projection_still_rejects_malformed_and_unfinished_history() -> TestResult {
        let source = policy()?;
        let mut target = source.clone().with_strict_thinking_replay(true);
        target.effective_provider_configuration = effective_id(b"new-effort")?;
        let replay = bind_replay_payload(
            &source,
            ModelId::new("claude-exact")?,
            SIGNATURE_REPLAY_KIND,
            raw("\"secret\"")?,
        )?;
        let mut malformed: Value = serde_json::from_str(replay.payload().get())?;
        malformed["value"] = serde_json::json!({"not": "a signature string"});
        for (payload, unfinished) in [
            (serde_json::to_string(&malformed)?, false),
            (replay.payload().get().into(), true),
        ] {
            let carried = ReplayEnvelope::new(
                replay.protocol().clone(),
                None,
                replay.scope().clone(),
                replay.kind().clone(),
                replay.version(),
                replay.classification(),
                raw(&payload)?,
            )?;
            let mut blocks = vec![ContentBlock::Reasoning(plakous_model::ReasoningBlock::new(
                ReasoningVisibility::Visible,
                Some("private thought".into()),
                Some(carried),
            )?)];
            if unfinished {
                blocks.push(batch_call(1)?);
            }
            let history = ConversationView::new(vec![batch_item(100, Actor::Assistant, blocks)?]);
            let closed = target
                .clone()
                .with_completed_history_prefix(Some(history.semantic_prefix_identity(1)?));
            let mut diagnostics = Vec::new();
            let Err(error) = translate_history(
                &history,
                &closed,
                &ToolOwnership::default(),
                false,
                true,
                &mut diagnostics,
            ) else {
                return Err("malformed or unfinished history was accepted".into());
            };
            assert_eq!(error.kind(), MessagesRequestErrorKind::InvalidRequest);
            assert_eq!(diagnostics, [] as [String; 0]);
        }
        Ok(())
    }

    #[test]
    fn historical_projection_identity_excludes_payload_and_freshness() -> TestResult {
        let source = policy()?;
        let mut target = source.clone();
        target.effective_provider_configuration = effective_id(b"new-effort")?;
        let replay = bind_replay_payload(
            &source,
            ModelId::new("claude-exact")?,
            SIGNATURE_REPLAY_KIND,
            raw("\"first-secret\"")?,
        )?;
        let identity = historical_projection_identity(&replay, &target)?;
        let other_payload = bind_replay_payload(
            &source,
            ModelId::new("claude-exact")?,
            REDACTED_REPLAY_KIND,
            raw("\"second-secret\"")?,
        )?;
        target.descriptor_binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"freshness"])?;
        target.completed_history_prefix = None;
        assert_eq!(
            identity,
            historical_projection_identity(&other_payload, &target)?
        );
        let mut other_source = source.clone();
        other_source.expected_reported_model = Some(ModelId::new("claude-other-actual")?);
        let other_actual = bind_replay_payload(
            &other_source,
            ModelId::new("claude-other-actual")?,
            SIGNATURE_REPLAY_KIND,
            raw("\"first-secret\"")?,
        )?;
        assert_ne!(
            identity,
            historical_projection_identity(&other_actual, &target)?
        );
        target.expected_reported_model = Some(ModelId::new("claude-other-actual")?);
        assert_ne!(identity, historical_projection_identity(&replay, &target)?);
        Ok(())
    }

    #[test]
    fn bound_thinking_replay_survives_history_segment_codec_roundtrip() -> TestResult {
        use plakous_model::{
            ConversationItem, EntryId, HistorySegment, HistorySegmentKind, Provenance,
            ReasoningBlock,
        };
        use uuid::Uuid;

        let policy = policy()?;
        let model = ModelId::new("claude-exact")?;
        for signature in ["\"opaque\"", "\"\""] {
            let replay = bind_replay_payload(
                &policy,
                model.clone(),
                SIGNATURE_REPLAY_KIND,
                raw(signature)?,
            )?;
            let segment = HistorySegment::new(
                HistorySegmentKind::ReasoningReplay,
                vec![ConversationItem::new(
                    EntryId::try_from(Uuid::from_u128(1))?,
                    Actor::Assistant,
                    vec![ContentBlock::Reasoning(ReasoningBlock::new(
                        ReasoningVisibility::Omitted,
                        None,
                        Some(replay),
                    )?)],
                    Provenance::ModelOutput,
                )?],
            )?;
            let mut value: Value = serde_json::from_slice(segment.canonical_bytes())?;
            let object = value
                .as_object_mut()
                .ok_or("history segment canonical bytes were not an object")?;
            if object
                .insert(
                    "identity".to_owned(),
                    serde_json::to_value(segment.identity())?,
                )
                .is_some()
            {
                return Err("canonical history bytes already carried identity".into());
            }
            let roundtrip: HistorySegment = serde_json::from_value(value)?;
            assert_eq!(roundtrip.identity(), segment.identity(), "{signature}");
            assert_eq!(
                roundtrip.canonical_bytes(),
                segment.canonical_bytes(),
                "{signature}"
            );
        }
        Ok(())
    }

    #[test]
    fn replay_outer_binding_rejects_every_one_field_mutation_for_both_kinds() -> TestResult {
        let policy = policy()?;
        let model = ModelId::new("claude-exact")?;
        for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
            let source = bind_replay_payload(&policy, model.clone(), kind, raw("\"opaque\"")?)?;
            let foreign_kind = if kind == SIGNATURE_REPLAY_KIND {
                REDACTED_REPLAY_KIND
            } else {
                SIGNATURE_REPLAY_KIND
            };
            let variants = [
                rebound(
                    &source,
                    "foreign.messages",
                    None,
                    ReplayScope::ExactModel(model.clone()),
                    kind,
                    MESSAGES_REPLAY_SCHEMA_REVISION,
                    DataClassification::Sensitive,
                )?,
                rebound(
                    &source,
                    "anthropic.messages",
                    Some(ModelFamilyId::new("foreign-family")?),
                    ReplayScope::ExactModel(model.clone()),
                    kind,
                    MESSAGES_REPLAY_SCHEMA_REVISION,
                    DataClassification::Sensitive,
                )?,
                rebound(
                    &source,
                    "anthropic.messages",
                    None,
                    ReplayScope::ExactModel(ModelId::new("claude-foreign")?),
                    kind,
                    MESSAGES_REPLAY_SCHEMA_REVISION,
                    DataClassification::Sensitive,
                )?,
                rebound(
                    &source,
                    "anthropic.messages",
                    None,
                    ReplayScope::Protocol,
                    kind,
                    MESSAGES_REPLAY_SCHEMA_REVISION,
                    DataClassification::Sensitive,
                )?,
                rebound(
                    &source,
                    "anthropic.messages",
                    None,
                    ReplayScope::ExactModel(model.clone()),
                    kind,
                    MESSAGES_REPLAY_SCHEMA_REVISION - 1,
                    DataClassification::Sensitive,
                )?,
                rebound(
                    &source,
                    "anthropic.messages",
                    None,
                    ReplayScope::ExactModel(model.clone()),
                    kind,
                    MESSAGES_REPLAY_SCHEMA_REVISION,
                    DataClassification::Public,
                )?,
                rebound(
                    &source,
                    "anthropic.messages",
                    None,
                    ReplayScope::ExactModel(model.clone()),
                    foreign_kind,
                    MESSAGES_REPLAY_SCHEMA_REVISION,
                    DataClassification::Sensitive,
                )?,
            ];
            for replay in variants {
                assert!(!projectable_historical_thinking(&replay, &policy, kind));
                let Err(error) = replay_payload(&replay, &policy, kind) else {
                    return Err("foreign outer replay axis was accepted".into());
                };
                assert_eq!(error.kind(), MessagesRequestErrorKind::InvalidRequest);
            }
        }
        Ok(())
    }

    #[test]
    fn replay_inner_binding_rejects_every_one_field_mutation_for_both_kinds() -> TestResult {
        let policy = policy()?;
        let model = ModelId::new("claude-exact")?;
        for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
            let source = bind_replay_payload(&policy, model.clone(), kind, raw("\"opaque\"")?)?;
            let base: Value = serde_json::from_str(source.payload().get())?;
            let mut identities = std::collections::HashSet::new();
            identities.insert(historical_projection_identity(&source, &policy)?);
            let mutations = [
                ("requested_model", serde_json::json!("claude-foreign")),
                ("actual_model", serde_json::json!("claude-foreign")),
                (
                    "semantic_descriptor_binding",
                    serde_json::to_value(Digest::sha256(
                        DigestDomain::ModelBinding,
                        1,
                        &[b"foreign-descriptor"],
                    )?)?,
                ),
                (
                    "account_binding",
                    serde_json::to_value(Digest::sha256(
                        DigestDomain::ModelBinding,
                        1,
                        &[b"foreign-account"],
                    )?)?,
                ),
                (
                    "selected_provider_configuration",
                    serde_json::to_value(selected_id(b"foreign-selected")?)?,
                ),
                (
                    "effective_provider_configuration",
                    serde_json::to_value(effective_id(b"foreign-effective")?)?,
                ),
                (
                    "replay_schema_revision",
                    serde_json::json!(MESSAGES_REPLAY_SCHEMA_REVISION + 1),
                ),
                ("api_version", serde_json::json!("2099-01-01")),
                ("beta_header", serde_json::json!("foreign-beta")),
            ];
            for (field, replacement) in mutations {
                let mut changed = base.clone();
                changed[field] = replacement;
                let replay = ReplayEnvelope::new(
                    source.protocol().clone(),
                    None,
                    source.scope().clone(),
                    source.kind().clone(),
                    source.version(),
                    source.classification(),
                    raw(&serde_json::to_string(&changed)?)?,
                )?;
                let Err(error) = replay_payload(&replay, &policy, kind) else {
                    return Err(format!("foreign inner replay axis was accepted: {field}").into());
                };
                assert_eq!(
                    error.kind(),
                    MessagesRequestErrorKind::InvalidRequest,
                    "{field}"
                );
                if matches!(field, "actual_model" | "replay_schema_revision") {
                    assert!(!projectable_historical_thinking(&replay, &policy, kind));
                } else {
                    assert!(projectable_historical_thinking(&replay, &policy, kind));
                    assert!(
                        identities.insert(historical_projection_identity(&replay, &policy)?),
                        "{field}"
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn replay_debug_and_errors_redact_signed_and_redacted_payloads() -> TestResult {
        let policy = policy()?;
        for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
            let sentinel = format!("opaque-secret-{kind}");
            let replay = bind_replay_payload(
                &policy,
                ModelId::new("claude-exact")?,
                kind,
                raw(&serde_json::to_string(&sentinel)?)?,
            )?;
            assert!(!format!("{replay:?}").contains(&sentinel));
            let foreign = policy_with_authority(b"foreign-account", b"selected", b"effective")?;
            let Err(error) = replay_payload(&replay, &foreign, kind) else {
                return Err("foreign replay account was accepted".into());
            };
            assert!(!error.to_string().contains(&sentinel));
            assert!(error.to_string().len() < 128);
        }
        Ok(())
    }

    #[test]
    fn foreign_summary_and_opaque_reasoning_project_portably() -> TestResult {
        let policy = policy()?;
        let summary = plakous_model::ReasoningBlock::new(
            ReasoningVisibility::Summary,
            Some("portable reasoning summary".to_owned()),
            None,
        )?;
        let opaque = plakous_model::ReasoningBlock::new(
            ReasoningVisibility::OpaqueReplay,
            None,
            Some(ReplayEnvelope::new(
                ProtocolId::new("openai.codex.responses")?,
                Some(ModelFamilyId::new("gpt")?),
                ReplayScope::ExactModel(ModelId::new("gpt-5.6-sol")?),
                ReplayKindId::new("responses.output_item")?,
                1,
                DataClassification::Sensitive,
                raw("\"encrypted\"")?,
            )?),
        )?;
        let visible_without_replay = plakous_model::ReasoningBlock::new(
            ReasoningVisibility::Visible,
            Some("displayable thought".to_owned()),
            None,
        )?;
        let (blocks, pending) = translate_assistant(
            &[
                ContentBlock::Text(plakous_model::TextBlock::new(
                    "prior answer",
                    Some(plakous_model::TextPhase::Final),
                )?),
                ContentBlock::Reasoning(summary),
                ContentBlock::Reasoning(opaque),
                ContentBlock::Reasoning(visible_without_replay),
            ],
            &policy,
        )?;
        assert!(pending.is_empty());
        let texts: Vec<&str> = blocks
            .iter()
            .filter_map(|block| match block {
                MessageBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            [
                "prior answer",
                "portable reasoning summary",
                "displayable thought"
            ]
        );
        assert!(blocks.iter().all(|block| !matches!(
            block,
            MessageBlock::Thinking { .. } | MessageBlock::RedactedThinking { .. }
        )));

        let native = bind_replay_payload(
            &policy,
            ModelId::new("claude-exact")?,
            SIGNATURE_REPLAY_KIND,
            raw("\"opaque\"")?,
        )?;
        let visible = plakous_model::ReasoningBlock::new(
            ReasoningVisibility::Visible,
            Some("same-family thought".to_owned()),
            Some(native),
        )?;
        let foreign = policy_with_authority(b"foreign-account", b"selected", b"effective")?;
        let (folded, pending) = translate_assistant(&[ContentBlock::Reasoning(visible)], &foreign)?;
        assert!(pending.is_empty());
        assert!(matches!(
            folded.as_slice(),
            [MessageBlock::Text { text, .. }] if text == "same-family thought"
        ));
        Ok(())
    }

    fn foreign_responses_opaque(fallback: OpaqueFallback) -> TestResult<ContentBlock> {
        Ok(ContentBlock::Opaque(plakous_model::OpaqueBlock::new(
            ReplayEnvelope::new(
                ProtocolId::new("openai.codex.responses")?,
                Some(ModelFamilyId::new("gpt")?),
                ReplayScope::ExactModel(ModelId::new("gpt-5.6-sol")?),
                ReplayKindId::new("responses.output_item")?,
                1,
                DataClassification::Sensitive,
                raw("\"encrypted\"")?,
            )?,
            fallback,
        )?))
    }

    #[test]
    fn foreign_opaque_siblings_project_portably() -> TestResult {
        let policy = policy()?;
        let (text_opaque, pending) = translate_assistant(
            &[
                ContentBlock::Text(plakous_model::TextBlock::new(
                    "prior answer",
                    Some(plakous_model::TextPhase::Final),
                )?),
                foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?,
            ],
            &policy,
        )?;
        assert!(pending.is_empty());
        assert!(matches!(
            text_opaque.as_slice(),
            [MessageBlock::Text { text, .. }] if text == "prior answer"
        ));

        let (commentary_opaque, pending) = translate_assistant(
            &[
                ContentBlock::Text(plakous_model::TextBlock::new(
                    "live comment",
                    Some(plakous_model::TextPhase::Commentary),
                )?),
                foreign_responses_opaque(OpaqueFallback::Omit)?,
            ],
            &policy,
        )?;
        assert!(pending.is_empty());
        assert!(matches!(
            commentary_opaque.as_slice(),
            [MessageBlock::Text { text, .. }] if text == "live comment"
        ));

        let call = ToolCallBlock::new(
            CallId::try_from(uuid::Uuid::from_u128(11))?,
            ToolName::new("process_session_start")?,
            ToolExecutionOwner::PlakLocal,
            plakous_model::ToolCallState::Requested,
            plakous_model::ToolCorrelation::new(
                Some("call_1".into()),
                None,
                ToolCallerLineage::Direct,
            )?,
            "{\"command\":\"pwd\"}",
        )?;
        let (tool_opaque, pending) = translate_assistant(
            &[
                ContentBlock::ToolCall(call),
                foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?,
            ],
            &policy,
        )?;
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            tool_opaque.as_slice(),
            [MessageBlock::ToolUse { id, name, .. }]
                if id == "call_1" && name == "process_session_start"
        ));
        Ok(())
    }

    /// Replay sends the exact name the model called, never the canonical
    /// name the call resolved to.
    #[test]
    fn an_aliased_call_replays_its_exposed_name() -> TestResult {
        let policy = policy()?;
        let call = ToolCallBlock::new(
            CallId::try_from(uuid::Uuid::from_u128(12))?,
            ToolName::new("Bash")?,
            ToolExecutionOwner::PlakLocal,
            plakous_model::ToolCallState::Requested,
            plakous_model::ToolCorrelation::new(
                Some("call_2".into()),
                None,
                ToolCallerLineage::Direct,
            )?,
            "{\"command\":\"pwd\"}",
        )?
        .with_canonical_name(ToolName::new("process_session_start")?);
        let (blocks, pending) = translate_assistant(&[ContentBlock::ToolCall(call)], &policy)?;
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            blocks.as_slice(),
            [MessageBlock::ToolUse { id, name, .. }] if id == "call_2" && name == "Bash"
        ));
        Ok(())
    }

    /// Rebind re-projection onto Messages: a Codex Code Mode
    /// `exec` cell (UTF-8 input, canonical as a JSON string literal) becomes a
    /// `tool_use` whose object input wraps the program as `{"input": ...}`;
    /// a call whose text never parsed as JSON wraps its raw text the same way.
    #[test]
    fn code_mode_exec_cell_replays_as_an_object_wrapped_tool_use() -> TestResult {
        let policy = policy()?;
        let program = "const out = await tools.exec_command({cmd: 'pwd'});\nyield_(out);";
        let exec_call = ToolCallBlock::new(
            CallId::try_from(uuid::Uuid::from_u128(21))?,
            ToolName::new("exec")?,
            ToolExecutionOwner::PlakLocal,
            plakous_model::ToolCallState::Requested,
            plakous_model::ToolCorrelation::new(
                Some("call_exec_1".into()),
                None,
                ToolCallerLineage::Direct,
            )?,
            serde_json::to_string(program)?,
        )?;
        let (blocks, pending) = translate_assistant(&[ContentBlock::ToolCall(exec_call)], &policy)?;
        assert_eq!(pending.len(), 1);
        let [
            MessageBlock::ToolUse {
                id, name, input, ..
            },
        ] = blocks.as_slice()
        else {
            return Err(format!("expected one tool_use, got {blocks:?}").into());
        };
        assert_eq!(id, "call_exec_1");
        assert_eq!(name, "exec");
        assert_eq!(input, &serde_json::json!({ "input": program }));

        let unparsed = ToolCallBlock::new(
            CallId::try_from(uuid::Uuid::from_u128(22))?,
            ToolName::new("exec")?,
            ToolExecutionOwner::PlakLocal,
            plakous_model::ToolCallState::Requested,
            plakous_model::ToolCorrelation::new(
                Some("call_exec_2".into()),
                None,
                ToolCallerLineage::Direct,
            )?,
            "not json at all",
        )?;
        assert!(unparsed.arguments().is_none());
        let (blocks, _) = translate_assistant(&[ContentBlock::ToolCall(unparsed)], &policy)?;
        let [MessageBlock::ToolUse { input, .. }] = blocks.as_slice() else {
            return Err(format!("expected one tool_use, got {blocks:?}").into());
        };
        assert_eq!(input, &serde_json::json!({ "input": "not json at all" }));
        Ok(())
    }

    #[test]
    fn local_programmatic_lineage_is_rejected_at_provider_wire_boundary() -> TestResult {
        let call = ToolCallBlock::new(
            CallId::try_from(uuid::Uuid::from_u128(12))?,
            ToolName::new("local_lookup")?,
            ToolExecutionOwner::PlakLocal,
            plakous_model::ToolCallState::Requested,
            plakous_model::ToolCorrelation::new(
                Some("call_local_1".into()),
                None,
                ToolCallerLineage::local_programmatic(
                    91,
                    4,
                    "cell-4",
                    7,
                    CallId::try_from(uuid::Uuid::from_u128(12))?,
                    1,
                )?,
            )?,
            r#"{"query":"local"}"#,
        )?;
        let Err(error) = translate_assistant(&[ContentBlock::ToolCall(call)], &policy()?) else {
            return Err("local lineage translated to provider wire".into());
        };
        assert!(error.to_string().contains("local programmatic caller"));
        Ok(())
    }

    #[test]
    fn foreign_citation_refusal_and_opaque_summary_project_portably() -> TestResult {
        let policy = policy()?;
        let citation = plakous_model::CitationBlock::new(
            plakous_model::CitationTarget::new(
                plakous_model::EntryId::try_from(uuid::Uuid::from_u128(3))?,
                0,
            ),
            plakous_model::CitationSource::new("src", "ann", None, None)?,
            None,
        );
        let (cited, pending) = translate_assistant(
            &[
                ContentBlock::Text(plakous_model::TextBlock::new("cited", None)?),
                ContentBlock::Citation(citation),
                foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?,
            ],
            &policy,
        )?;
        assert!(pending.is_empty());
        assert!(matches!(
            cited.as_slice(),
            [MessageBlock::Text { text, .. }] if text == "cited"
        ));

        let (refused, pending) = translate_assistant(
            &[ContentBlock::Refusal(plakous_model::RefusalBlock::new(
                "cannot",
            )?)],
            &policy,
        )?;
        assert!(pending.is_empty());
        assert!(matches!(
            refused.as_slice(),
            [MessageBlock::Text { text, .. }] if text == "cannot"
        ));

        let (summarized, pending) = translate_assistant(
            &[foreign_responses_opaque(OpaqueFallback::SemanticSummary(
                "portable opaque summary".into(),
            ))?],
            &policy,
        )?;
        assert!(pending.is_empty());
        assert!(matches!(
            summarized.as_slice(),
            [MessageBlock::Text { text, .. }] if text == "portable opaque summary"
        ));

        let (opaque_only, pending) = translate_assistant(
            &[foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?],
            &policy,
        )?;
        assert!(pending.is_empty());
        assert_eq!(opaque_only, [] as [MessageBlock; 0]);

        let (compacted, pending) = translate_assistant(
            &[ContentBlock::Compaction(
                plakous_model::CompactionBlock::new("earlier turns were compacted")?,
            )],
            &policy,
        )?;
        assert!(pending.is_empty());
        assert!(matches!(
            compacted.as_slice(),
            [MessageBlock::Text { text, .. }] if text == "earlier turns were compacted"
        ));

        Ok(())
    }

    #[test]
    fn native_compaction_refuses_messages_lowering() -> TestResult {
        let policy = policy()?;
        let native = plakous_model::CompactionBlock::new("ciphertext placeholder")?
            .with_native_item("{\"type\":\"compaction\",\"encrypted_content\":\"x\"}".into());
        let error = translate_assistant(&[ContentBlock::Compaction(native)], &policy)
            .err()
            .ok_or("Messages must not send native ciphertext as text")?;
        assert_eq!(error.kind(), MessagesRequestErrorKind::ProtectedHistory);
        assert!(!error.message().contains("ciphertext placeholder"));
        Ok(())
    }

    #[test]
    fn replay_rebind_and_configuration_mismatches_refuse_both_kinds() -> TestResult {
        let source_policy = policy()?;
        let foreign_policies = [
            policy_with_authority(b"foreign-account", b"selected", b"effective")?,
            policy_with_authority(b"account", b"foreign-selected", b"effective")?,
            policy_with_authority(b"account", b"selected", b"foreign-effective")?,
        ];
        for kind in [SIGNATURE_REPLAY_KIND, REDACTED_REPLAY_KIND] {
            let replay = bind_replay_payload(
                &source_policy,
                ModelId::new("claude-exact")?,
                kind,
                raw("\"opaque\"")?,
            )?;
            for foreign in &foreign_policies {
                if replay_payload(&replay, foreign, kind).is_ok() {
                    return Err("rebind or configuration mismatch reused replay".into());
                }
            }
        }
        Ok(())
    }

    #[test]
    fn incompatible_present_replay_emits_bounded_diagnostic() -> TestResult {
        let source = policy()?;
        let foreign = policy_with_authority(b"foreign-account", b"selected", b"effective")?;
        for (kind, visibility) in [
            (SIGNATURE_REPLAY_KIND, ReasoningVisibility::Visible),
            (REDACTED_REPLAY_KIND, ReasoningVisibility::Redacted),
        ] {
            let replay = bind_replay_payload(
                &source,
                ModelId::new("claude-exact")?,
                kind,
                raw("\"opaque\"")?,
            )?;
            let reasoning = plakous_model::ReasoningBlock::new(
                visibility,
                (visibility != ReasoningVisibility::Redacted).then(|| "portable".into()),
                Some(replay),
            )?;
            let mut diagnostics = Vec::new();
            let block = native_or_portable_thinking(
                &reasoning,
                &foreign,
                kind,
                visibility != ReasoningVisibility::Redacted,
                &mut diagnostics,
            );
            if visibility == ReasoningVisibility::Redacted {
                assert!(block.is_none());
            } else {
                assert!(
                    matches!(block, Some(MessageBlock::Text { text, .. }) if text == "portable")
                );
            }
            assert_eq!(
                diagnostics,
                ["incompatible Messages thinking replay was stripped"]
            );
        }
        Ok(())
    }

    /// Under strict replay, reasoning that cannot be replayed natively is
    /// dropped. It is never flattened into assistant text, whatever is wrong
    /// with it.
    ///
    /// Anthropic's `reasoning_extraction` classifier refused a resumed Claude
    /// Opus 5 turn that received the model's own chain of thought back as text.
    #[test]
    fn strict_replay_drops_unreplayable_reasoning_instead_of_sending_it_as_text() -> TestResult {
        let source = policy()?;
        let strict = policy()?.with_strict_thinking_replay(true);
        let lenient = policy()?;
        let foreign = policy_with_authority(b"foreign-account", b"selected", b"effective")?
            .with_strict_thinking_replay(true);

        // No signature at all: the shape a restored turn has when the envelope
        // did not survive persistence.
        let bare = plakous_model::ReasoningBlock::new(
            ReasoningVisibility::Visible,
            Some("private chain of thought".into()),
            None,
        )?;
        let mut diagnostics = Vec::new();
        assert!(
            native_or_portable_thinking(
                &bare,
                &strict,
                SIGNATURE_REPLAY_KIND,
                true,
                &mut diagnostics
            )
            .is_none()
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("never sent as text"));
        // Lenient lowering is unchanged, so nothing else on this wire moves.
        let mut lenient_diagnostics = Vec::new();
        assert!(matches!(
            native_or_portable_thinking(
                &bare,
                &lenient,
                SIGNATURE_REPLAY_KIND,
                true,
                &mut lenient_diagnostics
            ),
            Some(MessageBlock::Text { text, .. }) if text == "private chain of thought"
        ));
        assert_eq!(lenient_diagnostics, [] as [String; 0]);

        // A signature from another protocol is refused the same way.
        let foreign_replay = bind_replay_payload(
            &source,
            ModelId::new("claude-exact")?,
            SIGNATURE_REPLAY_KIND,
            raw("\"opaque\"")?,
        )?;
        let carried = plakous_model::ReasoningBlock::new(
            ReasoningVisibility::Visible,
            Some("private chain of thought".into()),
            Some(foreign_replay),
        )?;
        let mut diagnostics = Vec::new();
        assert!(
            native_or_portable_thinking(
                &carried,
                &foreign,
                SIGNATURE_REPLAY_KIND,
                true,
                &mut diagnostics
            )
            .is_none()
        );
        assert!(diagnostics[0].contains("never sent as text"));

        // A summary has no native Messages form, so it is dropped too.
        let summary = plakous_model::ReasoningBlock::new(
            ReasoningVisibility::Summary,
            Some("a summary of the private chain of thought".into()),
            None,
        )?;
        let mut diagnostics = Vec::new();
        assert!(translate_reasoning_block(&summary, &strict, &mut diagnostics).is_none());
        assert!(diagnostics[0].contains("never sent as text"));
        Ok(())
    }

    #[test]
    fn update_after_terminal_result_is_rejected_without_reordering() -> TestResult {
        let call_id = CallId::try_from(TestUuid::from_u128(42))?;
        let correlation = plakous_model::ToolCorrelation::new(
            Some("remote-exec".into()),
            None,
            ToolCallerLineage::Direct,
        )?;
        let result = plakous_model::ToolResultBlock::new(
            call_id,
            ToolExecutionOwner::PlakLocal,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TestTextBlock::new("done", None)?)],
            correlation.clone(),
        )?;
        let update = ToolResultUpdateBlock::new(
            call_id,
            ToolExecutionOwner::PlakLocal,
            TestTextBlock::new("late", None)?,
            correlation.clone(),
        )?;
        let history = ConversationView::new(vec![ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(43))?,
            Actor::Tool,
            vec![
                ContentBlock::ToolResult(result),
                ContentBlock::ToolResultUpdate(update),
            ],
            Provenance::ToolExecution,
        )?]);
        let mut pending = vec![PendingTool {
            call_id,
            remote_id: "remote-exec".into(),
            owner: ToolExecutionOwner::PlakLocal,
            correlation,
        }];
        let mut index = 0;
        let Err(error) = translate_tool_messages(history.items(), &mut index, &mut pending) else {
            return Err("post-terminal update was reordered instead of rejected".into());
        };
        assert_eq!(error.kind(), MessagesRequestErrorKind::InvalidRequest);
        assert!(error.message().contains("followed its terminal result"));
        Ok(())
    }

    #[test]
    fn updates_and_media_fold_into_one_portable_terminal_result() -> TestResult {
        let call_id = CallId::try_from(uuid::Uuid::from_u128(42))?;
        let correlation = plakous_model::ToolCorrelation::new(
            Some("remote-exec".into()),
            None,
            ToolCallerLineage::Direct,
        )?;
        let update = ToolResultUpdateBlock::new(
            call_id,
            ToolExecutionOwner::PlakLocal,
            plakous_model::TextBlock::new("working", None)?,
            correlation.clone(),
        )?;
        let result = plakous_model::ToolResultBlock::new(
            call_id,
            ToolExecutionOwner::PlakLocal,
            ToolResultKind::Success,
            vec![
                ToolResultPart::Image(plakous_model::ImageBlock::with_detail(
                    "image/png",
                    plakous_model::MediaSource::data_url("data:image/png;base64,AAAA")?,
                    Some(plakous_model::ImageDetailHint::High),
                )?),
                ToolResultPart::Audio(plakous_model::AudioBlock::new(
                    "audio/wav",
                    plakous_model::MediaSource::data_url("data:audio/wav;base64,BBBB")?,
                )?),
            ],
            correlation.clone(),
        )?;
        let block = translate_tool_result(
            &result,
            &PendingTool {
                call_id,
                remote_id: "remote-exec".into(),
                owner: ToolExecutionOwner::PlakLocal,
                correlation,
            },
            &[&update],
        )?;
        let value = serde_json::to_value(block)?;
        assert_eq!(value["tool_use_id"], "remote-exec");
        assert_eq!(value["content"][0]["text"], "working");
        assert_eq!(value["content"][1]["text"], "[image result: image/png]");
        assert_eq!(value["content"][2]["text"], "[audio result: audio/wav]");
        Ok(())
    }

    fn batch_call(n: u128) -> TestResult<ContentBlock> {
        Ok(ContentBlock::ToolCall(ToolCallBlock::new(
            CallId::try_from(TestUuid::from_u128(n))?,
            ToolName::new("read_file")?,
            ToolExecutionOwner::PlakLocal,
            plakous_model::ToolCallState::Requested,
            plakous_model::ToolCorrelation::new(
                Some(format!("call_{n}")),
                None,
                ToolCallerLineage::Direct,
            )?,
            "{}",
        )?))
    }

    fn batch_result(n: u128) -> TestResult<ContentBlock> {
        Ok(ContentBlock::ToolResult(
            plakous_model::ToolResultBlock::new(
                CallId::try_from(TestUuid::from_u128(n))?,
                ToolExecutionOwner::PlakLocal,
                ToolResultKind::Success,
                vec![ToolResultPart::Text(TestTextBlock::new("ok", None)?)],
                plakous_model::ToolCorrelation::new(
                    Some(format!("call_{n}")),
                    None,
                    ToolCallerLineage::Direct,
                )?,
            )?,
        ))
    }

    fn batch_item(
        id: u128,
        actor: Actor,
        blocks: Vec<ContentBlock>,
    ) -> TestResult<ConversationItem> {
        let provenance = if actor == Actor::Tool {
            Provenance::ToolExecution
        } else {
            Provenance::ModelOutput
        };
        Ok(ConversationItem::new(
            TestEntryId::try_from(TestUuid::from_u128(id))?,
            actor,
            blocks,
            provenance,
        )?)
    }

    fn lower(items: Vec<ConversationItem>) -> Result<Vec<MessageParam>, MessagesRequestError> {
        let policy = policy().map_err(|_| invalid("test policy"))?;
        translate_history(
            &ConversationView::new(items),
            &policy,
            &ToolOwnership::default(),
            false,
            true,
            &mut Vec::new(),
        )
        .map(|(messages, _, _)| messages)
    }

    #[test]
    fn a_run_of_tool_call_items_folds_into_one_assistant_message() -> TestResult {
        let messages = lower(vec![
            batch_item(100, Actor::Assistant, vec![batch_call(1)?])?,
            batch_item(101, Actor::Assistant, vec![batch_call(2)?])?,
            batch_item(102, Actor::Tool, vec![batch_result(1)?, batch_result(2)?])?,
        ])?;
        let value = serde_json::to_value(&messages)?;
        assert_eq!(value.as_array().map(Vec::len), Some(2), "{value}");
        assert_eq!(value[0]["role"], "assistant");
        assert_eq!(value[0]["content"][0]["type"], "tool_use");
        assert_eq!(value[0]["content"][0]["id"], "call_1");
        assert_eq!(value[0]["content"][1]["type"], "tool_use");
        assert_eq!(value[0]["content"][1]["id"], "call_2");
        assert_eq!(value[1]["role"], "user");
        assert_eq!(value[1]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(value[1]["content"][1]["tool_use_id"], "call_2");
        Ok(())
    }

    #[test]
    fn thinking_or_text_inside_an_open_tool_batch_is_refused() -> TestResult {
        let policy = policy()?;
        let replay = bind_replay_payload(
            &policy,
            ModelId::new("claude-exact")?,
            SIGNATURE_REPLAY_KIND,
            raw("\"opaque\"")?,
        )?;
        let thinking = ContentBlock::Reasoning(plakous_model::ReasoningBlock::new(
            ReasoningVisibility::Omitted,
            None,
            Some(replay),
        )?);
        let text = ContentBlock::Text(TestTextBlock::new("more", None)?);
        for intervening in [vec![thinking, batch_call(2)?], vec![text, batch_call(2)?]] {
            let Err(error) = lower(vec![
                batch_item(100, Actor::Assistant, vec![batch_call(1)?])?,
                batch_item(101, Actor::Assistant, intervening)?,
                batch_item(102, Actor::Tool, vec![batch_result(1)?, batch_result(2)?])?,
            ]) else {
                return Err("a block that is not tool_use folded behind an open tool_use".into());
            };
            assert_eq!(error.kind(), MessagesRequestErrorKind::InvalidRequest);
            assert!(
                error.message().contains("intervened"),
                "{}",
                error.message()
            );
        }
        Ok(())
    }
}
