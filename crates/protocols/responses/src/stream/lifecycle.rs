//! Shared Responses-family stream lifecycle (item/content/reasoning/function
//! correlation).
//!
//! Pure [`serde_json::Value`] natives only — no dialect item types and no
//! `plakous-model`. Dialects parse events, then call into
//! [`ResponsesLifecycle`] with natives and map [`ResponsesLifecycleError`].

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};
use thiserror::Error;

use crate::json_fields::{ResponsesFieldError, required_object, required_string};

/// Dialect policy flags that select xAI / OpenRouter / OpenAI lifecycle variants.
///
/// Construct via [`LifecycleDialectPolicy::xai`],
/// [`LifecycleDialectPolicy::openrouter`], or [`LifecycleDialectPolicy::openai`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleDialectPolicy {
    /// When true, `response.created` requires a non-empty response ID.
    pub require_created_id: bool,
    /// When true, `response.created` requires a non-empty model.
    pub require_created_model: bool,
    /// Whether provider-reported response model identity must equal the exact
    /// requested wire model. Some proxies report a routed backend identity;
    /// those dialects still require create/terminal consistency and preserve
    /// the reported identity as the actual model.
    pub model_binding: ResponseModelBinding,
    /// Reasoning summary event machine.
    pub reasoning: ReasoningLifecycleMode,
    /// When true: seed/track content `text_done`, refusal accumulation, and
    /// message-done may use accumulated parts without `content_part.done`.
    pub content_text_done: bool,
    /// When true, reasoning items may stream `reasoning_text` content parts.
    pub reasoning_content_text: bool,
    /// When true, non-terminal intermediate events must carry matching
    /// `response_id`. Current OpenRouter and OpenAI Responses correlate via
    /// `item_id` / `output_index` instead and leave this false.
    pub intermediate_response_id: bool,
    /// When true, terminal `output` must match streamed items 1:1 by index and
    /// argument done must equal streamed deltas exactly. OpenAI Platform may
    /// still re-encrypt a reasoning item's `encrypted_content` at the terminal
    /// ([`Self::reasoning_terminal_ciphertext_refresh`]); every other difference
    /// fails closed.
    /// When false (Codex/ChatGPT), stream `output_item.done` is authoritative:
    /// each streamed item must complete, but the terminal envelope may rewrite,
    /// reorder, or include extra items (e.g. reasoning).
    /// Soft tool-args correlation may accept re-serialized done payloads for
    /// lifecycle bookkeeping; **ModelEvent projection** uses SoftSeedEmptyOnly
    /// (seed when empty; keep non-empty deltas — see OpenAI `project_tool_input_done`).
    pub strict_terminal_output: bool,
    /// When true, a strict terminal reasoning item may carry a different
    /// `encrypted_content` if every other field is identical. OpenAI Platform
    /// re-wraps that opaque token in the terminal response. The terminal item
    /// stays the replay authority and is not rewritten to the streamed
    /// `output_item.done` ciphertext. Compaction and every other field still
    /// fail closed. Other strict routes leave this false.
    pub reasoning_terminal_ciphertext_refresh: bool,
    /// Whether a sparse terminal may recover its omitted model exclusively
    /// from the exact prepared request binding.
    pub sparse_terminal_model_from_prepared: bool,
    /// Whether a sparse terminal may recover its omitted output exclusively
    /// from fully completed streamed items.
    pub sparse_terminal_output_from_completed: bool,
    /// SuperGrok subscription may end a length-limited response with one
    /// unfinished assistant text item whose terminal snapshot matches deltas.
    pub incomplete_length_text_snapshot: bool,
}

impl LifecycleDialectPolicy {
    /// xAI Responses dialect: optional create identity, delta-only reasoning,
    /// no intermediate `response_id`, no content text_done events.
    #[must_use]
    pub const fn xai() -> Self {
        Self {
            require_created_id: false,
            require_created_model: false,
            model_binding: ResponseModelBinding::ProviderReported,
            reasoning: ReasoningLifecycleMode::DeltaOnly,
            content_text_done: false,
            reasoning_content_text: false,
            intermediate_response_id: false,
            strict_terminal_output: true,
            reasoning_terminal_ciphertext_refresh: false,
            sparse_terminal_model_from_prepared: false,
            sparse_terminal_output_from_completed: false,
            incomplete_length_text_snapshot: false,
        }
    }

    /// SuperGrok subscription lifecycle, including its length-limited text
    /// snapshot. The xAI Platform API retains the stricter [`Self::xai`] policy.
    #[must_use]
    pub const fn grok() -> Self {
        let mut policy = Self::xai();
        policy.incomplete_length_text_snapshot = true;
        policy
    }

    /// OpenRouter Responses dialect: required create response ID, optional
    /// create model, OpenAI-style intermediate item correlation, exact
    /// prepared-model recovery for a sparse terminal, and completed-item
    /// recovery for sparse terminal output.
    #[must_use]
    pub const fn openrouter() -> Self {
        Self {
            require_created_id: true,
            require_created_model: false,
            model_binding: ResponseModelBinding::ExactRequest,
            reasoning: ReasoningLifecycleMode::FullParts,
            content_text_done: true,
            reasoning_content_text: true,
            intermediate_response_id: false,
            strict_terminal_output: true,
            reasoning_terminal_ciphertext_refresh: false,
            sparse_terminal_model_from_prepared: true,
            sparse_terminal_output_from_completed: true,
            incomplete_length_text_snapshot: false,
        }
    }

    /// Codex / ChatGPT subscription Responses dialect (soft terminal / tool-arg).
    ///
    /// **Mirrors OpenRouter** for: required create identity,
    /// [`ReasoningLifecycleMode::FullParts`], `content_text_done`, and
    /// `item_id` / `output_index` intermediate correlation.
    ///
    /// **Differs from OpenRouter:**
    /// - `strict_terminal_output: false` — Codex/ChatGPT may emit terminal output
    ///   that is a superset of streamed items, and may re-serialize tool args.
    /// - `ProviderReported` stream model — official CLI ignores SSE
    ///   `response.model` equality with the requested catalog slug.
    ///
    /// Paid OpenAI Platform API uses [`Self::openai_platform_api`] (strict terminal).
    #[must_use]
    pub const fn openai_codex() -> Self {
        Self {
            require_created_id: true,
            require_created_model: true,
            model_binding: ResponseModelBinding::ProviderReported,
            reasoning: ReasoningLifecycleMode::FullParts,
            content_text_done: true,
            reasoning_content_text: false,
            intermediate_response_id: false,
            strict_terminal_output: false,
            reasoning_terminal_ciphertext_refresh: false,
            sparse_terminal_model_from_prepared: false,
            sparse_terminal_output_from_completed: false,
            incomplete_length_text_snapshot: false,
        }
    }

    /// Paid OpenAI Platform API Responses dialect (strict terminal correlation).
    ///
    /// Reasoning `encrypted_content` may differ between `output_item.done` and
    /// the terminal item. The terminal ciphertext is what replay keeps.
    #[must_use]
    pub const fn openai_platform_api() -> Self {
        Self {
            require_created_id: true,
            require_created_model: true,
            model_binding: ResponseModelBinding::ExactRequest,
            reasoning: ReasoningLifecycleMode::FullParts,
            content_text_done: true,
            reasoning_content_text: false,
            intermediate_response_id: false,
            strict_terminal_output: true,
            reasoning_terminal_ciphertext_refresh: true,
            sparse_terminal_model_from_prepared: false,
            sparse_terminal_output_from_completed: false,
            incomplete_length_text_snapshot: false,
        }
    }

    /// Alias for [`Self::openai_codex`] (historical name).
    #[must_use]
    pub const fn openai() -> Self {
        Self::openai_codex()
    }
}

/// Binding policy for provider-reported response model identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseModelBinding {
    /// Every reported response model must equal the exact requested wire model.
    ExactRequest,
    /// The provider may report a routed backend identity; lifecycle consistency
    /// remains mandatory and the terminal identity is retained as actual model.
    ProviderReported,
}

/// Reasoning summary lifecycle depth.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReasoningLifecycleMode {
    /// Accumulate summary text deltas only (xAI).
    DeltaOnly,
    /// Part added / text delta / text done / part done (OpenRouter superset).
    FullParts,
}

/// Fail-closed Responses lifecycle failure (no provider identity).
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesLifecycleError {
    /// Ordering or correlation violated a documented lifecycle rule.
    #[error("invalid response lifecycle: {reason}")]
    InvalidLifecycle {
        /// Static redacted reason suitable for diagnostics (identical across dialects).
        reason: &'static str,
    },
    /// Provider-reported model does not match the prepared attempt binding.
    #[error("response lifecycle binding mismatch")]
    BindingMismatch,
    /// The response stopped at its output bound with an item still open.
    ///
    /// Distinct from an invalid lifecycle because the runtime can act on it:
    /// the unfinished item never becomes output, and the model can be told.
    #[error("response stopped at its output bound with an unfinished item")]
    OutputBoundReached,
    /// A required wire field was absent.
    #[error("missing JSON field `{field}`")]
    MissingField {
        /// Wire field name.
        field: &'static str,
    },
    /// A wire field had the wrong JSON type or shape.
    #[error("invalid JSON field `{field}`")]
    InvalidField {
        /// Wire field name.
        field: &'static str,
    },
}

impl From<ResponsesFieldError> for ResponsesLifecycleError {
    fn from(error: ResponsesFieldError) -> Self {
        match error {
            ResponsesFieldError::Missing { field } => Self::MissingField { field },
            ResponsesFieldError::Invalid { field } => Self::InvalidField { field },
        }
    }
}

fn lifecycle_json_equivalent(a: &str, b: &str) -> bool {
    match (
        serde_json::from_str::<Value>(a),
        serde_json::from_str::<Value>(b),
    ) {
        (Ok(va), Ok(vb)) => va == vb,
        _ => false,
    }
}

fn invalid_lifecycle(reason: &'static str) -> ResponsesLifecycleError {
    ResponsesLifecycleError::InvalidLifecycle { reason }
}

/// Shared response stream lifecycle state machine.
#[derive(Clone, Debug)]
pub struct ResponsesLifecycle {
    policy: LifecycleDialectPolicy,
    created: Option<CreatedResponse>,
    items: BTreeMap<u32, ItemLifecycle>,
    item_ids: BTreeSet<String>,
    call_ids: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct CreatedResponse {
    id: Option<String>,
    model: Option<String>,
}

#[derive(Clone, Debug)]
struct ItemLifecycle {
    added: Value,
    done: Option<Value>,
    content: BTreeMap<u32, ContentLifecycle>,
    reasoning_summaries: BTreeMap<u32, ReasoningLifecycle>,
    function_arguments: String,
    function_arguments_done: Option<String>,
}

#[derive(Clone, Debug)]
struct ReasoningLifecycle {
    added: Value,
    done: Option<Value>,
    text: String,
    text_done: Option<String>,
}

#[derive(Clone, Debug)]
struct ContentLifecycle {
    added: Value,
    done: Option<Value>,
    text: String,
    text_done: Option<String>,
}

impl ResponsesLifecycle {
    /// Creates an empty lifecycle for one dialect policy.
    #[must_use]
    pub fn new(policy: LifecycleDialectPolicy) -> Self {
        Self {
            policy,
            created: None,
            items: BTreeMap::new(),
            item_ids: BTreeSet::new(),
            call_ids: BTreeSet::new(),
        }
    }

    /// Returns the policy fixed for this lifecycle instance.
    #[must_use]
    pub const fn policy(&self) -> LifecycleDialectPolicy {
        self.policy
    }

    /// Reports whether `response.created` has been accepted.
    #[must_use]
    pub const fn is_created(&self) -> bool {
        self.created.is_some()
    }

    /// Returns the created response id when present.
    #[must_use]
    pub fn created_id(&self) -> Option<&str> {
        self.created
            .as_ref()
            .and_then(|created| created.id.as_deref())
    }

    /// Returns the created response model when present.
    #[must_use]
    pub fn created_model(&self) -> Option<&str> {
        self.created
            .as_ref()
            .and_then(|created| created.model.as_deref())
    }

    /// Returns the number of output items accepted so far.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    /// Returns the `type` of a content part previously added, for dialect
    /// event discrimination (e.g. OpenRouter `content_part.delta`).
    ///
    /// # Errors
    ///
    /// Returns [`ResponsesLifecycleError::InvalidLifecycle`] when the part has
    /// not been added yet.
    pub fn content_part_kind(
        &self,
        output_index: u32,
        content_index: u32,
    ) -> Result<&str, ResponsesLifecycleError> {
        self.items
            .get(&output_index)
            .and_then(|item| item.content.get(&content_index))
            .and_then(|content| content.added.get("type"))
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_lifecycle("content-part delta preceded typed part addition"))
    }

    /// Validates an intermediate event's provider item identity against its
    /// already-added output coordinate.
    ///
    /// # Errors
    ///
    /// Returns a lifecycle failure when the output item is absent or a
    /// reported item ID differs from the item bound to `output_index`.
    pub fn validate_output_item_id(
        &self,
        output_index: u32,
        item_id: Option<&str>,
    ) -> Result<(), ResponsesLifecycleError> {
        self.require_created()?;
        let state = self
            .items
            .get(&output_index)
            .ok_or_else(|| invalid_lifecycle("output event preceded item addition"))?;
        if item_id.is_some_and(|item_id| Some(item_id) != output_item_id(&state.added)) {
            return Err(invalid_lifecycle(
                "event item ID does not match its output item",
            ));
        }
        Ok(())
    }

    /// Validates intermediate-event `response_id` against creation (OpenRouter).
    ///
    /// No-op when [`LifecycleDialectPolicy::intermediate_response_id`] is false.
    /// Terminal and create/error event types are skipped.
    ///
    /// # Errors
    ///
    /// Lifecycle ordering/identity failures, or missing/invalid `response_id`.
    pub fn validate_intermediate_response_id(
        &self,
        object: &Map<String, Value>,
        event_type: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        if !self.policy.intermediate_response_id {
            return Ok(());
        }
        if matches!(
            event_type,
            "response.created"
                | "response.done"
                | "response.incomplete"
                | "response.failed"
                | "response.error"
                | "error"
        ) {
            return Ok(());
        }
        let created = self
            .created
            .as_ref()
            .ok_or_else(|| invalid_lifecycle("intermediate event preceded response creation"))?;
        let created_id = created
            .id
            .as_deref()
            .ok_or_else(|| invalid_lifecycle("intermediate event preceded response creation"))?;
        let response_id = required_string(object, "response_id")?;
        if response_id != created_id {
            return Err(invalid_lifecycle(
                "intermediate event response ID differs from response creation",
            ));
        }
        Ok(())
    }

    /// Records `response.created` and validates the nested response envelope.
    ///
    /// `expected_model` is the prepared attempt model; mismatch yields
    /// [`ResponsesLifecycleError::BindingMismatch`].
    ///
    /// # Errors
    ///
    /// Lifecycle, binding, or field errors.
    pub fn response_created(
        &mut self,
        object: &Map<String, Value>,
        response_id: Option<&str>,
        model: Option<&str>,
        expected_model: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        if self.created.is_some() {
            return Err(invalid_lifecycle("response was created more than once"));
        }
        if !self.items.is_empty() {
            return Err(invalid_lifecycle("response was created after output began"));
        }
        if self.policy.require_created_id {
            if response_id.is_none_or(str::is_empty) {
                return Err(invalid_lifecycle("response ID is empty"));
            }
        } else if response_id.is_some_and(str::is_empty) {
            return Err(invalid_lifecycle("response ID is empty"));
        }
        if self.policy.require_created_model {
            if model.is_none_or(str::is_empty) {
                return Err(invalid_lifecycle("response model is empty"));
            }
        } else if model.is_some_and(str::is_empty) {
            return Err(invalid_lifecycle("response model is empty"));
        }
        // Dialects with optional create identity may omit the model here. A
        // model that is present must still match the prepared attempt; the
        // authoritative terminal envelope always carries and validates it.
        if self.policy.model_binding == ResponseModelBinding::ExactRequest
            && model.is_some_and(|model| model != expected_model)
        {
            return Err(ResponsesLifecycleError::BindingMismatch);
        }
        let response = required_object(object, "response")?;
        // OpenAI Platform goldens often omit `status` on create; when present it
        // must still be an active create status. xAI/OpenRouter always send it.
        if let Some(status) = response.get("status") {
            let status = status
                .as_str()
                .ok_or(ResponsesLifecycleError::InvalidField { field: "status" })?;
            if !matches!(status, "queued" | "in_progress") {
                return Err(invalid_lifecycle("created response status is not active"));
            }
        }
        // Missing `output` is treated as empty (OpenAI Platform create envelopes).
        match response.get("output") {
            None => {}
            Some(value) => {
                let output = value
                    .as_array()
                    .ok_or(ResponsesLifecycleError::InvalidField { field: "output" })?;
                if !output.is_empty() {
                    return Err(invalid_lifecycle(
                        "created response already contains output",
                    ));
                }
            }
        }
        self.created = Some(CreatedResponse {
            id: response_id.map(str::to_owned),
            model: model.map(str::to_owned),
        });
        Ok(())
    }

    /// Accepts `response.output_item.added` for a pure native item.
    ///
    /// # Errors
    ///
    /// Lifecycle or field errors.
    pub fn output_item_added(
        &mut self,
        output_index: u32,
        item: &Value,
    ) -> Result<(), ResponsesLifecycleError> {
        self.require_created()?;
        if usize::try_from(output_index).ok() != Some(self.items.len()) {
            return Err(invalid_lifecycle(
                "output item indices are not contiguous and ordered",
            ));
        }
        let item_id = output_item_id(item);
        if item_id.is_some_and(str::is_empty) {
            return Err(invalid_lifecycle("output item ID is empty"));
        }
        if item_id.is_some_and(|item_id| !self.item_ids.insert(item_id.to_owned())) {
            return Err(invalid_lifecycle("output item ID was reused"));
        }
        let object = item
            .as_object()
            .ok_or(ResponsesLifecycleError::InvalidField { field: "item" })?;
        let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
        let (function_arguments, call_id) = match kind {
            "message" => {
                require_added_status(item_status(item))?;
                let role = object.get("role").and_then(Value::as_str).unwrap_or("");
                let content_empty = object
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty);
                if role != "assistant" || !content_empty {
                    return Err(invalid_lifecycle(
                        "added message must be an empty assistant item",
                    ));
                }
                (String::new(), None)
            }
            "reasoning" => {
                require_added_status(item_status(item))?;
                // Summary must start empty. `encrypted_content` may already be
                // seeded on add (OpenAI Platform); it is opaque seed material,
                // not streamed summary completion.
                let summary_empty = match object.get("summary") {
                    None => true,
                    Some(value) => value.as_array().is_some_and(Vec::is_empty),
                };
                if !summary_empty {
                    return Err(invalid_lifecycle(
                        "added reasoning item already contains completed material",
                    ));
                }
                (String::new(), None)
            }
            "function_call" | "custom_tool_call" => {
                require_added_status(item_status(item))?;
                let call_id = object.get("call_id").and_then(Value::as_str).unwrap_or("");
                let name = object.get("name").and_then(Value::as_str).unwrap_or("");
                let args_field = if kind == "custom_tool_call" {
                    "input"
                } else {
                    "arguments"
                };
                let arguments = object.get(args_field).and_then(Value::as_str).unwrap_or("");
                if call_id.is_empty() || name.is_empty() || !arguments.is_empty() {
                    return Err(invalid_lifecycle(
                        "added function call has invalid initial correlation or arguments",
                    ));
                }
                (arguments.to_owned(), Some(call_id))
            }
            "compaction" => {
                require_added_status(item_status(item))?;
                (String::new(), None)
            }
            _ => {
                // Opaque item: optional/in_progress status (OpenAI may omit) + id.
                require_added_status(item_status(item))?;
                required_string(object, "id")?;
                (String::new(), None)
            }
        };
        if call_id.is_some_and(|call_id| !self.call_ids.insert(call_id.to_owned())) {
            return Err(invalid_lifecycle("function call ID was reused"));
        }
        self.items.insert(
            output_index,
            ItemLifecycle {
                added: item.clone(),
                done: None,
                content: BTreeMap::new(),
                reasoning_summaries: BTreeMap::new(),
                function_arguments,
                function_arguments_done: None,
            },
        );
        Ok(())
    }

    /// Accepts `response.output_item.done` for a pure native item.
    ///
    /// # Errors
    ///
    /// Lifecycle or field errors.
    pub fn output_item_done(
        &mut self,
        output_index: u32,
        item: &Value,
    ) -> Result<(), ResponsesLifecycleError> {
        let policy = self.policy;
        let state = self.item_mut(output_index)?;
        if state.done.is_some() {
            return Err(invalid_lifecycle("output item completed more than once"));
        }
        // Soft OpenAI/Codex: some subscription streams put final tool arguments
        // only on `output_item.done` and never emit `function_call_arguments.done`.
        // Seed lifecycle args from the done item so validation and projection agree.
        if !policy.strict_terminal_output {
            soft_seed_function_arguments_from_done(state, item);
        }
        validate_done_item(state, item, policy)?;
        state.done = Some(item.clone());
        Ok(())
    }

    /// Accepts `response.content_part.added`.
    ///
    /// # Errors
    ///
    /// Lifecycle or field errors.
    pub fn content_part_added(
        &mut self,
        output_index: u32,
        content_index: u32,
        part: &Value,
    ) -> Result<(), ResponsesLifecycleError> {
        let policy = self.policy;
        let state = self.item_mut(output_index)?;
        if state.done.is_some() {
            return Err(invalid_lifecycle(
                "content part was added after item completion",
            ));
        }
        let item_kind = item_kind(&state.added);
        if item_kind != Some("message")
            && !(policy.reasoning_content_text && item_kind == Some("reasoning"))
        {
            return Err(invalid_lifecycle("content part is outside a message item"));
        }
        if usize::try_from(content_index).ok() != Some(state.content.len()) {
            return Err(invalid_lifecycle(
                "content part indices are not contiguous and ordered",
            ));
        }
        let object = part
            .as_object()
            .ok_or(ResponsesLifecycleError::InvalidField { field: "part" })?;
        let kind = required_string(object, "type")?;
        let text = match kind {
            "output_text" => {
                if object
                    .get("annotations")
                    .is_some_and(|value| !value.is_array())
                {
                    return Err(ResponsesLifecycleError::InvalidField { field: "part" });
                }
                required_string(object, "text")?.to_owned()
            }
            "refusal" => {
                if policy.content_text_done {
                    required_string(object, "refusal")?.to_owned()
                } else {
                    required_string(object, "refusal")?;
                    String::new()
                }
            }
            "reasoning_text" if policy.reasoning_content_text && item_kind == Some("reasoning") => {
                let text = required_string(object, "text")?;
                if !text.is_empty() {
                    return Err(invalid_lifecycle(
                        "reasoning content part must start with empty text",
                    ));
                }
                String::new()
            }
            _ => String::new(),
        };
        state.content.insert(
            content_index,
            ContentLifecycle {
                added: part.clone(),
                done: None,
                text,
                text_done: None,
            },
        );
        Ok(())
    }

    /// Accepts `response.content_part.done`.
    ///
    /// # Errors
    ///
    /// Lifecycle or field errors.
    pub fn content_part_done(
        &mut self,
        output_index: u32,
        content_index: u32,
        part: &Value,
    ) -> Result<(), ResponsesLifecycleError> {
        let policy = self.policy;
        let state = self.item_mut(output_index)?;
        if state.done.is_some() {
            return Err(invalid_lifecycle(
                "content part completed after item completion",
            ));
        }
        let content = state
            .content
            .get_mut(&content_index)
            .ok_or_else(|| invalid_lifecycle("content part completed before it was added"))?;
        if content.done.is_some() {
            return Err(invalid_lifecycle("content part completed more than once"));
        }
        // Compare type + accumulated text only. Annotations and other part
        // fields may finalize on done (OpenAI url_citation annotations).
        let object = part
            .as_object()
            .ok_or(ResponsesLifecycleError::InvalidField { field: "part" })?;
        let kind = required_string(object, "type")?;
        let expected_kind = content
            .added
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if kind != expected_kind {
            return Err(invalid_lifecycle(
                "content part completion does not match its accumulated content",
            ));
        }
        let text_ok = match kind {
            "output_text" => required_string(object, "text")? == content.text,
            "refusal" if policy.content_text_done => {
                required_string(object, "refusal")? == content.text
            }
            "reasoning_text" if policy.reasoning_content_text => {
                required_string(object, "text")? == content.text
            }
            "refusal" => {
                // Non-text-done dialects: keep full reconstructed equality.
                let expected = completed_content_part(content, policy)?;
                part == &expected
            }
            _ => {
                let expected = completed_content_part(content, policy)?;
                part == &expected
            }
        };
        if !text_ok {
            return Err(invalid_lifecycle(
                "content part completion does not match its accumulated content",
            ));
        }
        content.done = Some(part.clone());
        Ok(())
    }

    /// Accepts `response.output_text.delta`.
    ///
    /// # Errors
    ///
    /// Lifecycle errors.
    pub fn output_text_delta(
        &mut self,
        output_index: u32,
        content_index: u32,
        delta: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        let policy = self.policy;
        let state = self.item_mut(output_index)?;
        if state.done.is_some() {
            return Err(invalid_lifecycle("text delta followed item completion"));
        }
        let content = state
            .content
            .get_mut(&content_index)
            .ok_or_else(|| invalid_lifecycle("text delta preceded content-part addition"))?;
        if content.done.is_some() || (policy.content_text_done && content.text_done.is_some()) {
            return Err(invalid_lifecycle(if policy.content_text_done {
                "text delta followed text or content-part completion"
            } else {
                "text delta followed content-part completion"
            }));
        }
        if content.added.get("type").and_then(Value::as_str) != Some("output_text") {
            return Err(invalid_lifecycle("text delta targets a non-text part"));
        }
        content.text.push_str(delta);
        Ok(())
    }

    /// Accepts `response.output_text.done` (OpenRouter).
    ///
    /// # Errors
    ///
    /// Lifecycle errors.
    pub fn output_text_done(
        &mut self,
        output_index: u32,
        content_index: u32,
        text: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        self.content_text_done(output_index, content_index, "output_text", text)
    }

    /// Accepts `response.refusal.delta` (OpenRouter).
    ///
    /// # Errors
    ///
    /// Lifecycle errors.
    pub fn refusal_delta(
        &mut self,
        output_index: u32,
        content_index: u32,
        delta: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        let state = self.item_mut(output_index)?;
        if state.done.is_some() {
            return Err(invalid_lifecycle("refusal delta followed item completion"));
        }
        let content = state
            .content
            .get_mut(&content_index)
            .ok_or_else(|| invalid_lifecycle("refusal delta preceded content-part addition"))?;
        if content.done.is_some() || content.text_done.is_some() {
            return Err(invalid_lifecycle(
                "refusal delta followed text or content-part completion",
            ));
        }
        if content.added.get("type").and_then(Value::as_str) != Some("refusal") {
            return Err(invalid_lifecycle(
                "refusal delta targets a non-refusal part",
            ));
        }
        content.text.push_str(delta);
        Ok(())
    }

    /// Accepts `response.refusal.done` (OpenRouter).
    ///
    /// # Errors
    ///
    /// Lifecycle errors.
    pub fn refusal_done(
        &mut self,
        output_index: u32,
        content_index: u32,
        refusal: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        self.content_text_done(output_index, content_index, "refusal", refusal)
    }

    /// Accepts `response.reasoning_text.delta` for an active reasoning content part.
    pub fn reasoning_content_text_delta(
        &mut self,
        output_index: u32,
        content_index: u32,
        item_id: Option<&str>,
        delta: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        if !self.policy.reasoning_content_text {
            return Err(invalid_lifecycle("reasoning content text is not admitted"));
        }
        let state = self.item_mut(output_index)?;
        validate_reasoning_content_target(state, item_id)?;
        let content = state.content.get_mut(&content_index).ok_or_else(|| {
            invalid_lifecycle("reasoning text delta preceded content-part addition")
        })?;
        if state.done.is_some()
            || content.done.is_some()
            || content.text_done.is_some()
            || content.added.get("type").and_then(Value::as_str) != Some("reasoning_text")
        {
            return Err(invalid_lifecycle(
                "reasoning text delta targets an inactive reasoning content part",
            ));
        }
        content.text.push_str(delta);
        Ok(())
    }

    /// Accepts `response.reasoning_text.done` for an active reasoning content part.
    pub fn reasoning_content_text_done(
        &mut self,
        output_index: u32,
        content_index: u32,
        item_id: Option<&str>,
        text: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        if !self.policy.reasoning_content_text {
            return Err(invalid_lifecycle("reasoning content text is not admitted"));
        }
        let state = self.item_mut(output_index)?;
        validate_reasoning_content_target(state, item_id)?;
        let content = state.content.get_mut(&content_index).ok_or_else(|| {
            invalid_lifecycle("reasoning text completion preceded content-part addition")
        })?;
        if state.done.is_some()
            || content.done.is_some()
            || content.text_done.is_some()
            || content.added.get("type").and_then(Value::as_str) != Some("reasoning_text")
            || content.text != text
        {
            return Err(invalid_lifecycle(
                "reasoning text completion disagrees with accumulated deltas",
            ));
        }
        content.text_done = Some(text.to_owned());
        Ok(())
    }

    fn content_text_done(
        &mut self,
        output_index: u32,
        content_index: u32,
        expected_kind: &'static str,
        text: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        let state = self.item_mut(output_index)?;
        if state.done.is_some() {
            return Err(invalid_lifecycle(
                "text completion followed item completion",
            ));
        }
        let content = state
            .content
            .get_mut(&content_index)
            .ok_or_else(|| invalid_lifecycle("text completion preceded content-part addition"))?;
        if content.done.is_some()
            || content.text_done.is_some()
            || content.added.get("type").and_then(Value::as_str) != Some(expected_kind)
            || content.text != text
        {
            return Err(invalid_lifecycle(
                "text completion disagrees with its content-part lifecycle",
            ));
        }
        content.text_done = Some(text.to_owned());
        Ok(())
    }

    /// Accepts `response.reasoning_summary_part.added` (OpenRouter FullParts).
    ///
    /// # Errors
    ///
    /// Lifecycle or field errors.
    pub fn reasoning_part_added(
        &mut self,
        output_index: u32,
        summary_index: u32,
        part: &Value,
    ) -> Result<(), ResponsesLifecycleError> {
        let state = self.item_mut(output_index)?;
        if state.done.is_some() || item_kind(&state.added) != Some("reasoning") {
            return Err(invalid_lifecycle(
                "reasoning summary part is outside an active reasoning item",
            ));
        }
        if usize::try_from(summary_index).ok() != Some(state.reasoning_summaries.len()) {
            return Err(invalid_lifecycle(
                "reasoning summary indices are not contiguous and ordered",
            ));
        }
        let object = part
            .as_object()
            .ok_or(ResponsesLifecycleError::InvalidField { field: "part" })?;
        if required_string(object, "type")? != "summary_text" {
            return Err(invalid_lifecycle(
                "reasoning summary part has an unsupported typed shape",
            ));
        }
        let text = required_string(object, "text")?.to_owned();
        state.reasoning_summaries.insert(
            summary_index,
            ReasoningLifecycle {
                added: part.clone(),
                done: None,
                text,
                text_done: None,
            },
        );
        Ok(())
    }

    /// Accepts `response.reasoning_summary_text.delta`.
    ///
    /// # Errors
    ///
    /// Lifecycle errors.
    pub fn reasoning_delta(
        &mut self,
        output_index: u32,
        summary_index: u32,
        delta: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        let mode = self.policy.reasoning;
        let state = self.item_mut(output_index)?;
        if state.done.is_some() {
            return Err(invalid_lifecycle(
                "reasoning delta followed item completion",
            ));
        }
        if item_kind(&state.added) != Some("reasoning") {
            return Err(invalid_lifecycle(
                "reasoning delta targets a non-reasoning item",
            ));
        }
        match mode {
            ReasoningLifecycleMode::DeltaOnly => {
                if !state.reasoning_summaries.contains_key(&summary_index)
                    && usize::try_from(summary_index).ok() != Some(state.reasoning_summaries.len())
                {
                    return Err(invalid_lifecycle(
                        "reasoning summary indices are not contiguous and ordered",
                    ));
                }
                state
                    .reasoning_summaries
                    .entry(summary_index)
                    .or_insert_with(|| ReasoningLifecycle {
                        added: serde_json::json!({"type":"summary_text","text":""}),
                        done: None,
                        text: String::new(),
                        text_done: None,
                    })
                    .text
                    .push_str(delta);
            }
            ReasoningLifecycleMode::FullParts => {
                if !state.reasoning_summaries.contains_key(&summary_index) {
                    if usize::try_from(summary_index).ok() != Some(state.reasoning_summaries.len())
                    {
                        return Err(invalid_lifecycle(
                            "reasoning summary indices are not contiguous and ordered",
                        ));
                    }
                    state.reasoning_summaries.insert(
                        summary_index,
                        ReasoningLifecycle {
                            added: serde_json::json!({"type":"summary_text","text":""}),
                            done: None,
                            text: String::new(),
                            text_done: None,
                        },
                    );
                }
                let summary = state
                    .reasoning_summaries
                    .get_mut(&summary_index)
                    .ok_or_else(|| invalid_lifecycle("reasoning summary disappeared"))?;
                if summary.done.is_some() || summary.text_done.is_some() {
                    return Err(invalid_lifecycle(
                        "reasoning delta followed summary completion",
                    ));
                }
                summary.text.push_str(delta);
            }
        }
        Ok(())
    }

    /// Accepts `response.reasoning_summary_text.done` (OpenRouter).
    ///
    /// # Errors
    ///
    /// Lifecycle errors.
    pub fn reasoning_text_done(
        &mut self,
        output_index: u32,
        summary_index: u32,
        text: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        let state = self.item_mut(output_index)?;
        let summary = state
            .reasoning_summaries
            .get_mut(&summary_index)
            .ok_or_else(|| invalid_lifecycle("reasoning text completed before its summary part"))?;
        if state.done.is_some()
            || summary.done.is_some()
            || summary.text_done.is_some()
            || summary.text != text
        {
            return Err(invalid_lifecycle(
                "reasoning text completion disagrees with accumulated deltas",
            ));
        }
        summary.text_done = Some(text.to_owned());
        Ok(())
    }

    /// Accepts `response.reasoning_summary_part.done` (OpenRouter).
    ///
    /// # Errors
    ///
    /// Lifecycle or field errors.
    pub fn reasoning_part_done(
        &mut self,
        output_index: u32,
        summary_index: u32,
        part: &Value,
    ) -> Result<(), ResponsesLifecycleError> {
        let state = self.item_mut(output_index)?;
        let summary = state
            .reasoning_summaries
            .get_mut(&summary_index)
            .ok_or_else(|| invalid_lifecycle("reasoning part completed before addition"))?;
        let mut expected = summary.added.clone();
        expected
            .as_object_mut()
            .ok_or(ResponsesLifecycleError::InvalidField { field: "part" })?
            .insert("text".to_owned(), Value::String(summary.text.clone()));
        if state.done.is_some()
            || summary.done.is_some()
            || summary
                .text_done
                .as_deref()
                .is_some_and(|text| text != summary.text)
            || part != &expected
        {
            return Err(invalid_lifecycle(
                "reasoning part completion disagrees with accumulated text",
            ));
        }
        summary.done = Some(part.clone());
        Ok(())
    }

    /// Accepts `response.function_call_arguments.delta`.
    ///
    /// # Errors
    ///
    /// Lifecycle errors.
    pub fn function_arguments_delta(
        &mut self,
        output_index: u32,
        item_id: Option<&str>,
        delta: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        let state = self.item_mut(output_index)?;
        validate_function_event_target(state, item_id)?;
        if state.done.is_some() {
            return Err(invalid_lifecycle("argument delta followed item completion"));
        }
        if state.function_arguments_done.is_some() {
            return Err(invalid_lifecycle(
                "argument delta followed argument completion",
            ));
        }
        state.function_arguments.push_str(delta);
        Ok(())
    }

    /// Accepts `response.function_call_arguments.done`.
    ///
    /// # Errors
    ///
    /// Lifecycle errors.
    pub fn function_arguments_done(
        &mut self,
        output_index: u32,
        item_id: Option<&str>,
        arguments: &str,
    ) -> Result<(), ResponsesLifecycleError> {
        let policy = self.policy;
        let state = self.item_mut(output_index)?;
        validate_function_event_target(state, item_id)?;
        if state.done.is_some() {
            return Err(invalid_lifecycle(
                "arguments completed after item completion",
            ));
        }
        if state.function_arguments_done.is_some() {
            return Err(invalid_lifecycle("arguments completed more than once"));
        }
        if arguments != state.function_arguments {
            // Soft path: cosmetic re-serialization (whitespace/key order) keeps
            // deltas; semantic diverge prefers authoritative done. Strict fails.
            if policy.strict_terminal_output {
                return Err(invalid_lifecycle(
                    "completed arguments do not match argument deltas",
                ));
            }
            if state.function_arguments.is_empty()
                || !lifecycle_json_equivalent(arguments, &state.function_arguments)
            {
                arguments.clone_into(&mut state.function_arguments);
            }
            // else: keep deltas (JSON-equivalent to done).
        }
        // Authoritative lifecycle args for later validate_function_done: deltas
        // when present, else the done payload we just seeded.
        state.function_arguments_done = Some(state.function_arguments.clone());
        Ok(())
    }

    /// Validates a terminal response against creation identity, outcome
    /// semantics, and completed streamed output natives.
    ///
    /// `response_status` is the provider status string
    /// (`"completed"` / `"incomplete"` / `"failed"`).
    /// `output` is ordered native items from the terminal envelope.
    ///
    /// # Errors
    ///
    /// Lifecycle errors.
    pub fn terminal_response(
        &self,
        response_id: &str,
        response_model: Option<&str>,
        event_status: &'static str,
        response_status: &str,
        incomplete_details: Option<&Value>,
        has_error: bool,
        output: &[&Value],
    ) -> Result<(), ResponsesLifecycleError> {
        let created = self
            .created
            .as_ref()
            .ok_or_else(|| invalid_lifecycle("terminal response preceded response creation"))?;
        if created.id.as_deref().is_some_and(|id| id != response_id)
            || created
                .model
                .as_deref()
                .is_some_and(|model| response_model != Some(model))
        {
            return Err(invalid_lifecycle(
                "terminal response identity differs from response creation",
            ));
        }
        validate_terminal_semantics(
            event_status,
            response_status,
            incomplete_details.is_some(),
            has_error,
        )?;
        let output_bound_reached = event_status == "incomplete"
            && incomplete_details
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                == Some("max_output_tokens");
        let unfinished = if output_bound_reached {
            ResponsesLifecycleError::OutputBoundReached
        } else {
            invalid_lifecycle("terminal response preceded item completion")
        };
        if self.policy.strict_terminal_output {
            if output.len() != self.items.len() {
                return Err(invalid_lifecycle(
                    "terminal response output count differs from streamed output",
                ));
            }
            let allow_partial_text =
                self.policy.incomplete_length_text_snapshot && output_bound_reached;
            let mut partial_count = 0;
            for (index, state) in &self.items {
                let index = usize::try_from(*index).map_err(|_| {
                    invalid_lifecycle("output index cannot address terminal output")
                })?;
                let terminal = output.get(index).ok_or_else(|| {
                    invalid_lifecycle("terminal response omitted streamed output")
                })?;
                if let Some(done) = state.done.as_ref() {
                    if !strict_terminal_item_matches(done, terminal, self.policy) {
                        return Err(invalid_lifecycle(
                            "terminal response changed a completed provider item",
                        ));
                    }
                } else if allow_partial_text
                    && index + 1 == output.len()
                    && partial_count == 0
                    && item_kind(&state.added) == Some("message")
                {
                    validate_incomplete_text_snapshot(state, terminal)?;
                    partial_count += 1;
                } else {
                    return Err(unfinished);
                }
            }
        } else {
            // Soft OpenAI/Codex: stream `output_item.done` is authoritative.
            // ChatGPT subscription terminals may rewrite, reorder, or drop items
            // relative to the stream while still completing successfully.
            if self.items.values().any(|state| state.done.is_none()) {
                return Err(unfinished);
            }
            let _ = output;
        }
        Ok(())
    }

    /// Builds missing terminal `output` from the exact ordered completed
    /// streamed items (OpenRouter sparse-terminal and Codex soft paths).
    ///
    /// # Errors
    ///
    /// Lifecycle errors when any item is incomplete.
    pub fn synthesize_missing_output(&self) -> Result<Vec<Value>, ResponsesLifecycleError> {
        self.items
            .values()
            .map(|item| {
                item.done
                    .clone()
                    .ok_or_else(|| invalid_lifecycle("terminal preceded output item completion"))
            })
            .collect()
    }

    fn require_created(&self) -> Result<(), ResponsesLifecycleError> {
        if self.created.is_some() {
            Ok(())
        } else {
            Err(invalid_lifecycle("output event preceded response creation"))
        }
    }

    fn item_mut(
        &mut self,
        output_index: u32,
    ) -> Result<&mut ItemLifecycle, ResponsesLifecycleError> {
        self.require_created()?;
        self.items
            .get_mut(&output_index)
            .ok_or_else(|| invalid_lifecycle("output event preceded item addition"))
    }
}

/// Strict terminal items must be identical, except an OpenAI Platform reasoning
/// item may carry a refreshed `encrypted_content`. The caller keeps `terminal`
/// unchanged: that native is the replay authority.
fn strict_terminal_item_matches(
    done: &Value,
    terminal: &Value,
    policy: LifecycleDialectPolicy,
) -> bool {
    if terminal == done {
        return true;
    }
    if !policy.reasoning_terminal_ciphertext_refresh
        || item_kind(done) != Some("reasoning")
        || item_kind(terminal) != Some("reasoning")
    {
        return false;
    }
    without_encrypted_content(done) == without_encrypted_content(terminal)
}

fn without_encrypted_content(value: &Value) -> Value {
    let mut cloned = value.clone();
    if let Some(object) = cloned.as_object_mut() {
        object.remove("encrypted_content");
    }
    cloned
}

fn item_kind(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

fn item_status(item: &Value) -> Option<&str> {
    item.get("status").and_then(Value::as_str)
}

fn output_item_id(item: &Value) -> Option<&str> {
    item.get("id").and_then(Value::as_str)
}

fn require_added_status(status: Option<&str>) -> Result<(), ResponsesLifecycleError> {
    if matches!(status, None | Some("in_progress")) {
        Ok(())
    } else {
        Err(invalid_lifecycle(
            "added output item status is not in progress",
        ))
    }
}

fn require_done_status(status: Option<&str>) -> Result<(), ResponsesLifecycleError> {
    // Platform/Codex subscription streams may omit item `status` on
    // `output_item.done` (same soft posture as create). When present, accept
    // the documented terminal item labels plus the occasional `done` alias.
    if matches!(status, None | Some("completed" | "incomplete" | "done")) {
        Ok(())
    } else {
        Err(invalid_lifecycle(
            "completed output item has invalid status",
        ))
    }
}

fn completed_content_part(
    content: &ContentLifecycle,
    policy: LifecycleDialectPolicy,
) -> Result<Value, ResponsesLifecycleError> {
    let mut expected = content.added.clone();
    let text_field = match expected.get("type").and_then(Value::as_str) {
        Some("output_text") => Some("text"),
        Some("refusal") if policy.content_text_done => Some("refusal"),
        Some("reasoning_text") if policy.reasoning_content_text => Some("text"),
        _ => None,
    };
    if let Some(text_field) = text_field {
        expected
            .as_object_mut()
            .ok_or(ResponsesLifecycleError::InvalidField { field: "part" })?
            .insert(text_field.to_owned(), Value::String(content.text.clone()));
    }
    Ok(expected)
}

fn validate_function_event_target(
    state: &ItemLifecycle,
    item_id: Option<&str>,
) -> Result<(), ResponsesLifecycleError> {
    if !matches!(
        item_kind(&state.added),
        Some("function_call" | "custom_tool_call")
    ) {
        return Err(invalid_lifecycle(
            "arguments event targets a non-function item",
        ));
    }
    if item_id != output_item_id(&state.added) {
        return Err(invalid_lifecycle(
            "arguments event item ID does not match its function item",
        ));
    }
    Ok(())
}

fn validate_reasoning_content_target(
    state: &ItemLifecycle,
    item_id: Option<&str>,
) -> Result<(), ResponsesLifecycleError> {
    if item_kind(&state.added) != Some("reasoning") {
        return Err(invalid_lifecycle(
            "reasoning text event targets a non-reasoning item",
        ));
    }
    if item_id.is_some_and(|item_id| Some(item_id) != output_item_id(&state.added)) {
        return Err(invalid_lifecycle(
            "reasoning text event item ID does not match its reasoning item",
        ));
    }
    Ok(())
}

fn validate_done_item(
    state: &ItemLifecycle,
    done: &Value,
    policy: LifecycleDialectPolicy,
) -> Result<(), ResponsesLifecycleError> {
    if output_item_id(&state.added) != output_item_id(done) {
        return Err(invalid_lifecycle("output item ID changed at completion"));
    }
    let added_kind = item_kind(&state.added);
    let done_kind = item_kind(done);
    match (added_kind, done_kind) {
        (Some("message"), Some("message")) => validate_message_done(state, done, policy),
        (Some("reasoning"), Some("reasoning")) => validate_reasoning_done(state, done, policy),
        (Some("function_call"), Some("function_call"))
        | (Some("custom_tool_call"), Some("custom_tool_call")) => {
            validate_function_done(state, done, policy)
        }
        (Some("compaction"), Some("compaction")) => validate_compaction_done(done),
        (Some(added), Some(done_k)) if added == done_k => {
            // Opaque: same non-typed kind.
            validate_opaque_done(&state.added, done, policy)
        }
        _ => Err(invalid_lifecycle("output item type changed at completion")),
    }
}

fn validate_message_done(
    state: &ItemLifecycle,
    done: &Value,
    policy: LifecycleDialectPolicy,
) -> Result<(), ResponsesLifecycleError> {
    let object = done
        .as_object()
        .ok_or(ResponsesLifecycleError::InvalidField { field: "item" })?;
    require_done_status(item_status(done))?;
    let role = object.get("role").and_then(Value::as_str).unwrap_or("");
    let added_role = state
        .added
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("");
    let native_content = done
        .get("content")
        .and_then(Value::as_array)
        .ok_or(ResponsesLifecycleError::InvalidField { field: "content" })?;
    if role != added_role || role != "assistant" || native_content.len() != state.content.len() {
        return Err(invalid_lifecycle("message shape changed at completion"));
    }
    for (index, part) in native_content.iter().enumerate() {
        let index = u32::try_from(index)
            .map_err(|_| invalid_lifecycle("message content index overflowed"))?;
        if policy.content_text_done {
            let content = state
                .content
                .get(&index)
                .ok_or_else(|| invalid_lifecycle("item completed before its content part"))?;
            let accumulated = completed_content_part(content, policy)?;
            let expected = content.done.as_ref().unwrap_or(&accumulated);
            if part != expected {
                return Err(invalid_lifecycle(
                    "completed item changed a completed content part",
                ));
            }
        } else {
            let expected = state
                .content
                .get(&index)
                .and_then(|content| content.done.as_ref())
                .ok_or_else(|| invalid_lifecycle("item completed before its content part"))?;
            if part != expected {
                return Err(invalid_lifecycle(
                    "completed item changed a completed content part",
                ));
            }
        }
    }
    Ok(())
}

fn validate_incomplete_text_snapshot(
    state: &ItemLifecycle,
    terminal: &Value,
) -> Result<(), ResponsesLifecycleError> {
    let object = terminal
        .as_object()
        .ok_or(ResponsesLifecycleError::InvalidField { field: "item" })?;
    if item_kind(&state.added) != Some("message")
        || item_kind(terminal) != Some("message")
        || output_item_id(&state.added) != output_item_id(terminal)
        || item_status(terminal) != Some("incomplete")
        || state.added.get("role").and_then(Value::as_str) != Some("assistant")
        || object.get("role").and_then(Value::as_str) != Some("assistant")
    {
        return Err(invalid_lifecycle(
            "incomplete text snapshot changed message identity or status",
        ));
    }
    let Some(parts) = object.get("content").and_then(Value::as_array) else {
        return Err(ResponsesLifecycleError::InvalidField { field: "content" });
    };
    let Some(content) = state.content.get(&0) else {
        return Err(invalid_lifecycle(
            "incomplete text snapshot lacks streamed content",
        ));
    };
    if parts.len() != 1
        || state.content.len() != 1
        || content.added.get("type").and_then(Value::as_str) != Some("output_text")
        || content.done.is_some()
        || content.text_done.is_some()
        || content.text.is_empty()
        || parts[0] != completed_content_part(content, LifecycleDialectPolicy::grok())?
    {
        return Err(invalid_lifecycle(
            "incomplete text snapshot differs from streamed text",
        ));
    }
    let mut expected = state.added.clone();
    let expected_object = expected
        .as_object_mut()
        .ok_or(ResponsesLifecycleError::InvalidField { field: "item" })?;
    expected_object.insert("status".to_owned(), Value::String("incomplete".to_owned()));
    expected_object.insert("content".to_owned(), Value::Array(parts.clone()));
    if &expected != terminal {
        return Err(invalid_lifecycle(
            "incomplete text snapshot changed added message fields",
        ));
    }
    Ok(())
}

fn validate_reasoning_done(
    state: &ItemLifecycle,
    done: &Value,
    policy: LifecycleDialectPolicy,
) -> Result<(), ResponsesLifecycleError> {
    require_done_status(item_status(done))?;
    let native_summary = done
        .get("summary")
        .and_then(Value::as_array)
        .ok_or(ResponsesLifecycleError::InvalidField { field: "summary" })?;
    if policy.reasoning_content_text && !state.content.is_empty() {
        if !native_summary.is_empty() || !state.reasoning_summaries.is_empty() {
            return Err(invalid_lifecycle(
                "reasoning content text must not be relabeled as summary",
            ));
        }
        let native_content = done
            .get("content")
            .and_then(Value::as_array)
            .ok_or(ResponsesLifecycleError::InvalidField { field: "content" })?;
        if native_content.len() != state.content.len() {
            return Err(invalid_lifecycle("reasoning content count changed"));
        }
        for (index, part) in native_content.iter().enumerate() {
            let index = u32::try_from(index)
                .map_err(|_| invalid_lifecycle("reasoning content index overflowed"))?;
            let content = state.content.get(&index).ok_or_else(|| {
                invalid_lifecycle("reasoning item completed before its content part")
            })?;
            let expected = content.done.as_ref().ok_or_else(|| {
                invalid_lifecycle("reasoning item completed before its content part")
            })?;
            if content.text_done.as_deref() != Some(content.text.as_str()) || part != expected {
                return Err(invalid_lifecycle(
                    "reasoning completion differs from reasoning content text",
                ));
            }
        }
        return Ok(());
    }
    match policy.reasoning {
        ReasoningLifecycleMode::DeltaOnly => {
            if native_summary.len() != state.reasoning_summaries.len() {
                return Err(invalid_lifecycle("reasoning summary count changed"));
            }
            for (index, part) in native_summary.iter().enumerate() {
                let index = u32::try_from(index)
                    .map_err(|_| invalid_lifecycle("reasoning summary index overflowed"))?;
                if part.get("type").and_then(Value::as_str) != Some("summary_text")
                    || part.get("text").and_then(Value::as_str)
                        != state
                            .reasoning_summaries
                            .get(&index)
                            .map(|summary| summary.text.as_str())
                {
                    return Err(invalid_lifecycle(
                        "reasoning completion does not match summary deltas",
                    ));
                }
            }
        }
        ReasoningLifecycleMode::FullParts => {
            // Typed summary length is always derived from native after parse; the
            // OpenRouter golden still checks each summary_text against deltas and
            // that every lifecycle index remains a summary_text part.
            for (index, part) in native_summary.iter().enumerate() {
                let index = u32::try_from(index)
                    .map_err(|_| invalid_lifecycle("reasoning summary index overflowed"))?;
                if part.get("type").and_then(Value::as_str) == Some("summary_text")
                    && part.get("text").and_then(Value::as_str)
                        != state
                            .reasoning_summaries
                            .get(&index)
                            .map(|summary| summary.text.as_str())
                {
                    return Err(invalid_lifecycle(
                        "reasoning completion does not match summary deltas",
                    ));
                }
            }
            if state.reasoning_summaries.keys().any(|index| {
                usize::try_from(*index)
                    .ok()
                    .and_then(|index| native_summary.get(index))
                    .is_none_or(|part| {
                        part.get("type").and_then(Value::as_str) != Some("summary_text")
                    })
            }) {
                return Err(invalid_lifecycle(
                    "reasoning summary lifecycle changed its typed part",
                ));
            }
        }
    }
    Ok(())
}

fn soft_seed_function_arguments_from_done(state: &mut ItemLifecycle, done: &Value) {
    let kind = item_kind(&state.added);
    if !matches!(kind, Some("function_call" | "custom_tool_call")) {
        return;
    }
    if state.function_arguments_done.is_some() {
        return;
    }
    let args_field = if kind == Some("custom_tool_call") {
        "input"
    } else {
        "arguments"
    };
    let arguments = done
        .get(args_field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    // SoftSeedEmptyOnly: seed from done only when no deltas; keep non-empty deltas
    // when Codex omits `function_call_arguments.done` but rewrites on item.done.
    if state.function_arguments.is_empty() {
        state.function_arguments = arguments;
    }
    state.function_arguments_done = Some(state.function_arguments.clone());
}

fn validate_function_done(
    state: &ItemLifecycle,
    done: &Value,
    policy: LifecycleDialectPolicy,
) -> Result<(), ResponsesLifecycleError> {
    require_done_status(item_status(done))?;
    let added_call_id = state
        .added
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let added_name = state
        .added
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let call_id = done.get("call_id").and_then(Value::as_str).unwrap_or("");
    let name = done.get("name").and_then(Value::as_str).unwrap_or("");
    let args_field = if item_kind(&state.added) == Some("custom_tool_call") {
        "input"
    } else {
        "arguments"
    };
    let arguments = done.get(args_field).and_then(Value::as_str).unwrap_or("");
    if call_id != added_call_id || name != added_name {
        return Err(invalid_lifecycle(
            "function completion changed correlation or arguments",
        ));
    }
    // SoftSeedEmptyOnly: when not strict, done item args may re-serialize vs
    // kept deltas; require lifecycle args tracking completed consistently.
    if policy.strict_terminal_output {
        if arguments != state.function_arguments
            || state.function_arguments_done.as_deref() != Some(arguments)
        {
            return Err(invalid_lifecycle(
                "function completion changed correlation or arguments",
            ));
        }
    } else if state.function_arguments_done.as_deref() != Some(state.function_arguments.as_str()) {
        return Err(invalid_lifecycle(
            "function completion changed correlation or arguments",
        ));
    }
    Ok(())
}

fn validate_compaction_done(done: &Value) -> Result<(), ResponsesLifecycleError> {
    require_done_status(item_status(done))?;
    let encrypted = done
        .get("encrypted_content")
        .and_then(Value::as_str)
        .unwrap_or("");
    if encrypted.is_empty() {
        return Err(invalid_lifecycle(
            "compaction item omitted encrypted_content",
        ));
    }
    Ok(())
}

fn validate_opaque_done(
    added_native: &Value,
    native: &Value,
    policy: LifecycleDialectPolicy,
) -> Result<(), ResponsesLifecycleError> {
    let Some(status) = item_status(native) else {
        if policy.strict_terminal_output {
            return Err(invalid_lifecycle("opaque completion status is missing"));
        }
        // Codex/ChatGPT may omit status on opaque `output_item.done`.
        let mut added = added_native.clone();
        let mut done = native.clone();
        added
            .as_object_mut()
            .ok_or_else(|| invalid_lifecycle("opaque added item is not an object"))?
            .remove("status");
        if let Some(object) = done.as_object_mut() {
            object.remove("status");
        }
        if added != done {
            return Err(invalid_lifecycle(
                "opaque item changed fields other than terminal status",
            ));
        }
        return Ok(());
    };
    if !matches!(status, "completed" | "incomplete") {
        return Err(invalid_lifecycle(
            "opaque completion status is not terminal",
        ));
    }
    let mut expected = added_native.clone();
    expected
        .as_object_mut()
        .ok_or_else(|| invalid_lifecycle("opaque added item is not an object"))?
        .insert("status".to_owned(), Value::String(status.to_owned()));
    if &expected != native {
        return Err(invalid_lifecycle(
            "opaque item changed fields other than terminal status",
        ));
    }
    Ok(())
}

fn validate_terminal_semantics(
    event_status: &'static str,
    response_status: &str,
    has_incomplete_details: bool,
    has_error: bool,
) -> Result<(), ResponsesLifecycleError> {
    let valid = match event_status {
        "completed" => response_status == "completed" && !has_incomplete_details && !has_error,
        "incomplete" => response_status == "incomplete" && has_incomplete_details && !has_error,
        "failed" => response_status == "failed" && !has_incomplete_details && has_error,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(invalid_lifecycle(
            "terminal event and response outcome semantics disagree",
        ))
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn xai_policy_rejects_double_create() {
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::xai());
        let event = json!({
            "type": "response.created",
            "response": {
                "id": "resp_1",
                "model": "grok-3",
                "status": "in_progress",
                "output": []
            }
        });
        let object = event.as_object().unwrap();
        life.response_created(object, Some("resp_1"), Some("grok-3"), "grok-3")
            .unwrap();
        let err = life
            .response_created(object, Some("resp_1"), Some("grok-3"), "grok-3")
            .unwrap_err();
        assert_eq!(
            err,
            ResponsesLifecycleError::InvalidLifecycle {
                reason: "response was created more than once"
            }
        );
    }

    #[test]
    fn openrouter_policy_requires_create_identity() {
        assert!(!LifecycleDialectPolicy::openrouter().intermediate_response_id);
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openrouter());
        let event = json!({
            "type": "response.created",
            "response": {
                "id": "resp_1",
                "model": "m",
                "status": "in_progress",
                "output": []
            }
        });
        let object = event.as_object().unwrap();
        let err = life
            .response_created(object, None, Some("m"), "m")
            .unwrap_err();
        assert_eq!(
            err,
            ResponsesLifecycleError::InvalidLifecycle {
                reason: "response ID is empty"
            }
        );
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openrouter());
        life.response_created(object, Some("resp_1"), Some("m"), "m")
            .expect("valid OpenRouter create identity");
        let intermediate = json!({
            "type": "response.output_text.delta",
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "delta": "ok"
        });
        life.validate_intermediate_response_id(
            intermediate.as_object().expect("intermediate object"),
            "response.output_text.delta",
        )
        .expect("current OpenRouter intermediate event omits response_id");
    }

    #[test]
    fn openai_policy_requires_create_identity_and_skips_response_id() {
        assert_eq!(
            LifecycleDialectPolicy::openai(),
            LifecycleDialectPolicy {
                require_created_id: true,
                require_created_model: true,
                model_binding: ResponseModelBinding::ProviderReported,
                reasoning: ReasoningLifecycleMode::FullParts,
                content_text_done: true,
                reasoning_content_text: false,
                intermediate_response_id: false,
                strict_terminal_output: false,
                reasoning_terminal_ciphertext_refresh: false,
                sparse_terminal_model_from_prepared: false,
                sparse_terminal_output_from_completed: false,
                incomplete_length_text_snapshot: false,
            }
        );
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai());
        let err = life
            .output_item_added(0, &json!({"type": "message", "id": "m"}))
            .expect_err("openai requires create before items");
        assert!(matches!(
            err,
            ResponsesLifecycleError::InvalidLifecycle { .. }
        ));
        // Platform create often omits status/output.
        let event = json!({
            "type": "response.created",
            "response": {
                "id": "resp_1",
                "model": "gpt-test"
            }
        });
        let object = event.as_object().unwrap();
        life.response_created(object, Some("resp_1"), Some("gpt-test"), "gpt-test")
            .expect("create without status/output");
        // Intermediate response_id is not enforced for OpenAI.
        life.validate_intermediate_response_id(object, "response.output_item.added")
            .expect("no intermediate response_id policy");
        life.output_item_added(
            0,
            &json!({"type": "message", "id": "m", "role": "assistant", "content": []}),
        )
        .expect("item after create");
        // Contiguous indices + double create.
        let err = life
            .output_item_added(
                2,
                &json!({"type": "message", "id": "m2", "role": "assistant", "content": []}),
            )
            .expect_err("non-contiguous");
        assert_eq!(
            err,
            ResponsesLifecycleError::InvalidLifecycle {
                reason: "output item indices are not contiguous and ordered"
            }
        );
        let err = life
            .response_created(object, Some("resp_1"), Some("gpt-test"), "gpt-test")
            .expect_err("double create");
        assert_eq!(
            err,
            ResponsesLifecycleError::InvalidLifecycle {
                reason: "response was created more than once"
            }
        );
    }

    #[test]
    fn openai_accepts_seeded_encrypted_reasoning_and_custom_tool() {
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai());
        let event = json!({
            "type": "response.created",
            "response": { "id": "resp_1", "model": "gpt-test", "status": "in_progress" }
        });
        life.response_created(
            event.as_object().unwrap(),
            Some("resp_1"),
            Some("gpt-test"),
            "gpt-test",
        )
        .unwrap();
        life.output_item_added(
            0,
            &json!({
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "opaque-fixture",
                "summary": []
            }),
        )
        .expect("encrypted seed on add");
        life.output_item_added(
            1,
            &json!({
                "type": "custom_tool_call",
                "id": "custom_one",
                "call_id": "custom_call_one",
                "name": "write_code",
                "input": ""
            }),
        )
        .expect("custom tool add");
        life.function_arguments_delta(1, Some("custom_one"), "print(1)")
            .unwrap();
        life.function_arguments_done(1, Some("custom_one"), "print(1)")
            .unwrap();
        life.output_item_done(
            1,
            &json!({
                "type": "custom_tool_call",
                "id": "custom_one",
                "call_id": "custom_call_one",
                "name": "write_code",
                "input": "print(1)",
                "status": "completed"
            }),
        )
        .expect("custom tool done");
    }

    #[test]
    fn openai_codex_accepts_compaction_items_without_status() {
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai_codex());
        let created = json!({
            "type": "response.created",
            "response": {
                "id": "resp_compact",
                "model": "gpt-5.3-codex-spark"
            }
        });
        life.response_created(
            created.as_object().unwrap(),
            Some("resp_compact"),
            Some("gpt-5.3-codex-spark"),
            "gpt-5.3-codex-spark",
        )
        .expect("create");
        life.output_item_added(
            0,
            &json!({
                "type": "compaction",
                "id": "cmp_1",
                "encrypted_content": "ciphertext"
            }),
        )
        .expect("compaction add");
        life.output_item_done(
            0,
            &json!({
                "type": "compaction",
                "id": "cmp_1",
                "encrypted_content": "ciphertext"
            }),
        )
        .expect("compaction done without status");
    }

    #[test]
    fn openai_codex_accepts_created_model_distinct_from_requested_catalog_slug() {
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai_codex());
        let created = json!({
            "type": "response.created",
            "response": {
                "id": "resp_mini",
                "model": "gpt-5.4-mini-2026-03-17",
                "status": "in_progress",
                "output": []
            }
        });
        life.response_created(
            created.as_object().unwrap(),
            Some("resp_mini"),
            Some("gpt-5.4-mini-2026-03-17"),
            "gpt-5.4-mini",
        )
        .expect("Codex OAuth does not ExactRequest SSE response.model");
        assert_eq!(life.created_model(), Some("gpt-5.4-mini-2026-03-17"));
        assert!(
            life.terminal_response(
                "resp_mini",
                Some("other-backend"),
                "completed",
                "completed",
                None,
                false,
                &[],
            )
            .is_err(),
            "later events must still agree with the created reported model"
        );
    }

    #[test]
    fn openai_platform_rejects_created_model_distinct_from_requested() {
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai_platform_api());
        let created = json!({
            "type": "response.created",
            "response": {
                "id": "resp_plat",
                "model": "gpt-5.4-mini-2026-03-17",
                "status": "in_progress",
                "output": []
            }
        });
        assert_eq!(
            life.response_created(
                created.as_object().unwrap(),
                Some("resp_plat"),
                Some("gpt-5.4-mini-2026-03-17"),
                "gpt-5.4-mini",
            ),
            Err(ResponsesLifecycleError::BindingMismatch)
        );
    }

    #[test]
    fn openai_codex_accepts_opaque_items_without_status() {
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openai_codex());
        let created = json!({
            "type": "response.created",
            "response": {
                "id": "resp_opaque",
                "model": "gpt-5.3-codex-spark"
            }
        });
        life.response_created(
            created.as_object().unwrap(),
            Some("resp_opaque"),
            Some("gpt-5.3-codex-spark"),
            "gpt-5.3-codex-spark",
        )
        .expect("create");
        life.output_item_added(
            0,
            &json!({
                "type": "codex_something",
                "id": "op_1"
            }),
        )
        .expect("opaque add");
        life.output_item_done(
            0,
            &json!({
                "type": "codex_something",
                "id": "op_1"
            }),
        )
        .expect("opaque done without status");
    }

    #[test]
    fn xai_rejects_opaque_items_without_status() {
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::xai());
        let created = json!({
            "type": "response.created",
            "response": {
                "status": "in_progress",
                "output": []
            }
        });
        life.response_created(created.as_object().unwrap(), None, None, "grok-4.5")
            .expect("create");
        life.output_item_added(
            0,
            &json!({
                "type": "codex_something",
                "id": "op_1"
            }),
        )
        .expect("opaque add");
        let err = life
            .output_item_done(
                0,
                &json!({
                    "type": "codex_something",
                    "id": "op_1"
                }),
            )
            .expect_err("xAI requires opaque status");
        assert_eq!(
            err,
            ResponsesLifecycleError::InvalidLifecycle {
                reason: "opaque completion status is missing"
            }
        );
    }

    #[test]
    fn exact_binding_mismatch_on_model() {
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::openrouter());
        let event = json!({
            "type": "response.created",
            "response": {
                "id": "resp_1",
                "model": "other",
                "status": "in_progress",
                "output": []
            }
        });
        let object = event.as_object().unwrap();
        assert_eq!(
            life.response_created(object, Some("resp_1"), Some("other"), "expected"),
            Err(ResponsesLifecycleError::BindingMismatch)
        );
    }

    #[test]
    fn xai_accepts_omitted_create_identity() {
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::xai());
        let event = json!({
            "type": "response.created",
            "response": {
                "status": "in_progress",
                "output": []
            }
        });
        life.response_created(event.as_object().unwrap(), None, None, "grok-4.5")
            .expect("xAI create identity is optional");
    }

    #[test]
    fn xai_retains_provider_reported_model_and_requires_terminal_consistency() {
        let mut life = ResponsesLifecycle::new(LifecycleDialectPolicy::xai());
        let event = json!({
            "type": "response.created",
            "response": {
                "id": "resp_1",
                "model": "routed-backend",
                "status": "in_progress",
                "output": []
            }
        });
        life.response_created(
            event.as_object().unwrap(),
            Some("resp_1"),
            Some("routed-backend"),
            "grok-4.5",
        )
        .expect("xAI preserves provider-reported routed identity");
        assert_eq!(life.created_model(), Some("routed-backend"));
        assert!(
            life.terminal_response(
                "resp_1",
                Some("another-backend"),
                "completed",
                "completed",
                None,
                false,
                &[],
            )
            .is_err()
        );
    }

    fn reasoning_item(ciphertext: &str) -> Value {
        json!({
            "id": "rs_1",
            "type": "reasoning",
            "status": "completed",
            "summary": [],
            "encrypted_content": ciphertext
        })
    }

    fn finish_one_item(
        policy: LifecycleDialectPolicy,
        done: &Value,
        terminal: &Value,
    ) -> Result<(), ResponsesLifecycleError> {
        let mut life = ResponsesLifecycle::new(policy);
        let created = json!({
            "type": "response.created",
            "response": {
                "id": "resp_1",
                "model": "gpt-test",
                "status": "in_progress",
                "output": []
            }
        });
        life.response_created(
            created.as_object().expect("created object"),
            Some("resp_1"),
            Some("gpt-test"),
            "gpt-test",
        )?;
        let mut added = done.clone();
        if let Some(object) = added.as_object_mut() {
            object.remove("status");
            if object.get("type").and_then(Value::as_str) == Some("reasoning") {
                object.insert("summary".to_owned(), json!([]));
            }
        }
        life.output_item_added(0, &added)?;
        life.output_item_done(0, done)?;
        life.terminal_response(
            "resp_1",
            Some("gpt-test"),
            "completed",
            "completed",
            None,
            false,
            &[terminal],
        )
    }

    #[test]
    fn platform_terminal_accepts_reencrypted_reasoning_only() {
        assert!(
            LifecycleDialectPolicy::openai_platform_api().reasoning_terminal_ciphertext_refresh
        );
        assert!(!LifecycleDialectPolicy::openai_codex().reasoning_terminal_ciphertext_refresh);
        assert!(!LifecycleDialectPolicy::xai().reasoning_terminal_ciphertext_refresh);
        assert!(!LifecycleDialectPolicy::grok().reasoning_terminal_ciphertext_refresh);
        assert!(!LifecycleDialectPolicy::openrouter().reasoning_terminal_ciphertext_refresh);

        let done = reasoning_item("synthetic-streamed-ciphertext");
        let mut terminal = reasoning_item("synthetic-terminal-ciphertext");
        finish_one_item(
            LifecycleDialectPolicy::openai_platform_api(),
            &done,
            &terminal,
        )
        .expect("platform keeps a reasoning ciphertext refresh");
        // The lifecycle does not rewrite the terminal item. Replay uses it.
        assert_eq!(
            terminal["encrypted_content"],
            "synthetic-terminal-ciphertext"
        );

        terminal["summary"] = json!([{"type": "summary_text", "text": "changed"}]);
        let err = finish_one_item(
            LifecycleDialectPolicy::openai_platform_api(),
            &done,
            &terminal,
        )
        .expect_err("another reasoning field still fails");
        assert_eq!(
            err,
            ResponsesLifecycleError::InvalidLifecycle {
                reason: "terminal response changed a completed provider item"
            }
        );

        let compaction_done = json!({
            "id": "cmp_1",
            "type": "compaction",
            "encrypted_content": "synthetic-streamed-ciphertext"
        });
        let compaction_terminal = json!({
            "id": "cmp_1",
            "type": "compaction",
            "encrypted_content": "synthetic-terminal-ciphertext"
        });
        let err = finish_one_item(
            LifecycleDialectPolicy::openai_platform_api(),
            &compaction_done,
            &compaction_terminal,
        )
        .expect_err("compaction ciphertext stays exact");
        assert_eq!(
            err,
            ResponsesLifecycleError::InvalidLifecycle {
                reason: "terminal response changed a completed provider item"
            }
        );

        for policy in [
            LifecycleDialectPolicy::xai(),
            LifecycleDialectPolicy::openrouter(),
            LifecycleDialectPolicy::grok(),
        ] {
            let err = finish_one_item(
                policy,
                &reasoning_item("synthetic-streamed-ciphertext"),
                &reasoning_item("synthetic-terminal-ciphertext"),
            )
            .expect_err("only the platform route refreshes reasoning ciphertext");
            assert_eq!(
                err,
                ResponsesLifecycleError::InvalidLifecycle {
                    reason: "terminal response changed a completed provider item"
                }
            );
        }
    }
}
