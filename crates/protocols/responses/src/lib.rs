//! Responses-family protocol kernel primitives.
//!
//! ## Ownership
//!
//! | Layer | This crate | Dialect plugins (`OpenAI` / xAI / `OpenRouter`, …) |
//! | --- | --- | --- |
//! | Wire grammar tables | Sequence, terminal/`[DONE]`, BOM policies | Choose a [`ResponsesDialectProfile`] |
//! | Stream front-half | Framing, BOM, limits, sequence, `[DONE]`/terminal finish via [`ResponsesStreamKernel`] | [`DialectStreamHooks`] JSON event-shape parse |
//! | Stream lifecycle | Pure [`ResponsesLifecycle`] (items/content/reasoning/function correlation) | Parse → native `Value`s; call lifecycle; map [`ResponsesLifecycleError`] |
//! | Stage-2 ModelEvent projection | Shared [`ResponseProjectorCore`] (xAI / OpenRouter / OpenAI host) | Event-kind dispatch, tool admission, replay kinds, residual dialect hooks |
//! | Request wire | Pure `request` helpers (effort, tool_choice, items, flags) | Origin, paths, auth, dialect deltas (`provider{}`, SuperGrok omit `tool_choice`, `previous_response_id`) |
//! | Attempt authority | Never — lives in `plakous-attempt` | Origin, paths, headers, auth, catalog, dialect observation |
//! | Model-facing policy | Never | Never in this crate |
//!
//! **Attempt lifecycle / ledger / await** is family-agnostic and owned by
//! `plakous-attempt`. Depend on that crate directly; this kernel is pure wire.
//!
//! **Anthropic is Messages-family** and is deliberately **not** hosted here
//! for wire/event machines. Messages may adopt `plakous-attempt` commons only.
//! Do not merge Messages stream semantics into this kernel for LOC alone.
//!
//! ### Stream kernel status
//!
//! [`ResponsesStreamKernel`] centralizes SSE framing, optional BOM strip,
//! total-byte/event limits, sequence validation, and terminal/`[DONE]` policy.
//! [`ResponsesLifecycle`] centralizes response item/content/reasoning/function
//! lifecycle correlation (xAI, OpenRouter, and OpenAI Platform). Stage-2
//! ModelEvent projection is shared via [`ResponseProjectorCore`] (xAI/OpenRouter
//! live/strict; OpenAI Platform host with SoftSeedEmptyOnly + DeferToFinish).
//! Dialects own event-shape parsing in [`DialectStreamHooks::decode_json_event`]
//! and residual hooks (citations, custom tools, incomplete terminals).
//!
//! ### Request helpers
//!
//! Pure wire helpers for common body fields (reasoning effort labels,
//! function `tool_choice`, function tool defs, instruction join, stream/store,
//! input-item Value builders). Dialects own full encode/prepare and
//! fail-closed deltas.
//!
//! ## Not in this crate
//!
//! - Provider HTTP clients, credentials, or route identities
//! - Agent loop / session / history stacks
//! - Anthropic Messages event machines
//! - Canonical model type definitions: the projector emits `plakous-model`
//!   events but defines none

#![forbid(unsafe_code)]
// Protocol docs name OpenAI/OpenRouter/SuperGrok; Result-heavy pure wire helpers;
// lifecycle policy bool tables; projector/tool start arity matches wire correlation.
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::struct_excessive_bools,
    clippy::must_use_candidate,
    clippy::too_many_arguments,
    reason = "product names in ownership docs; pure Result helpers; policy bool tables; wire correlation arity"
)]

mod artifact;
mod dialect;
mod json_fields;
mod project;
mod request;
mod stream;

pub use artifact::{
    MAX_RESPONSES_ARTIFACT_BYTES, MAX_RESPONSES_REPLAY_BYTES, MAX_RESPONSES_REPLAY_ITEMS,
    RESPONSES_ARTIFACT_CODEC_REVISION, RESPONSES_ARTIFACT_SCHEMA_REVISION, ResponsesArtifact,
    ResponsesArtifactBinding, ResponsesArtifactCodec, ResponsesArtifactError,
    ResponsesArtifactFamily, ResponsesArtifactProduction, ResponsesArtifactReplayKind,
    ResponsesArtifactReuse, ResponsesArtifactReuseKind, ResponsesAuthorizedReplay,
    ResponsesCacheFact, ResponsesContinuation, ResponsesReplayContract, ResponsesReplayItem,
    ResponsesReplayPurpose, ResponsesSemanticRevisions, ResponsesStorageMode,
};
pub use dialect::{
    BomPolicy, DoneMarkerDecision, FinishPolicy, OPENAI_PLATFORM_SEQUENCE_FIELDS,
    OPENROUTER_SEQUENCE_FIELDS, ResponsesDialectProfile, SequencePolicy, SequenceState,
    SequenceValidationError, TerminalPolicy, UTF8_BOM, XAI_SEQUENCE_FIELDS, decide_done_marker,
    terminal_finish_ok, validate_next_sequence,
};
pub use json_fields::{
    MAX_PROVIDER_LABEL_BYTES, ResponsesFieldError, optional_object, optional_provider_label,
    optional_string, optional_u64, optional_u64_fields, required_object, required_provider_label,
    required_string, required_u32, required_u64, required_value, validate_provider_label,
};
pub use project::{
    CanonicalIdSources, FamilyProjectionError, ProjectedBlock, ProjectedItem, ProjectedItemKind,
    ProjectorLimits, ProjectorPolicy, ReasoningEmitMode, ResponseProjectorCore,
    ToolArgsFinishPolicy,
};
pub use request::{
    FunctionToolChoiceWire, FunctionToolWire, REASONING_SUMMARY_AUTO, ReasoningConfigWire,
    ReasoningEffortWire, function_call_item, function_call_output_item, input_text_part,
    insert_stream_store, join_instruction_texts, message_item, output_text_part, reasoning_object,
};
pub use stream::{
    DialectStreamHooks, LifecycleDialectPolicy, ReasoningLifecycleMode, ResponseModelBinding,
    ResponsesDecodeLimitError, ResponsesDecodeLimits, ResponsesLifecycle, ResponsesLifecycleError,
    ResponsesStreamKernel, ResponsesStreamLimitError, ResponsesStreamLimits,
    is_comment_only_unfinished,
};
