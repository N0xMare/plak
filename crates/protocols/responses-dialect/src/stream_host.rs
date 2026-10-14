//! Shared stream front-half around [`ResponsesStreamKernel`].
//!
//! Dialects keep JSON event-kind parse hooks; the host owns admitted push/finish
//! trial semantics and common error maps / event-kind shapes.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use plakous_attempt::{is_decoding, mark_decode_failed, mark_decoded};
use plakous_http::SseError;
use plakous_model::{Digest, DigestDomain};
use plakous_responses::{
    BomPolicy, DialectStreamHooks, FinishPolicy, LifecycleDialectPolicy, ReasoningLifecycleMode,
    ResponsesDecodeLimits, ResponsesDialectProfile, ResponsesFieldError, ResponsesLifecycle,
    ResponsesLifecycleError, ResponsesStreamKernel, ResponsesStreamLimits, SequencePolicy,
    SequenceValidationError, TerminalPolicy, required_object, required_provider_label,
    required_string, required_u32,
};
use serde_json::{Map, Value};

use crate::response_host::{
    ResponsesAnnotation, ResponsesHostDecodeError, ResponsesOutputItem, ResponsesProviderError,
    ResponsesProviderErrorPolicy, ResponsesResponse, ResponsesResponseLimits,
    ResponsesResponseStatus, ResponsesUsagePolicy, parse_annotation, parse_output_item,
    parse_provider_error_with_canonical_type, parse_unique_json_value,
    validate_bounded_unknown_item,
};

/// Typed projection of documented Responses stream events.
///
/// `ResponseCreated` uses optional identity fields so xAI (optional) and
/// OpenRouter (required-at-parse) share one shape; OpenRouter parse always
/// fills `Some`.
#[derive(Clone, PartialEq)]
#[non_exhaustive]
pub enum ResponsesStreamEventKind<Response, ProviderError> {
    /// The response envelope was created.
    ResponseCreated {
        /// Provider response ID when reported.
        response_id: Option<String>,
        /// Exact provider-reported model when reported.
        model: Option<String>,
    },
    /// A later in-progress envelope reaffirmed response identity.
    ResponseInProgress {
        /// Provider response ID.
        response_id: String,
        /// Exact provider model.
        model: String,
    },
    /// An ordered output item was announced.
    OutputItemAdded {
        /// Output-array index.
        output_index: u32,
        /// Initial typed or opaque item.
        item: ResponsesOutputItem,
    },
    /// An output item reached its complete representation.
    OutputItemDone {
        /// Output-array index.
        output_index: u32,
        /// Complete typed or opaque item.
        item: ResponsesOutputItem,
    },
    /// A message content part was announced.
    ContentPartAdded {
        /// Output-array index.
        output_index: u32,
        /// Content-array index.
        content_index: u32,
        /// Full provider-native part.
        part: Value,
    },
    /// A message content part completed.
    ContentPartDone {
        /// Output-array index.
        output_index: u32,
        /// Content-array index.
        content_index: u32,
        /// Full provider-native part.
        part: Value,
    },
    /// Complete visible output text for lifecycle validation.
    OutputTextDone {
        /// Output-array index.
        output_index: u32,
        /// Content-array index.
        content_index: u32,
        /// Complete text.
        text: String,
    },
    /// Visible output text bytes.
    OutputTextDelta {
        /// Output-array index.
        output_index: u32,
        /// Content-array index.
        content_index: u32,
        /// Text delta.
        delta: String,
    },
    /// Visible refusal bytes.
    RefusalDelta {
        /// Output-array index.
        output_index: u32,
        /// Content-array index.
        content_index: u32,
        /// Refusal-text delta.
        delta: String,
    },
    /// Complete refusal text for lifecycle validation.
    RefusalDone {
        /// Output-array index.
        output_index: u32,
        /// Content-array index.
        content_index: u32,
        /// Complete refusal text.
        refusal: String,
    },
    /// Provider-authorized visible reasoning bytes.
    ReasoningTextDelta {
        /// Output-array index.
        output_index: u32,
        /// Reasoning content-array index.
        content_index: u32,
        /// Provider item ID when reported.
        item_id: Option<String>,
        /// Visible reasoning-text delta.
        delta: String,
    },
    /// Complete visible reasoning text for lifecycle validation.
    ReasoningTextDone {
        /// Output-array index.
        output_index: u32,
        /// Reasoning content-array index.
        content_index: u32,
        /// Provider item ID when reported.
        item_id: Option<String>,
        /// Complete visible reasoning text.
        text: String,
    },
    /// A reasoning summary part was announced.
    ReasoningSummaryPartAdded {
        /// Output-array index.
        output_index: u32,
        /// Summary-array index.
        summary_index: u32,
        /// Initial provider-native summary part.
        part: Value,
    },
    /// A provider-authorized reasoning summary delta.
    ReasoningSummaryTextDelta {
        /// Output-array index.
        output_index: u32,
        /// Summary-array index.
        summary_index: u32,
        /// Summary text delta.
        delta: String,
    },
    /// Complete reasoning summary text for lifecycle validation.
    ReasoningSummaryTextDone {
        /// Output-array index.
        output_index: u32,
        /// Summary-array index.
        summary_index: u32,
        /// Complete summary text.
        text: String,
    },
    /// A reasoning summary part completed.
    ReasoningSummaryPartDone {
        /// Output-array index.
        output_index: u32,
        /// Summary-array index.
        summary_index: u32,
        /// Complete provider-native summary part.
        part: Value,
    },
    /// A fragment of raw function-call argument JSON.
    FunctionCallArgumentsDelta {
        /// Output-array index.
        output_index: u32,
        /// Provider item ID when reported.
        item_id: Option<String>,
        /// Raw argument-text delta.
        delta: String,
    },
    /// A complete function-call argument string.
    FunctionCallArgumentsDone {
        /// Output-array index.
        output_index: u32,
        /// Provider item ID when reported.
        item_id: Option<String>,
        /// Complete raw argument JSON text.
        arguments: String,
    },
    /// A fragment of arbitrary custom-tool input.
    CustomToolCallInputDelta {
        /// Output-array index.
        output_index: u32,
        /// Provider item ID when reported.
        item_id: Option<String>,
        /// Raw custom-tool input delta.
        delta: String,
    },
    /// Complete arbitrary custom-tool input.
    CustomToolCallInputDone {
        /// Output-array index.
        output_index: u32,
        /// Provider item ID when reported.
        item_id: Option<String>,
        /// Complete raw custom-tool input.
        input: String,
    },
    /// A typed output-text annotation/citation was announced.
    OutputTextAnnotationAdded {
        /// Output-array index.
        output_index: u32,
        /// Content-array index.
        content_index: u32,
        /// Annotation-array index.
        annotation_index: u32,
        /// Typed or bounded opaque annotation.
        annotation: ResponsesAnnotation,
    },
    /// A successful terminal response.
    ResponseCompleted(Response),
    /// A terminal response with explicit incomplete details.
    ResponseIncomplete(Response),
    /// A failed terminal response retaining its response/error payload.
    ResponseFailed(Response),
    /// A terminal in-stream provider error.
    Error(ProviderError),
    /// A future event retained exactly without a positive feature claim.
    Opaque {
        /// Provider event type discriminator.
        kind: String,
    },
    /// A future event retained as an exact bounded typed unknown.
    Unknown {
        /// Provider event type discriminator.
        kind: String,
        /// Exact bounded provider-native event.
        native: Value,
    },
}

impl<Response, ProviderError> ResponsesStreamEventKind<Response, ProviderError> {
    /// Reports whether this event is a typed terminal condition.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::ResponseCompleted(_)
                | Self::ResponseIncomplete(_)
                | Self::ResponseFailed(_)
                | Self::Error(_)
        )
    }
}

impl<Response, ProviderError> ResponsesStreamEventKind<Response, ProviderError> {
    /// Applies non-terminal lifecycle rules for intermediate stream events.
    ///
    /// Returns `Ok(true)` when the event was fully handled, `Ok(false)` when the
    /// dialect must run terminal/response-specific checks (completed/incomplete/
    /// failed aggregates).
    ///
    /// # Errors
    ///
    /// Propagates [`ResponsesLifecycleError`] from the pure lifecycle machine.
    pub fn apply_intermediate_lifecycle(
        &self,
        lifecycle: &mut ResponsesLifecycle,
        object: &Map<String, Value>,
        expected_model: &str,
    ) -> Result<bool, ResponsesLifecycleError> {
        match self {
            Self::ResponseCreated { response_id, model } => {
                lifecycle.response_created(
                    object,
                    response_id.as_deref(),
                    model.as_deref(),
                    expected_model,
                )?;
                Ok(true)
            }
            Self::OutputItemAdded { output_index, item } => {
                lifecycle.output_item_added(*output_index, item.native())?;
                Ok(true)
            }
            Self::OutputItemDone { output_index, item } => {
                lifecycle.output_item_done(*output_index, item.native())?;
                Ok(true)
            }
            Self::ContentPartAdded {
                output_index,
                content_index,
                part,
            } => {
                lifecycle.content_part_added(*output_index, *content_index, part)?;
                Ok(true)
            }
            Self::ContentPartDone {
                output_index,
                content_index,
                part,
            } => {
                lifecycle.content_part_done(*output_index, *content_index, part)?;
                Ok(true)
            }
            Self::OutputTextDone {
                output_index,
                content_index,
                text,
            } => {
                lifecycle.output_text_done(*output_index, *content_index, text)?;
                Ok(true)
            }
            Self::OutputTextDelta {
                output_index,
                content_index,
                delta,
            } => {
                lifecycle.output_text_delta(*output_index, *content_index, delta)?;
                Ok(true)
            }
            Self::RefusalDelta {
                output_index,
                content_index,
                delta,
            } => {
                lifecycle.refusal_delta(*output_index, *content_index, delta)?;
                Ok(true)
            }
            Self::RefusalDone {
                output_index,
                content_index,
                refusal,
            } => {
                lifecycle.refusal_done(*output_index, *content_index, refusal)?;
                Ok(true)
            }
            Self::ReasoningTextDelta {
                output_index,
                content_index,
                item_id,
                delta,
            } => {
                lifecycle.reasoning_content_text_delta(
                    *output_index,
                    *content_index,
                    item_id.as_deref(),
                    delta,
                )?;
                Ok(true)
            }
            Self::ReasoningTextDone {
                output_index,
                content_index,
                item_id,
                text,
            } => {
                lifecycle.reasoning_content_text_done(
                    *output_index,
                    *content_index,
                    item_id.as_deref(),
                    text,
                )?;
                Ok(true)
            }
            Self::ReasoningSummaryPartAdded {
                output_index,
                summary_index,
                part,
            } => {
                lifecycle.reasoning_part_added(*output_index, *summary_index, part)?;
                Ok(true)
            }
            Self::ReasoningSummaryTextDelta {
                output_index,
                summary_index,
                delta,
            } => {
                lifecycle.reasoning_delta(*output_index, *summary_index, delta)?;
                Ok(true)
            }
            Self::ReasoningSummaryTextDone {
                output_index,
                summary_index,
                text,
            } => {
                lifecycle.reasoning_text_done(*output_index, *summary_index, text)?;
                Ok(true)
            }
            Self::ReasoningSummaryPartDone {
                output_index,
                summary_index,
                part,
            } => {
                lifecycle.reasoning_part_done(*output_index, *summary_index, part)?;
                Ok(true)
            }
            Self::FunctionCallArgumentsDelta {
                output_index,
                item_id,
                delta,
            }
            | Self::CustomToolCallInputDelta {
                output_index,
                item_id,
                delta,
            } => {
                lifecycle.function_arguments_delta(*output_index, item_id.as_deref(), delta)?;
                Ok(true)
            }
            Self::FunctionCallArgumentsDone {
                output_index,
                item_id,
                arguments,
            }
            | Self::CustomToolCallInputDone {
                output_index,
                item_id,
                input: arguments,
            } => {
                lifecycle.function_arguments_done(*output_index, item_id.as_deref(), arguments)?;
                Ok(true)
            }
            Self::ResponseCompleted(_) | Self::ResponseIncomplete(_) | Self::ResponseFailed(_) => {
                Ok(false)
            }
            Self::ResponseInProgress { .. }
            | Self::OutputTextAnnotationAdded { .. }
            | Self::Error(_)
            | Self::Opaque { .. }
            | Self::Unknown { .. } => Ok(true),
        }
    }
}

impl<Response: fmt::Debug, ProviderError: fmt::Debug> fmt::Debug
    for ResponsesStreamEventKind<Response, ProviderError>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redacted structural Debug: never dumps provider payloads.
        match self {
            Self::ResponseCreated { .. } => f
                .debug_struct("ResponseCreated")
                .field("provider_identity", &"<redacted>")
                .finish(),
            Self::ResponseInProgress { .. } => f
                .debug_struct("ResponseInProgress")
                .field("provider_identity", &"<redacted>")
                .finish(),
            Self::OutputItemAdded { output_index, .. }
            | Self::OutputItemDone { output_index, .. } => {
                let name = if matches!(self, Self::OutputItemAdded { .. }) {
                    "OutputItemAdded"
                } else {
                    "OutputItemDone"
                };
                f.debug_struct(name)
                    .field("output_index", output_index)
                    .field("item", &"<provider item redacted>")
                    .finish()
            }
            Self::ContentPartAdded {
                output_index,
                content_index,
                ..
            }
            | Self::ContentPartDone {
                output_index,
                content_index,
                ..
            } => {
                let name = if matches!(self, Self::ContentPartAdded { .. }) {
                    "ContentPartAdded"
                } else {
                    "ContentPartDone"
                };
                f.debug_struct(name)
                    .field("output_index", output_index)
                    .field("content_index", content_index)
                    .field("part", &"<redacted>")
                    .finish()
            }
            Self::OutputTextDone {
                output_index,
                content_index,
                text,
            }
            | Self::OutputTextDelta {
                output_index,
                content_index,
                delta: text,
            }
            | Self::RefusalDelta {
                output_index,
                content_index,
                delta: text,
            }
            | Self::RefusalDone {
                output_index,
                content_index,
                refusal: text,
            }
            | Self::ReasoningTextDelta {
                output_index,
                content_index,
                delta: text,
                ..
            }
            | Self::ReasoningTextDone {
                output_index,
                content_index,
                text,
                ..
            } => {
                let name = match self {
                    Self::OutputTextDone { .. } => "OutputTextDone",
                    Self::OutputTextDelta { .. } => "OutputTextDelta",
                    Self::RefusalDelta { .. } => "RefusalDelta",
                    Self::RefusalDone { .. } => "RefusalDone",
                    Self::ReasoningTextDelta { .. } => "ReasoningTextDelta",
                    _ => "ReasoningTextDone",
                };
                f.debug_struct(name)
                    .field("output_index", output_index)
                    .field("content_index", content_index)
                    .field("bytes", &text.len())
                    .finish()
            }
            Self::ReasoningSummaryPartAdded {
                output_index,
                summary_index,
                ..
            }
            | Self::ReasoningSummaryPartDone {
                output_index,
                summary_index,
                ..
            }
            | Self::ReasoningSummaryTextDelta {
                output_index,
                summary_index,
                ..
            }
            | Self::ReasoningSummaryTextDone {
                output_index,
                summary_index,
                ..
            } => {
                let name = match self {
                    Self::ReasoningSummaryPartAdded { .. } => "ReasoningSummaryPartAdded",
                    Self::ReasoningSummaryPartDone { .. } => "ReasoningSummaryPartDone",
                    Self::ReasoningSummaryTextDelta { .. } => "ReasoningSummaryTextDelta",
                    _ => "ReasoningSummaryTextDone",
                };
                f.debug_struct(name)
                    .field("output_index", output_index)
                    .field("summary_index", summary_index)
                    .finish()
            }
            Self::FunctionCallArgumentsDelta {
                output_index,
                item_id,
                delta,
            }
            | Self::FunctionCallArgumentsDone {
                output_index,
                item_id,
                arguments: delta,
            }
            | Self::CustomToolCallInputDelta {
                output_index,
                item_id,
                delta,
            }
            | Self::CustomToolCallInputDone {
                output_index,
                item_id,
                input: delta,
            } => {
                let name = match self {
                    Self::FunctionCallArgumentsDelta { .. } => "FunctionCallArgumentsDelta",
                    Self::FunctionCallArgumentsDone { .. } => "FunctionCallArgumentsDone",
                    Self::CustomToolCallInputDelta { .. } => "CustomToolCallInputDelta",
                    _ => "CustomToolCallInputDone",
                };
                f.debug_struct(name)
                    .field("output_index", output_index)
                    .field("item_id", &item_id.as_ref().map(|_| "<redacted>"))
                    .field("bytes", &delta.len())
                    .finish()
            }
            Self::OutputTextAnnotationAdded {
                output_index,
                content_index,
                annotation_index,
                ..
            } => f
                .debug_struct("OutputTextAnnotationAdded")
                .field("output_index", output_index)
                .field("content_index", content_index)
                .field("annotation_index", annotation_index)
                .field("annotation", &"<redacted>")
                .finish(),
            Self::ResponseCompleted(r) => f.debug_tuple("ResponseCompleted").field(r).finish(),
            Self::ResponseIncomplete(r) => f.debug_tuple("ResponseIncomplete").field(r).finish(),
            Self::ResponseFailed(r) => f.debug_tuple("ResponseFailed").field(r).finish(),
            Self::Error(e) => f.debug_tuple("Error").field(e).finish(),
            Self::Opaque { .. } => f
                .debug_struct("Opaque")
                .field("kind", &"<redacted>")
                .finish(),
            Self::Unknown { .. } => f
                .debug_struct("Unknown")
                .field("kind", &"<redacted>")
                .field("native", &"<redacted>")
                .finish(),
        }
    }
}

/// Closed capability data for one admitted Responses stream dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesStreamCapabilities {
    enabled: u8,
}

impl ResponsesStreamCapabilities {
    const CUSTOM_TOOLS: u8 = 1;
    const CITATIONS: u8 = 1 << 1;
    const INCOMPLETE_TERMINAL: u8 = 1 << 2;
    const FAILED_TERMINAL: u8 = 1 << 3;
    const RETAIN_UNKNOWN: u8 = 1 << 4;
    const DONE_TERMINAL: u8 = 1 << 5;
    const REASONING_CONTENT_TEXT: u8 = 1 << 6;

    /// Capability data required by the Codex Responses surface.
    #[must_use]
    pub const fn codex() -> Self {
        Self {
            enabled: Self::CUSTOM_TOOLS
                | Self::CITATIONS
                | Self::INCOMPLETE_TERMINAL
                | Self::FAILED_TERMINAL
                | Self::RETAIN_UNKNOWN,
        }
    }

    /// Builds a dialect with no optional extension surface admitted.
    #[must_use]
    pub const fn minimal() -> Self {
        Self { enabled: 0 }
    }

    /// Capability data required by the SuperGrok Responses surface.
    #[must_use]
    pub const fn grok() -> Self {
        Self {
            enabled: Self::INCOMPLETE_TERMINAL | Self::FAILED_TERMINAL | Self::RETAIN_UNKNOWN,
        }
    }

    /// Capability data admitted by the OpenRouter beta fixture surface.
    #[must_use]
    pub const fn openrouter() -> Self {
        Self {
            enabled: Self::INCOMPLETE_TERMINAL
                | Self::FAILED_TERMINAL
                | Self::DONE_TERMINAL
                | Self::REASONING_CONTENT_TEXT,
        }
    }

    const fn has(self, capability: u8) -> bool {
        self.enabled & capability != 0
    }
}

/// Closed typed-event allowlist for one Responses dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesEventPolicy {
    /// Shared OpenAI-compatible documented event surface.
    OpenAiCompatible,
    /// Exact documented OpenRouter Responses beta event surface.
    OpenRouter,
    /// Exact SuperGrok typed surface; every other bounded event is preserved unknown.
    Grok,
}

impl ResponsesEventPolicy {
    fn is_typed(self, kind: &str) -> bool {
        match self {
            Self::OpenAiCompatible => true,
            Self::OpenRouter => matches!(
                kind,
                "response.created"
                    | "response.in_progress"
                    | "response.output_item.added"
                    | "response.output_item.done"
                    | "response.content_part.added"
                    | "response.content_part.delta"
                    | "response.content_part.done"
                    | "response.output_text.delta"
                    | "response.output_text.done"
                    | "response.refusal.delta"
                    | "response.refusal.done"
                    | "response.reasoning_text.delta"
                    | "response.reasoning_text.done"
                    | "response.reasoning.delta"
                    | "response.reasoning_summary_part.added"
                    | "response.reasoning_summary_text.delta"
                    | "response.reasoning_summary_text.done"
                    | "response.reasoning_summary_part.done"
                    | "response.function_call_arguments.delta"
                    | "response.function_call_arguments.done"
                    | "response.completed"
                    | "response.incomplete"
                    | "response.failed"
                    | "response.done"
                    | "response.error"
                    | "error"
            ),
            Self::Grok => matches!(
                kind,
                "response.created"
                    | "response.output_item.added"
                    | "response.output_item.done"
                    | "response.content_part.added"
                    | "response.content_part.done"
                    | "response.output_text.delta"
                    | "response.reasoning_summary_text.delta"
                    | "response.function_call_arguments.delta"
                    | "response.function_call_arguments.done"
                    | "response.completed"
                    | "response.incomplete"
                    | "response.failed"
                    | "error"
            ),
        }
    }
}

/// Whether stream JSON rejects duplicate object members before Value decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponsesJsonPolicy {
    /// Standard serde JSON decoding.
    Standard,
    /// Reject duplicate members at every nesting depth.
    UniqueMembers,
}

/// Immutable family/dialect construction data for concrete stream decoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesStreamDialectData {
    profile: ResponsesDialectProfile,
    lifecycle: LifecycleDialectPolicy,
    expected_model: String,
    usage_policy: ResponsesUsagePolicy,
    response_limits: ResponsesResponseLimits,
    capabilities: ResponsesStreamCapabilities,
    event_policy: ResponsesEventPolicy,
    error_policy: ResponsesProviderErrorPolicy,
    json_policy: ResponsesJsonPolicy,
    typed_terminal_cutoff: bool,
}

impl ResponsesStreamDialectData {
    /// Builds explicit data for one admitted Responses dialect.
    #[must_use]
    pub fn new(
        profile: ResponsesDialectProfile,
        lifecycle: LifecycleDialectPolicy,
        expected_model: impl Into<String>,
        usage_policy: ResponsesUsagePolicy,
        response_limits: ResponsesResponseLimits,
        capabilities: ResponsesStreamCapabilities,
    ) -> Self {
        Self {
            profile,
            lifecycle,
            expected_model: expected_model.into(),
            usage_policy,
            response_limits,
            capabilities,
            event_policy: ResponsesEventPolicy::OpenAiCompatible,
            error_policy: ResponsesProviderErrorPolicy::OpenAiCompatible,
            json_policy: ResponsesJsonPolicy::Standard,
            typed_terminal_cutoff: false,
        }
    }

    /// Stops reading the body at the first valid typed terminal event and
    /// ignores any bytes after it, as the Codex and xAI reference clients do.
    #[must_use]
    pub const fn with_typed_terminal_cutoff(mut self) -> Self {
        self.typed_terminal_cutoff = true;
        self
    }

    /// Codex subscription Responses data under an already admitted model.
    #[must_use]
    pub fn codex(expected_model: impl Into<String>) -> Self {
        let mut data = Self::new(
            ResponsesDialectProfile::openai_platform(),
            LifecycleDialectPolicy::openai_codex(),
            expected_model,
            ResponsesUsagePolicy::DeriveTotalWhenOmitted,
            ResponsesResponseLimits::default(),
            ResponsesStreamCapabilities::codex(),
        );
        data.typed_terminal_cutoff = true;
        data
    }

    /// SuperGrok subscription data under an already admitted model.
    #[must_use]
    pub fn grok(expected_model: impl Into<String>) -> Self {
        let mut data = Self::new(
            ResponsesDialectProfile::xai(),
            LifecycleDialectPolicy::grok(),
            expected_model,
            ResponsesUsagePolicy::RequireTotal,
            ResponsesResponseLimits::default(),
            ResponsesStreamCapabilities::grok(),
        );
        data.event_policy = ResponsesEventPolicy::Grok;
        data.error_policy = ResponsesProviderErrorPolicy::Grok;
        data.json_policy = ResponsesJsonPolicy::UniqueMembers;
        data.typed_terminal_cutoff = true;
        data
    }

    /// OpenRouter stateless Responses beta data under an exact admitted model.
    #[must_use]
    pub fn openrouter(expected_model: impl Into<String>) -> Self {
        let mut data = Self::new(
            ResponsesDialectProfile::openrouter(),
            LifecycleDialectPolicy::openrouter(),
            expected_model,
            ResponsesUsagePolicy::RequireTotal,
            ResponsesResponseLimits::default(),
            ResponsesStreamCapabilities::openrouter(),
        );
        data.event_policy = ResponsesEventPolicy::OpenRouter;
        data.error_policy = ResponsesProviderErrorPolicy::OpenRouter;
        data.json_policy = ResponsesJsonPolicy::UniqueMembers;
        data
    }

    /// Replaces response/decode bounds without changing dialect semantics.
    #[must_use]
    pub fn with_response_limits(mut self, limits: ResponsesResponseLimits) -> Self {
        self.response_limits = limits;
        self
    }

    /// Returns the exact aggregate usage policy.
    #[must_use]
    pub const fn usage_policy(&self) -> ResponsesUsagePolicy {
        self.usage_policy
    }

    pub(crate) const fn typed_terminal_cutoff(&self) -> bool {
        self.typed_terminal_cutoff
    }

    /// Returns how provider-reported response model identity is bound.
    #[must_use]
    pub const fn model_binding(&self) -> plakous_responses::ResponseModelBinding {
        self.lifecycle.model_binding
    }

    pub(crate) fn execution_identity(&self) -> Option<Digest> {
        let mut owned = Vec::new();
        let sequence = match self.profile.sequence() {
            SequencePolicy::RequiredStrict => b"required-strict".as_slice(),
            SequencePolicy::OptionalMonotonic { fields } => {
                owned.extend(fields.iter().map(|field| field.as_bytes().to_vec()));
                b"optional-monotonic".as_slice()
            }
            _ => return None,
        };
        let terminal = match self.profile.terminal() {
            TerminalPolicy::TypedOnly => b"typed-only".as_slice(),
            TerminalPolicy::TypedThenDone => b"typed-then-done".as_slice(),
            TerminalPolicy::TypedOptionalDone => b"typed-optional-done".as_slice(),
            _ => return None,
        };
        let finish = match self.profile.finish() {
            FinishPolicy::BlankLineClosed => b"blank-line-closed".as_slice(),
            FinishPolicy::AutoDispatchResidual => b"auto-dispatch-residual".as_slice(),
            _ => return None,
        };
        let bom = match self.profile.bom() {
            BomPolicy::Strip => b"strip".as_slice(),
            BomPolicy::Ignore => b"ignore".as_slice(),
            _ => return None,
        };
        let reasoning = match self.lifecycle.reasoning {
            ReasoningLifecycleMode::DeltaOnly => b"delta-only".as_slice(),
            ReasoningLifecycleMode::FullParts => b"full-parts".as_slice(),
            _ => return None,
        };
        let model_binding = match self.lifecycle.model_binding {
            plakous_responses::ResponseModelBinding::ExactRequest => b"exact-request".as_slice(),
            plakous_responses::ResponseModelBinding::ProviderReported => {
                b"provider-reported".as_slice()
            }
        };
        let usage = match self.usage_policy {
            ResponsesUsagePolicy::RequireTotal => b"require-total".as_slice(),
            ResponsesUsagePolicy::DeriveTotalWhenOmitted => b"derive-total".as_slice(),
        };
        let lifecycle = [
            u8::from(self.lifecycle.require_created_id),
            u8::from(self.lifecycle.require_created_model),
            u8::from(self.lifecycle.content_text_done),
            u8::from(self.lifecycle.reasoning_content_text),
            u8::from(self.lifecycle.intermediate_response_id),
            u8::from(self.lifecycle.strict_terminal_output),
            u8::from(self.lifecycle.reasoning_terminal_ciphertext_refresh),
            u8::from(self.lifecycle.sparse_terminal_model_from_prepared),
            u8::from(self.lifecycle.sparse_terminal_output_from_completed),
            u8::from(self.lifecycle.incomplete_length_text_snapshot),
            u8::from(self.typed_terminal_cutoff),
        ];
        let response_bytes = self.response_limits.max_response_bytes().to_be_bytes();
        let output_items = self.response_limits.max_output_items().to_be_bytes();
        let unknown_bytes = self.response_limits.max_unknown_bytes().to_be_bytes();
        let capabilities = [self.capabilities.enabled];
        let event_policy = match self.event_policy {
            ResponsesEventPolicy::OpenAiCompatible => b"openai-compatible".as_slice(),
            ResponsesEventPolicy::OpenRouter => b"openrouter".as_slice(),
            ResponsesEventPolicy::Grok => b"grok".as_slice(),
        };
        let error_policy = match self.error_policy {
            ResponsesProviderErrorPolicy::OpenAiCompatible => b"openai-compatible".as_slice(),
            ResponsesProviderErrorPolicy::OpenRouter => b"openrouter".as_slice(),
            ResponsesProviderErrorPolicy::Grok => b"grok".as_slice(),
        };
        let json_policy = match self.json_policy {
            ResponsesJsonPolicy::Standard => b"standard".as_slice(),
            ResponsesJsonPolicy::UniqueMembers => b"unique-members".as_slice(),
        };
        let mut parts = vec![
            b"responses-stream-dialect-v3".as_slice(),
            sequence,
            terminal,
            finish,
            bom,
            reasoning,
            model_binding,
            usage,
            lifecycle.as_slice(),
            self.expected_model.as_bytes(),
            response_bytes.as_slice(),
            output_items.as_slice(),
            unknown_bytes.as_slice(),
            capabilities.as_slice(),
            event_policy,
            error_policy,
            json_policy,
        ];
        parts.extend(owned.iter().map(Vec::as_slice));
        Digest::sha256(DigestDomain::ModelBinding, 1, &parts).ok()
    }
}

/// Concrete Responses stream host failure (redaction-safe and provider-neutral).
#[derive(Clone, Copy, Debug, Eq, thiserror::Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesFamilyStreamError {
    /// Invalid UTF-8 in SSE framing.
    #[error("invalid UTF-8 in Responses stream")]
    InvalidUtf8,
    /// Malformed SSE framing.
    #[error("malformed Responses SSE framing")]
    MalformedSse,
    /// Malformed JSON payload.
    #[error("malformed Responses JSON event")]
    InvalidJson,
    /// A required or documented field was invalid.
    #[error("invalid Responses event field: {0}")]
    InvalidField(&'static str),
    /// Event name and JSON type differed.
    #[error("Responses SSE event name does not match JSON type")]
    EventNameMismatch,
    /// Sequence policy failed.
    #[error("Responses sequence policy failed")]
    InvalidSequence,
    /// Shared item/content/tool lifecycle failed.
    #[error("Responses lifecycle failed")]
    InvalidLifecycle,
    /// Shared lifecycle failed with one static redaction-safe reason.
    #[error("Responses lifecycle failed: {0}")]
    Lifecycle(&'static str),
    /// The response stopped at its output bound with an item still open, so
    /// the item never became output.
    #[error("Responses stream stopped at the output bound with an unfinished item")]
    OutputBoundReached,
    /// Event arrived after a typed terminal.
    #[error("Responses event arrived after terminal")]
    EventAfterTerminal,
    /// EOF arrived inside an unfinished frame.
    #[error("truncated Responses SSE frame")]
    Truncated,
    /// Required typed terminal was absent.
    #[error("Responses stream omitted a typed terminal")]
    MissingTerminal,
    /// Required trailing DONE marker was absent or invalid.
    #[error("Responses DONE marker policy failed")]
    DonePolicy,
    /// A declared event surface was not admitted.
    #[error("Responses event surface is not admitted: {0}")]
    Unsupported(&'static str),
    /// A configured resource bound was exceeded.
    #[error("Responses stream resource limit exceeded: {0}")]
    ResourceLimit(&'static str),
    /// Decoder is already terminally failed.
    #[error("Responses decoder already failed")]
    DecoderFailed,
}

impl From<ResponsesFieldError> for ResponsesFamilyStreamError {
    fn from(error: ResponsesFieldError) -> Self {
        match error {
            ResponsesFieldError::Missing { field } | ResponsesFieldError::Invalid { field } => {
                Self::InvalidField(field)
            }
            _ => Self::InvalidField("unknown Responses field"),
        }
    }
}

impl From<ResponsesHostDecodeError> for ResponsesFamilyStreamError {
    fn from(error: ResponsesHostDecodeError) -> Self {
        match error {
            ResponsesHostDecodeError::MissingField(field)
            | ResponsesHostDecodeError::InvalidField(field) => Self::InvalidField(field),
            ResponsesHostDecodeError::ResourceLimit(resource) => Self::ResourceLimit(resource),
            ResponsesHostDecodeError::InvalidUsageAccounting => {
                Self::InvalidField("usage accounting")
            }
        }
    }
}

impl ResponsesStreamHostError for ResponsesFamilyStreamError {
    fn invalid_utf8() -> Self {
        Self::InvalidUtf8
    }

    fn resource_limit_frame(_maximum: usize) -> Self {
        Self::ResourceLimit("SSE frame bytes")
    }

    fn resource_limit(resource: &'static str, _maximum: usize) -> Self {
        Self::ResourceLimit(resource)
    }

    fn decoder_failed() -> Self {
        Self::DecoderFailed
    }

    fn malformed_sse() -> Self {
        Self::MalformedSse
    }

    fn duplicate_or_reordered(_previous: u64, _sequence: u64) -> Self {
        Self::InvalidSequence
    }

    fn sequence_gap(_expected: u64, _sequence: u64) -> Self {
        Self::InvalidSequence
    }

    fn inconsistent_sequence_presence() -> Self {
        Self::InvalidSequence
    }

    fn done_forbidden() -> Self {
        Self::DonePolicy
    }

    fn event_after_terminal() -> Self {
        Self::EventAfterTerminal
    }

    fn truncated() -> Self {
        Self::Truncated
    }

    fn missing_terminal() -> Self {
        Self::MissingTerminal
    }

    fn missing_done() -> Self {
        Self::DonePolicy
    }

    fn attempt_not_admitted() -> Self {
        Self::InvalidLifecycle
    }

    fn from_field_error(error: ResponsesFieldError) -> Self {
        error.into()
    }
}

/// Concrete typed event emitted by [`ResponsesFamilySseDecoder`].
pub type ResponsesFamilyStreamEvent =
    ResponsesStreamEventKind<ResponsesResponse, ResponsesProviderError>;

struct ResponsesFamilyHooks {
    data: ResponsesStreamDialectData,
    lifecycle: ResponsesLifecycle,
}

impl ResponsesFamilyHooks {
    fn new(data: ResponsesStreamDialectData) -> Self {
        let lifecycle = ResponsesLifecycle::new(data.lifecycle);
        Self { data, lifecycle }
    }

    fn correlated_item_id(
        &self,
        object: &Map<String, Value>,
        output_index: u32,
    ) -> Result<Option<String>, ResponsesFamilyStreamError> {
        let item_id = plakous_responses::optional_provider_label(object, "item_id")?;
        if self.data.event_policy == ResponsesEventPolicy::OpenRouter && item_id.is_none() {
            return Err(ResponsesFamilyStreamError::InvalidField("item_id"));
        }
        self.lifecycle
            .validate_output_item_id(output_index, item_id)
            .map_err(map_lifecycle_error)?;
        Ok(item_id.map(str::to_owned))
    }

    fn parse_event(
        &mut self,
        value: &Value,
    ) -> Result<ResponsesFamilyStreamEvent, ResponsesFamilyStreamError> {
        let object = value
            .as_object()
            .ok_or(ResponsesFamilyStreamError::InvalidField("event"))?;
        let kind = required_provider_label(object, "type")?;
        self.lifecycle
            .validate_intermediate_response_id(object, kind)
            .map_err(map_lifecycle_error)?;
        if !self.data.event_policy.is_typed(kind) {
            if !self
                .data
                .capabilities
                .has(ResponsesStreamCapabilities::RETAIN_UNKNOWN)
            {
                return Err(ResponsesFamilyStreamError::Unsupported("unknown event"));
            }
            if value.to_string().len() > self.data.response_limits.max_unknown_bytes() {
                return Err(ResponsesFamilyStreamError::ResourceLimit(
                    "unknown event bytes",
                ));
            }
            return Ok(ResponsesStreamEventKind::Unknown {
                kind: kind.to_owned(),
                native: value.clone(),
            });
        }
        let event = match kind {
            "response.created" => {
                let response = required_object(object, "response")?;
                ResponsesStreamEventKind::ResponseCreated {
                    response_id: plakous_responses::optional_provider_label(response, "id")?
                        .map(str::to_owned),
                    model: plakous_responses::optional_provider_label(response, "model")?
                        .map(str::to_owned),
                }
            }
            "response.in_progress" => {
                let response = required_object(object, "response")?;
                let response_id = required_provider_label(response, "id")?;
                let model = required_provider_label(response, "model")?;
                if self.lifecycle.created_id() != Some(response_id)
                    || self.lifecycle.created_model() != Some(model)
                {
                    return Err(ResponsesFamilyStreamError::InvalidLifecycle);
                }
                ResponsesStreamEventKind::ResponseInProgress {
                    response_id: response_id.to_owned(),
                    model: model.to_owned(),
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                let output_index = required_output_index::<ResponsesFamilyStreamError>(
                    object,
                    ResponsesDecodeLimits::new(
                        self.data.response_limits.max_unknown_bytes(),
                        self.data.response_limits.max_response_bytes(),
                        usize::MAX,
                        self.data.response_limits.max_output_items(),
                    )
                    .map_err(|_| ResponsesFamilyStreamError::ResourceLimit("decode limits"))?,
                )?;
                let item = parse_output_item::<ResponsesHostDecodeError>(
                    object
                        .get("item")
                        .ok_or(ResponsesFamilyStreamError::InvalidField("item"))?,
                )?;
                validate_bounded_unknown_item(&item, self.data.response_limits)?;
                if matches!(item, ResponsesOutputItem::CustomToolCall { .. })
                    && !self
                        .data
                        .capabilities
                        .has(ResponsesStreamCapabilities::CUSTOM_TOOLS)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported("custom tools"));
                }
                if kind.ends_with("added") {
                    ResponsesStreamEventKind::OutputItemAdded { output_index, item }
                } else {
                    ResponsesStreamEventKind::OutputItemDone { output_index, item }
                }
            }
            "response.content_part.added" | "response.content_part.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                let content_index = required_u32(object, "content_index")?;
                let part = object
                    .get("part")
                    .filter(|part| part.is_object())
                    .ok_or(ResponsesFamilyStreamError::InvalidField("part"))?
                    .clone();
                if part.get("type").and_then(Value::as_str) == Some("reasoning_text")
                    && !self
                        .data
                        .capabilities
                        .has(ResponsesStreamCapabilities::REASONING_CONTENT_TEXT)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported(
                        "reasoning content text",
                    ));
                }
                if !matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("output_text" | "refusal")
                ) && part.to_string().len() > self.data.response_limits.max_unknown_bytes()
                {
                    return Err(ResponsesFamilyStreamError::ResourceLimit(
                        "unknown content part bytes",
                    ));
                }
                if kind.ends_with("added") {
                    ResponsesStreamEventKind::ContentPartAdded {
                        output_index,
                        content_index,
                        part,
                    }
                } else {
                    ResponsesStreamEventKind::ContentPartDone {
                        output_index,
                        content_index,
                        part,
                    }
                }
            }
            "response.content_part.delta"
                if self.data.event_policy == ResponsesEventPolicy::OpenRouter =>
            {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                let content_index = required_u32(object, "content_index")?;
                let delta = required_string(object, "delta")?.to_owned();
                match self
                    .lifecycle
                    .content_part_kind(output_index, content_index)
                    .map_err(map_lifecycle_error)?
                {
                    "output_text" => ResponsesStreamEventKind::OutputTextDelta {
                        output_index,
                        content_index,
                        delta,
                    },
                    "refusal" => ResponsesStreamEventKind::RefusalDelta {
                        output_index,
                        content_index,
                        delta,
                    },
                    _ => return Err(ResponsesFamilyStreamError::InvalidLifecycle),
                }
            }
            "response.output_text.delta" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::OutputTextDelta {
                    output_index,
                    content_index: required_u32(object, "content_index")?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.output_text.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::OutputTextDone {
                    output_index,
                    content_index: required_u32(object, "content_index")?,
                    text: required_string(object, "text")?.to_owned(),
                }
            }
            "response.refusal.delta" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::RefusalDelta {
                    output_index,
                    content_index: required_u32(object, "content_index")?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.refusal.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::RefusalDone {
                    output_index,
                    content_index: required_u32(object, "content_index")?,
                    refusal: required_string(object, "refusal")?.to_owned(),
                }
            }
            "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                let summary_index = required_u32(object, "summary_index")?;
                let part = object
                    .get("part")
                    .filter(|part| part.is_object())
                    .ok_or(ResponsesFamilyStreamError::InvalidField("part"))?
                    .clone();
                if kind.ends_with("added") {
                    ResponsesStreamEventKind::ReasoningSummaryPartAdded {
                        output_index,
                        summary_index,
                        part,
                    }
                } else {
                    ResponsesStreamEventKind::ReasoningSummaryPartDone {
                        output_index,
                        summary_index,
                        part,
                    }
                }
            }
            "response.reasoning_summary_text.delta" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::ReasoningSummaryTextDelta {
                    output_index,
                    summary_index: required_u32(object, "summary_index")?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_text.done"
                if self
                    .data
                    .capabilities
                    .has(ResponsesStreamCapabilities::REASONING_CONTENT_TEXT) =>
            {
                let output_index = output_index(object, self.data.response_limits)?;
                let content_index = required_u32(object, "content_index")?;
                let item_id = self.correlated_item_id(object, output_index)?;
                if kind.ends_with("delta") {
                    ResponsesStreamEventKind::ReasoningTextDelta {
                        output_index,
                        content_index,
                        item_id,
                        delta: required_string(object, "delta")?.to_owned(),
                    }
                } else {
                    ResponsesStreamEventKind::ReasoningTextDone {
                        output_index,
                        content_index,
                        item_id,
                        text: required_string(object, "text")?.to_owned(),
                    }
                }
            }
            "response.reasoning.delta"
                if self.data.event_policy == ResponsesEventPolicy::OpenRouter =>
            {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::ReasoningSummaryTextDelta {
                    output_index,
                    summary_index: object
                        .get("summary_index")
                        .map_or(Ok(0), |_| required_u32(object, "summary_index"))?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.reasoning_summary_text.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::ReasoningSummaryTextDone {
                    output_index,
                    summary_index: required_u32(object, "summary_index")?,
                    text: required_string(object, "text")?.to_owned(),
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = output_index(object, self.data.response_limits)?;
                ResponsesStreamEventKind::FunctionCallArgumentsDelta {
                    output_index,
                    item_id: self.correlated_item_id(object, output_index)?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.function_call_arguments.done" => {
                let output_index = output_index(object, self.data.response_limits)?;
                ResponsesStreamEventKind::FunctionCallArgumentsDone {
                    output_index,
                    item_id: self.correlated_item_id(object, output_index)?,
                    arguments: required_string(object, "arguments")?.to_owned(),
                }
            }
            "response.custom_tool_call_input.delta" => {
                if !self
                    .data
                    .capabilities
                    .has(ResponsesStreamCapabilities::CUSTOM_TOOLS)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported("custom tools"));
                }
                let output_index = output_index(object, self.data.response_limits)?;
                ResponsesStreamEventKind::CustomToolCallInputDelta {
                    output_index,
                    item_id: self.correlated_item_id(object, output_index)?,
                    delta: required_string(object, "delta")?.to_owned(),
                }
            }
            "response.custom_tool_call_input.done" => {
                if !self
                    .data
                    .capabilities
                    .has(ResponsesStreamCapabilities::CUSTOM_TOOLS)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported("custom tools"));
                }
                let output_index = output_index(object, self.data.response_limits)?;
                ResponsesStreamEventKind::CustomToolCallInputDone {
                    output_index,
                    item_id: self.correlated_item_id(object, output_index)?,
                    input: required_string(object, "input")?.to_owned(),
                }
            }
            "response.output_text.annotation.added" => {
                if !self
                    .data
                    .capabilities
                    .has(ResponsesStreamCapabilities::CITATIONS)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported("citations"));
                }
                let annotation = parse_annotation::<ResponsesHostDecodeError>(
                    object
                        .get("annotation")
                        .ok_or(ResponsesFamilyStreamError::InvalidField("annotation"))?,
                )?;
                if matches!(
                    &annotation,
                    ResponsesAnnotation::Opaque { native, .. }
                        if native.to_string().len()
                            > self.data.response_limits.max_unknown_bytes()
                ) {
                    return Err(ResponsesFamilyStreamError::ResourceLimit(
                        "unknown annotation bytes",
                    ));
                }
                let output_index = output_index(object, self.data.response_limits)?;
                let _ = self.correlated_item_id(object, output_index)?;
                ResponsesStreamEventKind::OutputTextAnnotationAdded {
                    output_index,
                    content_index: required_u32(object, "content_index")?,
                    annotation_index: required_u32(object, "annotation_index")?,
                    annotation,
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" | "response.done" => {
                self.parse_terminal(kind, object)?
            }
            "response.error" if self.data.event_policy == ResponsesEventPolicy::OpenRouter => {
                let payload = object.get("error").unwrap_or(value);
                let request_id = plakous_responses::optional_provider_label(object, "request_id")?;
                let canonical_error_type =
                    plakous_responses::optional_provider_label(object, "error_type")?;
                ResponsesStreamEventKind::Error(
                    parse_provider_error_with_canonical_type(
                        payload,
                        self.data.response_limits,
                        self.data.error_policy,
                        canonical_error_type,
                    )?
                    .with_request_id(request_id)?,
                )
            }
            "error" => {
                let payload = object.get("error").unwrap_or(value);
                let request_id = plakous_responses::optional_provider_label(object, "request_id")?;
                let canonical_error_type =
                    if self.data.error_policy == ResponsesProviderErrorPolicy::OpenRouter {
                        plakous_responses::optional_provider_label(object, "error_type")?
                    } else {
                        None
                    };
                ResponsesStreamEventKind::Error(
                    parse_provider_error_with_canonical_type(
                        payload,
                        self.data.response_limits,
                        self.data.error_policy,
                        canonical_error_type,
                    )?
                    .with_request_id(request_id)?,
                )
            }
            _ => {
                if kind.starts_with("response.custom_tool_call")
                    || kind.starts_with("response.function_call_arguments")
                    || kind.starts_with("response.output_text.annotation")
                {
                    return Err(ResponsesFamilyStreamError::Unsupported(
                        "declared event subvariant",
                    ));
                }
                if !self
                    .data
                    .capabilities
                    .has(ResponsesStreamCapabilities::RETAIN_UNKNOWN)
                {
                    return Err(ResponsesFamilyStreamError::Unsupported("unknown event"));
                }
                if value.to_string().len() > self.data.response_limits.max_unknown_bytes() {
                    return Err(ResponsesFamilyStreamError::ResourceLimit(
                        "unknown event bytes",
                    ));
                }
                ResponsesStreamEventKind::Unknown {
                    kind: kind.to_owned(),
                    native: value.clone(),
                }
            }
        };
        if !matches!(
            event,
            ResponsesStreamEventKind::ResponseCompleted(_)
                | ResponsesStreamEventKind::ResponseIncomplete(_)
                | ResponsesStreamEventKind::ResponseFailed(_)
                | ResponsesStreamEventKind::Error(_)
        ) {
            event
                .apply_intermediate_lifecycle(
                    &mut self.lifecycle,
                    object,
                    &self.data.expected_model,
                )
                .map_err(map_lifecycle_error)?;
        }
        Ok(event)
    }

    fn parse_terminal(
        &self,
        event_kind: &str,
        object: &Map<String, Value>,
    ) -> Result<ResponsesFamilyStreamEvent, ResponsesFamilyStreamError> {
        let response_value = object
            .get("response")
            .ok_or(ResponsesFamilyStreamError::InvalidField("response"))?;
        let mut reconciled_response = response_value.clone();
        let response_object = reconciled_response
            .as_object_mut()
            .ok_or(ResponsesFamilyStreamError::InvalidField("response"))?;
        if !response_object.contains_key("model")
            && self.data.lifecycle.sparse_terminal_model_from_prepared
        {
            response_object.insert(
                "model".to_owned(),
                Value::String(self.data.expected_model.clone()),
            );
        }
        if !response_object.contains_key("output")
            && self.data.lifecycle.sparse_terminal_output_from_completed
        {
            let output = self
                .lifecycle
                .synthesize_missing_output()
                .map_err(map_lifecycle_error)?;
            response_object.insert("output".to_owned(), Value::Array(output));
        }
        let mut response = ResponsesResponse::parse_reconciled_with_native(
            &reconciled_response,
            response_value,
            self.data.usage_policy,
            self.data.error_policy,
            self.data.response_limits,
        )?;
        if self.data.lifecycle.model_binding
            == plakous_responses::ResponseModelBinding::ExactRequest
            && response.model() != self.data.expected_model
        {
            return Err(ResponsesFamilyStreamError::InvalidLifecycle);
        }
        let (expected_status, allow) = match event_kind {
            "response.completed" => ("completed", true),
            "response.done" => (
                "completed",
                self.data
                    .capabilities
                    .has(ResponsesStreamCapabilities::DONE_TERMINAL),
            ),
            "response.incomplete" => (
                "incomplete",
                self.data
                    .capabilities
                    .has(ResponsesStreamCapabilities::INCOMPLETE_TERMINAL),
            ),
            "response.failed" => (
                "failed",
                self.data
                    .capabilities
                    .has(ResponsesStreamCapabilities::FAILED_TERMINAL),
            ),
            _ => return Err(ResponsesFamilyStreamError::InvalidLifecycle),
        };
        if !allow {
            return Err(ResponsesFamilyStreamError::Unsupported("terminal status"));
        }
        let actual_status = match response.status() {
            ResponsesResponseStatus::Completed => "completed",
            ResponsesResponseStatus::Incomplete => "incomplete",
            ResponsesResponseStatus::Failed => "failed",
            _ => return Err(ResponsesFamilyStreamError::InvalidLifecycle),
        };
        let output = response
            .output()
            .iter()
            .map(ResponsesOutputItem::native)
            .collect::<Vec<_>>();
        self.lifecycle
            .terminal_response(
                response.id(),
                Some(response.model()),
                expected_status,
                actual_status,
                response
                    .incomplete_details()
                    .map(crate::ResponsesIncompleteDetails::native),
                response.error().is_some(),
                &output,
            )
            .map_err(map_lifecycle_error)?;
        if !self.data.lifecycle.strict_terminal_output {
            let streamed = self
                .lifecycle
                .synthesize_missing_output()
                .map_err(map_lifecycle_error)?;
            if !streamed.is_empty() {
                response = response
                    .with_authoritative_stream_output(&streamed, self.data.response_limits)?;
            }
        }
        Ok(match response.status() {
            ResponsesResponseStatus::Completed => {
                ResponsesStreamEventKind::ResponseCompleted(response)
            }
            ResponsesResponseStatus::Incomplete => {
                ResponsesStreamEventKind::ResponseIncomplete(response)
            }
            ResponsesResponseStatus::Failed => ResponsesStreamEventKind::ResponseFailed(response),
            _ => return Err(ResponsesFamilyStreamError::InvalidLifecycle),
        })
    }
}

impl DialectStreamHooks for ResponsesFamilyHooks {
    type Event = ResponsesFamilyStreamEvent;
    type Error = ResponsesFamilyStreamError;

    fn map_sse_error(error: SseError, limits: &ResponsesStreamLimits) -> Self::Error {
        map_sse_error_standard(error, limits)
    }

    fn map_sequence_error(error: SequenceValidationError) -> Self::Error {
        map_sequence_error_standard(error)
    }

    fn map_done_forbidden() -> Self::Error {
        ResponsesFamilyStreamError::DonePolicy
    }

    fn map_event_after_terminal() -> Self::Error {
        ResponsesFamilyStreamError::EventAfterTerminal
    }

    fn map_truncated() -> Self::Error {
        ResponsesFamilyStreamError::Truncated
    }

    fn map_missing_terminal() -> Self::Error {
        ResponsesFamilyStreamError::MissingTerminal
    }

    fn map_missing_done() -> Self::Error {
        ResponsesFamilyStreamError::DonePolicy
    }

    fn map_resource_limit(resource: &'static str, _maximum: usize) -> Self::Error {
        ResponsesFamilyStreamError::ResourceLimit(resource)
    }

    fn map_field_error(error: ResponsesFieldError) -> Self::Error {
        error.into()
    }

    fn event_is_terminal(event: &Self::Event) -> bool {
        event.is_terminal()
    }

    fn decode_json_event(
        &mut self,
        data: &str,
        event_name: Option<&str>,
        _sequence: Option<u64>,
    ) -> Result<Option<Self::Event>, Self::Error> {
        let value: Value = match self.data.json_policy {
            ResponsesJsonPolicy::Standard => serde_json::from_str(data),
            ResponsesJsonPolicy::UniqueMembers => parse_unique_json_value(data),
        }
        .map_err(|_| ResponsesFamilyStreamError::InvalidJson)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ResponsesFamilyStreamError::InvalidField("type"))?;
        if event_name.is_some_and(|event_name| event_name != kind) {
            return Err(ResponsesFamilyStreamError::EventNameMismatch);
        }
        self.parse_event(&value).map(Some)
    }
}

/// Concrete non-cloning Responses SSE decoder used by Codex-family migration.
pub struct ResponsesFamilySseDecoder {
    kernel: Option<ResponsesStreamKernel>,
    hooks: ResponsesFamilyHooks,
    failed: bool,
}

impl ResponsesFamilySseDecoder {
    /// Creates a decoder from immutable admitted family/dialect data.
    #[must_use]
    pub fn new(data: ResponsesStreamDialectData, limits: ResponsesStreamLimits) -> Self {
        let mut kernel = ResponsesStreamKernel::new(data.profile, limits);
        if data.typed_terminal_cutoff {
            kernel = kernel.with_typed_terminal_cutoff();
        }
        Self {
            kernel: Some(kernel),
            hooks: ResponsesFamilyHooks::new(data),
            failed: false,
        }
    }

    fn with_kernel_mut<T>(
        &mut self,
        operation: impl FnOnce(&mut ResponsesStreamKernel, &mut ResponsesFamilyHooks) -> T,
    ) -> T {
        let Some(mut kernel) = self.kernel.take() else {
            unreachable!("Responses kernel cannot be re-entered");
        };
        let output = operation(&mut kernel, &mut self.hooks);
        self.kernel = Some(kernel);
        output
    }

    /// Feeds arbitrary byte fragments and returns every completed event.
    ///
    /// # Errors
    ///
    /// Returns a terminal family error for malformed, over-limit, or invalid
    /// lifecycle input.
    pub fn push(
        &mut self,
        bytes: &[u8],
    ) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
        if self.failed {
            return Err(ResponsesFamilyStreamError::DecoderFailed);
        }
        match self.with_kernel_mut(|kernel, hooks| kernel.push(bytes, hooks)) {
            Ok(events) => Ok(events),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    /// Validates EOF and returns auto-dispatched residual events.
    ///
    /// # Errors
    ///
    /// Returns a terminal family error for truncation or missing terminal state.
    pub fn finish(
        &mut self,
    ) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
        if self.failed {
            return Err(ResponsesFamilyStreamError::DecoderFailed);
        }
        match self.with_kernel_mut(ResponsesStreamKernel::finish) {
            Ok(events) => Ok(events),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
}

impl fmt::Debug for ResponsesFamilySseDecoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesFamilySseDecoder")
            .field("kernel", &self.kernel)
            .field("expected_model", &"<redacted>")
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

fn output_index(
    object: &Map<String, Value>,
    limits: ResponsesResponseLimits,
) -> Result<u32, ResponsesFamilyStreamError> {
    let index = required_u32(object, "output_index")?;
    if usize::try_from(index).map_or(true, |index| index >= limits.max_output_items()) {
        return Err(ResponsesFamilyStreamError::ResourceLimit("output items"));
    }
    Ok(index)
}

fn map_lifecycle_error(error: ResponsesLifecycleError) -> ResponsesFamilyStreamError {
    match error {
        ResponsesLifecycleError::InvalidLifecycle { reason } => {
            ResponsesFamilyStreamError::Lifecycle(reason)
        }
        ResponsesLifecycleError::BindingMismatch => {
            ResponsesFamilyStreamError::Lifecycle("response lifecycle binding mismatch")
        }
        ResponsesLifecycleError::OutputBoundReached => {
            ResponsesFamilyStreamError::OutputBoundReached
        }
        ResponsesLifecycleError::MissingField { field }
        | ResponsesLifecycleError::InvalidField { field } => {
            ResponsesFamilyStreamError::InvalidField(field)
        }
        _ => ResponsesFamilyStreamError::InvalidLifecycle,
    }
}

/// Stream-error factories for shared SSE / sequence maps.
pub trait ResponsesStreamHostError: Sized {
    /// Invalid UTF-8 in an SSE frame.
    fn invalid_utf8() -> Self;
    /// SSE frame byte limit.
    fn resource_limit_frame(maximum: usize) -> Self;
    /// Named resource limit.
    fn resource_limit(resource: &'static str, maximum: usize) -> Self;
    /// Decoder already failed.
    fn decoder_failed() -> Self;
    /// Malformed SSE.
    fn malformed_sse() -> Self;
    /// Duplicate or reordered sequence.
    fn duplicate_or_reordered(previous: u64, sequence: u64) -> Self;
    /// Sequence gap.
    fn sequence_gap(expected: u64, sequence: u64) -> Self;
    /// Sequence presence inconsistency.
    fn inconsistent_sequence_presence() -> Self;
    /// Forbidden / unexpected `[DONE]`.
    fn done_forbidden() -> Self;
    /// Event after terminal.
    fn event_after_terminal() -> Self;
    /// Truncated frame.
    fn truncated() -> Self;
    /// Missing typed terminal.
    fn missing_terminal() -> Self;
    /// Missing required `[DONE]`.
    fn missing_done() -> Self;
    /// Attempt not admitted / not decoding.
    fn attempt_not_admitted() -> Self;
    /// Map field errors.
    fn from_field_error(error: ResponsesFieldError) -> Self;
}

/// Standard SSE framing error map used by OpenRouter / xAI-style dialects.
pub fn map_sse_error_standard<E: ResponsesStreamHostError>(
    error: SseError,
    limits: &ResponsesStreamLimits,
) -> E {
    match error {
        SseError::InvalidUtf8 => E::invalid_utf8(),
        SseError::ResourceLimit("SSE line bytes" | "SSE event bytes") => {
            E::resource_limit_frame(limits.max_frame_bytes())
        }
        SseError::ResourceLimit(resource) => E::resource_limit(resource, limits.max_frame_bytes()),
        SseError::AlreadyFailed => E::decoder_failed(),
        _ => E::malformed_sse(),
    }
}

/// Standard sequence-policy error map.
pub fn map_sequence_error_standard<E: ResponsesStreamHostError>(
    error: SequenceValidationError,
) -> E {
    match error {
        SequenceValidationError::DuplicateOrReordered { previous, sequence } => {
            E::duplicate_or_reordered(previous, sequence)
        }
        SequenceValidationError::Gap { expected, sequence } => E::sequence_gap(expected, sequence),
        SequenceValidationError::MissingRequired
        | SequenceValidationError::InconsistentPresence
        | _ => E::inconsistent_sequence_presence(),
    }
}

/// Validates `output_index` against decode limits.
///
/// # Errors
///
/// Returns `E` when the field is missing/invalid or exceeds the item budget.
pub fn required_output_index<E>(
    object: &Map<String, Value>,
    limits: ResponsesDecodeLimits,
) -> Result<u32, E>
where
    E: From<ResponsesFieldError> + ResponsesStreamHostError,
{
    let index = required_u32(object, "output_index")?;
    let index_usize = usize::try_from(index).map_err(|_| {
        E::from_field_error(ResponsesFieldError::Invalid {
            field: "output_index",
        })
    })?;
    if index_usize >= limits.max_output_items() {
        return Err(E::resource_limit("output items", limits.max_output_items()));
    }
    Ok(index)
}

/// Admitted SSE decoder front-half around one kernel + hook state.
///
/// A decode failure poisons the operation, so pushes mutate the owned kernel and
/// hooks directly. This avoids cloning all framing, lifecycle, and projection
/// state for every transport fragment.
pub struct ResponsesSseDecoderFront<H> {
    attempt_lifecycle: Arc<AtomicU8>,
    kernel: ResponsesStreamKernel,
    hooks: H,
    failed: bool,
}

impl<H> ResponsesSseDecoderFront<H> {
    /// Builds a front-half decoder.
    #[must_use]
    pub fn new(attempt_lifecycle: Arc<AtomicU8>, kernel: ResponsesStreamKernel, hooks: H) -> Self {
        Self {
            attempt_lifecycle,
            kernel,
            hooks,
            failed: false,
        }
    }

    /// Returns a shared handle to dialect hooks.
    #[must_use]
    pub const fn hooks(&self) -> &H {
        &self.hooks
    }

    /// Returns a mutable handle to dialect hooks.
    pub const fn hooks_mut(&mut self) -> &mut H {
        &mut self.hooks
    }

    /// Returns the stream kernel.
    #[must_use]
    pub const fn kernel(&self) -> &ResponsesStreamKernel {
        &self.kernel
    }

    /// Whether the decoder is terminally failed.
    #[must_use]
    pub const fn failed(&self) -> bool {
        self.failed
    }

    /// Feeds bytes through the kernel under attempt-admission checks.
    ///
    /// # Errors
    ///
    /// Returns dialect stream errors from hooks or admission failures.
    pub fn push<E>(&mut self, chunk: &[u8]) -> Result<Vec<H::Event>, E>
    where
        H: DialectStreamHooks<Error = E>,
        E: ResponsesStreamHostError,
    {
        if self.failed {
            return Err(E::decoder_failed());
        }
        if !is_decoding(&self.attempt_lifecycle) {
            return Err(E::attempt_not_admitted());
        }
        match self.kernel.push(chunk, &mut self.hooks) {
            Ok(events) => Ok(events),
            Err(error) => {
                mark_decode_failed(&self.attempt_lifecycle);
                self.failed = true;
                Err(error)
            }
        }
    }

    /// Finishes the stream, returns residual decoded events, and marks the
    /// attempt decoded on success.
    ///
    /// Auto-dispatch dialects may complete a final SSE frame only at EOF. Those
    /// events are semantic stream output and must be delivered to the caller.
    ///
    /// # Errors
    ///
    /// Returns dialect stream errors from hooks or admission failures.
    pub fn finish_events<E>(mut self) -> Result<Vec<H::Event>, E>
    where
        H: DialectStreamHooks<Error = E>,
        E: ResponsesStreamHostError,
    {
        if self.failed {
            return Err(E::decoder_failed());
        }
        if !is_decoding(&self.attempt_lifecycle) {
            return Err(E::attempt_not_admitted());
        }
        match self.kernel.finish(&mut self.hooks) {
            Ok(residual) => {
                mark_decoded(&self.attempt_lifecycle).map_err(|_| E::attempt_not_admitted())?;
                Ok(residual)
            }
            Err(error) => {
                mark_decode_failed(&self.attempt_lifecycle);
                Err(error)
            }
        }
    }

    /// Finishes a stream whose dialect requires blank-line-closed frames.
    ///
    /// Existing xAI/OpenRouter callers use this compatibility convenience; its
    /// profiles cannot produce residual events. Auto-dispatch dialects must use
    /// [`Self::finish_events`] so EOF-decoded events are not discarded.
    ///
    /// # Errors
    ///
    /// Returns dialect stream errors from hooks or admission failures.
    pub fn finish<E>(self) -> Result<(), E>
    where
        H: DialectStreamHooks<Error = E>,
        E: ResponsesStreamHostError,
    {
        let residual = self.finish_events()?;
        debug_assert!(residual.is_empty());
        Ok(())
    }
}

impl<H> fmt::Debug for ResponsesSseDecoderFront<H> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesSseDecoderFront")
            .field("attempt_decoding", &is_decoding(&self.attempt_lifecycle))
            .field("kernel", &self.kernel)
            .field("failed", &self.failed)
            .field("hooks", &"<redacted dialect hooks>")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response_host::{ResponsesProviderErrorClass, ResponsesUsage};

    const MODEL: &str = "gpt-5.4";
    const MIXED: &str = include_str!("../../../providers/openai/tests/fixtures/response_mixed.sse");
    const O1_SURFACES: &str =
        include_str!("../../../providers/openai/tests/fixtures/response_o1_surfaces.sse");
    const INCOMPLETE: &str =
        include_str!("../../../providers/openai/tests/fixtures/response_incomplete.sse");
    const FAILED_RESIDUAL: &str =
        include_str!("../../../providers/openai/tests/fixtures/response_failed.sse");

    #[test]
    fn stream_identity_covers_model_binding_and_reasoning_text_lifecycle()
    -> Result<(), &'static str> {
        let base = ResponsesStreamDialectData::codex(MODEL);
        let base_identity = base.execution_identity().ok_or("base identity")?;

        let mut different_binding = base.clone();
        different_binding.lifecycle.model_binding =
            plakous_responses::ResponseModelBinding::ExactRequest;
        assert_ne!(
            base_identity,
            different_binding
                .execution_identity()
                .ok_or("model-binding identity")?
        );

        let mut reasoning_text = base.clone();
        reasoning_text.lifecycle.reasoning_content_text =
            !reasoning_text.lifecycle.reasoning_content_text;
        assert_ne!(
            base_identity,
            reasoning_text
                .execution_identity()
                .ok_or("reasoning-text identity")?
        );

        let mut grok_length = ResponsesStreamDialectData::grok("grok-test");
        let length_identity = grok_length
            .execution_identity()
            .ok_or("Grok length identity")?;
        grok_length.lifecycle.incomplete_length_text_snapshot = false;
        assert_ne!(
            length_identity,
            grok_length
                .execution_identity()
                .ok_or("strict Grok identity")?
        );

        let mut refreshed = base.clone();
        refreshed.lifecycle.reasoning_terminal_ciphertext_refresh = true;
        assert_ne!(
            base_identity,
            refreshed
                .execution_identity()
                .ok_or("reasoning ciphertext identity")?
        );
        Ok(())
    }

    fn decode_in_chunks(
        source: &str,
        chunk_bytes: usize,
    ) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
        let mut decoder = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::codex(MODEL),
            ResponsesStreamLimits::default(),
        );
        let mut events = Vec::new();
        for chunk in source.as_bytes().chunks(chunk_bytes) {
            events.extend(decoder.push(chunk)?);
        }
        events.extend(decoder.finish()?);
        Ok(events)
    }

    fn decode_openrouter(
        source: &str,
    ) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
        let mut decoder = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::openrouter(MODEL),
            ResponsesStreamLimits::default(),
        );
        let mut events = decoder.push(source.as_bytes())?;
        events.extend(decoder.finish()?);
        Ok(events)
    }

    fn grok_incomplete_text_events() -> Vec<Value> {
        vec![
            serde_json::json!({"type":"response.created","sequence_number":0,
                "response":{"id":"resp-grok-length","model":"grok-test"}}),
            serde_json::json!({"type":"response.output_item.added","sequence_number":1,
                "output_index":0,"item":{"id":"reason","type":"reasoning","summary":[]}}),
            serde_json::json!({"type":"response.output_item.done","sequence_number":2,
                "output_index":0,"item":{"id":"reason","type":"reasoning","status":"completed","summary":[]}}),
            serde_json::json!({"type":"response.output_item.added","sequence_number":3,
                "output_index":1,"item":{"id":"message","type":"message","role":"assistant","content":[]}}),
            serde_json::json!({"type":"response.content_part.added","sequence_number":4,
                "item_id":"message","output_index":1,"content_index":0,
                "part":{"type":"output_text","text":"","annotations":[]}}),
            serde_json::json!({"type":"response.output_text.delta","sequence_number":5,
                "item_id":"message","output_index":1,"content_index":0,"delta":"summary"}),
            serde_json::json!({"type":"response.incomplete","sequence_number":6,
                "response":{"id":"resp-grok-length","object":"response","status":"incomplete",
                    "model":"grok-test","output":[
                        {"id":"reason","type":"reasoning","status":"completed","summary":[]},
                        {"id":"message","type":"message","role":"assistant","status":"incomplete",
                            "content":[{"type":"output_text","text":"summary","annotations":[]}]}
                    ],"usage":{"input_tokens":2,"output_tokens":7,"total_tokens":9},
                    "incomplete_details":{"reason":"max_output_tokens"}}}),
        ]
    }

    fn decode_grok_events(
        events: &[Value],
    ) -> Result<Vec<ResponsesFamilyStreamEvent>, ResponsesFamilyStreamError> {
        let mut decoder = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::grok("grok-test"),
            ResponsesStreamLimits::default(),
        );
        let mut output = Vec::new();
        for event in events {
            let frame = format!("data: {event}\n\n");
            for chunk in frame.as_bytes().chunks(11) {
                output.extend(decoder.push(chunk)?);
            }
        }
        output.extend(decoder.finish()?);
        Ok(output)
    }

    #[test]
    fn grok_length_terminal_accepts_exact_unclosed_text_after_completed_reasoning()
    -> Result<(), ResponsesFamilyStreamError> {
        let events = decode_grok_events(&grok_incomplete_text_events())?;
        assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
        let Some(ResponsesStreamEventKind::ResponseIncomplete(response)) = events.last() else {
            panic!("Grok length terminal remains typed incomplete");
        };
        assert_eq!(response.output().len(), 2);
        assert_eq!(
            response.usage().and_then(ResponsesUsage::total_tokens),
            Some(9)
        );
        Ok(())
    }

    #[test]
    fn grok_length_terminal_rejects_mismatched_or_non_length_unclosed_items() {
        for mutate in [
            |events: &mut Vec<Value>| {
                events[6]["response"]["output"][1]["content"][0]["text"] =
                    serde_json::json!("changed");
            },
            |events: &mut Vec<Value>| {
                events[6]["response"]["output"][1]["id"] = serde_json::json!("other");
            },
            |events: &mut Vec<Value>| {
                events[6]["response"]["output"][1]["role"] = serde_json::json!("user");
            },
            |events: &mut Vec<Value>| events[4]["content_index"] = serde_json::json!(1),
            |events: &mut Vec<Value>| events[6]["response"]["incomplete_details"] = Value::Null,
            |events: &mut Vec<Value>| {
                events[6]["response"]["incomplete_details"]["reason"] = serde_json::json!("other");
            },
            |events: &mut Vec<Value>| {
                events[6]["type"] = serde_json::json!("response.completed");
                events[6]["response"]["status"] = serde_json::json!("completed");
                events[6]["response"]["incomplete_details"] = Value::Null;
            },
            |events: &mut Vec<Value>| {
                events[3]["item"]["type"] = serde_json::json!("function_call");
            },
        ] {
            let mut events = grok_incomplete_text_events();
            mutate(&mut events);
            assert!(decode_grok_events(&events).is_err());
        }
    }

    #[test]
    fn grok_length_terminal_never_finishes_an_unclosed_function_call() {
        let events = vec![
            serde_json::json!({"type":"response.created","sequence_number":0,
                "response":{"id":"resp-call","model":"grok-test"}}),
            serde_json::json!({"type":"response.output_item.added","sequence_number":1,
                "output_index":0,"item":{"id":"call-item","type":"function_call",
                    "call_id":"call-1","name":"lookup","arguments":""}}),
            serde_json::json!({"type":"response.function_call_arguments.delta","sequence_number":2,
                "item_id":"call-item","output_index":0,"delta":"{\"q\":1}"}),
            serde_json::json!({"type":"response.incomplete","sequence_number":3,
                "response":{"id":"resp-call","object":"response","status":"incomplete",
                    "model":"grok-test","output":[{"id":"call-item","type":"function_call",
                        "status":"incomplete","call_id":"call-1","name":"lookup",
                        "arguments":"{\"q\":1}"}],
                    "usage":{"input_tokens":2,"output_tokens":7,"total_tokens":9},
                    "incomplete_details":{"reason":"max_output_tokens"}}}),
        ];
        assert!(matches!(
            decode_grok_events(&events),
            Err(ResponsesFamilyStreamError::OutputBoundReached)
        ));
    }

    #[test]
    fn current_openrouter_item_correlated_lifecycle_maps_to_family_events() {
        let stream = format!(
            concat!(
                "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-or\",\"model\":\"{}\"}}}}\n\n",
                "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"id\":\"msg-or\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}}}\n\n",
                "data: {{\"type\":\"response.content_part.added\",\"item_id\":\"msg-or\",\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":\"\",\"annotations\":[]}}}}\n\n",
                "data: {{\"type\":\"response.content_part.delta\",\"item_id\":\"msg-or\",\"output_index\":0,\"content_index\":0,\"delta\":\"answer\"}}\n\n",
                "data: {{\"type\":\"response.output_text.done\",\"item_id\":\"msg-or\",\"output_index\":0,\"content_index\":0,\"text\":\"answer\"}}\n\n",
                "data: {{\"type\":\"response.content_part.done\",\"item_id\":\"msg-or\",\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":\"answer\",\"annotations\":[]}}}}\n\n",
                "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"id\":\"msg-or\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{{\"type\":\"output_text\",\"text\":\"answer\",\"annotations\":[]}}]}}}}\n\n",
                "data: {{\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{{\"id\":\"rs-or\",\"type\":\"reasoning\",\"summary\":[]}}}}\n\n",
                "data: {{\"type\":\"response.reasoning_summary_part.added\",\"item_id\":\"rs-or\",\"output_index\":1,\"summary_index\":0,\"part\":{{\"type\":\"summary_text\",\"text\":\"\"}}}}\n\n",
                "data: {{\"type\":\"response.reasoning.delta\",\"item_id\":\"rs-or\",\"output_index\":1,\"delta\":\"think\"}}\n\n",
                "data: {{\"type\":\"response.reasoning_summary_text.done\",\"item_id\":\"rs-or\",\"output_index\":1,\"summary_index\":0,\"text\":\"think\"}}\n\n",
                "data: {{\"type\":\"response.reasoning_summary_part.done\",\"item_id\":\"rs-or\",\"output_index\":1,\"summary_index\":0,\"part\":{{\"type\":\"summary_text\",\"text\":\"think\"}}}}\n\n",
                "data: {{\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{{\"id\":\"rs-or\",\"type\":\"reasoning\",\"status\":\"completed\",\"summary\":[{{\"type\":\"summary_text\",\"text\":\"think\"}}]}}}}\n\n",
                "data: {{\"type\":\"response.done\",\"response\":{{\"id\":\"resp-or\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"{}\",\"output\":[{{\"id\":\"msg-or\",\"type\":\"message\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{{\"type\":\"output_text\",\"text\":\"answer\",\"annotations\":[]}}]}},{{\"id\":\"rs-or\",\"type\":\"reasoning\",\"status\":\"completed\",\"summary\":[{{\"type\":\"summary_text\",\"text\":\"think\"}}]}}],\"usage\":{{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}}}}\n\n",
                "data: [DONE]\n\n"
            ),
            MODEL, MODEL
        );
        let events = decode_openrouter(&stream)
            .unwrap_or_else(|error| panic!("documented OpenRouter deltas must decode: {error}"));
        assert!(events.iter().any(|event| matches!(
            event,
            ResponsesStreamEventKind::OutputTextDelta { delta, .. } if delta == "answer"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ResponsesStreamEventKind::ReasoningSummaryTextDelta { delta, .. } if delta == "think"
        )));

        let substituted = stream.replacen(
            "\"item_id\":\"msg-or\",\"output_index\":0,\"content_index\":0,\"delta\"",
            "\"item_id\":\"rs-or\",\"output_index\":0,\"content_index\":0,\"delta\"",
            1,
        );
        assert!(matches!(
            decode_openrouter(&substituted),
            Err(ResponsesFamilyStreamError::Lifecycle(_))
        ));

        let missing = stream.replacen("\"item_id\":\"msg-or\",", "", 1);
        assert!(matches!(
            decode_openrouter(&missing),
            Err(ResponsesFamilyStreamError::InvalidField("item_id"))
        ));
    }

    #[test]
    fn openrouter_response_error_and_plain_error_use_top_level_canonical_type() {
        for (event_type, canonical, expected) in [
            (
                "response.error",
                "authentication",
                ResponsesProviderErrorClass::Authentication,
            ),
            (
                "error",
                "rate_limit_exceeded",
                ResponsesProviderErrorClass::RateLimited,
            ),
        ] {
            let stream = format!(
                concat!(
                    "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-error\",\"model\":\"{}\"}}}}\n\n",
                    "data: {{\"type\":\"{}\",\"error\":{{\"code\":\"server_error\",\"message\":\"redacted\"}},\"error_type\":\"{}\"}}\n\n",
                    "data: [DONE]\n\n"
                ),
                MODEL, event_type, canonical
            );
            let Ok(events) = decode_openrouter(&stream) else {
                panic!("OpenRouter error event must decode");
            };
            let Some(ResponsesStreamEventKind::Error(error)) = events.last() else {
                panic!("OpenRouter stream must retain the typed provider error");
            };
            assert_eq!(error.class(), expected);
            assert_eq!(error.kind(), Some(canonical));
            assert_eq!(error.code(), Some("server_error"));
        }
    }

    #[test]
    fn openrouter_alias_events_do_not_broaden_codex_typing() {
        let mut hooks = ResponsesFamilyHooks::new(ResponsesStreamDialectData::codex(MODEL));
        let created = serde_json::json!({
            "type": "response.created",
            "response": {"id": "resp-codex", "model": MODEL}
        });
        assert!(hooks.parse_event(&created).is_ok());
        for kind in [
            "response.content_part.delta",
            "response.reasoning.delta",
            "response.error",
        ] {
            let value = serde_json::json!({"type": kind, "delta": "not typed"});
            assert!(matches!(
                hooks.parse_event(&value),
                Ok(ResponsesStreamEventKind::Unknown { kind: retained, .. })
                    if retained == kind
            ));
        }
    }

    #[test]
    fn every_byte_split_preserves_text_reasoning_tools_unknown_usage_and_cache() {
        let Ok(events) = decode_in_chunks(MIXED, 1) else {
            panic!("mixed fixture must decode at every byte boundary");
        };
        assert!(events.iter().any(|event| matches!(
            event,
            ResponsesStreamEventKind::OutputTextDelta { delta, .. } if delta.contains('🌍')
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ResponsesStreamEventKind::ReasoningSummaryTextDelta { delta, .. } if delta == "Checked."
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ResponsesStreamEventKind::FunctionCallArgumentsDone { arguments, .. }
                if arguments == "{\"q\":\"rust\"}"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ResponsesStreamEventKind::Unknown { kind, .. }
                if kind == "response.specialized_future_event"
        )));
        let Some(ResponsesStreamEventKind::ResponseCompleted(response)) = events.last() else {
            panic!("mixed fixture must end in a completed response");
        };
        let Some(usage) = response.usage() else {
            panic!("mixed fixture must retain usage");
        };
        assert_eq!(usage.total_tokens(), Some(20));
        assert_eq!(usage.cached_input_tokens(), Some(4));
        assert_eq!(usage.reasoning_tokens(), Some(3));
    }

    #[test]
    fn codex_custom_tools_and_url_citations_are_typed() {
        let Ok(events) = decode_in_chunks(O1_SURFACES, 7) else {
            panic!("Codex surface fixture must decode");
        };
        assert!(events.iter().any(|event| matches!(
            event,
            ResponsesStreamEventKind::OutputTextAnnotationAdded {
                annotation: ResponsesAnnotation::UrlCitation { url, .. },
                ..
            } if url == "https://example.test/docs"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ResponsesStreamEventKind::CustomToolCallInputDelta { delta, .. }
                if delta == "print(1)"
        )));
        let Some(ResponsesStreamEventKind::ResponseCompleted(response)) = events.last() else {
            panic!("Codex fixture must end in a completed response");
        };
        assert!(matches!(
            response.output().last(),
            Some(ResponsesOutputItem::CustomToolCall { input, .. }) if input == "print(1)"
        ));
    }

    #[test]
    fn codex_soft_terminal_uses_completed_stream_items_when_terminal_output_is_empty() {
        let stream = format!(
            concat!(
                "data: {{\"type\":\"response.created\",\"sequence_number\":0,\"response\":{{\"id\":\"resp-soft\",\"model\":\"{}\"}}}}\n\n",
                "data: {{\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{{\"id\":\"msg-soft\",\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"content\":[]}}}}\n\n",
                "data: {{\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"msg-soft\",\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":\"\",\"annotations\":[]}}}}\n\n",
                "data: {{\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"msg-soft\",\"output_index\":0,\"content_index\":0,\"delta\":\"ok\"}}\n\n",
                "data: {{\"type\":\"response.output_text.done\",\"sequence_number\":4,\"item_id\":\"msg-soft\",\"output_index\":0,\"content_index\":0,\"text\":\"ok\"}}\n\n",
                "data: {{\"type\":\"response.content_part.done\",\"sequence_number\":5,\"item_id\":\"msg-soft\",\"output_index\":0,\"content_index\":0,\"part\":{{\"type\":\"output_text\",\"text\":\"ok\",\"annotations\":[]}}}}\n\n",
                "data: {{\"type\":\"response.output_item.done\",\"sequence_number\":6,\"output_index\":0,\"item\":{{\"id\":\"msg-soft\",\"type\":\"message\",\"role\":\"assistant\",\"phase\":\"final_answer\",\"status\":\"completed\",\"content\":[{{\"type\":\"output_text\",\"text\":\"ok\",\"annotations\":[]}}]}}}}\n\n",
                "data: {{\"type\":\"response.completed\",\"sequence_number\":7,\"response\":{{\"id\":\"resp-soft\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"{}\",\"output\":[],\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n\n"
            ),
            MODEL, MODEL
        );
        let Ok(events) = decode_in_chunks(&stream, 11) else {
            panic!("soft Codex terminal must decode");
        };
        let Some(ResponsesStreamEventKind::ResponseCompleted(response)) = events.last() else {
            panic!("soft Codex fixture must end in completion");
        };
        assert_eq!(response.output().len(), 1);
        assert!(matches!(
            response.output().first(),
            Some(ResponsesOutputItem::Message { .. })
        ));
        assert_eq!(response.native()["output"], serde_json::json!([]));
    }

    #[test]
    fn incomplete_and_failed_terminals_remain_typed() {
        let Ok(incomplete) = decode_in_chunks(INCOMPLETE, 13) else {
            panic!("incomplete fixture must decode");
        };
        let Some(ResponsesStreamEventKind::ResponseIncomplete(response)) = incomplete.last() else {
            panic!("incomplete fixture must retain its terminal");
        };
        assert_eq!(
            response
                .incomplete_details()
                .and_then(|details| details.reason()),
            Some("max_output_tokens")
        );

        let mut decoder = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::codex(MODEL),
            ResponsesStreamLimits::default(),
        );
        let Ok(before_finish) = decoder.push(FAILED_RESIDUAL.as_bytes()) else {
            panic!("failed residual prefix must decode");
        };
        assert!(
            !before_finish
                .iter()
                .any(ResponsesStreamEventKind::is_terminal)
        );
        let Ok(residual) = decoder.finish() else {
            panic!("EOF must dispatch the residual failed terminal");
        };
        let [ResponsesStreamEventKind::ResponseFailed(response)] = residual.as_slice() else {
            panic!("finish must deliver exactly the residual failed terminal");
        };
        assert_eq!(
            response.error().map(ResponsesProviderError::class),
            Some(ResponsesProviderErrorClass::RateLimited)
        );
    }

    #[test]
    fn malformed_and_limit_failures_poison_without_cloning_state() {
        let mut malformed = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::codex(MODEL),
            ResponsesStreamLimits::default(),
        );
        assert_eq!(
            malformed.push(b"data: {not-json}\n\n"),
            Err(ResponsesFamilyStreamError::InvalidJson)
        );
        assert_eq!(
            malformed.push(b"data: {}\n\n"),
            Err(ResponsesFamilyStreamError::DecoderFailed)
        );

        let limits = ResponsesStreamLimits {
            max_frame_bytes: 32,
            max_total_bytes: 128,
            max_events: 2,
        };
        let mut bounded =
            ResponsesFamilySseDecoder::new(ResponsesStreamDialectData::codex(MODEL), limits);
        assert_eq!(
            bounded.push(b"data: {\"type\":\"response.created\",\"sequence_number\":0}\n\n"),
            Err(ResponsesFamilyStreamError::ResourceLimit("SSE frame bytes"))
        );
        assert_eq!(
            bounded.finish(),
            Err(ResponsesFamilyStreamError::DecoderFailed)
        );

        let Ok(response_limits) = ResponsesResponseLimits::new(1_024, 8, 32) else {
            panic!("valid bounded-unknown test limits");
        };
        let mut unknown = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::codex(MODEL).with_response_limits(response_limits),
            ResponsesStreamLimits::default(),
        );
        let created = format!(
            "data: {{\"type\":\"response.created\",\"sequence_number\":0,\"response\":{{\"id\":\"r\",\"model\":\"{MODEL}\"}}}}\n\n"
        );
        let unknown_event = format!(
            "data: {{\"type\":\"response.future\",\"sequence_number\":1,\"future\":\"{}\"}}\n\n",
            "x".repeat(64)
        );
        assert!(unknown.push(created.as_bytes()).is_ok());
        assert_eq!(
            unknown.push(unknown_event.as_bytes()),
            Err(ResponsesFamilyStreamError::ResourceLimit(
                "unknown event bytes"
            ))
        );
    }

    #[test]
    fn grok_allowlist_preserves_foreign_events_and_requires_exact_usage() {
        let stream = concat!(
            "data: {\"type\":\"response.created\",\"sequence_number\":1,\"response\":{\"id\":\"resp-grok\",\"model\":\"grok-test\"}}\n\n",
            "data: {\"type\":\"response.output_text.done\",\"sequence_number\":2,\"output_index\":0,\"content_index\":0,\"text\":\"future\"}\n\n",
            "data: {\"type\":\"response.completed\",\"sequence_number\":3,\"response\":{\"id\":\"resp-grok\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"grok-test\",\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":3,\"total_tokens\":5},\"incomplete_details\":null,\"error\":null}}\n\n"
        );
        let mut decoder = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::grok("grok-test"),
            ResponsesStreamLimits::default(),
        );
        let mut events = decoder
            .push(stream.as_bytes())
            .unwrap_or_else(|error| panic!("Grok stream must decode: {error:?}"));
        events.extend(
            decoder
                .finish()
                .unwrap_or_else(|error| panic!("Grok stream must finish: {error:?}")),
        );
        assert!(matches!(
            &events[1],
            ResponsesStreamEventKind::Unknown { kind, native }
                if kind == "response.output_text.done"
                    && native["text"] == "future"
        ));
        let Some(ResponsesStreamEventKind::ResponseCompleted(response)) = events.last() else {
            panic!("typed Grok terminal");
        };
        assert_eq!(
            response.usage().and_then(ResponsesUsage::total_tokens),
            Some(5)
        );

        let missing_total = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-grok\",\"model\":\"grok-test\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-grok\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"grok-test\",\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":3},\"incomplete_details\":null,\"error\":null}}\n\n"
        );
        let mut decoder = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::grok("grok-test"),
            ResponsesStreamLimits::default(),
        );
        assert_eq!(
            decoder.push(missing_total.as_bytes()),
            Err(ResponsesFamilyStreamError::InvalidField("total_tokens"))
        );
    }

    #[test]
    fn grok_completed_terminal_ignores_trailing_events_and_bytes() {
        let completed = concat!(
            "data: {\"type\":\"response.created\",\"sequence_number\":1,\"response\":{\"id\":\"resp-grok\",\"model\":\"grok-test\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"sequence_number\":2,\"response\":{\"id\":\"resp-grok\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"grok-test\",\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":3,\"total_tokens\":5},\"incomplete_details\":null,\"error\":null}}\n\n"
        );
        let trailers: [&[u8]; 4] = [
            b"data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"delta\":\"late\"}\n\n",
            b"data: [DONE]\n\n",
            b"\xff\xfe not json\n\n",
            b"data: {\"type\":\"response.comp",
        ];
        for trailer in trailers {
            for split in [false, true] {
                let mut decoder = ResponsesFamilySseDecoder::new(
                    ResponsesStreamDialectData::grok("grok-test"),
                    ResponsesStreamLimits::default(),
                );
                let mut events = Vec::new();
                if split {
                    events.extend(
                        decoder
                            .push(completed.as_bytes())
                            .unwrap_or_else(|error| panic!("Grok stream must decode: {error:?}")),
                    );
                    events.extend(decoder.push(trailer).unwrap_or_else(|error| {
                        panic!("trailer after the terminal must be ignored: {error:?}")
                    }));
                } else {
                    let mut body = completed.as_bytes().to_vec();
                    body.extend_from_slice(trailer);
                    events.extend(decoder.push(&body).unwrap_or_else(|error| {
                        panic!("buffered trailer must be ignored: {error:?}")
                    }));
                }
                events.extend(
                    decoder
                        .finish()
                        .unwrap_or_else(|error| panic!("Grok stream must finish: {error:?}")),
                );
                assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
                assert!(matches!(
                    events.last(),
                    Some(ResponsesStreamEventKind::ResponseCompleted(_))
                ));
            }
        }
    }

    #[test]
    fn grok_error_allowlist_and_unique_json_are_strict() {
        let error = "data: {\"type\":\"error\",\"error\":{\"type\":\"server_error\",\"code\":\"internal_error\"}}\n\n";
        let mut decoder = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::grok("grok-test"),
            ResponsesStreamLimits::default(),
        );
        let events = decoder
            .push(error.as_bytes())
            .unwrap_or_else(|error| panic!("Grok error event must decode: {error:?}"));
        assert!(matches!(
            events.as_slice(),
            [ResponsesStreamEventKind::Error(error)]
                if error.class() == ResponsesProviderErrorClass::Server
        ));
        decoder
            .finish()
            .unwrap_or_else(|error| panic!("typed error terminal must finish: {error:?}"));

        let duplicate = "data: {\"type\":\"error\",\"error\":{\"code\":\"invalid_api_key\",\"code\":\"server_error\"}}\n\n";
        let mut decoder = ResponsesFamilySseDecoder::new(
            ResponsesStreamDialectData::grok("grok-test"),
            ResponsesStreamLimits::default(),
        );
        assert_eq!(
            decoder.push(duplicate.as_bytes()),
            Err(ResponsesFamilyStreamError::InvalidJson)
        );
    }
}
