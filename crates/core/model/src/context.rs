//! Provider-neutral, descriptor-bound context capacity and admission.

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    ContextHistoryPlan, ContextPlannerDisposition, ContextPlannerDispositionKind, Digest,
    DigestDomain, DigestError, ModelId, ModelRequest, RouteId,
};

mod evidence;
mod measurement;

pub use evidence::{
    CapacityCertificate, CapacityRule, ContextContract, ContextEvidenceClass,
    ContextEvidenceSource, ContextNonGuaranteeDisposition, CountCertificateKind, EvidenceFreshness,
    IntegrationClass, MeasurementCertificate, SafeLimit, StandardIntegrationAuthority,
};
pub use measurement::{
    ContextAdmissionRequirement, ContextCapacity, ContextHeadroom, ContextMeasurement,
    ContextScope, ContextStatusKind, ContextUnavailable, CountBound, CountObservation,
    OutputReservation, QualifiedTokens,
};

mod status;
pub use status::{ContextAdmission, ContextStatus};

/// Conservative per-image default when a dialect has no tighter documented bound.
pub const CONSERVATIVE_IMAGE_MAX_TOKENS: u64 = 8192;

const MAX_CONTEXT_LABEL_BYTES: usize = 128;
const MAX_MEASUREMENT_CERTIFICATES: usize = 16;

/// One overrun on a specific capacity axis.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct LimitOverflow {
    required: u64,
    available: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitOverflowWire {
    required: u64,
    available: u64,
}

impl<'de> Deserialize<'de> for LimitOverflow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = LimitOverflowWire::deserialize(deserializer)?;
        if wire.required <= wire.available {
            return Err(serde::de::Error::custom(
                "context overflow must exceed available capacity",
            ));
        }
        Ok(Self {
            required: wire.required,
            available: wire.available,
        })
    }
}

impl LimitOverflow {
    /// Returns the safe upper bound that was required.
    #[must_use]
    pub const fn required(self) -> u64 {
        self.required
    }

    /// Returns the safe lower bound that was available.
    #[must_use]
    pub const fn available(self) -> u64 {
        self.available
    }
}

/// Typed context overflow across shared, input, and/or output axes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextOverflow {
    shared: Option<LimitOverflow>,
    input: Option<LimitOverflow>,
    output: Option<LimitOverflow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextOverflowWire {
    shared: Option<LimitOverflow>,
    input: Option<LimitOverflow>,
    output: Option<LimitOverflow>,
}

impl<'de> Deserialize<'de> for ContextOverflow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ContextOverflowWire::deserialize(deserializer)?;
        if wire.shared.is_none() && wire.input.is_none() && wire.output.is_none() {
            return Err(serde::de::Error::custom(
                "context overflow must name at least one capacity axis",
            ));
        }
        Ok(Self {
            shared: wire.shared,
            input: wire.input,
            output: wire.output,
        })
    }
}

impl ContextOverflow {
    /// Returns shared-window overflow when present.
    #[must_use]
    pub const fn shared(self) -> Option<LimitOverflow> {
        self.shared
    }

    /// Returns input-capacity overflow when present.
    #[must_use]
    pub const fn input(self) -> Option<LimitOverflow> {
        self.input
    }

    /// Returns output-capacity overflow when present.
    #[must_use]
    pub const fn output(self) -> Option<LimitOverflow> {
        self.output
    }
}

/// Byte composition of one exact lowered request body, by input part.
///
/// This is a measurement of the serialized wire body the harness is about to
/// send — the same bytes the admission count is derived from — split into the
/// parts an operator reasons about: the system prompt, the tool definitions,
/// the conversation messages, and replayed reasoning. Every figure is a byte
/// count read from the body except the supplied image-token bound. A product
/// consumer converts part bytes divided by four for occupancy display
/// (spec 02 §5.6). Admission still uses each vertical's own token bound.
///
/// The split is keyed on the three kernel body shapes (Responses
/// `instructions`/`tools`/`input`, Messages `system`/`tools`/`messages`, Chat
/// `messages` with a `system`/`developer` role) and names no provider. Bytes
/// the split does not attribute (model, options, metadata) are the difference
/// between `total_bytes` and the parts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextInputParts {
    total_bytes: u64,
    image_transport_bytes: u64,
    image_tokens: u64,
    system_bytes: u64,
    tool_definition_bytes: u64,
    tool_definition_count: u32,
    message_bytes: u64,
    reasoning_bytes: u64,
}

impl ContextInputParts {
    /// Measures one serialized request body. Returns `None` when the body is
    /// not a JSON object, so an unknown wire shape is reported as unmeasured
    /// rather than as a zero split.
    #[must_use]
    pub fn measure_wire(body: &[u8]) -> Option<Self> {
        let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
        let before = json_bytes(&value);
        let object = value.as_object_mut()?;
        for key in ["input", "messages"] {
            if let Some(field) = object.get_mut(key) {
                remove_image_payloads(field, Some(1)).ok()?;
            }
        }
        let image_transport_bytes = before.saturating_sub(json_bytes(&value));
        let object = value.as_object()?;
        let total_bytes = u64::try_from(body.len()).ok()?;
        let mut system_bytes = 0_u64;
        let mut message_bytes = 0_u64;
        let mut reasoning_bytes = 0_u64;
        // Responses `instructions` and Messages `system` are the whole system
        // prompt in one field.
        for key in ["instructions", "system"] {
            if let Some(field) = object.get(key) {
                system_bytes = system_bytes.saturating_add(json_bytes(field));
            }
        }
        let (tool_definition_bytes, tool_definition_count) = match object.get("tools") {
            Some(serde_json::Value::Array(tools)) => (
                json_bytes(object.get("tools")?),
                u32::try_from(tools.len()).unwrap_or(u32::MAX),
            ),
            Some(other) => (json_bytes(other), 0),
            None => (0, 0),
        };
        // Responses `input` items, or Messages/Chat `messages`.
        for key in ["input", "messages"] {
            let Some(serde_json::Value::Array(items)) = object.get(key) else {
                if let Some(other) = object.get(key) {
                    message_bytes = message_bytes.saturating_add(json_bytes(other));
                }
                continue;
            };
            for item in items {
                let bytes = json_bytes(item);
                let kind = item.get("type").and_then(serde_json::Value::as_str);
                let role = item.get("role").and_then(serde_json::Value::as_str);
                if matches!(kind, Some("reasoning" | "compaction")) {
                    reasoning_bytes = reasoning_bytes.saturating_add(bytes);
                } else if matches!(role, Some("system" | "developer")) {
                    system_bytes = system_bytes.saturating_add(bytes);
                } else {
                    // Thinking blocks and chat reasoning fields ride inside a
                    // message; they are moved from the message share to the
                    // reasoning share so neither is counted twice.
                    let inner = embedded_reasoning_bytes(item);
                    reasoning_bytes = reasoning_bytes.saturating_add(inner);
                    message_bytes = message_bytes.saturating_add(bytes.saturating_sub(inner));
                }
            }
        }
        Some(Self {
            total_bytes,
            image_transport_bytes,
            image_tokens: 0,
            system_bytes,
            tool_definition_bytes,
            tool_definition_count,
            message_bytes,
            reasoning_bytes,
        })
    }

    /// Returns the exact serialized body length.
    #[must_use]
    pub const fn total_bytes(self) -> u64 {
        self.total_bytes
    }
    /// Image transport bytes excluded from every text-token estimate.
    #[must_use]
    pub const fn image_transport_bytes(self) -> u64 {
        self.image_transport_bytes
    }

    /// The dialect-supplied image allowance for the measured input.
    #[must_use]
    pub const fn image_tokens(self) -> u64 {
        self.image_tokens
    }

    pub(crate) fn with_image_tokens(mut self, tokens: u64) -> Self {
        self.image_tokens = tokens;
        self
    }

    /// Returns the bytes of the system prompt (`instructions`, `system`, or
    /// system/developer-role messages).
    #[must_use]
    pub const fn system_bytes(self) -> u64 {
        self.system_bytes
    }
    /// Returns the bytes of the `tools` array.
    #[must_use]
    pub const fn tool_definition_bytes(self) -> u64 {
        self.tool_definition_bytes
    }
    /// Returns the number of entries in the `tools` array.
    #[must_use]
    pub const fn tool_definition_count(self) -> u32 {
        self.tool_definition_count
    }
    /// Returns the bytes of conversation items that are not system prompt or
    /// reasoning material.
    #[must_use]
    pub const fn message_bytes(self) -> u64 {
        self.message_bytes
    }
    /// Returns the bytes of replayed reasoning, encrypted reasoning, native
    /// compaction items, and thinking blocks.
    #[must_use]
    pub const fn reasoning_bytes(self) -> u64 {
        self.reasoning_bytes
    }

    /// Occupying tokens for chrome: each part's UTF-8 bytes divided by four
    /// (spec 02 §5.6). Envelope bytes are the unattributed remainder of the
    /// serialized body.
    #[must_use]
    pub const fn occupancy_tokens(self) -> u64 {
        const fn tokens(bytes: u64) -> u64 {
            bytes / 4
        }
        let attributed = self
            .system_bytes
            .saturating_add(self.tool_definition_bytes)
            .saturating_add(self.message_bytes)
            .saturating_add(self.reasoning_bytes);
        tokens(self.system_bytes)
            .saturating_add(tokens(self.tool_definition_bytes))
            .saturating_add(tokens(self.message_bytes))
            .saturating_add(tokens(self.reasoning_bytes))
            .saturating_add(tokens(
                self.total_bytes
                    .saturating_sub(self.image_transport_bytes)
                    .saturating_sub(attributed),
            ))
            .saturating_add(self.image_tokens)
    }
}

/// One lowered input item. Image transport bytes never enter its bound.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextWireItem {
    digest: String,
    bytes: u64,
    image_tokens: u64,
    response_output: bool,
    conversation_index: Option<usize>,
}

/// Item-level measurement of the input actually sent on the wire.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextWireInput {
    items: Vec<ContextWireItem>,
    conversation_count: usize,
}

impl ContextWireInput {
    /// Measures lowered JSON items with a dialect-supplied per-image maximum.
    /// An image without an explicit dialect bound is unavailable, never base64-sized.
    ///
    /// # Errors
    /// Returns a typed error for an unknown image bound or malformed body.
    pub fn measure_wire(
        body: &[u8],
        image_maximum: Option<u64>,
    ) -> Result<Self, ContextContractError> {
        let object: std::collections::BTreeMap<String, &serde_json::value::RawValue> =
            serde_json::from_slice(body).map_err(|error| ContextContractError::Serialization {
                reason: error.to_string(),
            })?;
        let mut items = Vec::new();
        let mut conversation_count = 0;
        for key in ["instructions", "system", "tools", "input", "messages"] {
            let Some(field) = object.get(key) else {
                continue;
            };
            let values: Vec<&serde_json::value::RawValue> = if field.get().starts_with('[') {
                serde_json::from_str(field.get()).map_err(|error| {
                    ContextContractError::Serialization {
                        reason: error.to_string(),
                    }
                })?
            } else {
                vec![field]
            };
            for raw in values {
                let value: serde_json::Value =
                    serde_json::from_str(raw.get()).map_err(|error| {
                        ContextContractError::Serialization {
                            reason: error.to_string(),
                        }
                    })?;
                let wire = raw.get().as_bytes();
                let normalized =
                    serde_json::to_vec(&WireDigestItem { raw, root: true }).map_err(|error| {
                        ContextContractError::Serialization {
                            reason: error.to_string(),
                        }
                    })?;
                let digest = crate::hex_lower(
                    Digest::sha256(
                        DigestDomain::TokenMeasurement,
                        5,
                        &[key.as_bytes(), &normalized],
                    )?
                    .bytes(),
                );
                let mut bounded = value.clone();
                let image_tokens = if matches!(key, "input" | "messages") {
                    remove_image_payloads(&mut bounded, image_maximum)?
                } else {
                    0
                };
                let bytes = u64::try_from(wire.len())
                    .unwrap_or(u64::MAX)
                    .saturating_sub(json_bytes(&value))
                    .saturating_add(json_bytes(&bounded));
                let index = if matches!(key, "input" | "messages") {
                    let index = conversation_count;
                    conversation_count += 1;
                    Some(index)
                } else {
                    None
                };
                let response_output = index.is_some()
                    && (value.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                        || matches!(
                            value.get("type").and_then(serde_json::Value::as_str),
                            Some("function_call" | "custom_tool_call" | "reasoning")
                        ));
                items.push(ContextWireItem {
                    digest,
                    bytes,
                    image_tokens,
                    response_output,
                    conversation_index: index,
                });
            }
        }
        // Output schemas and other token-affecting envelope fields are items
        // too. Transport settings are harmless conservative overhead.
        for (key, raw) in &object {
            if matches!(
                key.as_str(),
                "instructions" | "system" | "tools" | "input" | "messages"
            ) {
                continue;
            }
            items.push(ContextWireItem {
                digest: crate::hex_lower(
                    Digest::sha256(
                        DigestDomain::TokenMeasurement,
                        5,
                        &[key.as_bytes(), raw.get().as_bytes()],
                    )?
                    .bytes(),
                ),
                bytes: u64::try_from(raw.get().len()).unwrap_or(u64::MAX),
                image_tokens: 0,
                response_output: false,
                conversation_index: None,
            });
        }
        Ok(Self {
            items,
            conversation_count,
        })
    }

    pub(crate) fn image_tokens(&self) -> u64 {
        self.items
            .iter()
            .fold(0_u64, |total, item| total.saturating_add(item.image_tokens))
    }

    /// Conservative byte/token input bound, before the admission margin.
    #[must_use]
    pub(crate) fn upper_bound(&self) -> u64 {
        self.items.iter().fold(0_u64, |total, item| {
            total
                .saturating_add(item.bytes)
                .saturating_add(item.image_tokens)
                .saturating_add(16)
        })
    }

    /// Non-guaranteed unanchored estimate, excluding image transport bytes.
    #[must_use]
    pub(crate) fn estimate_tokens(&self) -> u64 {
        self.items.iter().fold(0_u64, |total, item| {
            total
                .saturating_add(item.bytes.div_ceil(4))
                .saturating_add(item.image_tokens)
                .saturating_add(16)
        })
    }
}

pub(crate) fn context_admission_margin(ceiling: u64) -> u64 {
    ceiling.div_ceil(100).max(2048.min(ceiling / 10))
}

// Preserve wire key order and opaque tool data while omitting transport annotations.
struct WireDigestItem<'a> {
    raw: &'a serde_json::value::RawValue,
    root: bool,
}

struct WireDigestFields<'a>(Vec<(String, &'a serde_json::value::RawValue)>);

impl<'de> Deserialize<'de> for WireDigestFields<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = WireDigestFields<'de>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("wire item fields in their original order")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut fields = Vec::new();
                while let Some(field) = map.next_entry()? {
                    fields.push(field);
                }
                Ok(WireDigestFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

impl Serialize for WireDigestItem<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error as _, SerializeMap as _, SerializeSeq as _};
        let raw = self.raw.get().trim_start();
        if raw.starts_with('[') {
            let values: Vec<&serde_json::value::RawValue> =
                serde_json::from_str(raw).map_err(S::Error::custom)?;
            let mut seq = serializer.serialize_seq(Some(values.len()))?;
            for raw in values {
                seq.serialize_element(&Self { raw, root: false })?;
            }
            return seq.end();
        }
        if !raw.starts_with('{') {
            return self.raw.serialize(serializer);
        }
        let WireDigestFields(fields) = serde_json::from_str(raw).map_err(S::Error::custom)?;
        let annotations = self.root
            || fields.iter().any(|(key, value)| {
                key == "type"
                    && matches!(
                        value.get(),
                        "\"text\""
                            | "\"image\""
                            | "\"tool_use\""
                            | "\"tool_result\""
                            | "\"thinking\""
                            | "\"redacted_thinking\""
                    )
            });
        let mut map = serializer.serialize_map(None)?;
        for (key, raw) in fields {
            if annotations && key == "cache_control" {
                continue;
            }
            if annotations && key == "content" {
                map.serialize_entry(&key, &Self { raw, root: false })?;
            } else {
                map.serialize_entry(&key, raw)?;
            }
        }
        map.end()
    }
}

fn remove_image_payloads(
    value: &mut serde_json::Value,
    maximum: Option<u64>,
) -> Result<u64, ContextContractError> {
    fn content(
        value: &mut serde_json::Value,
        maximum: Option<u64>,
        block: bool,
    ) -> Result<u64, ContextContractError> {
        if block
            && matches!(
                value.get("type").and_then(serde_json::Value::as_str),
                Some("image" | "image_url" | "input_image")
            )
        {
            let tokens = maximum
                .filter(|tokens| *tokens > 0)
                .ok_or(ContextContractError::ImageBoundUnavailable)?;
            *value = serde_json::json!({"type": "bounded_image"});
            return Ok(tokens);
        }
        match value {
            serde_json::Value::Array(values) => {
                values.iter_mut().try_fold(0_u64, |total, value| {
                    Ok(total.saturating_add(content(value, maximum, block)?))
                })
            }
            serde_json::Value::Object(object) => object
                .get_mut("content")
                .map_or(Ok(0), |value| content(value, maximum, true)),
            _ => Ok(0),
        }
    }
    content(value, maximum, false)
}

/// Completed-response authority for item-delta admission on one binding.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextRequestAnchor {
    binding: Digest,
    route: RouteId,
    model: ModelId,
    usage: crate::Usage,
    input: ContextWireInput,
    response_items: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextRequestAnchorWire {
    binding: Digest,
    route: RouteId,
    model: ModelId,
    usage: crate::Usage,
    input: ContextWireInput,
    response_items: Option<Vec<String>>,
}

impl<'de> Deserialize<'de> for ContextRequestAnchor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ContextRequestAnchorWire::deserialize(deserializer)?;
        let mut anchor = Self::new(wire.binding, wire.route, wire.model, wire.usage, wire.input)
            .ok_or_else(|| {
                serde::de::Error::custom("context anchor requires positive input usage")
            })?;
        anchor.response_items = wire.response_items;
        Ok(anchor)
    }
}

impl ContextRequestAnchor {
    /// Records a successful response with positive input usage; output may be zero.
    #[must_use]
    pub fn new(
        binding: Digest,
        route: RouteId,
        model: ModelId,
        usage: crate::Usage,
        input: ContextWireInput,
    ) -> Option<Self> {
        (usage.input_tokens() > 0).then_some(Self {
            binding,
            route,
            model,
            usage,
            input,
            response_items: None,
        })
    }

    /// Integrity identity used by durable references to this anchor.
    /// Derived response-item coverage is a cache, not a new authority.
    ///
    /// # Errors
    /// Returns an error if the anchor cannot be serialized or digested.
    pub fn identity(&self) -> Result<Digest, ContextContractError> {
        semantic_digest(
            DigestDomain::TokenMeasurement,
            5,
            &(
                self.binding,
                &self.route,
                &self.model,
                &self.usage,
                &self.input,
            ),
        )
    }

    /// Checks the observed endpoint/model against the current admitted binding.
    #[must_use]
    pub fn matches_descriptor(&self, descriptor: &crate::ResolvedModelDescriptor) -> bool {
        Some(self.binding) == descriptor.semantic_binding_digest().ok()
            && &self.route == descriptor.route()
            && descriptor.serves_same_model(&self.model)
    }

    pub(crate) fn observe_response_items(&mut self, current: &ContextWireInput) {
        if self.response_items.is_some() {
            return;
        }
        let mut output = Vec::new();
        for item in &current.items {
            if item
                .conversation_index
                .is_none_or(|index| index < self.input.conversation_count)
            {
                continue;
            }
            if !item.response_output {
                break;
            }
            if self.usage.output_tokens() > 0 {
                output.push(item.digest.clone());
            }
        }
        self.response_items = Some(output);
    }

    /// Provider input already includes cache reads and writes; add output once.
    #[must_use]
    pub(crate) fn occupied_tokens(&self, current: &ContextWireInput) -> u64 {
        let mut observed = self.clone();
        observed.observe_response_items(current);
        let mut covered: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for item in &self.input.items {
            *covered.entry(item.digest.as_str()).or_default() += 1;
        }
        for digest in observed.response_items.iter().flatten() {
            *covered.entry(digest.as_str()).or_default() += 1;
        }
        current.items.iter().fold(
            self.usage
                .input_tokens()
                .saturating_add(self.usage.output_tokens()),
            |total, item| {
                if let Some(count) = covered.get_mut(item.digest.as_str())
                    && *count > 0
                {
                    *count -= 1;
                    return total;
                }
                total
                    .saturating_add(item.bytes)
                    .saturating_add(item.image_tokens)
                    .saturating_add(16)
            },
        )
    }
}

/// The provider-reported input of one sent request, paired with the local
/// `bytes/4` estimate of that same request.
///
/// The provider's count (cached input included, as its usage reports it) is
/// the authoritative size of the prefix it received; the estimate is what
/// lets later growth be measured on the same local scale, so only the suffix
/// appended since that request is ever estimated.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "ContextAnchorWire")]
pub struct ContextAnchor {
    provider_input_tokens: u64,
    request_estimate_tokens: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextAnchorWire {
    provider_input_tokens: u64,
    request_estimate_tokens: u64,
}

impl TryFrom<ContextAnchorWire> for ContextAnchor {
    type Error = &'static str;

    fn try_from(wire: ContextAnchorWire) -> Result<Self, Self::Error> {
        Self::new(wire.provider_input_tokens, wire.request_estimate_tokens)
            .ok_or("a context anchor needs a positive provider input count")
    }
}

impl ContextAnchor {
    /// Pairs a provider input count with the estimate of the request it
    /// counted. `None` when the provider reported no input: a zero count is
    /// not an observation.
    #[must_use]
    pub const fn new(provider_input_tokens: u64, request_estimate_tokens: u64) -> Option<Self> {
        if provider_input_tokens == 0 {
            return None;
        }
        Some(Self {
            provider_input_tokens,
            request_estimate_tokens,
        })
    }

    /// Returns the provider-reported input tokens of the anchored request.
    #[must_use]
    pub const fn provider_input_tokens(self) -> u64 {
        self.provider_input_tokens
    }

    /// Returns the local estimate of the anchored request.
    #[must_use]
    pub const fn request_estimate_tokens(self) -> u64 {
        self.request_estimate_tokens
    }
}

/// The context measure: the anchored provider input plus the estimate of the
/// suffix appended since that request, or the whole estimate with no anchor.
///
/// `estimate_tokens` is the `bytes/4` estimate of the whole projected request
/// now, on the same scale as the anchor's request estimate. Only the
/// compaction fences and the context gauge read this anchored number;
/// compaction records, telemetry and `ToolResultsReplaced` use the unanchored
/// whole-request estimate (the bound when the request has no parts).
#[must_use]
pub const fn context_occupancy_tokens(estimate_tokens: u64, anchor: Option<ContextAnchor>) -> u64 {
    match anchor {
        Some(anchor) => anchor
            .provider_input_tokens
            .saturating_add(estimate_tokens.saturating_sub(anchor.request_estimate_tokens)),
        None => estimate_tokens,
    }
}

/// Compact serialized length of one JSON value, saturating at `u64::MAX`.
fn json_bytes(value: &serde_json::Value) -> u64 {
    serde_json::to_vec(value).map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
}

/// Reasoning material embedded in one message item: Messages `thinking` /
/// `redacted_thinking` content blocks, and Chat `reasoning` /
/// `reasoning_details` fields.
fn embedded_reasoning_bytes(item: &serde_json::Value) -> u64 {
    let mut bytes = 0_u64;
    if let Some(serde_json::Value::Array(blocks)) = item.get("content") {
        for block in blocks {
            if matches!(
                block.get("type").and_then(serde_json::Value::as_str),
                Some("thinking" | "redacted_thinking")
            ) {
                bytes = bytes.saturating_add(json_bytes(block));
            }
        }
    }
    for key in ["reasoning", "reasoning_details"] {
        if let Some(field) = item.get(key) {
            bytes = bytes.saturating_add(json_bytes(field));
        }
    }
    bytes
}

/// A context contract construction failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ContextContractError {
    /// The selected dialect has no documented image token bound.
    #[error("image context measurement requires a documented dialect bound")]
    ImageBoundUnavailable,
    /// A label was empty, oversized, or non-portable.
    #[error("{field} must be a nonempty portable identifier")]
    InvalidLabel {
        /// The invalid field.
        field: &'static str,
    },
    /// A safe capacity limit was zero.
    #[error("{field} must be nonzero")]
    ZeroLimit {
        /// The invalid capacity axis.
        field: &'static str,
    },
    /// A provider-reserved input ceiling exceeded its reported context window.
    #[error("provider-reserved input must not exceed the reported context window")]
    InvalidCapacityRelationship,
    /// Evidence expiry did not follow observation.
    #[error("evidence expiry must follow observation")]
    InvalidFreshness,
    /// Authenticated discovery lacked exact expiring authority scope.
    #[error("authenticated discovery requires an authority binding and finite expiry")]
    UnscopedDiscovery,
    /// A custom declaration lacked an exact configuration/provenance binding.
    #[error("a custom context declaration requires an authority binding")]
    UnscopedCustomDeclaration,
    /// Non-discovery evidence incorrectly carried account/credential scope.
    #[error("only authenticated discovery may carry an authority binding")]
    UnexpectedAuthorityBinding,
    /// Release-owned evidence construction lacked Standard authority.
    #[error("standard context evidence requires release-owned authority")]
    StandardAuthorityRequired,
    /// Descriptor ownership disagreed with its context evidence owner.
    #[error("model integration class disagrees with context evidence class")]
    IntegrationClassMismatch,
    /// A conservative count did not use a release-owned certificate.
    #[error("a conservative token upper bound requires release-owned certification")]
    UpperBoundNotReleaseVerified,
    /// Reference-client-envelope evidence was incomplete or was presented as certified.
    #[error(
        "reference-client-envelope evidence requires an input-only rule, exact receipt, and finite authenticated freshness"
    )]
    InvalidReferenceClientEnvelopeEvidence,
    /// Custom caller evidence was incomplete or crossed into a standard lane.
    #[error(
        "custom-envelope evidence requires an input-only rule, exact declaration, and non-guaranteed caller provenance"
    )]
    InvalidCustomEnvelopeEvidence,
    /// Capacity and measurement evidence classes or receipts differed.
    #[error("context capacity and measurement evidence classes do not match")]
    EvidenceClassMismatch,
    /// Too many measurement certificates were configured.
    #[error("a context contract accepts at most 16 measurement certificates")]
    TooManyMeasurementCertificates,
    /// Measurement certificate IDs must be unique within a contract.
    #[error("duplicate measurement certificate ID")]
    DuplicateMeasurementCertificate,
    /// A descriptor binding could not be computed.
    #[error("resolved descriptor binding failed: {reason}")]
    DescriptorBinding {
        /// Safe diagnostic reason.
        reason: String,
    },
    /// JSON serialization for an identity failed.
    #[error("context identity serialization failed: {reason}")]
    Serialization {
        /// Serialization failure.
        reason: String,
    },
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] DigestError),
    /// Serialized certificate facts disagreed with their identity.
    #[error("serialized {field} identity does not match its semantic facts")]
    IntegrityMismatch {
        /// The invalid semantic fact.
        field: &'static str,
    },
}

/// A typed denial or construction failure from context admission.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ContextAdmissionError {
    /// Safe context status was unavailable.
    #[error("context admission unavailable: {0:?}")]
    Unavailable(ContextUnavailable),
    /// The projection exceeded capacity and may be compactable.
    #[error("model input and output reservation exceed context capacity")]
    WouldOverflow(ContextOverflow),
    /// The projection exceeded capacity and only protected material remains.
    #[error("protected model input and output reservation exceed context capacity")]
    ProtectedOverflow(ContextOverflow),
    /// Contract or status identity construction failed.
    #[error(transparent)]
    Contract(#[from] ContextContractError),
    /// Admission identity construction failed.
    #[error(transparent)]
    Digest(#[from] DigestError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EvaluationFailure {
    Overflow(ContextOverflow),
    ArithmeticOverflow,
}

fn evaluate_rule(
    rule: CapacityRule,
    input: CountBound,
    reserved_output: Option<u64>,
) -> Result<ContextHeadroom, EvaluationFailure> {
    let input_upper = input.upper_bound();
    match rule {
        CapacityRule::ProviderReservedInput {
            maximum_projected_input,
            ..
        } => {
            if input_upper > maximum_projected_input {
                return Err(EvaluationFailure::Overflow(ContextOverflow {
                    shared: None,
                    input: Some(LimitOverflow {
                        required: input_upper,
                        available: maximum_projected_input,
                    }),
                    output: None,
                }));
            }
            Ok(ContextHeadroom::ProviderReservedInput {
                input: qualify(maximum_projected_input - input_upper, false),
            })
        }
        CapacityRule::SharedWindow { total } => {
            let reserved_output = reserved_output.ok_or(EvaluationFailure::ArithmeticOverflow)?;
            let required = input_upper
                .checked_add(reserved_output)
                .ok_or(EvaluationFailure::ArithmeticOverflow)?;
            let available = total.lower_bound();
            if required > available {
                return Err(EvaluationFailure::Overflow(ContextOverflow {
                    shared: Some(LimitOverflow {
                        required,
                        available,
                    }),
                    input: None,
                    output: None,
                }));
            }
            Ok(ContextHeadroom::Shared(qualify(
                available - required,
                total.is_exact() && input.is_exact(),
            )))
        }
        CapacityRule::Separate {
            input: input_limit,
            output,
        } => {
            let reserved_output = reserved_output.ok_or(EvaluationFailure::ArithmeticOverflow)?;
            let input_available = input_limit.lower_bound();
            let output_available = output.lower_bound();
            let overflow = ContextOverflow {
                shared: None,
                input: (input_upper > input_available).then_some(LimitOverflow {
                    required: input_upper,
                    available: input_available,
                }),
                output: (reserved_output > output_available).then_some(LimitOverflow {
                    required: reserved_output,
                    available: output_available,
                }),
            };
            if overflow.input.is_some() || overflow.output.is_some() {
                return Err(EvaluationFailure::Overflow(overflow));
            }
            Ok(ContextHeadroom::Separate {
                input: qualify(
                    input_available - input_upper,
                    input_limit.is_exact() && input.is_exact(),
                ),
                output: qualify(output_available - reserved_output, output.is_exact()),
            })
        }
        CapacityRule::SharedWindowWithOutputCap { total, output } => {
            let reserved_output = reserved_output.ok_or(EvaluationFailure::ArithmeticOverflow)?;
            let required = input_upper
                .checked_add(reserved_output)
                .ok_or(EvaluationFailure::ArithmeticOverflow)?;
            let total_available = total.lower_bound();
            let output_available = output.lower_bound();
            let overflow = ContextOverflow {
                shared: (required > total_available).then_some(LimitOverflow {
                    required,
                    available: total_available,
                }),
                input: None,
                output: (reserved_output > output_available).then_some(LimitOverflow {
                    required: reserved_output,
                    available: output_available,
                }),
            };
            if overflow.shared.is_some() || overflow.output.is_some() {
                return Err(EvaluationFailure::Overflow(overflow));
            }
            Ok(ContextHeadroom::SharedWithOutputCap {
                shared: qualify(
                    total_available - required,
                    total.is_exact() && input.is_exact(),
                ),
                output: qualify(output_available - reserved_output, output.is_exact()),
            })
        }
    }
}

const fn qualify(value: u64, exact: bool) -> QualifiedTokens {
    if exact {
        QualifiedTokens::Exact(value)
    } else {
        QualifiedTokens::GuaranteedMinimum(value)
    }
}

fn validate_label(value: &str, field: &'static str) -> Result<(), ContextContractError> {
    if value.is_empty()
        || value.len() > MAX_CONTEXT_LABEL_BYTES
        || !value.bytes().all(|byte| {
            matches!(
                byte,
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'
            )
        })
    {
        Err(ContextContractError::InvalidLabel { field })
    } else {
        Ok(())
    }
}

fn validate_evidence_authority(
    source: ContextEvidenceSource,
    authority_binding: Option<Digest>,
    freshness: EvidenceFreshness,
) -> Result<(), ContextContractError> {
    if source == ContextEvidenceSource::AuthenticatedDiscovery {
        if authority_binding.is_none() || freshness.expires_at().is_none() {
            Err(ContextContractError::UnscopedDiscovery)
        } else {
            Ok(())
        }
    } else if source == ContextEvidenceSource::CallerDeclaration {
        if authority_binding.is_none() {
            Err(ContextContractError::UnscopedCustomDeclaration)
        } else {
            Ok(())
        }
    } else if authority_binding.is_some() {
        Err(ContextContractError::UnexpectedAuthorityBinding)
    } else {
        Ok(())
    }
}

fn semantic_digest(
    domain: DigestDomain,
    version: u32,
    value: &impl Serialize,
) -> Result<Digest, ContextContractError> {
    let bytes = serde_json::to_vec(value).map_err(|error| ContextContractError::Serialization {
        reason: error.to_string(),
    })?;
    Digest::sha256(domain, version, &[&bytes]).map_err(Into::into)
}

#[cfg(test)]
mod occupancy_anchor_tests;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::cast_possible_truncation,
    reason = "unit tests use expect for deterministic fixture clarity and measure short literal bodies"
)]
mod input_parts_tests;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use expect for deterministic fixture clarity"
)]
mod tests;
