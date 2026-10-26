//! Canonical lowering into the Messages request grammar.

use std::collections::BTreeMap;
use std::fmt;

use plakous_attempt::ReplaySafety;
use plakous_model::{
    CallId, Digest, EffectiveProviderConfigurationId, ModelDiagnostic, ModelId, ProtocolId,
    SelectedProviderConfigurationId, ToolCallerDisposition, ToolExecutionOwner, ToolName,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

mod blocks;
mod lower;
mod native;
mod translate;

#[cfg(test)]
pub(crate) use native::native_replay_block;
pub(crate) use native::{
    NATIVE_REPLAY_KIND, NativeReplay, excluded_content_block, protected_tool_block,
};
pub(crate) const SKIPPED_BLOCKS_REPLAY_KIND: &str = "messages.skipped_blocks";

pub use lower::{lower_request, lower_request_with_active_artifact};
pub(crate) use lower::{
    messages_artifact_binding, messages_artifact_configuration_identity, messages_tool_set_identity,
};
pub use translate::bind_replay_payload;
pub(crate) use translate::{AssistantReplayState, translate_assistant_bound};
use translate::{
    mark_last_message_cache_breakpoint, push_message, text_block, translate_history,
    translate_reasoning, translate_tool_choice,
};

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
    #[allow(
        clippy::too_many_arguments,
        reason = "public signature; typed-parameter pass follows with the provider/hosted wave"
    )]
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
    #[serde(serialize_with = "native::serialize_wire_blocks")]
    content: Vec<MessageBlock>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum MessageBlock {
    // Keep a distinct persisted tag: older artifact decoders must refuse this
    // variant, not deserialize native cited text while discarding its citations.
    Native {
        block: Box<RawValue>,
    },
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
