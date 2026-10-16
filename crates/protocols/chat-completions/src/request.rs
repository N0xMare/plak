//! Deterministic lowering from canonical history to Chat messages.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use plakous_attempt::ReplaySafety;
use plakous_model::{
    Actor, CachePolicy, CallId, ContentBlock, DataClassification, Digest, DigestDomain,
    EffectiveProviderConfigurationId, InstructionSource, ModelId, OpaqueFallback, OutputFormat,
    ProtocolId, ReasoningEffort, ReasoningPolicy, ReasoningVisibility, ReplayScope,
    SelectedProviderConfigurationId, SemanticJsonV1, ToolCallState, ToolCallerLineage, ToolChoice,
    ToolCorrelation, ToolExecutionOwner, ToolInputContract, ToolName, ToolResultKind,
    ToolResultPart, ValidatedRequestPlan,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

/// Item-level Chat replay schema. Chat V1 has no aggregate family artifact.
pub const CHAT_REPLAY_SCHEMA_REVISION: u32 = 1;

pub(crate) const REASONING_DETAIL_REPLAY_KIND: &str = "chat.reasoning_detail";
const MAX_ROUTER_ENDPOINT_BYTES: usize = 256;
const MAX_ROUTER_DATA_COLLECTION_BYTES: usize = 32;
const MAX_REASONING_DETAIL_BYTES: usize = 1024 * 1024;
const MAX_REASONING_DETAILS: u32 = 4096;
const MAX_INERT_HISTORICAL_TOOL_BYTES: usize = 1024 * 1024;

/// Whether exact native reasoning-detail replay is admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatReasoningReplay {
    /// Visible reasoning text may be sent, but opaque native details are forbidden.
    VisibleOnly,
    /// Compatible opaque details are replayed byte-for-byte and in source order.
    ExactDetails,
}

/// Closed Chat-family spelling for the enforced output-token bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatOutputLimitField {
    /// Legacy and broadly gateway-compatible `max_tokens` spelling.
    MaxTokens,
    /// Newer OpenAI-compatible `max_completion_tokens` spelling.
    MaxCompletionTokens,
}

impl ChatReasoningReplay {
    /// Reports whether opaque native details must remain replay-visible.
    pub const fn includes_exact_details(self) -> bool {
        matches!(self, Self::ExactDetails)
    }
}

/// Closed request-lowering failure class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatRequestErrorKind {
    /// The plan requests a semantic excluded from Chat V1.
    UnsupportedSemantic,
    /// Canonical history, correlation, or target binding is invalid.
    InvalidRequest,
    /// History contains native material that cannot be replayed on this kernel.
    ProtectedHistory,
}

/// Content-free pre-dispatch lowering error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatRequestError {
    kind: ChatRequestErrorKind,
    message: String,
}

impl ChatRequestError {
    /// Returns the closed class.
    pub const fn kind(&self) -> ChatRequestErrorKind {
        self.kind
    }
    /// Returns the non-secret diagnostic.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ChatRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for ChatRequestError {}

fn unsupported(message: impl Into<String>) -> ChatRequestError {
    ChatRequestError {
        kind: ChatRequestErrorKind::UnsupportedSemantic,
        message: message.into(),
    }
}
fn invalid(message: impl Into<String>) -> ChatRequestError {
    ChatRequestError {
        kind: ChatRequestErrorKind::InvalidRequest,
        message: message.into(),
    }
}

/// Exact dialect facts consumed by neutral Chat lowering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatRequestPolicy {
    protocol: ProtocolId,
    requested_model: ModelId,
    descriptor_binding: Digest,
    account_binding: Digest,
    selected: SelectedProviderConfigurationId,
    effective: EffectiveProviderConfigurationId,
    maximum_output_tokens: u32,
    output_limit_field: ChatOutputLimitField,
    reasoning_replay: ChatReasoningReplay,
    router: Option<ChatRouterPolicy>,
    reasoning: ReasoningPolicy,
    reasoning_alias: crate::stream::ReasoningAliasPolicy,
    disabled_reasoning_wire: ChatDisabledReasoningWire,
    tool_choice_none_omits_tools: bool,
    tool_choice_auto_uses_default: bool,
    plugins: Option<&'static [ChatRequestPlugin]>,
    gateway_session_key: Option<String>,
}

/// Wire shape for disabled canonical reasoning on Chat requests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChatDisabledReasoningWire {
    /// Omit the `reasoning` key entirely.
    #[default]
    Omit,
    /// Emit `reasoning: { enabled: false }`.
    EnabledFalse,
}

/// One OpenRouter request plugin declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ChatRequestPlugin {
    id: &'static str,
    enabled: bool,
}

/// Disables OpenRouter middle-out truncation on every product Chat request.
pub const OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS: &[ChatRequestPlugin] =
    &[ChatRequestPlugin {
        id: "context-compression",
        enabled: false,
    }];

impl ChatRequestPolicy {
    /// Creates a complete credential-free lowering policy.
    pub fn new(
        protocol: ProtocolId,
        requested_model: ModelId,
        descriptor_binding: Digest,
        account_binding: Digest,
        selected: SelectedProviderConfigurationId,
        effective: EffectiveProviderConfigurationId,
        maximum_output_tokens: u32,
        output_limit_field: ChatOutputLimitField,
        reasoning_replay: ChatReasoningReplay,
        router: Option<ChatRouterPolicy>,
        reasoning: ReasoningPolicy,
        reasoning_alias: crate::stream::ReasoningAliasPolicy,
    ) -> Result<Self, ChatRequestError> {
        if maximum_output_tokens == 0 {
            return Err(invalid("Chat maximum output tokens must be nonzero"));
        }
        Ok(Self {
            protocol,
            requested_model,
            descriptor_binding,
            account_binding,
            selected,
            effective,
            maximum_output_tokens,
            output_limit_field,
            reasoning_replay,
            router,
            reasoning,
            reasoning_alias,
            disabled_reasoning_wire: ChatDisabledReasoningWire::Omit,
            tool_choice_none_omits_tools: false,
            tool_choice_auto_uses_default: false,
            plugins: None,
            gateway_session_key: None,
        })
    }

    /// Selects the exact wire encoding for disabled canonical reasoning.
    #[must_use]
    pub const fn with_disabled_reasoning_wire(
        mut self,
        disabled_reasoning_wire: ChatDisabledReasoningWire,
    ) -> Self {
        self.disabled_reasoning_wire = disabled_reasoning_wire;
        self
    }

    /// Omits explicit `auto` when the dialect documents it as the tools default.
    #[must_use]
    pub const fn with_tool_choice_auto_uses_default(mut self, enabled: bool) -> Self {
        self.tool_choice_auto_uses_default = enabled;
        self
    }

    /// Enables omitting `tools` and `tool_choice` when canonical choice is `None`.
    #[must_use]
    pub const fn with_tool_choice_none_omits_tools(mut self, enabled: bool) -> Self {
        self.tool_choice_none_omits_tools = enabled;
        self
    }

    /// Attaches OpenRouter plugin declarations to the lowered request body.
    #[must_use]
    pub const fn with_plugins(mut self, plugins: &'static [ChatRequestPlugin]) -> Self {
        self.plugins = Some(plugins);
        self
    }

    /// Emits OpenRouter `session_id` for sticky routing.
    ///
    /// Chat does not emit `prompt_cache_key` or top-level `cache_control`:
    /// those fields trip `require_parameters` on OpenAI-shaped endpoints.
    #[must_use]
    pub fn with_gateway_session_key(mut self, key: impl Into<String>) -> Self {
        let key = key.into();
        self.gateway_session_key = (!key.is_empty()).then_some(key);
        self
    }
    /// Returns the exact protocol identity.
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }
    /// Returns the requested model.
    pub const fn requested_model(&self) -> &ModelId {
        &self.requested_model
    }

    /// Returns the descriptor-independent replay target.
    pub fn replay_target(&self) -> ChatReplayTargetV1 {
        ChatReplayTargetV1::new(
            self.protocol.clone(),
            self.requested_model.clone(),
            self.account_binding,
            self.selected,
            self.effective,
        )
    }

    /// Returns the configured exact-detail replay disposition.
    pub const fn reasoning_replay(&self) -> ChatReasoningReplay {
        self.reasoning_replay
    }

    /// Returns the exact canonical reasoning policy.
    pub const fn reasoning(&self) -> ReasoningPolicy {
        self.reasoning
    }

    /// Returns the exact response-field alias policy bound to replay.
    pub const fn reasoning_alias(&self) -> crate::stream::ReasoningAliasPolicy {
        self.reasoning_alias
    }

    /// Returns the exact disabled-reasoning wire encoding.
    pub const fn disabled_reasoning_wire(&self) -> ChatDisabledReasoningWire {
        self.disabled_reasoning_wire
    }
}

/// Strict one-endpoint request extension for a routed Chat gateway.
///
/// Gateway-managed routing is represented by omitting this value. The family
/// kernel accepts only the closed, no-fallback wire shape; endpoint discovery
/// and qualification remain dialect responsibilities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatRouterPolicy {
    endpoint: Option<String>,
    data_collection: Option<String>,
    zdr: Option<bool>,
}

impl ChatRouterPolicy {
    /// Creates a bounded one-endpoint router restriction.
    pub fn strict(
        endpoint: impl Into<String>,
        data_collection: impl Into<String>,
        zdr: bool,
    ) -> Result<Self, ChatRequestError> {
        let endpoint = endpoint.into();
        let data_collection = data_collection.into();
        let reserved = endpoint.split(['/', ':']).any(|segment| {
            matches!(
                segment.to_ascii_lowercase().as_str(),
                "auto" | "free" | "latest" | "router" | "routers" | "preset" | "presets"
            )
        });
        if endpoint.is_empty()
            || endpoint.len() > MAX_ROUTER_ENDPOINT_BYTES
            || data_collection.is_empty()
            || data_collection.len() > MAX_ROUTER_DATA_COLLECTION_BYTES
            || !endpoint.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':')
            })
            || !data_collection
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            || reserved
        {
            return Err(invalid(
                "Chat router policy is not a bounded exact endpoint",
            ));
        }
        Ok(Self {
            endpoint: Some(endpoint),
            data_collection: Some(data_collection),
            zdr: Some(zdr),
        })
    }

    /// Creates gateway-managed routing with optional explicit retention facts.
    ///
    /// The resulting wire block never contains `only` or `order`. Request
    /// lowering always sets `require_parameters: true`.
    pub fn gateway_managed(
        data_collection: Option<String>,
        zdr: Option<bool>,
    ) -> Result<Self, ChatRequestError> {
        if data_collection.as_deref().is_some_and(|value| {
            value.is_empty()
                || value.len() > MAX_ROUTER_DATA_COLLECTION_BYTES
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }) {
            return Err(invalid("Chat router data collection policy is not bounded"));
        }
        Ok(Self {
            endpoint: None,
            data_collection,
            zdr,
        })
    }
}

/// Descriptor-independent durable target for exact Chat replay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatReplayTargetV1 {
    protocol: ProtocolId,
    requested_model: ModelId,
    account_binding: Digest,
    selected_provider_configuration: SelectedProviderConfigurationId,
    effective_provider_configuration: EffectiveProviderConfigurationId,
}

impl ChatReplayTargetV1 {
    /// Creates the durable target identity. Fresh descriptor evidence is
    /// intentionally checked separately at admission time.
    pub const fn new(
        protocol: ProtocolId,
        requested_model: ModelId,
        account_binding: Digest,
        selected_provider_configuration: SelectedProviderConfigurationId,
        effective_provider_configuration: EffectiveProviderConfigurationId,
    ) -> Self {
        Self {
            protocol,
            requested_model,
            account_binding,
            selected_provider_configuration,
            effective_provider_configuration,
        }
    }

    /// Returns the exact protocol.
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Returns the exact requested model.
    pub const fn requested_model(&self) -> &ModelId {
        &self.requested_model
    }
}

/// Byte-preserving, source-ordered native reasoning detail replay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatReasoningDetailReplayV1 {
    target: ChatReplayTargetV1,
    reasoning_policy: ReasoningPolicy,
    alias_policy: crate::stream::ReasoningAliasPolicy,
    bound_alias: crate::stream::ReasoningAliasPolicy,
    exact_json: String,
    source_ordinal: u32,
}

impl ChatReasoningDetailReplayV1 {
    /// Creates one bounded detail. JSON is validated but retained verbatim.
    pub fn new(
        target: ChatReplayTargetV1,
        reasoning_policy: ReasoningPolicy,
        alias_policy: crate::stream::ReasoningAliasPolicy,
        bound_alias: crate::stream::ReasoningAliasPolicy,
        exact_json: String,
        source_ordinal: u32,
    ) -> Result<Self, ChatRequestError> {
        let alias_matches = match (alias_policy, bound_alias) {
            (
                crate::stream::ReasoningAliasPolicy::EitherBindFirst,
                crate::stream::ReasoningAliasPolicy::Reasoning
                | crate::stream::ReasoningAliasPolicy::ReasoningContent,
            ) => true,
            (configured, bound) => configured == bound,
        };
        if !alias_matches
            || matches!(
                bound_alias,
                crate::stream::ReasoningAliasPolicy::EitherBindFirst
            )
            || exact_json.is_empty()
            || exact_json.len() > MAX_REASONING_DETAIL_BYTES
            || source_ordinal >= MAX_REASONING_DETAILS
            || !SemanticJsonV1::parse(&exact_json).is_ok_and(|value| value.value().is_object())
        {
            return Err(invalid(
                "Chat reasoning detail replay is malformed or unbounded",
            ));
        }
        Ok(Self {
            target,
            reasoning_policy,
            alias_policy,
            bound_alias,
            exact_json,
            source_ordinal,
        })
    }

    /// Returns the exact replay target.
    pub const fn target(&self) -> &ChatReplayTargetV1 {
        &self.target
    }

    /// Returns the canonical reasoning policy identity.
    pub const fn reasoning_policy(&self) -> ReasoningPolicy {
        self.reasoning_policy
    }

    /// Returns the exact response-field alias policy.
    pub const fn alias_policy(&self) -> crate::stream::ReasoningAliasPolicy {
        self.alias_policy
    }

    /// Returns the concrete response spelling bound for this detail.
    pub const fn bound_alias(&self) -> crate::stream::ReasoningAliasPolicy {
        self.bound_alias
    }

    /// Returns the byte-exact JSON detail.
    pub fn exact_json(&self) -> &str {
        &self.exact_json
    }

    /// Returns the dense source ordinal.
    pub const fn source_ordinal(&self) -> u32 {
        self.source_ordinal
    }
}

/// Credential-free deterministic Chat request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoweredChatRequest {
    body: Vec<u8>,
    operation_digest: Digest,
    replay_safety: ReplaySafety,
}
impl LoweredChatRequest {
    /// Returns what replaying the exact body can cost, classified from the
    /// serialized bytes.
    pub const fn replay_safety(&self) -> ReplaySafety {
        self.replay_safety
    }
    /// Returns exact JSON request bytes.
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    /// Returns the family operation identity.
    pub const fn operation_digest(&self) -> Digest {
        self.operation_digest
    }

    #[cfg(test)]
    pub(crate) fn test_fixture(body: Vec<u8>, operation_digest: Digest) -> Self {
        Self {
            replay_safety: classify_replay_safety(&body),
            body,
            operation_digest,
        }
    }
}

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoiceWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "slice_empty")]
    stop: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<RouterWire<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugins: Option<&'a [ChatRequestPlugin]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct RouterWire<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    only: Option<[&'a str; 1]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    order: Option<[&'a str; 1]>,
    allow_fallbacks: bool,
    require_parameters: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_collection: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    zdr: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ReasoningWire {
    Effort { effort: &'static str, exclude: bool },
    Disabled { enabled: bool },
}
fn slice_empty<T>(value: &[T]) -> bool {
    value.is_empty()
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct Tool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionDefinition,
}
#[derive(Serialize)]
struct FunctionDefinition {
    name: String,
    description: String,
    parameters: Value,
    strict: bool,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ToolChoiceWire {
    Mode(&'static str),
    Function {
        #[serde(rename = "type")]
        kind: &'static str,
        function: FunctionName,
    },
}
#[derive(Serialize)]
struct FunctionName {
    name: String,
}

#[derive(Serialize)]
struct Message {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reasoning_details: Vec<Box<RawValue>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCallWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// A plain string for text-only messages; ordered parts once an image is present.
#[derive(Serialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrlWire },
}

#[derive(Serialize)]
struct ImageUrlWire {
    url: String,
}

fn image_part(image: &plakous_model::ImageBlock) -> Result<ContentPart, ChatRequestError> {
    use base64::Engine as _;
    let source = image.source();
    let url = if let Some(bytes) = source.as_bytes() {
        format!(
            "data:{};base64,{}",
            image.media_type(),
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    } else {
        source
            .as_data_url()
            .or_else(|| source.as_uri())
            .ok_or_else(|| unsupported("image source is outside Chat V1"))?
            .to_owned()
    };
    Ok(ContentPart::ImageUrl {
        image_url: ImageUrlWire { url },
    })
}

/// User content: the existing text-only string form, or ordered parts when
/// any image is present.
fn user_content(blocks: &[ContentBlock]) -> Result<MessageContent, ChatRequestError> {
    if !blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)))
    {
        return only_text(blocks).map(MessageContent::Text);
    }
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => Ok(ContentPart::Text {
                text: text.text().to_owned(),
            }),
            ContentBlock::Image(image) => image_part(image),
            _ => Err(unsupported("Chat user messages carry only text and images")),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(MessageContent::Parts)
}

#[derive(Serialize)]
struct ToolCallWire {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionCallWire,
}
#[derive(Serialize)]
struct FunctionCallWire {
    name: String,
    arguments: String,
}

/// Lowers one already validated canonical plan into bounded Chat JSON.
#[allow(
    clippy::too_many_lines,
    reason = "the complete immutable request and its operation digest stay visibly adjacent"
)]
pub fn lower_request(
    plan: &ValidatedRequestPlan,
    policy: &ChatRequestPolicy,
) -> Result<LoweredChatRequest, ChatRequestError> {
    let request = plan.request();
    if plan.binding_digest() != policy.descriptor_binding
        || request.selected_provider_configuration_id() != policy.selected
        || request.effective_provider_configuration_id() != policy.effective
    {
        return Err(invalid(
            "Chat request authority does not match the admitted target",
        ));
    }
    if !request.native().entries().is_empty() {
        return Err(unsupported("native options are outside Chat V1"));
    }
    if !request.hosted_tools().is_empty() {
        return Err(unsupported("hosted tools are outside Chat V1"));
    }
    if !matches!(request.output().format(), OutputFormat::Text) {
        return Err(unsupported("structured output is outside Chat V1"));
    }
    if !matches!(request.cache(), CachePolicy::Disabled) {
        return Err(unsupported("managed cache is outside stateless Chat V1"));
    }
    if request.reasoning() != policy.reasoning {
        return Err(invalid(
            "canonical reasoning policy does not match the admitted Chat configuration",
        ));
    }
    let reasoning = lower_reasoning(policy.reasoning, policy.disabled_reasoning_wire)?;

    let mut tools = lower_tools(plan)?;
    let omit_tools =
        matches!(request.tool_choice(), ToolChoice::None) && policy.tool_choice_none_omits_tools;
    let wire_tools = if omit_tools {
        Vec::new()
    } else {
        std::mem::take(&mut tools.wire)
    };
    let has_tools = !wire_tools.is_empty();
    let tool_choice = if omit_tools {
        None
    } else {
        match request.tool_choice() {
            ToolChoice::Auto if policy.tool_choice_auto_uses_default => None,
            ToolChoice::Auto => has_tools.then_some(ToolChoiceWire::Mode("auto")),
            ToolChoice::None if has_tools => Some(ToolChoiceWire::Mode("none")),
            ToolChoice::None => None,
            ToolChoice::Required if has_tools => Some(ToolChoiceWire::Mode("required")),
            ToolChoice::Specific(name) if tools.exposed_names.contains_key(name) => {
                Some(ToolChoiceWire::Function {
                    kind: "function",
                    function: FunctionName {
                        name: tools.exposed_names[name].clone(),
                    },
                })
            }
            ToolChoice::Required | ToolChoice::Specific(_) => {
                return Err(invalid(
                    "Chat tool choice does not name an exposed function",
                ));
            }
            _ => return Err(unsupported("unknown tool choice is outside Chat V1")),
        }
    };

    let mut messages = Vec::new();
    for instruction in request.instructions() {
        let role = match instruction.source() {
            InstructionSource::System | InstructionSource::Profile | InstructionSource::Harness => {
                "system"
            }
            InstructionSource::Developer => "developer",
            _ => return Err(unsupported("unknown instruction source is outside Chat V1")),
        };
        messages.push(text_message(role, instruction.text().to_owned()));
    }
    messages.extend(lower_history(plan, policy, &tools)?);
    if messages.is_empty() {
        return Err(invalid("Chat request must contain at least one message"));
    }

    let maximum = request
        .limits()
        .max_output_tokens()
        .map_or(policy.maximum_output_tokens, std::num::NonZeroU32::get);
    if maximum > policy.maximum_output_tokens {
        return Err(invalid(
            "requested output bound exceeds the admitted maximum",
        ));
    }
    let body = serde_json::to_vec(&RequestBody {
        model: policy.requested_model.as_str(),
        messages,
        tools: wire_tools,
        tool_choice,
        max_tokens: matches!(policy.output_limit_field, ChatOutputLimitField::MaxTokens)
            .then_some(maximum),
        max_completion_tokens: matches!(
            policy.output_limit_field,
            ChatOutputLimitField::MaxCompletionTokens
        )
        .then_some(maximum),
        stop: request.limits().stop_sequences(),
        provider: policy.router.as_ref().map(|router| RouterWire {
            only: router.endpoint.as_deref().map(|endpoint| [endpoint]),
            order: router.endpoint.as_deref().map(|endpoint| [endpoint]),
            allow_fallbacks: false,
            require_parameters: true,
            data_collection: router.data_collection.as_deref(),
            zdr: router.zdr,
        }),
        reasoning,
        plugins: policy.plugins,
        session_id: policy.gateway_session_key.as_deref(),
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
    })
    .map_err(|_| invalid("Chat request serialization failed"))?;
    let operation_digest = Digest::sha256(
        DigestDomain::ProviderOperation,
        1,
        &[
            b"plakous-chat-completions/request-v1",
            &plan.digest().identity_bytes(),
            &policy.account_binding.identity_bytes(),
            &CHAT_REPLAY_SCHEMA_REVISION.to_be_bytes(),
            &body,
        ],
    )
    .map_err(|_| invalid("Chat operation identity failed"))?;
    Ok(LoweredChatRequest {
        replay_safety: classify_replay_safety(&body),
        body,
        operation_digest,
    })
}

/// Top-level Chat fields whose replay can only cost duplicate inference.
const COST_ONLY_FIELDS: &[&str] = &[
    "max_completion_tokens",
    "max_tokens",
    "messages",
    "model",
    "plugins",
    "provider",
    "reasoning",
    "session_id",
    "stop",
    "stream",
    "stream_options",
    "temperature",
    "tool_choice",
    "tools",
    "top_p",
];

/// Classifies the exact serialized body by allowlist: only `function` and
/// `custom` tools, disabled gateway plugins, and the fields above are
/// cost-only. Any other tool type, an enabled plugin, or any field nobody
/// classified is a server effect.
fn classify_replay_safety(body: &[u8]) -> ReplaySafety {
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(body) else {
        return ReplaySafety::ServerEffect;
    };
    let pure = fields.iter().all(|(name, value)| match name.as_str() {
        "tools" => value.as_array().is_some_and(|tools| {
            tools.iter().all(|tool| {
                matches!(
                    tool.get("type").and_then(Value::as_str),
                    Some("function" | "custom")
                )
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

fn lower_reasoning(
    policy: ReasoningPolicy,
    disabled_wire: ChatDisabledReasoningWire,
) -> Result<Option<ReasoningWire>, ChatRequestError> {
    if let ReasoningPolicy::Enabled { effort, .. } = policy {
        let effort = match effort {
            Some(ReasoningEffort::Low) => "low",
            Some(ReasoningEffort::Medium) => "medium",
            Some(ReasoningEffort::High) => "high",
            Some(ReasoningEffort::XHigh) => "xhigh",
            Some(ReasoningEffort::Max) => "max",
            Some(ReasoningEffort::Ultra) => "ultra",
            Some(ReasoningEffort::None) => "none",
            Some(ReasoningEffort::TokenBudget(_)) => {
                return Err(unsupported(
                    "token-budget reasoning has no admitted Chat wire representation",
                ));
            }
            None => {
                return Err(invalid(
                    "enabled Chat reasoning requires one exact admitted effort",
                ));
            }
            _ => return Err(unsupported("unknown reasoning effort is outside Chat V1")),
        };
        return Ok(Some(ReasoningWire::Effort {
            effort,
            exclude: false,
        }));
    }
    if matches!(policy, ReasoningPolicy::Disabled) {
        return match disabled_wire {
            ChatDisabledReasoningWire::Omit => Ok(None),
            ChatDisabledReasoningWire::EnabledFalse => {
                Ok(Some(ReasoningWire::Disabled { enabled: false }))
            }
        };
    }
    Err(unsupported("unknown reasoning policy is outside Chat V1"))
}

struct LoweredTools {
    wire: Vec<Tool>,
    /// Canonical name to its one Chat exposure, for a specific tool choice
    /// only. A canonical tool presented under several names (pi's `edit` and
    /// `write` over one patch tool) is absent: a choice cannot name it.
    exposed_names: BTreeMap<ToolName, String>,
    /// Every exposed function name. History replays a call by the exact
    /// exposed name it recorded, never through the canonical map.
    exposures: BTreeSet<String>,
}

fn lower_tools(plan: &ValidatedRequestPlan) -> Result<LoweredTools, ChatRequestError> {
    let request = plan.request();
    if let Some(projection) = request.tool_projection() {
        let mut exposed_names = BTreeMap::new();
        let mut ambiguous = BTreeSet::new();
        let mut exposures = BTreeSet::new();
        let wire = projection
            .definitions()
            .iter()
            .map(|entry| {
                if !is_chat_function_name(entry.exposed_name().as_str()) {
                    return Err(invalid(
                        "projected tool name is outside Chat function-name grammar",
                    ));
                }
                let schema = entry
                    .input_schema()
                    .ok_or_else(|| unsupported("free-form tools are outside Chat V1"))?;
                if !exposures.insert(entry.exposed_name().as_str().to_owned()) {
                    return Err(invalid("one Chat function name is exposed twice"));
                }
                for canonical in entry.covered_canonical_names() {
                    if exposed_names
                        .insert(canonical.clone(), entry.exposed_name().as_str().to_owned())
                        .is_some()
                    {
                        ambiguous.insert(canonical.clone());
                    }
                }
                Ok(Tool {
                    kind: "function",
                    function: FunctionDefinition {
                        name: entry.exposed_name().as_str().to_owned(),
                        description: entry.description().to_owned(),
                        parameters: schema.wire_document(),
                        strict: chat_function_strict(schema.document().value().as_ref()),
                    },
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        exposed_names.retain(|canonical, _| !ambiguous.contains(canonical));
        return Ok(LoweredTools {
            wire,
            exposed_names,
            exposures,
        });
    }
    let mut exposed_names = BTreeMap::new();
    let mut exposures = BTreeSet::new();
    let wire = request
        .tools()
        .iter()
        .map(|configured| {
            let definition = configured.definition();
            if !is_chat_function_name(definition.name().as_str()) {
                return Err(invalid(
                    "configured tool name is outside Chat function-name grammar",
                ));
            }
            let ToolInputContract::Structured(schema) = definition.input() else {
                return Err(unsupported("free-form tools are outside Chat V1"));
            };
            exposed_names.insert(
                definition.name().clone(),
                definition.name().as_str().to_owned(),
            );
            exposures.insert(definition.name().as_str().to_owned());
            Ok(Tool {
                kind: "function",
                function: FunctionDefinition {
                    name: definition.name().as_str().to_owned(),
                    description: definition.description().to_owned(),
                    parameters: schema.wire_document(),
                    strict: chat_function_strict(schema.document().value().as_ref()),
                },
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LoweredTools {
        wire,
        exposed_names,
        exposures,
    })
}

/// OpenAI `strict` functions require `required` to list every `properties` key.
fn chat_function_strict(parameters: &Value) -> bool {
    let Some(properties) = parameters.get("properties").and_then(Value::as_object) else {
        return false;
    };
    let Some(required) = parameters.get("required").and_then(Value::as_array) else {
        return properties.is_empty();
    };
    properties.keys().all(|key| {
        required
            .iter()
            .any(|entry| entry.as_str() == Some(key.as_str()))
    })
}

fn is_chat_function_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Chat has no owner on the wire: a Plak-local and a Plak MCP tool are both
/// ordinary client functions.
const fn runtime_owned(owner: &ToolExecutionOwner) -> bool {
    matches!(
        owner,
        ToolExecutionOwner::Local | ToolExecutionOwner::Mcp { .. }
    )
}

fn text_message(role: &'static str, content: String) -> Message {
    content_message(role, MessageContent::Text(content))
}

fn content_message(role: &'static str, content: MessageContent) -> Message {
    Message {
        role,
        content: Some(content),
        reasoning: None,
        reasoning_details: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
    }
}

#[derive(Clone)]
struct PendingTool {
    call_id: CallId,
    correlation: ToolCorrelation,
    remote: Option<String>,
    inert_name: Option<String>,
}

#[allow(
    clippy::too_many_lines,
    reason = "the ordered actor state machine keeps adjacency and correlation checks visible"
)]
fn lower_history(
    plan: &ValidatedRequestPlan,
    policy: &ChatRequestPolicy,
    tools: &LoweredTools,
) -> Result<Vec<Message>, ChatRequestError> {
    let items = plan.request().input().items();
    let mut messages = Vec::new();
    let mut pending = VecDeque::<PendingTool>::new();
    let mut pending_updates = BTreeMap::<CallId, String>::new();
    // Tool messages are text-only on Chat: a round's result images follow its
    // tool messages as one synthetic user message, derived here and never stored.
    let mut round_images = Vec::<ContentPart>::new();
    let mut call_ids = BTreeSet::new();
    let mut remote_ids = BTreeSet::new();
    for item in items {
        match item.actor() {
            Actor::User => {
                if !pending.is_empty() {
                    return Err(invalid("user input intervened before all tool results"));
                }
                messages.push(content_message("user", user_content(item.blocks())?));
            }
            Actor::System | Actor::Developer | Actor::Harness => {
                if !pending.is_empty() {
                    return Err(invalid("instruction intervened before all tool results"));
                }
                let role = if item.actor() == Actor::System {
                    "system"
                } else {
                    "developer"
                };
                messages.push(text_message(role, only_text(item.blocks())?));
            }
            Actor::Assistant => {
                let extending = !pending.is_empty();
                let mut content = String::new();
                let mut reasoning = String::new();
                let mut reasoning_details = Vec::new();
                let mut next_reasoning_detail_ordinal = 0_u32;
                let mut bound_reasoning_alias = None;
                let mut calls = Vec::new();
                let mut omitted_opaque = false;
                for block in item.blocks() {
                    match block {
                        ContentBlock::Text(text) => content.push_str(text.text()),
                        ContentBlock::Reasoning(value) => {
                            if matches!(
                                value.visibility(),
                                ReasoningVisibility::Visible | ReasoningVisibility::Summary
                            ) {
                                reasoning.push_str(
                                    value
                                        .text()
                                        .ok_or_else(|| invalid("visible reasoning lacks text"))?,
                                );
                            }
                            if let Some(replay) = value.replay() {
                                if policy.reasoning_replay != ChatReasoningReplay::ExactDetails
                                    || replay.protocol() != &policy.protocol
                                    || replay.model_family().is_some()
                                    || replay.scope()
                                        != &ReplayScope::ExactModel(policy.requested_model.clone())
                                    || replay.kind().as_str() != REASONING_DETAIL_REPLAY_KIND
                                    || replay.version() != CHAT_REPLAY_SCHEMA_REVISION
                                    || replay.classification() != DataClassification::Sensitive
                                {
                                    return Err(invalid(
                                        "reasoning replay is incompatible with the admitted Chat configuration",
                                    ));
                                }
                                let bound: ChatReasoningDetailReplayV1 =
                                    serde_json::from_str(replay.payload().get()).map_err(|_| {
                                        invalid("reasoning replay binding is malformed")
                                    })?;
                                if bound.target != policy.replay_target()
                                    || bound.reasoning_policy != policy.reasoning
                                    || bound.alias_policy != policy.reasoning_alias
                                    || !replay_alias_matches(
                                        policy.reasoning_alias,
                                        bound.bound_alias,
                                    )
                                    || bound_reasoning_alias
                                        .is_some_and(|alias| alias != bound.bound_alias)
                                    || bound.source_ordinal != next_reasoning_detail_ordinal
                                    || bound.source_ordinal >= MAX_REASONING_DETAILS
                                    || bound.exact_json.is_empty()
                                    || bound.exact_json.len() > MAX_REASONING_DETAIL_BYTES
                                    || !SemanticJsonV1::parse(&bound.exact_json)
                                        .is_ok_and(|value| value.value().is_object())
                                {
                                    return Err(invalid(
                                        "reasoning replay does not match the exact Chat target",
                                    ));
                                }
                                next_reasoning_detail_ordinal = next_reasoning_detail_ordinal
                                    .checked_add(1)
                                    .ok_or_else(|| invalid("too many Chat reasoning details"))?;
                                bound_reasoning_alias = Some(bound.bound_alias);
                                reasoning_details.push(
                                    RawValue::from_string(bound.exact_json).map_err(|_| {
                                        invalid("reasoning replay exact JSON is malformed")
                                    })?,
                                );
                            }
                        }
                        ContentBlock::ToolCall(call) => {
                            if !runtime_owned(call.owner())
                                || call.state() == ToolCallState::Pending
                                || call.correlation().container_id().is_some()
                                || call.correlation().caller_lineage() != &ToolCallerLineage::Direct
                            {
                                return Err(unsupported(
                                    "only direct runtime-owned tools are inside Chat V1",
                                ));
                            }
                            let projected_exposure =
                                tools.exposures.get(call.exposed_name().as_str());
                            if !call_ids.insert(call.call_id()) {
                                return Err(invalid("Chat tool call IDs must be unique"));
                            }
                            let (remote, inert_name) = if let Some(exposed) = projected_exposure {
                                if !is_chat_function_name(exposed) {
                                    return Err(invalid(
                                        "historical tool exposure is outside Chat function-name grammar",
                                    ));
                                }
                                let arguments = lower_chat_tool_arguments(call.raw_arguments())?;
                                let remote = call
                                    .correlation()
                                    .remote_call_id()
                                    .ok_or_else(|| {
                                        invalid(
                                            "Chat tool replay requires the exact remote call ID",
                                        )
                                    })?
                                    .to_owned();
                                if !remote_ids.insert(remote.clone()) {
                                    return Err(invalid("Chat tool call IDs must be unique"));
                                }
                                calls.push(ToolCallWire {
                                    id: remote.clone(),
                                    kind: "function",
                                    function: FunctionCallWire {
                                        name: exposed.clone(),
                                        arguments,
                                    },
                                });
                                (Some(remote), None)
                            } else {
                                append_inert_history(
                                    &mut content,
                                    "call",
                                    call.exposed_name().as_str(),
                                    call.raw_arguments(),
                                )?;
                                (None, Some(call.exposed_name().as_str().to_owned()))
                            };
                            pending.push_back(PendingTool {
                                call_id: call.call_id(),
                                correlation: call.correlation().clone(),
                                remote,
                                inert_name,
                            });
                        }
                        ContentBlock::Opaque(opaque) => match opaque.fallback() {
                            OpaqueFallback::SemanticSummary(summary) => {
                                content.push_str(summary);
                            }
                            OpaqueFallback::Omit | OpaqueFallback::CompatibleReplay => {
                                omitted_opaque = true;
                            }
                            _ => {
                                return Err(unsupported(
                                    "assistant opaque fallback is outside Chat V1",
                                ));
                            }
                        },
                        ContentBlock::Citation(_) => {}
                        ContentBlock::Compaction(compaction) => {
                            if compaction.native_item().is_some() {
                                return Err(ChatRequestError {
                                    kind: ChatRequestErrorKind::ProtectedHistory,
                                    message: "native compaction cannot be replayed on Chat".into(),
                                });
                            }
                            content.push_str(compaction.summary());
                        }
                        ContentBlock::Refusal(refusal) => {
                            content.push_str(refusal.text());
                        }
                        _ => {
                            return Err(unsupported(
                                "multimodal and result blocks are outside assistant Chat V1",
                            ));
                        }
                    }
                }
                if content.is_empty()
                    && reasoning.is_empty()
                    && reasoning_details.is_empty()
                    && calls.is_empty()
                {
                    if omitted_opaque {
                        continue;
                    }
                    return Err(invalid("assistant Chat message is empty"));
                }
                if extending {
                    if !content.is_empty() || !reasoning.is_empty() || !reasoning_details.is_empty()
                    {
                        return Err(invalid(
                            "assistant output intervened before all tool results",
                        ));
                    }
                    match messages.last_mut() {
                        Some(message) if message.role == "assistant" => {
                            message.tool_calls.extend(calls);
                        }
                        _ => {
                            return Err(invalid(
                                "assistant output intervened before all tool results",
                            ));
                        }
                    }
                } else {
                    messages.push(Message {
                        role: "assistant",
                        content: (!content.is_empty()).then_some(MessageContent::Text(content)),
                        reasoning: (!reasoning.is_empty()).then_some(reasoning),
                        reasoning_details,
                        tool_calls: calls,
                        tool_call_id: None,
                    });
                }
            }
            Actor::Tool => {
                for block in item.blocks() {
                    if let ContentBlock::ToolResultUpdate(update) = block {
                        let expected = pending
                            .iter()
                            .find(|expected| expected.call_id == update.call_id())
                            .ok_or_else(|| {
                                invalid("tool-result update has no preceding assistant call")
                            })?;
                        if !runtime_owned(update.owner())
                            || update.correlation() != &expected.correlation
                        {
                            return Err(invalid(
                                "tool-result update correlation differs from the exact assistant call",
                            ));
                        }
                        pending_updates
                            .entry(update.call_id())
                            .or_default()
                            .push_str(update.text().text());
                        continue;
                    }
                    let ContentBlock::ToolResult(result) = block else {
                        return Err(unsupported(
                            "Chat tool messages admit only text result updates and terminal text results",
                        ));
                    };
                    let expected = pending
                        .pop_front()
                        .ok_or_else(|| invalid("tool result has no preceding assistant call"))?;
                    if result.call_id() != expected.call_id
                        || !runtime_owned(result.owner())
                        || result.correlation() != &expected.correlation
                    {
                        return Err(invalid(
                            "tool result order or correlation differs from the exact assistant call",
                        ));
                    }
                    let mut text = pending_updates
                        .remove(&result.call_id())
                        .unwrap_or_default();
                    for part in result.parts() {
                        match part {
                            ToolResultPart::Text(value) => text.push_str(value.text()),
                            ToolResultPart::Image(image) => {
                                round_images.push(ContentPart::Text {
                                    text: format!(
                                        "image from tool call {}:",
                                        expected.remote.as_deref().map_or_else(
                                            || expected.call_id.to_string(),
                                            ToOwned::to_owned
                                        )
                                    ),
                                });
                                round_images.push(image_part(image)?);
                            }
                            _ => {
                                return Err(unsupported(
                                    "tool results other than text and images are outside Chat V1",
                                ));
                            }
                        }
                    }
                    if result.kind() == ToolResultKind::Failure {
                        text = format!("error: {text}");
                    }
                    if let Some(name) = expected.inert_name {
                        let mut content = String::new();
                        append_inert_history(&mut content, "result", &name, &text)?;
                        messages.push(text_message("assistant", content));
                    } else {
                        messages.push(Message {
                            role: "tool",
                            content: Some(MessageContent::Text(text)),
                            reasoning: None,
                            reasoning_details: Vec::new(),
                            tool_calls: Vec::new(),
                            tool_call_id: expected.remote,
                        });
                    }
                    if pending.is_empty() && !round_images.is_empty() {
                        messages.push(content_message(
                            "user",
                            MessageContent::Parts(std::mem::take(&mut round_images)),
                        ));
                    }
                }
            }
            _ => return Err(unsupported("unknown canonical actor is outside Chat V1")),
        }
    }
    if !pending.is_empty() {
        return Err(invalid("history ends before all Chat tool results"));
    }
    if !pending_updates.is_empty() {
        return Err(invalid("tool-result update lacks a terminal result"));
    }
    Ok(messages)
}

fn replay_alias_matches(
    configured: crate::stream::ReasoningAliasPolicy,
    bound: crate::stream::ReasoningAliasPolicy,
) -> bool {
    (match (configured, bound) {
        (
            crate::stream::ReasoningAliasPolicy::EitherBindFirst,
            crate::stream::ReasoningAliasPolicy::Reasoning
            | crate::stream::ReasoningAliasPolicy::ReasoningContent,
        ) => true,
        (configured, bound) => configured == bound,
    }) && !matches!(bound, crate::stream::ReasoningAliasPolicy::EitherBindFirst)
}

/// A recorded call's arguments are bounded by the canonical tool-call block
/// (16 MiB), not the tool-schema limits, so replay never refuses a call the
/// stream accepted.
const MAX_CHAT_TOOL_ARGUMENT_BYTES: usize = 16 * 1024 * 1024;

fn lower_chat_tool_arguments(raw_arguments: &str) -> Result<String, ChatRequestError> {
    if SemanticJsonV1::parse_document(raw_arguments, MAX_CHAT_TOOL_ARGUMENT_BYTES)
        .is_ok_and(|value| value.is_object())
    {
        return Ok(raw_arguments.to_owned());
    }
    Err(invalid(
        "Chat structured tool arguments must be duplicate-free JSON objects",
    ))
}

fn append_inert_history(
    output: &mut String,
    kind: &'static str,
    name: &str,
    value: &str,
) -> Result<(), ChatRequestError> {
    if value.len() > MAX_INERT_HISTORICAL_TOOL_BYTES {
        return Err(invalid(
            "inert historical tool record exceeds the Chat projection bound",
        ));
    }
    let record = serde_json::to_string(&BTreeMap::from([
        ("kind", kind),
        ("name", name),
        ("value", value),
    ]))
    .map_err(|_| invalid("inert historical tool record could not be encoded"))?;
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str("[Historical tool record; unavailable in the current target] ");
    output.push_str(&record);
    Ok(())
}

fn only_text(blocks: &[ContentBlock]) -> Result<String, ChatRequestError> {
    let mut output = String::new();
    for block in blocks {
        let ContentBlock::Text(text) = block else {
            return Err(unsupported(
                "Chat user and instruction messages are text-only",
            ));
        };
        output.push_str(text.text());
    }
    if output.is_empty() {
        Err(invalid("Chat text message is empty"))
    } else {
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;

    use plakous_model::CapabilityRequirement;
    use plakous_model::{
        AttemptId, BranchId, Capability, CapabilityConstraints, CapabilitySet, ConfiguredTool,
        ConversationItem, ConversationView, DataClassification, EffectiveProviderConfigurationId,
        EntryId, FactProvenance, FactSource, FeatureSupport, InstructionBlock,
        ModelAdapterProfileId, ModelCacheAffinity, ModelFamilyId, ModelRequest, OpaqueBlock,
        Provenance, ReasoningEffort, ReasoningVisibility, ReplayEnvelope, ReplayKindId,
        ReplayScope, RequestMetadata, ResolvedModelDescriptor, RouteId, RunId, SessionId, Support,
        TextBlock, Timestamp, ToolCallerDisposition, ToolChoice, ToolConcurrency, ToolDefinition,
        ToolEffect, ToolExecutionDisposition, ToolOutputContract, ToolResultBlock,
        ToolResultUpdateBlock, TurnId, ValidationMode,
    };
    use uuid::Uuid;

    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    struct Fixture {
        selected: SelectedProviderConfigurationId,
        effective: EffectiveProviderConfigurationId,
        descriptor: ResolvedModelDescriptor,
        tools: Vec<ConfiguredTool>,
    }

    fn configured_tool(name: &str) -> TestResult<ConfiguredTool> {
        Ok(ConfiguredTool::new(
            ToolDefinition::new(
                ToolName::new(name)?,
                "1",
                format!("{name} tool"),
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
        ))
    }

    impl Fixture {
        fn new() -> TestResult<Self> {
            let selected = serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::SelectedProviderConfiguration,
                5,
                &[b"chat-request-selected"],
            )?)?)?;
            let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"chat-request-effective"],
            )?)?)?;
            let provenance = FactProvenance::new(
                FactSource::TestFixture,
                Timestamp::new(1_700_000_000, 0)?,
                None,
            );
            let features = [
                Capability::Streaming,
                Capability::Tools,
                Capability::Caching,
                Capability::Reasoning,
            ]
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
                ProtocolId::new("openrouter.chat-completions")?,
                RouteId::new("openrouter-chat-test")?,
                ModelId::new("model/exact")?,
                None,
                None,
                ModelAdapterProfileId::new("chat-test")?,
                CapabilitySet::new(features, BTreeMap::new()),
            );
            let tools = ["alpha", "beta"]
                .into_iter()
                .map(configured_tool)
                .collect::<TestResult<Vec<_>>>()?;
            Ok(Self {
                selected,
                effective,
                descriptor,
                tools,
            })
        }

        fn plan(&self, items: Vec<ConversationItem>) -> TestResult<ValidatedRequestPlan> {
            let request = ModelRequest::builder(
                self.selected,
                self.effective,
                ConversationView::from_items(items)?,
                RequestMetadata::for_turn(
                    RunId::try_from(Uuid::from_u128(1))?,
                    TurnId::try_from(Uuid::from_u128(2))?,
                    AttemptId::try_from(Uuid::from_u128(3))?,
                    Timestamp::new(1_700_000_001, 0)?,
                ),
            )
            .tools(self.tools.clone())
            .build()?;
            Ok(self
                .descriptor
                .validate_request(request, ValidationMode::Strict)?)
        }

        fn plan_with_instructions(
            &self,
            items: Vec<ConversationItem>,
            instructions: Vec<InstructionBlock>,
        ) -> TestResult<ValidatedRequestPlan> {
            let request = ModelRequest::builder(
                self.selected,
                self.effective,
                ConversationView::from_items(items)?,
                RequestMetadata::for_turn(
                    RunId::try_from(Uuid::from_u128(1))?,
                    TurnId::try_from(Uuid::from_u128(2))?,
                    AttemptId::try_from(Uuid::from_u128(3))?,
                    Timestamp::new(1_700_000_001, 0)?,
                ),
            )
            .tools(self.tools.clone())
            .instructions(instructions)
            .build()?;
            Ok(self
                .descriptor
                .validate_request(request, ValidationMode::Strict)?)
        }

        fn policy(&self, plan: &ValidatedRequestPlan) -> TestResult<ChatRequestPolicy> {
            Ok(ChatRequestPolicy::new(
                ProtocolId::new("openrouter.chat-completions")?,
                ModelId::new("model/exact")?,
                plan.binding_digest(),
                Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
                self.selected,
                self.effective,
                512,
                ChatOutputLimitField::MaxCompletionTokens,
                ChatReasoningReplay::VisibleOnly,
                None,
                ReasoningPolicy::Disabled,
                crate::stream::ReasoningAliasPolicy::Reasoning,
            )?)
        }

        fn exact_details_policy(
            &self,
            plan: &ValidatedRequestPlan,
        ) -> TestResult<ChatRequestPolicy> {
            Ok(ChatRequestPolicy::new(
                ProtocolId::new("openrouter.chat-completions")?,
                ModelId::new("model/exact")?,
                plan.binding_digest(),
                Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
                self.selected,
                self.effective,
                512,
                ChatOutputLimitField::MaxCompletionTokens,
                ChatReasoningReplay::ExactDetails,
                None,
                ReasoningPolicy::Disabled,
                crate::stream::ReasoningAliasPolicy::Reasoning,
            )?)
        }
    }

    fn reasoning_detail_replay(
        policy: &ChatRequestPolicy,
        exact_json: &str,
        ordinal: u32,
    ) -> TestResult<ContentBlock> {
        let bound = ChatReasoningDetailReplayV1::new(
            policy.replay_target(),
            policy.reasoning(),
            policy.reasoning_alias(),
            policy.reasoning_alias(),
            exact_json.to_owned(),
            ordinal,
        )?;
        let payload = serde_json::value::to_raw_value(&bound)?;
        let envelope = ReplayEnvelope::new(
            policy.protocol().clone(),
            None,
            ReplayScope::ExactModel(ModelId::new("model/exact")?),
            ReplayKindId::new(REASONING_DETAIL_REPLAY_KIND)?,
            CHAT_REPLAY_SCHEMA_REVISION,
            DataClassification::Sensitive,
            payload,
        )?;
        Ok(ContentBlock::Reasoning(plakous_model::ReasoningBlock::new(
            ReasoningVisibility::OpaqueReplay,
            None,
            Some(envelope),
        )?))
    }

    fn detail_values(body: &Value, message_index: usize) -> Vec<Value> {
        body["messages"][message_index]["reasoning_details"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn seed_plan(fixture: &Fixture) -> TestResult<ValidatedRequestPlan> {
        fixture.plan(vec![item(
            100,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("seed", None)?)],
        )?])
    }

    fn item(id: u128, actor: Actor, blocks: Vec<ContentBlock>) -> TestResult<ConversationItem> {
        let provenance = match actor {
            Actor::User => Provenance::UserInput,
            Actor::Assistant => Provenance::ModelOutput,
            Actor::Tool => Provenance::ToolExecution,
            _ => Provenance::Instruction,
        };
        Ok(ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(id))?,
            actor,
            blocks,
            provenance,
        )?)
    }

    fn call(
        id: u128,
        name: &str,
        remote: &str,
    ) -> TestResult<(CallId, ToolCorrelation, ContentBlock)> {
        call_with_state(id, name, remote, ToolCallState::Requested)
    }

    fn call_with_state(
        id: u128,
        name: &str,
        remote: &str,
        state: ToolCallState,
    ) -> TestResult<(CallId, ToolCorrelation, ContentBlock)> {
        call_with_raw_state(id, name, remote, state, "{}")
    }

    fn call_with_raw_state(
        id: u128,
        name: &str,
        remote: &str,
        state: ToolCallState,
        raw_arguments: &str,
    ) -> TestResult<(CallId, ToolCorrelation, ContentBlock)> {
        let call_id = CallId::try_from(Uuid::from_u128(id))?;
        let correlation =
            ToolCorrelation::new(Some(remote.to_owned()), None, ToolCallerLineage::Direct)?;
        let block = ContentBlock::ToolCall(plakous_model::ToolCallBlock::new(
            call_id,
            ToolName::new(name)?,
            ToolExecutionOwner::Local,
            state,
            correlation.clone(),
            raw_arguments,
        )?);
        Ok((call_id, correlation, block))
    }

    fn result(
        call_id: CallId,
        correlation: ToolCorrelation,
        text: &str,
    ) -> TestResult<ContentBlock> {
        Ok(ContentBlock::ToolResult(ToolResultBlock::new(
            call_id,
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TextBlock::new(text, None)?)],
            correlation,
        )?))
    }

    fn result_update(
        call_id: CallId,
        correlation: ToolCorrelation,
        text: &str,
    ) -> TestResult<ContentBlock> {
        Ok(ContentBlock::ToolResultUpdate(ToolResultUpdateBlock::new(
            call_id,
            ToolExecutionOwner::Local,
            TextBlock::new(text, None)?,
            correlation,
        )?))
    }

    fn foreign_responses_opaque(fallback: OpaqueFallback) -> TestResult<ContentBlock> {
        let payload = serde_json::value::to_raw_value("encrypted-response-item")?;
        let replay = ReplayEnvelope::new(
            ProtocolId::new("openai.codex.responses")?,
            Some(ModelFamilyId::new("gpt")?),
            ReplayScope::ExactModel(ModelId::new("gpt-5.6-sol")?),
            ReplayKindId::new("responses.output_item")?,
            1,
            DataClassification::Sensitive,
            payload,
        )?;
        Ok(ContentBlock::Opaque(OpaqueBlock::new(replay, fallback)?))
    }

    #[test]
    fn seeded_content_filter_guidance_lowers_on_chat() -> TestResult {
        let instruction =
            "<content_filter_guidance>\nOffer a permitted alternative.\n</content_filter_guidance>";
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![
            item(
                100,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new(
                    "original request",
                    None,
                )?)],
            )?,
            ConversationItem::new(
                EntryId::try_from(Uuid::from_u128(101))?,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new(instruction, None)?)],
                Provenance::Instruction,
            )?,
        ])?;
        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], instruction);
        Ok(())
    }

    #[test]
    fn profile_and_harness_instructions_lower_as_system_developer_stays_developer() -> TestResult {
        let fixture = Fixture::new()?;
        let plan = fixture.plan_with_instructions(
            vec![item(
                10,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
            )?],
            vec![
                InstructionBlock::new(InstructionSource::Profile, "profile text")?,
                InstructionBlock::new(InstructionSource::Harness, "harness text")?,
                InstructionBlock::new(InstructionSource::Developer, "developer text")?,
                InstructionBlock::new(InstructionSource::System, "system text")?,
            ],
        )?;
        let body: Value =
            serde_json::from_slice(lower_request(&plan, &fixture.policy(&plan)?)?.body())?;
        assert_eq!(
            body["messages"][0],
            serde_json::json!({"role": "system", "content": "profile text"})
        );
        assert_eq!(
            body["messages"][1],
            serde_json::json!({"role": "system", "content": "harness text"})
        );
        assert_eq!(
            body["messages"][2],
            serde_json::json!({"role": "developer", "content": "developer text"})
        );
        assert_eq!(
            body["messages"][3],
            serde_json::json!({"role": "system", "content": "system text"})
        );
        assert_eq!(body["messages"][4]["role"], "user");
        Ok(())
    }

    #[test]
    fn lowers_stateless_text_and_local_structured_tools_deterministically() -> TestResult {
        let fixture = Fixture::new()?;
        let (call_id, correlation, call) = call(11, "alpha", "remote-alpha")?;
        let plan = fixture.plan(vec![
            item(
                10,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
            )?,
            item(
                11,
                Actor::Assistant,
                vec![
                    ContentBlock::Reasoning(plakous_model::ReasoningBlock::new(
                        ReasoningVisibility::Visible,
                        Some("because".to_owned()),
                        None,
                    )?),
                    call,
                ],
            )?,
            item(12, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
        ])?;
        let first = lower_request(&plan, &fixture.policy(&plan)?)?;
        let second = lower_request(&plan, &fixture.policy(&plan)?)?;
        assert_eq!(first.body(), second.body());
        assert_eq!(first.operation_digest(), second.operation_digest());
        let body: Value = serde_json::from_slice(first.body())?;
        assert_eq!(body["model"], "model/exact");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert_eq!(body["messages"][1]["reasoning"], "because");
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "remote-alpha");
        assert_eq!(body["messages"][2]["tool_call_id"], "remote-alpha");
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_completion_tokens"], 512);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("artifact").is_none());
        assert!(body.get("previous_response_id").is_none());
        Ok(())
    }

    #[test]
    fn skips_foreign_opaque_only_assistant_items() -> TestResult {
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![
            item(
                30,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("before", None)?)],
            )?,
            item(
                31,
                Actor::Assistant,
                vec![foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?],
            )?,
            item(
                32,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("after", None)?)],
            )?,
        ])?;

        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(
            body["messages"],
            serde_json::json!([
                {"role": "user", "content": "before"},
                {"role": "user", "content": "after"}
            ])
        );
        Ok(())
    }

    #[test]
    fn native_compaction_refuses_before_chat_body_is_built() -> TestResult {
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![item(
            41,
            Actor::Assistant,
            vec![ContentBlock::Compaction(
                plakous_model::CompactionBlock::new("opaque-ciphertext")?.with_native_item(
                    r#"{"type":"compaction","encrypted_content":"opaque-ciphertext"}"#.into(),
                ),
            )],
        )?])?;
        let error = lower_request(&plan, &fixture.policy(&plan)?)
            .err()
            .ok_or("Chat must not send native ciphertext as text")?;
        assert_eq!(error.kind(), ChatRequestErrorKind::ProtectedHistory);
        assert!(!error.message().contains("opaque-ciphertext"));
        Ok(())
    }

    #[test]
    fn compact_history_replays_compaction_markers_as_assistant_text() -> TestResult {
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![
            item(
                40,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("before", None)?)],
            )?,
            item(
                41,
                Actor::Assistant,
                vec![ContentBlock::Compaction(
                    plakous_model::CompactionBlock::new("earlier turns")?,
                )],
            )?,
            item(
                42,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("summarize", None)?)],
            )?,
        ])?;
        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][1]["content"], "earlier turns");
        assert_eq!(body["messages"][2]["content"], "summarize");
        Ok(())
    }

    #[test]
    fn preserves_text_beside_omitted_foreign_opaque_blocks() -> TestResult {
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![
            item(
                33,
                Actor::Assistant,
                vec![
                    ContentBlock::Text(TextBlock::new("compatible", None)?),
                    foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?,
                ],
            )?,
            item(
                34,
                Actor::Assistant,
                vec![
                    ContentBlock::Text(TextBlock::new("omitted", None)?),
                    foreign_responses_opaque(OpaqueFallback::Omit)?,
                ],
            )?,
        ])?;

        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(body["messages"][0]["content"], "compatible");
        assert_eq!(body["messages"][1]["content"], "omitted");
        Ok(())
    }

    #[test]
    fn replays_signed_reasoning_details_before_tool_results_byte_identically() -> TestResult {
        let fixture = Fixture::new()?;
        let policy = fixture.exact_details_policy(&seed_plan(&fixture)?)?;
        let first_detail = r#"{"type":"reasoning.text","text":"plan the lookup","signature":"sig-one","id":"r1","format":"anthropic-claude-v1","index":0}"#;
        let second_detail = r#"{"type":"reasoning.text","text":"follow up thought","signature":"sig-two","id":"r2","format":"anthropic-claude-v1","index":0}"#;
        let (call_id, correlation, call) = call(50, "alpha", "remote-signed-replay")?;
        let plan = fixture.plan(vec![
            item(
                50,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("run tool", None)?)],
            )?,
            item(
                51,
                Actor::Assistant,
                vec![reasoning_detail_replay(&policy, first_detail, 0)?, call],
            )?,
            item(52, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
            item(
                53,
                Actor::Assistant,
                vec![reasoning_detail_replay(&policy, second_detail, 0)?],
            )?,
        ])?;
        let lowered = lower_request(&plan, &policy)?;
        let body: Value = serde_json::from_slice(lowered.body())?;

        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(
            detail_values(&body, 1),
            vec![serde_json::from_str::<Value>(first_detail)?]
        );
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["id"],
            "remote-signed-replay"
        );
        assert_eq!(body["messages"][2]["tool_call_id"], "remote-signed-replay");
        assert_eq!(body["messages"][3]["role"], "assistant");
        assert_eq!(
            detail_values(&body, 3),
            vec![serde_json::from_str::<Value>(second_detail)?]
        );
        assert!(body["messages"][3].get("tool_calls").is_none());
        Ok(())
    }

    #[test]
    fn accepts_assistant_turn_with_empty_reasoning_details() -> TestResult {
        let fixture = Fixture::new()?;
        let (call_id, correlation, call) = call(60, "alpha", "remote-empty-details")?;
        let plan = fixture.plan(vec![
            item(60, Actor::Assistant, vec![call])?,
            item(61, Actor::Tool, vec![result(call_id, correlation, "ok")?])?,
        ])?;
        let policy = fixture.exact_details_policy(&plan)?;
        let lowered = lower_request(&plan, &policy)?;
        let body: Value = serde_json::from_slice(lowered.body())?;

        assert_eq!(body["messages"][0]["role"], "assistant");
        assert!(body["messages"][0].get("reasoning_details").is_none());
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["id"],
            "remote-empty-details"
        );
        Ok(())
    }

    #[test]
    fn drops_unsigned_reasoning_text_under_exact_details_while_preserving_encrypted() -> TestResult
    {
        let fixture = Fixture::new()?;
        let policy = fixture.exact_details_policy(&seed_plan(&fixture)?)?;
        let encrypted = r#"{"type":"reasoning.encrypted","data":"opaque-payload","id":"e1","format":"some-upstream","index":0}"#;
        let unsigned_visible = "unsigned visible reasoning";
        let (call_id, correlation, call_block) = call(70, "alpha", "remote-mixed-reasoning")?;

        let exact_plan = fixture.plan(vec![
            item(
                70,
                Actor::Assistant,
                vec![reasoning_detail_replay(&policy, encrypted, 0)?, call_block],
            )?,
            item(
                71,
                Actor::Tool,
                vec![result(call_id, correlation.clone(), "ok")?],
            )?,
        ])?;
        let exact_body: Value =
            serde_json::from_slice(lower_request(&exact_plan, &policy)?.body())?;
        assert_eq!(
            detail_values(&exact_body, 0),
            vec![serde_json::from_str::<Value>(encrypted)?]
        );
        assert!(exact_body["messages"][0].get("reasoning").is_none());

        let (visible_call_id, visible_correlation, visible_call_block) =
            call(72, "alpha", "remote-visible-only")?;
        let visible_plan = fixture.plan(vec![
            item(
                72,
                Actor::Assistant,
                vec![
                    ContentBlock::Reasoning(plakous_model::ReasoningBlock::new(
                        ReasoningVisibility::Visible,
                        Some(unsigned_visible.to_owned()),
                        None,
                    )?),
                    visible_call_block,
                ],
            )?,
            item(
                73,
                Actor::Tool,
                vec![result(visible_call_id, visible_correlation, "ok")?],
            )?,
        ])?;
        let visible_policy = fixture.policy(&visible_plan)?;
        let visible_body: Value =
            serde_json::from_slice(lower_request(&visible_plan, &visible_policy)?.body())?;
        assert_eq!(visible_body["messages"][0]["reasoning"], unsigned_visible);
        assert!(
            visible_body["messages"][0]
                .get("reasoning_details")
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn preserves_mixed_reasoning_details_order_and_encrypted_data_exactly() -> TestResult {
        let fixture = Fixture::new()?;
        let policy = fixture.exact_details_policy(&seed_plan(&fixture)?)?;
        let signed_text = r#"{"type":"reasoning.text","text":"step-a","signature":"sig-a","id":"r1","format":"anthropic-claude-v1","index":0}"#;
        let encrypted = r#"{"type":"reasoning.encrypted","data":"A\\u003dB","id":"r2","format":"anthropic-claude-v1","index":1}"#;
        let summary = r#"{"type":"reasoning.summary","text":"condensed","id":"r3","index":2}"#;
        let (call_id, correlation, call) = call(80, "alpha", "remote-mixed-details")?;
        let plan = fixture.plan(vec![
            item(
                80,
                Actor::Assistant,
                vec![
                    reasoning_detail_replay(&policy, signed_text, 0)?,
                    reasoning_detail_replay(&policy, encrypted, 1)?,
                    reasoning_detail_replay(&policy, summary, 2)?,
                    call,
                ],
            )?,
            item(81, Actor::Tool, vec![result(call_id, correlation, "ok")?])?,
        ])?;
        let lowered = lower_request(&plan, &policy)?;
        let body: Value = serde_json::from_slice(lowered.body())?;

        let expected = [
            serde_json::from_str::<Value>(signed_text)?,
            serde_json::from_str::<Value>(encrypted)?,
            serde_json::from_str::<Value>(summary)?,
        ];
        assert_eq!(detail_values(&body, 0), expected);
        assert_eq!(
            detail_values(&body, 0)[1]["data"],
            serde_json::from_str::<Value>(encrypted)?["data"]
        );
        Ok(())
    }

    #[test]
    fn projects_foreign_opaque_semantic_summary_as_assistant_text() -> TestResult {
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![item(
            35,
            Actor::Assistant,
            vec![foreign_responses_opaque(OpaqueFallback::SemanticSummary(
                "portable summary".to_owned(),
            ))?],
        )?])?;

        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert_eq!(body["messages"][0]["content"], "portable summary");
        Ok(())
    }

    #[test]
    fn lowers_responses_tool_exchange_with_foreign_opaque_sibling() -> TestResult {
        let fixture = Fixture::new()?;
        let (call_id, correlation, call) = call(36, "alpha", "responses-call-alpha")?;
        let plan = fixture.plan(vec![
            item(
                36,
                Actor::Assistant,
                vec![
                    call,
                    foreign_responses_opaque(OpaqueFallback::CompatibleReplay)?,
                ],
            )?,
            item(
                37,
                Actor::Tool,
                vec![
                    result_update(call_id, correlation.clone(), "working\n")?,
                    result(call_id, correlation, "done")?,
                ],
            )?,
        ])?;

        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["id"],
            "responses-call-alpha"
        );
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"]["name"],
            "alpha"
        );
        assert_eq!(body["messages"][1]["tool_call_id"], "responses-call-alpha");
        assert_eq!(body["messages"][1]["content"], "working\ndone");
        Ok(())
    }

    #[test]
    fn a_run_of_tool_call_items_folds_into_one_assistant_message() -> TestResult {
        let fixture = Fixture::new()?;
        let (alpha_id, alpha_correlation, alpha) = call(30, "alpha", "remote-alpha")?;
        let (beta_id, beta_correlation, beta) = call(31, "beta", "remote-beta")?;
        let plan = fixture.plan(vec![
            item(30, Actor::Assistant, vec![alpha])?,
            item(31, Actor::Assistant, vec![beta])?,
            item(
                32,
                Actor::Tool,
                vec![
                    result(alpha_id, alpha_correlation, "a")?,
                    result(beta_id, beta_correlation, "b")?,
                ],
            )?,
        ])?;
        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        let messages = body["messages"].as_array().ok_or("messages")?;
        assert_eq!(messages.len(), 3, "{body}");
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "remote-alpha");
        assert_eq!(messages[0]["tool_calls"][1]["id"], "remote-beta");
        assert_eq!(messages[1]["tool_call_id"], "remote-alpha");
        assert_eq!(messages[2]["tool_call_id"], "remote-beta");
        Ok(())
    }

    #[test]
    fn text_inside_an_open_tool_batch_is_refused() -> TestResult {
        let fixture = Fixture::new()?;
        let (alpha_id, alpha_correlation, alpha) = call(33, "alpha", "remote-alpha")?;
        let (beta_id, beta_correlation, beta) = call(34, "beta", "remote-beta")?;
        let plan = fixture.plan(vec![
            item(33, Actor::Assistant, vec![alpha])?,
            item(
                34,
                Actor::Assistant,
                vec![ContentBlock::Text(TextBlock::new("more", None)?), beta],
            )?,
            item(
                35,
                Actor::Tool,
                vec![
                    result(alpha_id, alpha_correlation, "a")?,
                    result(beta_id, beta_correlation, "b")?,
                ],
            )?,
        ])?;
        let Err(error) = lower_request(&plan, &fixture.policy(&plan)?) else {
            return Err("text folded behind an open tool call".into());
        };
        assert!(
            error.message().contains("intervened"),
            "{}",
            error.message()
        );
        Ok(())
    }

    #[test]
    fn folds_canonical_tool_result_updates_into_the_terminal_chat_message() -> TestResult {
        let fixture = Fixture::new()?;
        let (call_id, correlation, call) = call(13, "alpha", "remote-alpha")?;
        let plan = fixture.plan(vec![
            item(13, Actor::Assistant, vec![call])?,
            item(
                14,
                Actor::Tool,
                vec![
                    result_update(call_id, correlation.clone(), "working\n")?,
                    result(call_id, correlation, "done")?,
                ],
            )?,
        ])?;

        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(body["messages"][1]["role"], "tool");
        assert_eq!(body["messages"][1]["tool_call_id"], "remote-alpha");
        assert_eq!(body["messages"][1]["content"], "working\ndone");
        Ok(())
    }

    #[test]
    fn mcp_owned_tool_exchange_lowers_exactly_like_a_local_one() -> TestResult {
        let fixture = Fixture::new()?;
        let lower = |owner: ToolExecutionOwner| -> TestResult<Value> {
            let (call_id, correlation, call) = call(13, "alpha", "remote-alpha")?;
            let ContentBlock::ToolCall(call) = call else {
                return Err("fixture call is a tool call".into());
            };
            let call = plakous_model::ToolCallBlock::new(
                call_id,
                call.exposed_name().clone(),
                owner.clone(),
                call.state(),
                correlation.clone(),
                call.raw_arguments(),
            )?;
            let plan = fixture.plan(vec![
                item(13, Actor::Assistant, vec![ContentBlock::ToolCall(call)])?,
                item(
                    14,
                    Actor::Tool,
                    vec![
                        ContentBlock::ToolResultUpdate(ToolResultUpdateBlock::new(
                            call_id,
                            owner.clone(),
                            TextBlock::new("working\n", None)?,
                            correlation.clone(),
                        )?),
                        ContentBlock::ToolResult(ToolResultBlock::new(
                            call_id,
                            owner,
                            ToolResultKind::Success,
                            vec![ToolResultPart::Text(TextBlock::new("done", None)?)],
                            correlation,
                        )?),
                    ],
                )?,
            ])?;
            let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
            Ok(serde_json::from_slice(lowered.body())?)
        };
        let mcp = ToolExecutionOwner::Mcp {
            server: plakous_model::McpServerId::new("github")?,
        };
        assert_eq!(lower(mcp)?, lower(ToolExecutionOwner::Local)?);
        Ok(())
    }

    #[test]
    fn replays_provider_visible_tool_names_from_projected_history() -> TestResult {
        let fixture = Fixture::new()?;
        let (call_id, correlation, call) = call(15, "exec_command", "remote-exec")?;
        let plan = fixture.plan(vec![
            item(15, Actor::Assistant, vec![call])?,
            item(16, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
        ])?;
        let tools = LoweredTools {
            wire: Vec::new(),
            exposed_names: BTreeMap::from([
                (ToolName::new("alpha")?, "exec_command".to_owned()),
                (ToolName::new("beta")?, "read_file".to_owned()),
            ]),
            exposures: BTreeSet::from(["exec_command".to_owned(), "read_file".to_owned()]),
        };

        let messages = lower_history(&plan, &fixture.policy(&plan)?, &tools)?;
        assert_eq!(messages[0].tool_calls[0].function.name, "exec_command");
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("remote-exec"));
        Ok(())
    }

    /// Two exposures over one canonical tool (pi's `edit` and `write` over the
    /// patch tool) leave the canonical name unmapped, and each call recorded
    /// under its exposed name still replays as that function.
    #[test]
    fn replays_each_exposure_of_a_canonical_tool_presented_twice() -> TestResult {
        let fixture = Fixture::new()?;
        let (call_id, correlation, call) = call(15, "write", "remote-write")?;
        let plan = fixture.plan(vec![
            item(15, Actor::Assistant, vec![call])?,
            item(16, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
        ])?;
        let tools = LoweredTools {
            wire: Vec::new(),
            exposed_names: BTreeMap::new(),
            exposures: BTreeSet::from(["edit".to_owned(), "write".to_owned()]),
        };

        let messages = lower_history(&plan, &fixture.policy(&plan)?, &tools)?;
        assert_eq!(messages[0].tool_calls[0].function.name, "write");
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("remote-write"));
        Ok(())
    }

    /// A call records the canonical name it resolved to and the exact name
    /// the model called; replay sends the exposed name, never the canonical
    /// map's exposure of the canonical name.
    #[test]
    fn replays_the_recorded_exposed_name_not_the_canonical_mapping() -> TestResult {
        let fixture = Fixture::new()?;
        let (call_id, correlation, call) = call(15, "write", "remote-write")?;
        let ContentBlock::ToolCall(call) = call else {
            return Err("fixture call is not a tool call".into());
        };
        let call = ContentBlock::ToolCall(call.with_canonical_name(ToolName::new("apply_patch")?));
        let plan = fixture.plan(vec![
            item(15, Actor::Assistant, vec![call])?,
            item(16, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
        ])?;
        let tools = LoweredTools {
            wire: Vec::new(),
            exposed_names: BTreeMap::from([(ToolName::new("apply_patch")?, "edit".to_owned())]),
            exposures: BTreeSet::from(["edit".to_owned(), "write".to_owned()]),
        };

        let messages = lower_history(&plan, &fixture.policy(&plan)?, &tools)?;
        assert_eq!(messages[0].tool_calls[0].function.name, "write");
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("remote-write"));
        Ok(())
    }

    fn png_image() -> TestResult<plakous_model::ImageBlock> {
        Ok(plakous_model::ImageBlock::new(
            "image/png",
            plakous_model::MediaSource::bytes(b"\x89PNG".to_vec())?,
        )?)
    }

    #[test]
    fn user_images_lower_to_ordered_image_url_parts() -> TestResult {
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![item(
            15,
            Actor::User,
            vec![
                ContentBlock::Text(TextBlock::new("what is this", None)?),
                ContentBlock::Image(png_image()?),
            ],
        )?])?;
        let tools = LoweredTools {
            wire: Vec::new(),
            exposed_names: BTreeMap::new(),
            exposures: BTreeSet::new(),
        };
        let messages = lower_history(&plan, &fixture.policy(&plan)?, &tools)?;
        assert_eq!(
            serde_json::to_value(&messages[0])?,
            serde_json::json!({"role": "user", "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw=="}}
            ]})
        );
        Ok(())
    }

    #[test]
    fn tool_result_images_follow_the_round_as_one_synthetic_user_message() -> TestResult {
        let fixture = Fixture::new()?;
        let (call_id, correlation, call) = call(15, "inspect_image", "remote-inspect")?;
        let result = ContentBlock::ToolResult(ToolResultBlock::new(
            call_id,
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![
                ToolResultPart::Text(TextBlock::new("media_type=image/png", None)?),
                ToolResultPart::Image(png_image()?),
            ],
            correlation,
        )?);
        let plan = fixture.plan(vec![
            item(15, Actor::Assistant, vec![call])?,
            item(16, Actor::Tool, vec![result])?,
        ])?;
        let tools = LoweredTools {
            wire: Vec::new(),
            exposed_names: BTreeMap::new(),
            exposures: BTreeSet::from(["inspect_image".to_owned()]),
        };
        let lowered = |plan| -> TestResult<Value> {
            Ok(serde_json::to_value(lower_history(
                plan,
                &fixture.policy(plan)?,
                &tools,
            )?)?)
        };
        let messages = lowered(&plan)?;
        assert_eq!(
            messages[1],
            serde_json::json!({"role": "tool", "content": "media_type=image/png", "tool_call_id": "remote-inspect"})
        );
        assert_eq!(
            messages[2],
            serde_json::json!({"role": "user", "content": [
                {"type": "text", "text": "image from tool call remote-inspect:"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw=="}}
            ]})
        );
        assert_eq!(messages.as_array().map(Vec::len), Some(3));
        assert_eq!(lowered(&plan)?, messages);
        Ok(())
    }

    #[test]
    fn projects_unexposed_historical_exchange_as_inert_assistant_text() -> TestResult {
        let fixture = Fixture::new()?;
        let historical_name = "retired-tool_012345678901234567890123456789012345678901234567890";
        assert_eq!(historical_name.len(), 64);
        let (call_id, correlation, call) = call_with_state(
            17,
            historical_name,
            "remote-retired",
            ToolCallState::Completed,
        )?;
        let plan = fixture.plan(vec![
            item(17, Actor::Assistant, vec![call])?,
            item(18, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
        ])?;

        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert!(body["messages"][0].get("tool_calls").is_none());
        assert!(body["messages"][0]["content"].as_str().is_some_and(|text| {
            text.contains(historical_name) && text.contains("\"kind\":\"call\"")
        }));
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert!(body["messages"][1].get("tool_call_id").is_none());
        assert!(
            body["messages"][1]["content"]
                .as_str()
                .is_some_and(|text| text.contains("\"kind\":\"result\"") && text.contains("done"))
        );
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        assert_eq!(body["tools"][0]["function"]["name"], "alpha");
        assert_eq!(body["tools"][1]["function"]["name"], "beta");
        Ok(())
    }

    #[test]
    fn preserves_exact_raw_completed_freeform_in_inert_history() -> TestResult {
        let fixture = Fixture::new()?;
        let raw = "const value = `quoted \\\"line\\\"`;\ntext(\"λ\\\\tail\");";
        let (call_id, correlation, call) = call_with_raw_state(
            19,
            "retired_exec",
            "remote-freeform",
            ToolCallState::Completed,
            raw,
        )?;
        let plan = fixture.plan(vec![
            item(19, Actor::Assistant, vec![call])?,
            item(20, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
        ])?;

        let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        let content = body["messages"][0]["content"]
            .as_str()
            .ok_or("historical record is absent")?;
        let record: Value = serde_json::from_str(
            content
                .strip_prefix("[Historical tool record; unavailable in the current target] ")
                .ok_or("historical record prefix is absent")?,
        )?;
        assert_eq!(record["value"], raw);
        assert_eq!(record["name"], "retired_exec");
        assert!(body["tools"].as_array().is_some_and(|tools| {
            tools
                .iter()
                .all(|tool| tool["function"]["name"] != "retired_exec")
        }));
        Ok(())
    }

    #[test]
    fn preserves_responses_shaped_requested_freeform_as_inert_history() -> TestResult {
        let fixture = Fixture::new()?;
        let raw = r#""const value = await tools.exec_command({cmd: 'pwd'});\ntext(value);""#;
        let (call_id, correlation, call) = call_with_raw_state(
            21,
            "exec",
            "remote-requested-freeform",
            ToolCallState::Requested,
            raw,
        )?;
        let complete = fixture.plan(vec![
            item(21, Actor::Assistant, vec![call])?,
            item(22, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
        ])?;
        let lowered = lower_request(&complete, &fixture.policy(&complete)?)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        let content = body["messages"][0]["content"]
            .as_str()
            .ok_or("historical record is absent")?;
        let record: Value = serde_json::from_str(
            content
                .strip_prefix("[Historical tool record; unavailable in the current target] ")
                .ok_or("historical record prefix is absent")?,
        )?;
        assert_eq!(record["value"], raw);
        assert_eq!(record["name"], "exec");
        Ok(())
    }

    #[test]
    fn inert_history_is_bounded_and_never_rewrites_current_exposure() -> TestResult {
        let fixture = Fixture::new()?;
        let (call_id, correlation, call) = call_with_raw_state(
            40,
            "alpha",
            "remote-bound",
            ToolCallState::Completed,
            "freeform",
        )?;
        let plan = fixture.plan(vec![
            item(40, Actor::Assistant, vec![call])?,
            item(41, Actor::Tool, vec![result(call_id, correlation, "done")?])?,
        ])?;
        let Err(error) = lower_request(&plan, &fixture.policy(&plan)?) else {
            return Err("current free-form tool history was silently rewritten".into());
        };
        assert_eq!(error.kind(), ChatRequestErrorKind::InvalidRequest);
        assert_eq!(
            error.message(),
            "Chat structured tool arguments must be duplicate-free JSON objects"
        );

        let mut record = String::new();
        append_inert_history(
            &mut record,
            "call",
            "retired",
            &"x".repeat(MAX_INERT_HISTORICAL_TOOL_BYTES),
        )?;
        assert!(record.contains("Historical tool record"));
        let Err(error) = append_inert_history(
            &mut String::new(),
            "call",
            "retired",
            &"x".repeat(MAX_INERT_HISTORICAL_TOOL_BYTES + 1),
        ) else {
            return Err("oversized inert historical record was accepted".into());
        };
        assert_eq!(error.kind(), ChatRequestErrorKind::InvalidRequest);
        Ok(())
    }

    #[test]
    fn unexposed_historical_names_never_become_chat_functions() -> TestResult {
        let fixture = Fixture::new()?;
        for (ordinal, historical_name) in [
            "retired.tool".to_owned(),
            "retired/tool".to_owned(),
            "retired:tool".to_owned(),
            "a".repeat(65),
        ]
        .into_iter()
        .enumerate()
        {
            let id = 20 + ordinal as u128 * 2;
            let (call_id, correlation, call) = call_with_state(
                id,
                &historical_name,
                &format!("remote-invalid-{ordinal}"),
                ToolCallState::Completed,
            )?;
            let plan = fixture.plan(vec![
                item(id, Actor::Assistant, vec![call])?,
                item(
                    id + 1,
                    Actor::Tool,
                    vec![result(call_id, correlation, "done")?],
                )?,
            ])?;

            let lowered = lower_request(&plan, &fixture.policy(&plan)?)?;
            let body: Value = serde_json::from_slice(lowered.body())?;
            assert!(body["messages"][0].get("tool_calls").is_none());
            assert!(
                body["messages"][0]["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(&historical_name))
            );
        }
        Ok(())
    }

    #[test]
    fn chat_function_name_grammar_is_exact() {
        assert!(is_chat_function_name("a"));
        assert!(is_chat_function_name("Az09_-"));
        assert!(is_chat_function_name(&"a".repeat(64)));
        assert!(!is_chat_function_name(""));
        assert!(!is_chat_function_name(&"a".repeat(65)));
        assert!(!is_chat_function_name("retired.tool"));
        assert!(!is_chat_function_name("tool/name"));
        assert!(!is_chat_function_name("tool:name"));
        assert!(!is_chat_function_name("tool name"));
        assert!(!is_chat_function_name("tööl"));
    }

    #[test]
    fn rejects_current_tool_names_outside_chat_function_grammar() -> TestResult {
        for invalid in [
            "tool.name".to_owned(),
            "tool/name".to_owned(),
            "a".repeat(65),
        ] {
            let mut fixture = Fixture::new()?;
            fixture.tools = vec![configured_tool(&invalid)?];
            let plan = fixture.plan(vec![item(
                14,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("inspect", None)?)],
            )?])?;
            let Err(error) = lower_request(&plan, &fixture.policy(&plan)?) else {
                return Err("invalid current Chat function name was accepted".into());
            };
            assert_eq!(error.kind(), ChatRequestErrorKind::InvalidRequest);
            assert_eq!(
                error.message(),
                "configured tool name is outside Chat function-name grammar"
            );
        }
        Ok(())
    }

    #[test]
    fn strict_gateway_managed_and_omitted_router_shapes_are_exact() -> TestResult {
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![item(
            15,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("route", None)?)],
        )?])?;
        let managed = lower_request(&plan, &fixture.policy(&plan)?)?;
        let managed_body: Value = serde_json::from_slice(managed.body())?;
        assert!(managed_body.get("provider").is_none());

        let mut legacy_limit_policy = fixture.policy(&plan)?;
        legacy_limit_policy.output_limit_field = ChatOutputLimitField::MaxTokens;
        let legacy_limit = lower_request(&plan, &legacy_limit_policy)?;
        let legacy_limit_body: Value = serde_json::from_slice(legacy_limit.body())?;
        assert_eq!(legacy_limit_body["max_tokens"], 512);
        assert!(legacy_limit_body.get("max_completion_tokens").is_none());
        assert_ne!(managed.operation_digest(), legacy_limit.operation_digest());

        let mut gateway_policy = fixture.policy(&plan)?;
        gateway_policy.router = Some(ChatRouterPolicy::gateway_managed(
            Some("deny".to_owned()),
            Some(true),
        )?);
        let gateway = lower_request(&plan, &gateway_policy)?;
        let gateway_body: Value = serde_json::from_slice(gateway.body())?;
        assert_eq!(
            gateway_body["provider"],
            serde_json::json!({
                "allow_fallbacks": false,
                "require_parameters": true,
                "data_collection": "deny",
                "zdr": true
            })
        );
        assert!(gateway_body["provider"].get("only").is_none());
        assert!(gateway_body["provider"].get("order").is_none());

        let mut strict_policy = fixture.policy(&plan)?;
        strict_policy.router = Some(ChatRouterPolicy::strict("endpoint/exact", "deny", true)?);
        let strict = lower_request(&plan, &strict_policy)?;
        let strict_body: Value = serde_json::from_slice(strict.body())?;
        assert_eq!(
            strict_body["provider"],
            serde_json::json!({
                "only": ["endpoint/exact"],
                "order": ["endpoint/exact"],
                "allow_fallbacks": false,
                "require_parameters": true,
                "data_collection": "deny",
                "zdr": true
            })
        );
        assert_ne!(managed.operation_digest(), strict.operation_digest());
        assert_ne!(managed.operation_digest(), gateway.operation_digest());
        assert!(ChatRouterPolicy::strict("router", "deny", true).is_err());
        assert!(ChatRouterPolicy::strict("endpoint/exact", "DENY", true).is_err());
        assert!(ChatRouterPolicy::gateway_managed(Some("DENY".to_owned()), Some(true)).is_err());
        Ok(())
    }

    #[test]
    fn openrouter_context_compression_plugin_is_disabled() -> TestResult {
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![item(
            16,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("hello", None)?)],
        )?])?;
        let policy = fixture
            .policy(&plan)?
            .with_plugins(OPENROUTER_DISABLE_CONTEXT_COMPRESSION_PLUGINS);
        let lowered = lower_request(&plan, &policy)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(
            body["plugins"],
            serde_json::json!([{ "id": "context-compression", "enabled": false }])
        );
        Ok(())
    }

    #[test]
    fn gateway_managed_requires_parameters_without_tools_or_reasoning() -> TestResult {
        let mut fixture = Fixture::new()?;
        fixture.tools.clear();
        let plan = fixture.plan(vec![item(
            16,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("route", None)?)],
        )?])?;
        let mut policy = fixture.policy(&plan)?;
        policy.router = Some(ChatRouterPolicy::gateway_managed(None, None)?);

        let lowered = lower_request(&plan, &policy)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(
            body["provider"],
            serde_json::json!({
                "allow_fallbacks": false,
                "require_parameters": true
            })
        );
        assert!(body.get("tools").is_none());
        assert!(body.get("reasoning").is_none());
        Ok(())
    }

    #[test]
    fn reasoning_is_exact_explicit_and_fails_closed() -> TestResult {
        use plakous_model::CapabilityRequirement;

        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![item(
            16,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("reason", None)?)],
        )?])?;
        let mut mismatched = fixture.policy(&plan)?;
        mismatched.reasoning = ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::High),
            requirement: CapabilityRequirement::Required,
        };
        let Err(error) = lower_request(&plan, &mismatched) else {
            return Err("reasoning mismatch was accepted".into());
        };
        assert_eq!(error.kind(), ChatRequestErrorKind::InvalidRequest);
        assert!(error.message().contains("does not match"));

        let efforts = [
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::XHigh, "xhigh"),
            (ReasoningEffort::Max, "max"),
            (ReasoningEffort::Ultra, "ultra"),
        ];
        for (effort, spelling) in efforts {
            let wire = lower_reasoning(
                ReasoningPolicy::Enabled {
                    effort: Some(effort),
                    requirement: CapabilityRequirement::Required,
                },
                ChatDisabledReasoningWire::Omit,
            )?
            .ok_or("enabled reasoning was omitted")?;
            let value = serde_json::to_value(wire)?;
            assert_eq!(
                value,
                serde_json::json!({"effort": spelling, "exclude": false})
            );
        }
        let budget =
            ReasoningEffort::TokenBudget(NonZeroU32::new(128).ok_or("nonzero reasoning budget")?);
        let Err(error) = lower_reasoning(
            ReasoningPolicy::Enabled {
                effort: Some(budget),
                requirement: CapabilityRequirement::Required,
            },
            ChatDisabledReasoningWire::Omit,
        ) else {
            return Err("token-budget reasoning was accepted".into());
        };
        assert_eq!(error.kind(), ChatRequestErrorKind::UnsupportedSemantic);
        Ok(())
    }

    #[test]
    fn rejects_out_of_order_tool_results_even_when_all_ids_exist() -> TestResult {
        let fixture = Fixture::new()?;
        let (alpha_id, alpha_correlation, alpha) = call(21, "alpha", "remote-alpha")?;
        let (beta_id, beta_correlation, beta) = call(22, "beta", "remote-beta")?;
        let plan = fixture.plan(vec![
            item(20, Actor::Assistant, vec![alpha, beta])?,
            item(
                21,
                Actor::Tool,
                vec![result(beta_id, beta_correlation, "beta")?],
            )?,
            item(
                22,
                Actor::Tool,
                vec![result(alpha_id, alpha_correlation, "alpha")?],
            )?,
        ])?;
        let Err(error) = lower_request(&plan, &fixture.policy(&plan)?) else {
            return Err("swapped results were accepted".into());
        };
        assert_eq!(error.kind(), ChatRequestErrorKind::InvalidRequest);
        assert!(error.message().contains("order or correlation"));
        Ok(())
    }

    #[test]
    fn gateway_session_key_emits_session_id_without_prompt_cache_key_or_cache_control() -> TestResult
    {
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![item(
            40,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("sticky", None)?)],
        )?])?;
        let mut policy = fixture
            .policy(&plan)?
            .with_gateway_session_key("affinity-hex");
        policy.router = Some(ChatRouterPolicy::gateway_managed(None, None)?);
        let lowered = lower_request(&plan, &policy)?;
        let body: Value = serde_json::from_slice(lowered.body())?;
        assert_eq!(body["session_id"], "affinity-hex");
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("cache_control").is_none());
        assert_eq!(body["provider"]["require_parameters"], true);
        Ok(())
    }

    #[test]
    fn forked_gateway_session_key_does_not_reuse_source_session_id() -> TestResult {
        let session = SessionId::try_from(Uuid::from_u128(91_001))?;
        let source = ModelCacheAffinity::from_runtime_entropy(
            session,
            BranchId::try_from(Uuid::from_u128(91_002))?,
            0,
            [0x41; 32],
        )?;
        let fork = ModelCacheAffinity::from_runtime_entropy(
            session,
            BranchId::try_from(Uuid::from_u128(91_003))?,
            0,
            [0x42; 32],
        )?;
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![item(
            43,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("fork", None)?)],
        )?])?;
        let mut source_policy = fixture
            .policy(&plan)?
            .with_gateway_session_key(source.identity().to_string());
        source_policy.router = Some(ChatRouterPolicy::gateway_managed(None, None)?);
        let mut fork_policy = fixture
            .policy(&plan)?
            .with_gateway_session_key(fork.identity().to_string());
        fork_policy.router = Some(ChatRouterPolicy::gateway_managed(None, None)?);
        let source_body: Value =
            serde_json::from_slice(lower_request(&plan, &source_policy)?.body())?;
        let fork_body: Value = serde_json::from_slice(lower_request(&plan, &fork_policy)?.body())?;
        assert_eq!(source_body["session_id"], source.identity().to_string());
        assert_eq!(fork_body["session_id"], fork.identity().to_string());
        assert_ne!(source_body["session_id"], fork_body["session_id"]);
        assert!(source_body.get("prompt_cache_key").is_none());
        assert!(fork_body.get("prompt_cache_key").is_none());
        assert!(source_body.get("cache_control").is_none());
        assert!(fork_body.get("cache_control").is_none());
        Ok(())
    }

    #[test]
    fn chat_function_strict_requires_every_property() {
        assert!(chat_function_strict(&serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        })));
        assert!(!chat_function_strict(&serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {"type": "string"},
                "file_path": {"type": "string"}
            },
            "required": []
        })));
    }

    #[test]
    fn union_function_schema_lowers_strict_false_complete_schema_strict_true() -> TestResult {
        fn tool_with_schema(name: &str, schema: Value) -> TestResult<ConfiguredTool> {
            Ok(ConfiguredTool::new(
                ToolDefinition::new(
                    ToolName::new(name)?,
                    "1",
                    format!("{name} tool"),
                    ToolInputContract::structured(schema)?,
                    ToolOutputContract::utf8(1024)?,
                    ToolConcurrency::Parallel,
                    ToolEffect::ReadOnly,
                )?,
                ToolExecutionDisposition::Local,
                ToolCallerDisposition::DirectOnly,
            ))
        }
        let mut fixture = Fixture::new()?;
        fixture.tools = vec![
            tool_with_schema(
                "apply_patch",
                serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {
                        "patch": {"type": "string"},
                        "file_path": {"type": "string"}
                    },
                    "required": [],
                    "additionalProperties": false
                }),
            )?,
            tool_with_schema(
                "read_file",
                serde_json::json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"}
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            )?,
        ];
        let plan = fixture.plan(vec![item(
            42,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("patch", None)?)],
        )?])?;
        let body: Value =
            serde_json::from_slice(lower_request(&plan, &fixture.policy(&plan)?)?.body())?;
        assert_eq!(body["tools"][0]["function"]["name"], "apply_patch");
        assert_eq!(body["tools"][0]["function"]["strict"], false);
        assert_eq!(body["tools"][1]["function"]["name"], "read_file");
        assert_eq!(body["tools"][1]["function"]["strict"], true);
        Ok(())
    }

    #[test]
    fn tool_choice_none_legacy_shape_when_omit_knob_off() -> TestResult {
        let fixture = Fixture::new()?;
        let request = ModelRequest::builder(
            fixture.selected,
            fixture.effective,
            ConversationView::from_items(vec![item(
                41,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("compact", None)?)],
            )?])?,
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(1))?,
                TurnId::try_from(Uuid::from_u128(2))?,
                AttemptId::try_from(Uuid::from_u128(3))?,
                Timestamp::new(1_700_000_001, 0)?,
            ),
        )
        .tools(fixture.tools.clone())
        .tool_choice(ToolChoice::None)
        .build()?;
        let plan = fixture
            .descriptor
            .validate_request(request, ValidationMode::Strict)?;
        let body: Value =
            serde_json::from_slice(lower_request(&plan, &fixture.policy(&plan)?)?.body())?;
        assert_eq!(body["tool_choice"], "none");
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        Ok(())
    }

    #[test]
    fn tool_choice_none_omits_tools_when_omit_knob_on() -> TestResult {
        let fixture = Fixture::new()?;
        let request = ModelRequest::builder(
            fixture.selected,
            fixture.effective,
            ConversationView::from_items(vec![item(
                41,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("compact", None)?)],
            )?])?,
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(1))?,
                TurnId::try_from(Uuid::from_u128(2))?,
                AttemptId::try_from(Uuid::from_u128(3))?,
                Timestamp::new(1_700_000_001, 0)?,
            ),
        )
        .tools(fixture.tools.clone())
        .tool_choice(ToolChoice::None)
        .build()?;
        let plan = fixture
            .descriptor
            .validate_request(request, ValidationMode::Strict)?;
        let policy = fixture
            .policy(&plan)?
            .with_tool_choice_none_omits_tools(true);
        let body: Value = serde_json::from_slice(lower_request(&plan, &policy)?.body())?;
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        Ok(())
    }

    #[test]
    fn tool_choice_auto_omission_is_dialect_controlled() -> TestResult {
        let fixture = Fixture::new()?;
        let plan = fixture.plan(vec![item(
            41,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("code", None)?)],
        )?])?;
        let body: Value =
            serde_json::from_slice(lower_request(&plan, &fixture.policy(&plan)?)?.body())?;
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        let policy = fixture
            .policy(&plan)?
            .with_tool_choice_auto_uses_default(true);
        let body: Value = serde_json::from_slice(lower_request(&plan, &policy)?.body())?;
        assert!(body.get("tool_choice").is_none());
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        Ok(())
    }

    #[test]
    fn automatic_default_policy_preserves_explicit_choices() -> TestResult {
        let fixture = Fixture::new()?;
        for (choice, expected) in [
            (ToolChoice::None, serde_json::json!("none")),
            (ToolChoice::Required, serde_json::json!("required")),
            (
                ToolChoice::Specific(ToolName::new("alpha")?),
                serde_json::json!({"type":"function","function":{"name":"alpha"}}),
            ),
        ] {
            let request = ModelRequest::builder(
                fixture.selected,
                fixture.effective,
                ConversationView::from_items(vec![item(
                    41,
                    Actor::User,
                    vec![ContentBlock::Text(TextBlock::new("code", None)?)],
                )?])?,
                RequestMetadata::for_turn(
                    RunId::try_from(Uuid::from_u128(1))?,
                    TurnId::try_from(Uuid::from_u128(2))?,
                    AttemptId::try_from(Uuid::from_u128(3))?,
                    Timestamp::new(1_700_000_001, 0)?,
                ),
            )
            .tools(fixture.tools.clone())
            .tool_choice(choice)
            .build()?;
            let plan = fixture
                .descriptor
                .validate_request(request, ValidationMode::Strict)?;
            let policy = fixture
                .policy(&plan)?
                .with_tool_choice_auto_uses_default(true);
            let body: Value = serde_json::from_slice(lower_request(&plan, &policy)?.body())?;
            assert_eq!(body["tool_choice"], expected);
            assert_eq!(body["tools"].as_array().map(Vec::len), Some(2));
        }
        Ok(())
    }

    #[test]
    fn disabled_reasoning_wire_matrix() -> TestResult {
        assert!(
            lower_reasoning(ReasoningPolicy::Disabled, ChatDisabledReasoningWire::Omit)?.is_none()
        );
        let disabled = lower_reasoning(
            ReasoningPolicy::Disabled,
            ChatDisabledReasoningWire::EnabledFalse,
        )?
        .ok_or("disabled reasoning wire was omitted")?;
        assert_eq!(
            serde_json::to_value(disabled)?,
            serde_json::json!({"enabled": false})
        );
        let enabled = lower_reasoning(
            ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::Medium),
                requirement: CapabilityRequirement::Required,
            },
            ChatDisabledReasoningWire::Omit,
        )?
        .ok_or("enabled reasoning wire was omitted")?;
        assert_eq!(
            serde_json::to_value(enabled)?,
            serde_json::json!({"effort": "medium", "exclude": false})
        );
        Ok(())
    }

    #[test]
    fn disabled_reasoning_wire_lower_request_body() -> TestResult {
        let fixture = Fixture::new()?;
        let request = ModelRequest::builder(
            fixture.selected,
            fixture.effective,
            ConversationView::from_items(vec![item(
                41,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("code", None)?)],
            )?])?,
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(1))?,
                TurnId::try_from(Uuid::from_u128(2))?,
                AttemptId::try_from(Uuid::from_u128(3))?,
                Timestamp::new(1_700_000_001, 0)?,
            ),
        )
        .reasoning(ReasoningPolicy::Disabled)
        .build()?;
        let plan = fixture
            .descriptor
            .validate_request(request, ValidationMode::Strict)?;
        let disabled_body: Value = serde_json::from_slice(
            lower_request(
                &plan,
                &fixture
                    .policy(&plan)?
                    .with_disabled_reasoning_wire(ChatDisabledReasoningWire::EnabledFalse),
            )?
            .body(),
        )?;
        assert_eq!(
            disabled_body["reasoning"],
            serde_json::json!({"enabled": false})
        );

        let enabled_request = ModelRequest::builder(
            fixture.selected,
            fixture.effective,
            ConversationView::from_items(vec![item(
                42,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("think", None)?)],
            )?])?,
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(1))?,
                TurnId::try_from(Uuid::from_u128(2))?,
                AttemptId::try_from(Uuid::from_u128(4))?,
                Timestamp::new(1_700_000_002, 0)?,
            ),
        )
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::High),
            requirement: CapabilityRequirement::Required,
        })
        .build()?;
        let enabled_plan = fixture
            .descriptor
            .validate_request(enabled_request, ValidationMode::Strict)?;
        let enabled_policy = ChatRequestPolicy::new(
            ProtocolId::new("openrouter.chat-completions")?,
            ModelId::new("model/exact")?,
            enabled_plan.binding_digest(),
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
            fixture.selected,
            fixture.effective,
            512,
            ChatOutputLimitField::MaxCompletionTokens,
            ChatReasoningReplay::VisibleOnly,
            None,
            ReasoningPolicy::Enabled {
                effort: Some(ReasoningEffort::High),
                requirement: CapabilityRequirement::Required,
            },
            crate::stream::ReasoningAliasPolicy::Reasoning,
        )?
        .with_disabled_reasoning_wire(ChatDisabledReasoningWire::EnabledFalse);
        let enabled_body: Value =
            serde_json::from_slice(lower_request(&enabled_plan, &enabled_policy)?.body())?;
        assert_eq!(
            enabled_body["reasoning"],
            serde_json::json!({"effort": "high", "exclude": false})
        );
        Ok(())
    }

    #[test]
    fn enabled_cache_policy_is_outside_stateless_chat_v1() -> TestResult {
        use plakous_model::CapabilityRequirement;

        let fixture = Fixture::new()?;
        let request = ModelRequest::builder(
            fixture.selected,
            fixture.effective,
            ConversationView::from_items(vec![item(
                41,
                Actor::User,
                vec![ContentBlock::Text(TextBlock::new("cached", None)?)],
            )?])?,
            RequestMetadata::for_turn(
                RunId::try_from(Uuid::from_u128(1))?,
                TurnId::try_from(Uuid::from_u128(2))?,
                AttemptId::try_from(Uuid::from_u128(3))?,
                Timestamp::new(1_700_000_001, 0)?,
            ),
        )
        .tools(fixture.tools.clone())
        .cache(CachePolicy::Enabled {
            requirement: CapabilityRequirement::Required,
        })
        .build()?;
        let plan = fixture
            .descriptor
            .validate_request(request, ValidationMode::Strict)?;
        let Err(error) = lower_request(&plan, &fixture.policy(&plan)?) else {
            return Err("managed cache was accepted by Chat V1".into());
        };
        assert_eq!(error.kind(), ChatRequestErrorKind::UnsupportedSemantic);
        assert!(error.message().contains("managed cache"));
        Ok(())
    }

    /// The main exchange classifies an enabled gateway plugin as a server
    /// effect; only the hosted-retrieval side request has its own allowlist.
    #[test]
    fn main_classification_keeps_an_enabled_web_plugin_a_server_effect() -> TestResult {
        let side = crate::ChatRetrievalRequest::new("model-literal", "rust").body()?;
        assert_eq!(classify_replay_safety(&side), ReplaySafety::ServerEffect);
        assert_eq!(
            classify_replay_safety(
                br#"{"model":"m","plugins":[{"id":"context-compression","enabled":false}]}"#
            ),
            ReplaySafety::CostOnly
        );
        Ok(())
    }
}
