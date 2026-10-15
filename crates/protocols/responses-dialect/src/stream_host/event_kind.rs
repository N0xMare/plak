//! Typed Responses stream event kinds shared by every dialect.

use super::{
    Map, ResponsesAnnotation, ResponsesLifecycle, ResponsesLifecycleError, ResponsesOutputItem,
    Value, fmt,
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
    #[allow(
        clippy::too_many_lines,
        reason = "one linear host step whose ordering is the invariant"
    )]
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
    #[allow(
        clippy::too_many_lines,
        reason = "one linear host step whose ordering is the invariant"
    )]
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
