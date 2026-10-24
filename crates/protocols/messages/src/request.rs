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
    /// Creates an exact mapping. An absent tool takes the owner its
    /// configured tool names: its MCP server, or local ownership.
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
            .unwrap_or(ToolExecutionOwner::Local)
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
            .or_insert_with(|| request.local_tool_owner(&name));
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
    Image {
        source: ImageSourceParam,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlParam>,
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
    Image {
        source: ImageSourceParam,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlParam>,
    },
}

/// Where a Messages image block reads its pixels from.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ImageSourceParam {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

fn image_source(
    image: &plakous_model::ImageBlock,
) -> Result<ImageSourceParam, MessagesRequestError> {
    use base64::Engine as _;
    let source = image.source();
    if let Some(bytes) = source.as_bytes() {
        return Ok(ImageSourceParam::Base64 {
            media_type: image.media_type().to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    if let Some(data_url) = source.as_data_url() {
        let (media_type, data) = data_url
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(";base64,"))
            .ok_or_else(|| unsupported("image data URL is not base64-encoded"))?;
        return Ok(ImageSourceParam::Base64 {
            media_type: media_type.to_owned(),
            data: data.to_owned(),
        });
    }
    source
        .as_uri()
        .map(|url| ImageSourceParam::Url {
            url: url.to_owned(),
        })
        .ok_or_else(|| unsupported("image source is outside the Messages profile"))
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
                MessageBlock::Text { cache_control, .. }
                | MessageBlock::Image { cache_control, .. } => {
                    *cache_control = Some(cache);
                    return;
                }
                MessageBlock::ToolResult { content, .. } => {
                    if let Some(
                        ToolResultContent::Text { cache_control, .. }
                        | ToolResultContent::Image { cache_control, .. },
                    ) = content.last_mut()
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
            ContentBlock::Image(image) => Ok(MessageBlock::Image {
                source: image_source(image)?,
                cache_control: None,
            }),
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
                    Ok(ToolResultContent::Image {
                        source: image_source(image)?,
                        cache_control: None,
                    })
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
mod tests;
