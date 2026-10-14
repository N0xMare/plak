//! Shared Responses-family aggregate value types and item parsers.
//!
//! These shapes are byte-twin across OpenRouter / xAI after brand stripping.
//! Dialects keep branded names via type aliases and residual envelope
//! fields (router metadata, optional model, provider-error taxonomy).

use std::collections::BTreeSet;
use std::fmt;

use plakous_responses::{
    ResponsesFieldError, optional_object, optional_provider_label, optional_string, optional_u64,
    required_provider_label, required_string, required_u64,
};
use serde::Deserialize;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// Maximum UTF-8 bytes retained for a URL-citation title.
pub const MAX_URL_CITATION_TITLE_BYTES: usize = 4 * 1024;

/// Parses JSON while rejecting duplicate object members at every depth.
///
/// `serde_json::Value` alone would silently retain only one duplicate member.
///
/// # Errors
///
/// Returns the JSON decoder error for malformed JSON or any duplicate object member.
pub fn parse_unique_json_value(input: &str) -> Result<Value, serde_json::Error> {
    struct Unique(Value);
    struct UniqueVisitor;

    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(UniqueVisitor)
        }
    }

    impl<'de> Visitor<'de> for UniqueVisitor {
        type Value = Unique;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("JSON without duplicate object members")
        }

        fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
            Ok(Unique(Value::Bool(value)))
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
            Ok(Unique(Value::Number(value.into())))
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
            Ok(Unique(Value::Number(value.into())))
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .map(Unique)
                .ok_or_else(|| E::custom("non-finite JSON number"))
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
            Ok(Unique(Value::String(value.to_owned())))
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
            Ok(Unique(Value::String(value)))
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(Unique(Value::Null))
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Unique(Value::Null))
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            Unique::deserialize(deserializer)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
            while let Some(value) = sequence.next_element::<Unique>()? {
                values.push(value.0);
            }
            Ok(Unique(Value::Array(values)))
        }

        fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = Map::new();
            while let Some(key) = object.next_key::<String>()? {
                if values.contains_key(&key) {
                    return Err(de::Error::custom("duplicate JSON object member"));
                }
                values.insert(key, object.next_value::<Unique>()?.0);
            }
            Ok(Unique(Value::Object(values)))
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(input);
    let value = Unique::deserialize(&mut deserializer)?.0;
    deserializer.end()?;
    Ok(value)
}

/// Hard bounds for a decoded Responses aggregate and bounded future shapes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesResponseLimits {
    response_bytes: usize,
    output_items: usize,
    unknown_bytes: usize,
}

impl ResponsesResponseLimits {
    /// Builds a positive, internally consistent limit set.
    ///
    /// # Errors
    ///
    /// Returns [`ResponsesHostDecodeError::ResourceLimit`] for zero limits or
    /// when one unknown value could exceed the whole response budget.
    pub const fn new(
        max_response_bytes: usize,
        max_output_items: usize,
        max_unknown_bytes: usize,
    ) -> Result<Self, ResponsesHostDecodeError> {
        if max_response_bytes == 0
            || max_output_items == 0
            || max_unknown_bytes == 0
            || max_unknown_bytes > max_response_bytes
        {
            return Err(ResponsesHostDecodeError::ResourceLimit(
                "Responses response limits",
            ));
        }
        Ok(Self {
            response_bytes: max_response_bytes,
            output_items: max_output_items,
            unknown_bytes: max_unknown_bytes,
        })
    }

    /// Maximum encoded response bytes.
    #[must_use]
    pub const fn max_response_bytes(self) -> usize {
        self.response_bytes
    }

    /// Maximum ordered output items.
    #[must_use]
    pub const fn max_output_items(self) -> usize {
        self.output_items
    }

    /// Maximum bytes retained for one unknown typed value.
    #[must_use]
    pub const fn max_unknown_bytes(self) -> usize {
        self.unknown_bytes
    }
}

impl Default for ResponsesResponseLimits {
    fn default() -> Self {
        Self {
            response_bytes: 64 * 1024 * 1024,
            output_items: 1_024,
            unknown_bytes: 1024 * 1024,
        }
    }
}

/// Whether the dialect requires `usage.total_tokens` on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesUsagePolicy {
    /// Require and validate the provider total (xAI/OpenRouter complete APIs).
    RequireTotal,
    /// Derive the total when omitted, while validating it when present (Codex).
    DeriveTotalWhenOmitted,
}

/// Concrete fail-closed error for the shared aggregate/event host.
#[derive(Clone, Copy, Debug, Eq, thiserror::Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesHostDecodeError {
    /// Required field was absent.
    #[error("missing Responses field `{0}`")]
    MissingField(&'static str),
    /// Field had an invalid type or closed shape.
    #[error("invalid Responses field `{0}`")]
    InvalidField(&'static str),
    /// A configured byte/count bound was exceeded.
    #[error("Responses resource limit exceeded: {0}")]
    ResourceLimit(&'static str),
    /// Token totals or detail subsets were inconsistent.
    #[error("invalid Responses usage accounting")]
    InvalidUsageAccounting,
}

impl From<ResponsesFieldError> for ResponsesHostDecodeError {
    fn from(error: ResponsesFieldError) -> Self {
        match error {
            ResponsesFieldError::Missing { field } => Self::MissingField(field),
            ResponsesFieldError::Invalid { field } => Self::InvalidField(field),
            _ => Self::InvalidField("unknown Responses field"),
        }
    }
}

impl ResponsesUsageError for ResponsesHostDecodeError {
    fn invalid_usage_accounting() -> Self {
        Self::InvalidUsageAccounting
    }

    fn invalid_field(field: &'static str) -> Self {
        Self::InvalidField(field)
    }

    fn missing_field(field: &'static str) -> Self {
        Self::MissingField(field)
    }
}

/// Provider-reported lifecycle state of a Responses aggregate.
#[derive(Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponsesResponseStatus {
    /// The response is queued but not sampling.
    Queued,
    /// The response is currently sampling or processing tools.
    InProgress,
    /// The response completed normally.
    Completed,
    /// The response ended with incomplete details.
    Incomplete,
    /// The response failed.
    Failed,
    /// A future status retained without inventing semantics.
    Unknown(String),
}

impl fmt::Debug for ResponsesResponseStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Queued => "Queued",
            Self::InProgress => "InProgress",
            Self::Completed => "Completed",
            Self::Incomplete => "Incomplete",
            Self::Failed => "Failed",
            Self::Unknown(_) => "Unknown(<redacted>)",
        })
    }
}

impl ResponsesResponseStatus {
    /// Parses a provider status string.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "queued" => Self::Queued,
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            "incomplete" => Self::Incomplete,
            "failed" => Self::Failed,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Reports whether this status is terminal.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Incomplete | Self::Failed)
    }
}

/// Token accounting retained from one Responses aggregate.
#[derive(Clone, Default, PartialEq)]
pub struct ResponsesUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    native: Value,
}

impl fmt::Debug for ResponsesUsage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesUsage")
            .field("input_tokens", &self.input_tokens)
            .field("output_tokens", &self.output_tokens)
            .field("total_tokens", &self.total_tokens)
            .field("cached_input_tokens", &self.cached_input_tokens)
            .field("reasoning_tokens", &self.reasoning_tokens)
            .field("native", &"<redacted>")
            .finish()
    }
}

impl ResponsesUsage {
    /// Parses usage with required totals and optional detail subsets.
    ///
    /// # Errors
    ///
    /// Returns `E` when accounting is inconsistent or fields have the wrong type.
    pub fn parse<E>(value: &Value) -> Result<Self, E>
    where
        E: From<ResponsesFieldError> + ResponsesUsageError,
    {
        Self::parse_with_policy(value, ResponsesUsagePolicy::RequireTotal)
    }

    /// Parses usage under an exact dialect total-token policy.
    ///
    /// # Errors
    ///
    /// Returns `E` when accounting is inconsistent or fields have the wrong type.
    pub fn parse_with_policy<E>(value: &Value, policy: ResponsesUsagePolicy) -> Result<Self, E>
    where
        E: From<ResponsesFieldError> + ResponsesUsageError,
    {
        let object = value.as_object().ok_or_else(|| E::invalid_field("usage"))?;
        let input_tokens = required_u64(object, "input_tokens")?;
        let output_tokens = required_u64(object, "output_tokens")?;
        let computed_total = input_tokens
            .checked_add(output_tokens)
            .ok_or_else(E::invalid_usage_accounting)?;
        let total_tokens = match policy {
            ResponsesUsagePolicy::RequireTotal => required_u64(object, "total_tokens")?,
            ResponsesUsagePolicy::DeriveTotalWhenOmitted => {
                optional_u64(object, "total_tokens")?.unwrap_or(computed_total)
            }
        };
        if computed_total != total_tokens {
            return Err(E::invalid_usage_accounting());
        }
        let input_details = optional_object(object, "input_tokens_details")?;
        let output_details = optional_object(object, "output_tokens_details")?;
        let cached_input_tokens = input_details
            .map(|details| optional_u64(details, "cached_tokens"))
            .transpose()?
            .flatten();
        let reasoning_tokens = output_details
            .map(|details| optional_u64(details, "reasoning_tokens"))
            .transpose()?
            .flatten();
        if cached_input_tokens.is_some_and(|cached| cached > input_tokens)
            || reasoning_tokens.is_some_and(|reasoning| reasoning > output_tokens)
        {
            return Err(E::invalid_usage_accounting());
        }
        Ok(Self {
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            total_tokens: Some(total_tokens),
            cached_input_tokens,
            reasoning_tokens,
            native: value.clone(),
        })
    }

    /// Returns provider-reported input tokens.
    #[must_use]
    pub const fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }

    /// Returns provider-reported output tokens.
    #[must_use]
    pub const fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }

    /// Returns provider-reported total tokens.
    #[must_use]
    pub const fn total_tokens(&self) -> Option<u64> {
        self.total_tokens
    }

    /// Returns cached input tokens when detailed usage was present.
    #[must_use]
    pub const fn cached_input_tokens(&self) -> Option<u64> {
        self.cached_input_tokens
    }

    /// Returns reasoning tokens when detailed usage was present.
    #[must_use]
    pub const fn reasoning_tokens(&self) -> Option<u64> {
        self.reasoning_tokens
    }

    /// Returns the native usage object.
    #[must_use]
    pub const fn native(&self) -> &Value {
        &self.native
    }
}

/// Dialect decode errors that can surface usage accounting failures.
pub trait ResponsesUsageError {
    /// Inconsistent totals or detail subsets.
    fn invalid_usage_accounting() -> Self;
    /// Wrong JSON type for a named field.
    fn invalid_field(field: &'static str) -> Self;
    /// Missing required field.
    fn missing_field(field: &'static str) -> Self;
}

/// Safe classification for an untrusted provider error payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponsesProviderErrorClass {
    /// The selected route was not authorized.
    Authentication,
    /// The selected route exhausted a provider quota.
    Quota,
    /// The provider requested rate limiting/backoff.
    RateLimited,
    /// The provider reported an internal service failure.
    Server,
    /// No allowlisted safe classification matched.
    Unknown,
}

/// Closed allowlist used to classify untrusted provider error labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesProviderErrorPolicy {
    /// OpenAI-compatible documented labels.
    OpenAiCompatible,
    /// OpenRouter Responses canonical top-level `error_type` labels.
    OpenRouter,
    /// Exact SuperGrok/xAI labels; foreign near-matches remain unknown.
    Grok,
}

/// Typed incomplete-terminal details with an exact bounded native value.
#[derive(Clone, PartialEq)]
pub struct ResponsesIncompleteDetails {
    reason: Option<String>,
    native: Value,
}

impl ResponsesIncompleteDetails {
    /// Provider incomplete reason when present.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// Exact provider-native details.
    #[must_use]
    pub const fn native(&self) -> &Value {
        &self.native
    }
}

impl fmt::Debug for ResponsesIncompleteDetails {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesIncompleteDetails")
            .field("reason", &self.reason.as_ref().map(|_| "<redacted>"))
            .field("native", &"<redacted>")
            .finish()
    }
}

/// Safe provider-error facts plus the exact bounded native payload.
#[derive(Clone, PartialEq)]
pub struct ResponsesProviderError {
    code: Option<String>,
    kind: Option<String>,
    request_id: Option<String>,
    class: ResponsesProviderErrorClass,
    native: Value,
}

impl ResponsesProviderError {
    /// Safe allowlisted error classification.
    #[must_use]
    pub const fn class(&self) -> ResponsesProviderErrorClass {
        self.class
    }

    /// Bounded provider code when present.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// Bounded provider error type/kind when present.
    #[must_use]
    pub fn kind(&self) -> Option<&str> {
        self.kind.as_deref()
    }

    /// Bounded provider request identity when present.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Native `message` string when present and non-empty.
    ///
    /// The payload remains on [`Self::native`]; this is the operator-facing
    /// detail Responses HTTP mapping may append. Control characters stay in
    /// the native value and are collapsed only at the model-error boundary.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.native
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| self.native.get("error").and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// Exact bounded native provider-error payload.
    #[must_use]
    pub const fn native(&self) -> &Value {
        &self.native
    }

    /// Attaches the outer event request identity after validation.
    ///
    /// # Errors
    ///
    /// Fails closed for an invalid provider label.
    pub fn with_request_id(
        mut self,
        request_id: Option<&str>,
    ) -> Result<Self, ResponsesFieldError> {
        if let Some(request_id) = request_id {
            plakous_responses::validate_provider_label(request_id, "request_id")?;
            self.request_id = Some(request_id.to_owned());
        }
        Ok(self)
    }
}

impl fmt::Debug for ResponsesProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesProviderError")
            .field("code", &self.code.as_ref().map(|_| "<redacted>"))
            .field("kind", &self.kind.as_ref().map(|_| "<redacted>"))
            .field(
                "request_id",
                &self.request_id.as_ref().map(|_| "<redacted>"),
            )
            .field("class", &self.class)
            .field("native", &"<redacted>")
            .finish()
    }
}

/// One typed provider annotation attached to output text.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum ResponsesAnnotation {
    /// A URL citation with provider-reported text offsets.
    UrlCitation {
        /// Inclusive citation start offset.
        start_index: u64,
        /// Exclusive citation end offset.
        end_index: u64,
        /// Citation URL.
        url: String,
        /// Human-readable title.
        title: String,
        /// Exact provider-native annotation.
        native: Value,
    },
    /// A future annotation retained exactly without a feature claim.
    Opaque {
        /// Provider type discriminator.
        kind: String,
        /// Exact bounded provider-native annotation.
        native: Value,
    },
}

impl ResponsesAnnotation {
    /// Exact provider-native annotation.
    #[must_use]
    pub const fn native(&self) -> &Value {
        match self {
            Self::UrlCitation { native, .. } | Self::Opaque { native, .. } => native,
        }
    }
}

impl fmt::Debug for ResponsesAnnotation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UrlCitation {
                start_index,
                end_index,
                ..
            } => formatter
                .debug_struct("UrlCitation")
                .field("start_index", start_index)
                .field("end_index", end_index)
                .field("provider_fields", &"<redacted>")
                .finish(),
            Self::Opaque { .. } => formatter
                .debug_struct("Opaque")
                .field("provider_fields", &"<redacted>")
                .finish(),
        }
    }
}

/// One typed content part in an assistant message item.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum ResponsesContentPart {
    /// Visible text plus provider-native annotations.
    OutputText {
        /// Complete text content.
        text: String,
        /// Ordered provider-native annotations or citations.
        annotations: Vec<ResponsesAnnotation>,
    },
    /// A model refusal.
    Refusal {
        /// Complete refusal text.
        refusal: String,
    },
    /// A future content-part kind retained without a support claim.
    Opaque {
        /// Provider type discriminator.
        kind: String,
        /// Full provider-native content part.
        native: Value,
    },
}

impl fmt::Debug for ResponsesContentPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputText {
                text, annotations, ..
            } => formatter
                .debug_struct("OutputText")
                .field("text_bytes", &text.len())
                .field("annotations", &annotations.len())
                .finish(),
            Self::Refusal { refusal } => formatter
                .debug_struct("Refusal")
                .field("refusal_bytes", &refusal.len())
                .finish(),
            Self::Opaque { .. } => formatter
                .debug_struct("Opaque")
                .field("kind", &"<redacted provider type>")
                .field("native", &"<redacted>")
                .finish(),
        }
    }
}

/// One reasoning summary part.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum ResponsesReasoningSummaryPart {
    /// Provider-authorized displayable summary text.
    SummaryText(String),
    /// A future summary kind retained without interpretation.
    Opaque {
        /// Provider type discriminator.
        kind: String,
        /// Full provider-native summary part.
        native: Value,
    },
}

/// One provider-authorized visible reasoning content part.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ResponsesReasoningContentPart {
    /// Visible reasoning text, distinct from a display summary.
    ReasoningText(String),
}

impl fmt::Debug for ResponsesReasoningSummaryPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SummaryText(text) => formatter
                .debug_tuple("SummaryText")
                .field(&format_args!("<{} bytes redacted>", text.len()))
                .finish(),
            Self::Opaque { .. } => formatter
                .debug_struct("Opaque")
                .field("kind", &"<redacted provider type>")
                .field("native", &"<redacted>")
                .finish(),
        }
    }
}

/// One ordered Responses output item.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum ResponsesOutputItem {
    /// An assistant message item.
    Message {
        /// Provider item ID.
        id: Option<String>,
        /// Provider item status.
        status: Option<String>,
        /// Provider role.
        role: String,
        /// Ordered content parts.
        content: Vec<ResponsesContentPart>,
        /// Full provider-native item.
        native: Value,
    },
    /// A reasoning item with summary and opaque encrypted replay.
    Reasoning {
        /// Provider item ID.
        id: Option<String>,
        /// Provider item status.
        status: Option<String>,
        /// Displayable summary parts.
        summary: Vec<ResponsesReasoningSummaryPart>,
        /// Provider-authorized visible reasoning content.
        content: Vec<ResponsesReasoningContentPart>,
        /// Opaque encrypted content retained exactly as a JSON string value.
        encrypted_content: Option<String>,
        /// Full provider-native item.
        native: Value,
    },
    /// A client function call.
    FunctionCall {
        /// Provider item ID.
        id: Option<String>,
        /// Provider item status.
        status: Option<String>,
        /// Stable provider call correlation ID.
        call_id: String,
        /// Model-facing function name.
        name: String,
        /// Complete raw JSON argument text.
        arguments: String,
        /// Full provider-native item.
        native: Value,
    },
    /// A custom tool call whose input is arbitrary UTF-8 rather than JSON.
    CustomToolCall {
        /// Provider item ID.
        id: Option<String>,
        /// Provider item status.
        status: Option<String>,
        /// Stable provider call correlation ID.
        call_id: String,
        /// Model-facing custom tool name.
        name: String,
        /// Complete raw custom-tool input.
        input: String,
        /// Full provider-native item.
        native: Value,
    },
    /// A future output item retained without local execution or support claim.
    Opaque {
        /// Provider type discriminator.
        kind: String,
        /// Full provider-native item.
        native: Value,
    },
    /// ChatGPT OAuth remote compaction item (`encrypted_content`, no status).
    Compaction {
        /// Provider item ID.
        id: Option<String>,
        /// Opaque encrypted compaction payload replayed verbatim.
        encrypted_content: String,
        /// Full provider-native item.
        native: Value,
    },
}

impl ResponsesOutputItem {
    /// Returns the complete provider-native item.
    #[must_use]
    pub const fn native(&self) -> &Value {
        match self {
            Self::Message { native, .. }
            | Self::Reasoning { native, .. }
            | Self::FunctionCall { native, .. }
            | Self::CustomToolCall { native, .. }
            | Self::Opaque { native, .. }
            | Self::Compaction { native, .. } => native,
        }
    }

    /// Returns the provider item type discriminator.
    #[must_use]
    pub fn kind(&self) -> &str {
        match self {
            Self::Message { .. } => "message",
            Self::Reasoning { .. } => "reasoning",
            Self::FunctionCall { .. } => "function_call",
            Self::CustomToolCall { .. } => "custom_tool_call",
            Self::Opaque { kind, .. } => kind,
            Self::Compaction { .. } => "compaction",
        }
    }
}

impl fmt::Debug for ResponsesOutputItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message { content, .. } => formatter
                .debug_struct("Message")
                .field("provider_fields", &"<redacted>")
                .field("content_parts", &content.len())
                .field("native", &"<redacted>")
                .finish(),
            Self::Reasoning {
                summary,
                encrypted_content,
                ..
            } => formatter
                .debug_struct("Reasoning")
                .field("provider_fields", &"<redacted>")
                .field("summary_parts", &summary.len())
                .field(
                    "encrypted_content",
                    &encrypted_content.as_ref().map(|_| "<redacted>"),
                )
                .field("native", &"<redacted>")
                .finish(),
            Self::FunctionCall { arguments, .. } => formatter
                .debug_struct("FunctionCall")
                .field("provider_fields", &"<redacted>")
                .field("arguments_bytes", &arguments.len())
                .field("native", &"<redacted>")
                .finish(),
            Self::CustomToolCall { input, .. } => formatter
                .debug_struct("CustomToolCall")
                .field("provider_fields", &"<redacted>")
                .field("input_bytes", &input.len())
                .field("native", &"<redacted>")
                .finish(),
            Self::Opaque { .. } => formatter
                .debug_struct("Opaque")
                .field("provider_fields", &"<redacted>")
                .field("native", &"<redacted>")
                .finish(),
            Self::Compaction { .. } => formatter
                .debug_struct("Compaction")
                .field("provider_fields", &"<redacted>")
                .field("native", &"<redacted>")
                .finish(),
        }
    }
}

/// Exact ordered Responses terminal aggregate.
#[derive(Clone, PartialEq)]
pub struct ResponsesResponse {
    id: String,
    model: String,
    status: ResponsesResponseStatus,
    end_turn: Option<bool>,
    output: Vec<ResponsesOutputItem>,
    usage: Option<ResponsesUsage>,
    incomplete_details: Option<ResponsesIncompleteDetails>,
    error: Option<ResponsesProviderError>,
    native: Value,
}

impl ResponsesResponse {
    pub(crate) fn parse_reconciled_with_native(
        reconciled: &Value,
        native: &Value,
        usage_policy: ResponsesUsagePolicy,
        error_policy: ResponsesProviderErrorPolicy,
        limits: ResponsesResponseLimits,
    ) -> Result<Self, ResponsesHostDecodeError> {
        if encoded_len(native) > limits.max_response_bytes() {
            return Err(ResponsesHostDecodeError::ResourceLimit("response bytes"));
        }
        let mut response =
            Self::parse_with_error_policy(reconciled, usage_policy, error_policy, limits)?;
        response.native = native.clone();
        Ok(response)
    }

    pub(crate) fn with_authoritative_stream_output(
        mut self,
        output_values: &[Value],
        limits: ResponsesResponseLimits,
    ) -> Result<Self, ResponsesHostDecodeError> {
        if output_values.len() > limits.max_output_items() {
            return Err(ResponsesHostDecodeError::ResourceLimit("output items"));
        }
        let output = output_values
            .iter()
            .map(parse_output_item::<ResponsesHostDecodeError>)
            .collect::<Result<Vec<_>, _>>()?;
        validate_output_identity_roles::<ResponsesHostDecodeError>(&output)?;
        for item in &output {
            validate_bounded_unknown_item(item, limits)?;
        }
        self.output = output;
        Ok(self)
    }

    /// Parses a terminal aggregate under exact dialect usage and resource data.
    ///
    /// # Errors
    ///
    /// Fails closed for malformed fields, contradictory terminal state,
    /// duplicate identities, or configured bounds.
    pub fn parse(
        value: &Value,
        usage_policy: ResponsesUsagePolicy,
        limits: ResponsesResponseLimits,
    ) -> Result<Self, ResponsesHostDecodeError> {
        Self::parse_with_error_policy(
            value,
            usage_policy,
            ResponsesProviderErrorPolicy::OpenAiCompatible,
            limits,
        )
    }

    /// Parses a terminal aggregate under exact usage, error, and resource data.
    ///
    /// # Errors
    ///
    /// Fails closed for malformed fields, contradictory terminal state,
    /// provider-error policy violations, duplicate identities, or configured bounds.
    pub fn parse_with_error_policy(
        value: &Value,
        usage_policy: ResponsesUsagePolicy,
        error_policy: ResponsesProviderErrorPolicy,
        limits: ResponsesResponseLimits,
    ) -> Result<Self, ResponsesHostDecodeError> {
        if encoded_len(value) > limits.max_response_bytes() {
            return Err(ResponsesHostDecodeError::ResourceLimit("response bytes"));
        }
        let object = value
            .as_object()
            .ok_or(ResponsesHostDecodeError::InvalidField("response"))?;
        let id = required_provider_label(object, "id")?.to_owned();
        let model = required_provider_label(object, "model")?.to_owned();
        let status = ResponsesResponseStatus::parse(required_provider_label(object, "status")?);
        let end_turn = match object.get("end_turn") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err(ResponsesHostDecodeError::InvalidField("end_turn")),
        };
        if matches!(status, ResponsesResponseStatus::Unknown(_)) {
            return Err(ResponsesHostDecodeError::InvalidField("status"));
        }
        match optional_string(object, "object")? {
            Some("response") | None if matches!(status, ResponsesResponseStatus::Failed) => {}
            Some("response") => {}
            _ => return Err(ResponsesHostDecodeError::InvalidField("object")),
        }
        let output_values: &[Value] = match object.get("output") {
            Some(value) => value
                .as_array()
                .ok_or(ResponsesHostDecodeError::InvalidField("output"))?,
            None if matches!(status, ResponsesResponseStatus::Failed) => &[],
            None => return Err(ResponsesHostDecodeError::MissingField("output")),
        };
        if output_values.len() > limits.max_output_items() {
            return Err(ResponsesHostDecodeError::ResourceLimit("output items"));
        }
        let output = output_values
            .iter()
            .map(parse_output_item::<ResponsesHostDecodeError>)
            .collect::<Result<Vec<_>, _>>()?;
        validate_output_identity_roles::<ResponsesHostDecodeError>(&output)?;
        for item in &output {
            validate_bounded_unknown_item(item, limits)?;
        }
        let usage = object
            .get("usage")
            .filter(|value| !value.is_null())
            .map(|value| {
                ResponsesUsage::parse_with_policy::<ResponsesHostDecodeError>(value, usage_policy)
            })
            .transpose()?;
        let incomplete_details = object
            .get("incomplete_details")
            .filter(|value| !value.is_null())
            .map(|value| parse_incomplete_details(value, limits))
            .transpose()?;
        let canonical_error_type = if error_policy == ResponsesProviderErrorPolicy::OpenRouter {
            optional_provider_label(object, "error_type")?
        } else {
            None
        };
        let error = object
            .get("error")
            .filter(|value| !value.is_null())
            .map(|value| {
                parse_provider_error_with_canonical_type(
                    value,
                    limits,
                    error_policy,
                    canonical_error_type,
                )
            })
            .transpose()?;
        let terminal_shape_ok = match status {
            ResponsesResponseStatus::Completed => incomplete_details.is_none() && error.is_none(),
            ResponsesResponseStatus::Incomplete => incomplete_details.is_some() && error.is_none(),
            ResponsesResponseStatus::Failed => incomplete_details.is_none() && error.is_some(),
            ResponsesResponseStatus::Queued
            | ResponsesResponseStatus::InProgress
            | ResponsesResponseStatus::Unknown(_) => false,
        };
        if !terminal_shape_ok {
            return Err(ResponsesHostDecodeError::InvalidField(
                "terminal response semantics",
            ));
        }
        Ok(Self {
            id,
            model,
            status,
            end_turn,
            output,
            usage,
            incomplete_details,
            error,
            native: value.clone(),
        })
    }

    /// Provider response identity.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Exact provider-reported model.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Terminal response status.
    #[must_use]
    pub const fn status(&self) -> &ResponsesResponseStatus {
        &self.status
    }

    /// Explicit assistant turn completion signal, when supplied by the provider.
    #[must_use]
    pub const fn end_turn(&self) -> Option<bool> {
        self.end_turn
    }

    /// Exact ordered output items.
    #[must_use]
    pub fn output(&self) -> &[ResponsesOutputItem] {
        &self.output
    }

    /// Provider usage and cache/reasoning subsets.
    #[must_use]
    pub const fn usage(&self) -> Option<&ResponsesUsage> {
        self.usage.as_ref()
    }

    /// Incomplete-terminal details.
    #[must_use]
    pub const fn incomplete_details(&self) -> Option<&ResponsesIncompleteDetails> {
        self.incomplete_details.as_ref()
    }

    /// Failed-terminal provider error.
    #[must_use]
    pub const fn error(&self) -> Option<&ResponsesProviderError> {
        self.error.as_ref()
    }

    /// Exact bounded native aggregate.
    #[must_use]
    pub const fn native(&self) -> &Value {
        &self.native
    }
}

impl fmt::Debug for ResponsesResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesResponse")
            .field("id_bytes", &self.id.len())
            .field("model_bytes", &self.model.len())
            .field("status", &self.status)
            .field("end_turn", &self.end_turn)
            .field("output_items", &self.output.len())
            .field("usage", &self.usage)
            .field("incomplete", &self.incomplete_details.is_some())
            .field("error", &self.error)
            .field("native", &"<redacted>")
            .finish()
    }
}

/// Parses one output item from a JSON value.
///
/// # Errors
///
/// Returns `E` for structural failures.
pub fn parse_output_item<E>(value: &Value) -> Result<ResponsesOutputItem, E>
where
    E: From<ResponsesFieldError> + ResponsesUsageError,
{
    let object = value
        .as_object()
        .ok_or_else(|| E::invalid_field("output item"))?;
    let kind = required_provider_label(object, "type")?;
    match kind {
        "message" => parse_message(object, value),
        "reasoning" => parse_reasoning(object, value),
        "function_call" => parse_function_call(object, value),
        "custom_tool_call" => parse_custom_tool_call(object, value),
        "program" => {
            required_provider_label(object, "id")?;
            required_provider_label(object, "call_id")?;
            required_string(object, "code")?;
            required_string(object, "fingerprint")?;
            Ok(ResponsesOutputItem::Opaque {
                kind: kind.to_owned(),
                native: value.clone(),
            })
        }
        "program_output" => {
            required_provider_label(object, "id")?;
            required_provider_label(object, "call_id")?;
            required_string(object, "result")?;
            match required_provider_label(object, "status")? {
                "in_progress" | "completed" | "incomplete" => {}
                _ => return Err(E::invalid_field("program_output status")),
            }
            Ok(ResponsesOutputItem::Opaque {
                kind: kind.to_owned(),
                native: value.clone(),
            })
        }
        "compaction" => {
            let encrypted_content = required_string(object, "encrypted_content")?.to_owned();
            Ok(ResponsesOutputItem::Compaction {
                id: optional_provider_label(object, "id")?.map(str::to_owned),
                encrypted_content,
                native: value.clone(),
            })
        }
        other => {
            required_provider_label(object, "id")?;
            Ok(ResponsesOutputItem::Opaque {
                kind: other.to_owned(),
                native: value.clone(),
            })
        }
    }
}

/// Parses a message output item.
///
/// # Errors
///
/// Returns `E` for structural failures.
pub fn parse_message<E>(
    object: &Map<String, Value>,
    native: &Value,
) -> Result<ResponsesOutputItem, E>
where
    E: From<ResponsesFieldError> + ResponsesUsageError,
{
    let content = object
        .get("content")
        .ok_or_else(|| E::missing_field("content"))?
        .as_array()
        .ok_or_else(|| E::invalid_field("content"))?
        .iter()
        .map(|part| parse_content_part::<E>(part))
        .collect::<Result<Vec<_>, E>>()?;
    Ok(ResponsesOutputItem::Message {
        id: optional_provider_label(object, "id")?.map(str::to_owned),
        status: optional_provider_label(object, "status")?.map(str::to_owned),
        role: required_provider_label(object, "role")?.to_owned(),
        content,
        native: native.clone(),
    })
}

/// Parses one message content part.
///
/// # Errors
///
/// Returns `E` for structural failures.
pub fn parse_content_part<E>(value: &Value) -> Result<ResponsesContentPart, E>
where
    E: From<ResponsesFieldError> + ResponsesUsageError,
{
    let object = value
        .as_object()
        .ok_or_else(|| E::invalid_field("message content part"))?;
    let kind = required_provider_label(object, "type")?;
    match kind {
        "output_text" => Ok(ResponsesContentPart::OutputText {
            text: required_string(object, "text")?.to_owned(),
            annotations: match object.get("annotations") {
                None => Vec::new(),
                Some(value) => value
                    .as_array()
                    .ok_or_else(|| E::invalid_field("annotations"))?
                    .iter()
                    .map(parse_annotation::<E>)
                    .collect::<Result<Vec<_>, E>>()?,
            },
        }),
        "refusal" => Ok(ResponsesContentPart::Refusal {
            refusal: required_string(object, "refusal")?.to_owned(),
        }),
        other => Ok(ResponsesContentPart::Opaque {
            kind: other.to_owned(),
            native: value.clone(),
        }),
    }
}

/// Parses one output-text annotation into a typed or opaque family value.
///
/// # Errors
///
/// Returns `E` for malformed documented citation fields.
pub fn parse_annotation<E>(value: &Value) -> Result<ResponsesAnnotation, E>
where
    E: From<ResponsesFieldError> + ResponsesUsageError,
{
    let object = value
        .as_object()
        .ok_or_else(|| E::invalid_field("annotation"))?;
    let kind = required_provider_label(object, "type")?;
    if kind != "url_citation" {
        return Ok(ResponsesAnnotation::Opaque {
            kind: kind.to_owned(),
            native: value.clone(),
        });
    }
    let start_index = required_u64(object, "start_index")?;
    let end_index = required_u64(object, "end_index")?;
    if start_index > end_index {
        return Err(E::invalid_field("citation range"));
    }
    let url = required_string(object, "url")?;
    let title = required_string(object, "title")?;
    if url.is_empty() || title.is_empty() || title.len() > MAX_URL_CITATION_TITLE_BYTES {
        return Err(E::invalid_field("citation"));
    }
    Ok(ResponsesAnnotation::UrlCitation {
        start_index,
        end_index,
        url: url.to_owned(),
        title: title.to_owned(),
        native: value.clone(),
    })
}

/// Parses a reasoning output item.
///
/// # Errors
///
/// Returns `E` for structural failures.
pub fn parse_reasoning<E>(
    object: &Map<String, Value>,
    native: &Value,
) -> Result<ResponsesOutputItem, E>
where
    E: From<ResponsesFieldError> + ResponsesUsageError,
{
    let summary = match object.get("summary") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .cloned()
            .ok_or_else(|| E::invalid_field("summary"))?,
    }
    .into_iter()
    .map(|value| {
        let object = value
            .as_object()
            .ok_or_else(|| E::invalid_field("reasoning summary part"))?;
        let kind = required_provider_label(object, "type")?;
        if kind == "summary_text" {
            Ok(ResponsesReasoningSummaryPart::SummaryText(
                required_string(object, "text")?.to_owned(),
            ))
        } else {
            Ok(ResponsesReasoningSummaryPart::Opaque {
                kind: kind.to_owned(),
                native: value,
            })
        }
    })
    .collect::<Result<Vec<_>, E>>()?;
    let content = match object.get("content") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or_else(|| E::invalid_field("reasoning content"))?
            .iter()
            .map(|value| {
                let object = value
                    .as_object()
                    .ok_or_else(|| E::invalid_field("reasoning content part"))?;
                if required_provider_label(object, "type")? != "reasoning_text" {
                    return Err(E::invalid_field("reasoning content part"));
                }
                Ok(ResponsesReasoningContentPart::ReasoningText(
                    required_string(object, "text")?.to_owned(),
                ))
            })
            .collect::<Result<Vec<_>, E>>()?,
    };
    Ok(ResponsesOutputItem::Reasoning {
        id: optional_provider_label(object, "id")?.map(str::to_owned),
        status: optional_provider_label(object, "status")?.map(str::to_owned),
        summary,
        content,
        encrypted_content: optional_string(object, "encrypted_content")?.map(str::to_owned),
        native: native.clone(),
    })
}

/// Parses a function-call output item.
///
/// # Errors
///
/// Returns `E` for structural failures.
pub fn parse_function_call<E>(
    object: &Map<String, Value>,
    native: &Value,
) -> Result<ResponsesOutputItem, E>
where
    E: From<ResponsesFieldError> + ResponsesUsageError,
{
    Ok(ResponsesOutputItem::FunctionCall {
        id: optional_provider_label(object, "id")?.map(str::to_owned),
        status: optional_provider_label(object, "status")?.map(str::to_owned),
        call_id: required_provider_label(object, "call_id")?.to_owned(),
        name: required_provider_label(object, "name")?.to_owned(),
        arguments: required_string(object, "arguments")?.to_owned(),
        native: native.clone(),
    })
}

/// Parses a custom-tool-call output item.
///
/// # Errors
///
/// Returns `E` for structural failures.
pub fn parse_custom_tool_call<E>(
    object: &Map<String, Value>,
    native: &Value,
) -> Result<ResponsesOutputItem, E>
where
    E: From<ResponsesFieldError> + ResponsesUsageError,
{
    Ok(ResponsesOutputItem::CustomToolCall {
        id: optional_provider_label(object, "id")?.map(str::to_owned),
        status: optional_provider_label(object, "status")?.map(str::to_owned),
        call_id: required_provider_label(object, "call_id")?.to_owned(),
        name: required_provider_label(object, "name")?.to_owned(),
        input: required_string(object, "input")?.to_owned(),
        native: native.clone(),
    })
}

/// Returns the provider item status when present.
#[must_use]
pub fn output_item_status(item: &ResponsesOutputItem) -> Option<&str> {
    match item {
        ResponsesOutputItem::Message { status, .. }
        | ResponsesOutputItem::Reasoning { status, .. }
        | ResponsesOutputItem::FunctionCall { status, .. }
        | ResponsesOutputItem::CustomToolCall { status, .. } => status.as_deref(),
        ResponsesOutputItem::Opaque { native, .. }
        | ResponsesOutputItem::Compaction { native, .. } => {
            native.get("status").and_then(Value::as_str)
        }
    }
}

/// Validates assistant roles and unique item/call identities.
///
/// # Errors
///
/// Returns `E` when roles or correlation IDs are inconsistent.
pub fn validate_output_identity_roles<E>(output: &[ResponsesOutputItem]) -> Result<(), E>
where
    E: ResponsesUsageError,
{
    let mut item_ids = BTreeSet::new();
    let mut remote_call_ids = BTreeSet::new();
    for item in output {
        let item_id = match item {
            ResponsesOutputItem::Message { id, role, .. } => {
                if role != "assistant" {
                    return Err(E::invalid_field("output role"));
                }
                id.as_deref()
            }
            ResponsesOutputItem::Reasoning { id, .. }
            | ResponsesOutputItem::Compaction { id, .. } => id.as_deref(),
            ResponsesOutputItem::FunctionCall { id, call_id, .. }
            | ResponsesOutputItem::CustomToolCall { id, call_id, .. } => {
                if !remote_call_ids.insert(call_id.as_str()) {
                    return Err(E::invalid_field("function call_id"));
                }
                validate_program_caller::<E>(item.native())?;
                id.as_deref()
            }
            ResponsesOutputItem::Opaque { kind, native } => {
                if kind == "program" {
                    let _call_id = native
                        .get("call_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| E::missing_field("program call_id"))?;
                } else if kind == "program_output" {
                    let _call_id = native
                        .get("call_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| E::missing_field("program_output call_id"))?;
                }
                native.get("id").and_then(Value::as_str)
            }
        };
        if item_id.is_some_and(|item_id| !item_ids.insert(item_id)) {
            return Err(E::invalid_field("output item id"));
        }
    }
    Ok(())
}

fn validate_program_caller<E>(native: &Value) -> Result<(), E>
where
    E: ResponsesUsageError,
{
    let Some(caller) = native.get("caller") else {
        return Ok(());
    };
    if caller.is_null() {
        return Ok(());
    }
    let object = caller
        .as_object()
        .ok_or_else(|| E::invalid_field("tool caller"))?;
    if object.len() == 1 && object.get("type").and_then(Value::as_str) == Some("direct") {
        return Ok(());
    }
    if object.len() != 2 || object.get("type").and_then(Value::as_str) != Some("program") {
        return Err(E::invalid_field("tool caller"));
    }
    let _caller_id = object
        .get("caller_id")
        .and_then(Value::as_str)
        .filter(|caller_id| !caller_id.is_empty())
        .ok_or_else(|| E::missing_field("tool caller_id"))?;
    Ok(())
}

/// Parses a bounded provider error using one exact classification allowlist.
///
/// # Errors
///
/// Fails closed for malformed labels or an over-limit payload.
pub fn parse_provider_error_with_policy(
    value: &Value,
    limits: ResponsesResponseLimits,
    policy: ResponsesProviderErrorPolicy,
) -> Result<ResponsesProviderError, ResponsesHostDecodeError> {
    parse_provider_error_with_canonical_type(value, limits, policy, None)
}

/// Selects the object to parse from a provider HTTP error body.
///
/// OpenAI-compatible bodies nest `{ "error": { ... } }`. SuperGrok/xAI uses
/// a top-level `{ "code": "...", "error": "<string>" }`. A string `error`
/// field is the operator message, not a nested object.
pub(crate) fn provider_error_object(value: &Value) -> &Value {
    match value.get("error") {
        Some(nested @ Value::Object(_)) => nested,
        _ => value,
    }
}

pub(crate) fn parse_provider_error_with_canonical_type(
    value: &Value,
    limits: ResponsesResponseLimits,
    policy: ResponsesProviderErrorPolicy,
    canonical_error_type: Option<&str>,
) -> Result<ResponsesProviderError, ResponsesHostDecodeError> {
    if encoded_len(value) > limits.max_unknown_bytes() {
        return Err(ResponsesHostDecodeError::ResourceLimit(
            "provider error bytes",
        ));
    }
    let object = value
        .as_object()
        .ok_or(ResponsesHostDecodeError::InvalidField("error"))?;
    let payment_required = policy == ResponsesProviderErrorPolicy::OpenRouter
        && object.get("code").and_then(Value::as_u64) == Some(402);
    let code = if payment_required {
        None
    } else {
        optional_provider_label(object, "code")?.map(str::to_owned)
    };
    let native_kind = optional_provider_label(object, "type")?
        .or(optional_provider_label(object, "kind")?)
        .map(str::to_owned);
    let native_usage_limit = native_kind
        .as_deref()
        .is_some_and(plakous_attempt::terminal_usage_error);
    let kind = canonical_error_type.map(str::to_owned).or(native_kind);
    let class = if payment_required
        || code
            .as_deref()
            .is_some_and(plakous_attempt::terminal_usage_error)
        || kind
            .as_deref()
            .is_some_and(plakous_attempt::terminal_usage_error)
        || native_usage_limit
    {
        ResponsesProviderErrorClass::Quota
    } else {
        match policy {
            ResponsesProviderErrorPolicy::OpenAiCompatible => {
                match code.as_deref().or(kind.as_deref()) {
                    Some("authentication_error" | "invalid_api_key" | "unauthorized") => {
                        ResponsesProviderErrorClass::Authentication
                    }
                    Some("rate_limit_exceeded" | "rate_limit_error") => {
                        ResponsesProviderErrorClass::RateLimited
                    }
                    Some("server_error" | "internal_error" | "server_is_overloaded") => {
                        ResponsesProviderErrorClass::Server
                    }
                    _ => ResponsesProviderErrorClass::Unknown,
                }
            }
            ResponsesProviderErrorPolicy::OpenRouter => match canonical_error_type {
                Some("authentication" | "permission_denied") => {
                    ResponsesProviderErrorClass::Authentication
                }
                Some("rate_limit_exceeded") => ResponsesProviderErrorClass::RateLimited,
                Some("provider_overloaded" | "provider_unavailable" | "timeout" | "server") => {
                    ResponsesProviderErrorClass::Server
                }
                Some(_) => ResponsesProviderErrorClass::Unknown,
                None => match code.as_deref().or(kind.as_deref()) {
                    Some("authentication_error" | "invalid_api_key" | "unauthorized") => {
                        ResponsesProviderErrorClass::Authentication
                    }
                    Some("rate_limit_exceeded" | "rate_limit_error") => {
                        ResponsesProviderErrorClass::RateLimited
                    }
                    Some("server_error" | "internal_error" | "server_is_overloaded") => {
                        ResponsesProviderErrorClass::Server
                    }
                    _ => ResponsesProviderErrorClass::Unknown,
                },
            },
            ResponsesProviderErrorPolicy::Grok => match (kind.as_deref(), code.as_deref()) {
                (Some("authentication_error"), _)
                | (_, Some("invalid_api_key" | "authentication_error")) => {
                    ResponsesProviderErrorClass::Authentication
                }
                (_, Some("rate_limit_exceeded")) | (Some("rate_limit_error"), _) => {
                    ResponsesProviderErrorClass::RateLimited
                }
                (_, Some("server_error")) | (Some("server_error"), _) => {
                    ResponsesProviderErrorClass::Server
                }
                _ => ResponsesProviderErrorClass::Unknown,
            },
        }
    };
    Ok(ResponsesProviderError {
        code,
        kind,
        request_id: None,
        class,
        native: value.clone(),
    })
}

fn parse_incomplete_details(
    value: &Value,
    limits: ResponsesResponseLimits,
) -> Result<ResponsesIncompleteDetails, ResponsesHostDecodeError> {
    if encoded_len(value) > limits.max_unknown_bytes() {
        return Err(ResponsesHostDecodeError::ResourceLimit(
            "incomplete detail bytes",
        ));
    }
    let object = value
        .as_object()
        .ok_or(ResponsesHostDecodeError::InvalidField("incomplete_details"))?;
    let reason = optional_provider_label(object, "reason")?.map(str::to_owned);
    Ok(ResponsesIncompleteDetails {
        reason,
        native: value.clone(),
    })
}

pub(crate) fn validate_bounded_unknown_item(
    item: &ResponsesOutputItem,
    limits: ResponsesResponseLimits,
) -> Result<(), ResponsesHostDecodeError> {
    match item {
        ResponsesOutputItem::Opaque { native, .. }
            if encoded_len(native) > limits.max_unknown_bytes() =>
        {
            Err(ResponsesHostDecodeError::ResourceLimit(
                "unknown output item bytes",
            ))
        }
        ResponsesOutputItem::Message { content, .. } => {
            for part in content {
                match part {
                    ResponsesContentPart::Opaque { native, .. }
                        if encoded_len(native) > limits.max_unknown_bytes() =>
                    {
                        return Err(ResponsesHostDecodeError::ResourceLimit(
                            "unknown content part bytes",
                        ));
                    }
                    ResponsesContentPart::OutputText { annotations, .. } => {
                        for annotation in annotations {
                            if matches!(annotation, ResponsesAnnotation::Opaque { native, .. } if encoded_len(native) > limits.max_unknown_bytes())
                            {
                                return Err(ResponsesHostDecodeError::ResourceLimit(
                                    "unknown annotation bytes",
                                ));
                            }
                        }
                    }
                    ResponsesContentPart::Refusal { .. } | ResponsesContentPart::Opaque { .. } => {}
                }
            }
            Ok(())
        }
        ResponsesOutputItem::Reasoning { summary, .. } => {
            for part in summary {
                if matches!(part, ResponsesReasoningSummaryPart::Opaque { native, .. } if encoded_len(native) > limits.max_unknown_bytes())
                {
                    return Err(ResponsesHostDecodeError::ResourceLimit(
                        "unknown reasoning part bytes",
                    ));
                }
            }
            Ok(())
        }
        ResponsesOutputItem::FunctionCall { .. }
        | ResponsesOutputItem::CustomToolCall { .. }
        | ResponsesOutputItem::Opaque { .. }
        | ResponsesOutputItem::Compaction { .. } => Ok(()),
    }
}

fn encoded_len(value: &Value) -> usize {
    value.to_string().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_numeric_payment_required_is_quota_without_a_synthesized_code() {
        let value = serde_json::json!({"code":402,"message":"fixture billing refusal"});
        let Ok(error) = parse_provider_error_with_policy(
            &value,
            ResponsesResponseLimits::default(),
            ResponsesProviderErrorPolicy::OpenRouter,
        ) else {
            panic!("numeric payment-required evidence must decode");
        };
        assert_eq!(error.class(), ResponsesProviderErrorClass::Quota);
        assert_eq!(error.code(), None);
        assert_eq!(error.native(), &value);
    }

    #[test]
    fn openai_overload_code_is_a_typed_server_failure() {
        let Ok(error) = parse_provider_error_with_policy(
            &serde_json::json!({
                "code": "server_is_overloaded",
                "message": "bounded provider detail"
            }),
            ResponsesResponseLimits::default(),
            ResponsesProviderErrorPolicy::OpenAiCompatible,
        ) else {
            panic!("observed Codex overload terminal must decode");
        };
        assert_eq!(error.class(), ResponsesProviderErrorClass::Server);
        assert_eq!(error.code(), Some("server_is_overloaded"));
        assert_eq!(error.message(), Some("bounded provider detail"));
    }

    #[test]
    fn openrouter_failed_response_prefers_canonical_type_and_retains_native_code() {
        let value = serde_json::json!({
            "id": "resp_openrouter_failed",
            "model": "openai/o4-mini",
            "status": "failed",
            "error": {"code": "server_error", "message": "redacted"},
            "error_type": "authentication"
        });
        let Ok(response) = ResponsesResponse::parse_with_error_policy(
            &value,
            ResponsesUsagePolicy::RequireTotal,
            ResponsesProviderErrorPolicy::OpenRouter,
            ResponsesResponseLimits::default(),
        ) else {
            panic!("documented OpenRouter failed response must decode");
        };
        let Some(error) = response.error() else {
            panic!("failed response must retain error");
        };
        assert_eq!(error.class(), ResponsesProviderErrorClass::Authentication);
        assert_eq!(error.kind(), Some("authentication"));
        assert_eq!(error.code(), Some("server_error"));
        assert_eq!(error.native(), &value["error"]);
    }

    #[test]
    fn openrouter_unknown_canonical_type_does_not_inherit_lossy_native_class() {
        let Ok(error) = parse_provider_error_with_canonical_type(
            &serde_json::json!({"code": "server_error", "message": "redacted"}),
            ResponsesResponseLimits::default(),
            ResponsesProviderErrorPolicy::OpenRouter,
            Some("future_error_type"),
        ) else {
            panic!("bounded future canonical type must be retained");
        };
        assert_eq!(error.class(), ResponsesProviderErrorClass::Unknown);
        assert_eq!(error.kind(), Some("future_error_type"));
        assert_eq!(error.code(), Some("server_error"));
    }

    #[test]
    fn opaque_output_items_may_omit_status() {
        let value = serde_json::json!({
            "id": "opaque_1",
            "type": "codex_something",
            "encrypted_content": "ciphertext"
        });
        let Ok(item) = parse_output_item::<ResponsesHostDecodeError>(&value) else {
            panic!("opaque item without status must parse");
        };
        assert!(matches!(
            item,
            ResponsesOutputItem::Opaque { kind, .. } if kind == "codex_something"
        ));
    }

    #[test]
    fn program_items_and_callers_validate_exact_order_and_shape() {
        let response = serde_json::json!({
            "id":"resp_ptc","object":"response","status":"completed","model":"gpt-5.6-sol",
            "output":[
                {"type":"program","id":"prog_1","call_id":"pcall_1","code":"await lookup({q:'x'})","fingerprint":"opaque-fp"},
                {"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{\"q\":\"x\"}","caller":{"type":"program","caller_id":"pcall_1"}},
                {"type":"program_output","id":"po_1","call_id":"pcall_1","result":"done","status":"completed"}
            ]
        });
        let Ok(parsed) = ResponsesResponse::parse(
            &response,
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
        ) else {
            panic!("exact program sequence must decode");
        };
        assert_eq!(parsed.output()[0].native()["fingerprint"], "opaque-fp");
        assert_eq!(
            parsed.output()[1].native()["caller"]["caller_id"],
            "pcall_1"
        );

        for malformed in [
            serde_json::json!({"type":"program","id":"p","call_id":"c","code":"x"}),
            serde_json::json!({"type":"program","id":"p","call_id":"c","fingerprint":"fp"}),
            serde_json::json!({"type":"program","id":"p","code":"x","fingerprint":"fp"}),
            serde_json::json!({"type":"program","call_id":"c","code":"x","fingerprint":"fp"}),
            serde_json::json!({"type":"program_output","id":"o","call_id":"c","status":"completed"}),
            serde_json::json!({"type":"program_output","id":"o","result":"x","status":"completed"}),
            serde_json::json!({"type":"program_output","call_id":"c","result":"x","status":"completed"}),
            serde_json::json!({"type":"function_call","id":"f","call_id":"c","name":"lookup","arguments":"{}","caller":{"type":"program"}}),
            serde_json::json!({"type":"function_call","id":"f","call_id":"c","name":"lookup","arguments":"{}","caller":{"type":"program","caller_id":"c","extra":true}}),
            serde_json::json!({"type":"program_output","id":"o","call_id":"c","result":"x","status":"future"}),
        ] {
            let candidate = serde_json::json!({
                "id":"bad","object":"response","status":"completed","model":"gpt-5.6-sol",
                "output":[malformed]
            });
            assert!(
                ResponsesResponse::parse(
                    &candidate,
                    ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                    ResponsesResponseLimits::default(),
                )
                .is_err()
            );
        }

        for later in [
            serde_json::json!({"type":"function_call","id":"f","call_id":"c","name":"lookup","arguments":"{}","caller":{"type":"program","caller_id":"prior"}}),
            serde_json::json!({"type":"program_output","id":"o","call_id":"prior","result":"x","status":"completed"}),
        ] {
            let candidate = serde_json::json!({
                "id":"later","object":"response","status":"completed","model":"gpt-5.6-sol",
                "output":[later]
            });
            assert!(
                ResponsesResponse::parse(
                    &candidate,
                    ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                    ResponsesResponseLimits::default(),
                )
                .is_ok(),
                "aggregate decoding must defer prior-artifact lineage to the projector"
            );
        }

        let incomplete = serde_json::json!({
            "id":"resp_ptc_incomplete","object":"response","status":"incomplete","model":"gpt-5.6-sol",
            "incomplete_details":{"reason":"max_output_tokens"},
            "output":[
                {"type":"program","id":"p","call_id":"c","code":"x","fingerprint":"fp"},
                {"type":"program_output","id":"o","call_id":"c","result":"partial","status":"incomplete"}
            ]
        });
        assert!(
            ResponsesResponse::parse(
                &incomplete,
                ResponsesUsagePolicy::DeriveTotalWhenOmitted,
                ResponsesResponseLimits::default(),
            )
            .is_ok()
        );
    }

    #[test]
    fn unknown_output_item_is_opaque_and_member_preserving() {
        let native = serde_json::json!({
            "type":"future_provider_item","id":"future_1","future_member":{"nested":true}
        });
        let Ok(item) = parse_output_item::<ResponsesHostDecodeError>(&native) else {
            panic!("bounded unknown output item must decode");
        };
        assert!(
            matches!(&item, ResponsesOutputItem::Opaque { kind, .. } if kind == "future_provider_item")
        );
        assert_eq!(item.native(), &native);
    }
}
