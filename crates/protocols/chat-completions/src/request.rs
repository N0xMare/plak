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
    #[allow(
        clippy::too_many_arguments,
        reason = "complete credential-free wire policy"
    )]
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
mod tests;
