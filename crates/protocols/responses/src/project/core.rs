//! Stateful projector core shared by Responses-family stage-2 dialects.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use plakous_model::{
    Actor, BlockCompleted, BlockStart, BlockStarted, CallId, Clock, ContentBlock, ConversationItem,
    DataClassification, Digest, EntryId, IdGenerator, ModelEvent, ModelFamilyId, ModelId,
    ModelResponseMetadata, OutputLocation, ProtocolId, Provenance, ReasoningBlock, ReasoningDelta,
    ReasoningVisibility, RefusalBlock, ReplayEnvelope, ReplayKindId, ReplayScope, ResponseStarted,
    SystemClock, SystemIdGenerator, TextBlock, TextDelta, TextPhase, ToolCallBlock, ToolCallState,
    ToolCallerLineage, ToolCorrelation, ToolExecutionOwner, ToolInputDelta, ToolName,
};
use serde_json::Value;
use serde_json::value::RawValue;

use super::error::FamilyProjectionError;

type ProjResult<T> = Result<T, FamilyProjectionError>;

/// Injectable canonical identity sources used once per projected item and call.
#[derive(Clone)]
pub struct CanonicalIdSources {
    clock: Arc<dyn Clock>,
    generator: Arc<dyn IdGenerator>,
}

impl CanonicalIdSources {
    /// Creates identity sources with caller-owned clock and UUID generation.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, generator: Arc<dyn IdGenerator>) -> Self {
        Self { clock, generator }
    }

    /// Production `UUIDv7` identity sources (system clock + generator).
    #[must_use]
    pub fn system() -> Self {
        Self::default()
    }

    /// Identity sources with an injected clock and the system UUID generator.
    #[must_use]
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self::new(clock, Arc::new(SystemIdGenerator))
    }

    /// Generates one entry identity.
    pub fn entry_id(&self) -> ProjResult<EntryId> {
        EntryId::generate(self.clock.as_ref(), self.generator.as_ref()).map_err(Into::into)
    }

    /// Generates one tool-call identity.
    pub fn call_id(&self) -> ProjResult<CallId> {
        CallId::generate(self.clock.as_ref(), self.generator.as_ref()).map_err(Into::into)
    }
}

impl Default for CanonicalIdSources {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock), Arc::new(SystemIdGenerator))
    }
}

impl fmt::Debug for CanonicalIdSources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CanonicalIdSources")
            .field("clock", &"<injected>")
            .field("generator", &"<injected>")
            .finish()
    }
}

/// Hard bounds for one stateful Responses-family-to-canonical projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectorLimits {
    /// Maximum provider events accepted.
    pub max_events: usize,
    /// Maximum canonical output items retained.
    pub max_items: usize,
    /// Maximum canonical blocks retained.
    pub max_blocks: usize,
    /// Maximum accumulated model-visible and opaque bytes.
    pub max_accumulated_bytes: usize,
}

impl Default for ProjectorLimits {
    fn default() -> Self {
        Self {
            max_events: 100_000,
            max_items: 1_024,
            max_blocks: 4_096,
            max_accumulated_bytes: 64 * 1024 * 1024,
        }
    }
}

/// How function/tool argument `done` interacts with streamed deltas.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum ToolArgsFinishPolicy {
    /// Empty known seeds from done; non-empty disagree → error (xAI/OR default).
    #[default]
    Strict,
    /// Seed from done **only when projected bytes are empty**; empty done → `{}`.
    /// Non-empty deltas that disagree with done are **kept** (never silent-replace).
    /// OpenAI Platform / Codex product policy.
    SoftSeedEmptyOnly,
}

/// When reasoning summary deltas become ModelEvents.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReasoningEmitMode {
    /// Emit `BlockStarted` / `ReasoningDelta` / `BlockCompleted` live (xAI/OR).
    #[default]
    Live,
    /// Buffer chunks; emit started+deltas+completed at finish (OpenAI Platform).
    DeferToFinish,
}

/// Options that pin dialect policy on a shared projector core.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProjectorPolicy {
    /// Tool-args finish behaviour.
    pub tool_args: ToolArgsFinishPolicy,
    /// Reasoning delta emission timing.
    pub reasoning_emit: ReasoningEmitMode,
}

impl ProjectorPolicy {
    /// xAI / OpenRouter defaults.
    #[must_use]
    pub const fn live_strict() -> Self {
        Self {
            tool_args: ToolArgsFinishPolicy::Strict,
            reasoning_emit: ReasoningEmitMode::Live,
        }
    }

    /// OpenAI Platform / Codex product defaults.
    #[must_use]
    pub const fn openai_platform() -> Self {
        Self {
            tool_args: ToolArgsFinishPolicy::SoftSeedEmptyOnly,
            reasoning_emit: ReasoningEmitMode::DeferToFinish,
        }
    }
}

/// In-progress or completed projected content block.
#[derive(Debug)]
pub enum ProjectedBlock {
    /// Streaming text.
    Text {
        /// Optional phase.
        phase: Option<TextPhase>,
        /// Accumulated bytes.
        bytes: Vec<u8>,
        /// Completion flag.
        complete: bool,
    },
    /// Streaming refusal.
    Refusal {
        /// Accumulated bytes.
        bytes: Vec<u8>,
        /// Completion flag.
        complete: bool,
    },
    /// Streaming reasoning summary.
    Reasoning {
        /// Accumulated bytes.
        bytes: Vec<u8>,
        /// Per-delta chunks for [`ReasoningEmitMode::DeferToFinish`].
        chunks: Vec<Vec<u8>>,
        /// Bound replay at completion.
        replay: Option<ReplayEnvelope>,
        /// Completion flag.
        complete: bool,
    },
    /// Streaming tool arguments.
    Tool {
        /// Canonical call id.
        call_id: CallId,
        /// Tool name.
        name: ToolName,
        /// Execution owner.
        owner: ToolExecutionOwner,
        /// Provider correlation.
        correlation: ToolCorrelation,
        /// Accumulated argument bytes.
        bytes: Vec<u8>,
        /// Completion flag.
        complete: bool,
    },
    /// Fully materialised block.
    Complete(ContentBlock),
}

/// Kind of a projected output item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectedItemKind {
    /// Assistant message.
    Message,
    /// Reasoning item.
    Reasoning,
    /// Function/tool call.
    Function,
    /// Opaque retained item.
    Opaque,
}

/// One projected output item with provider-index correlation.
#[derive(Debug)]
pub struct ProjectedItem {
    /// Contiguous canonical item index.
    pub canonical_index: u32,
    /// Stable entry identity.
    pub entry_id: EntryId,
    /// Item kind.
    pub kind: ProjectedItemKind,
    /// Provider item id (xAI identity tracking).
    pub provider_id: Option<String>,
    /// Remote function call_id (xAI correlation tracking).
    pub remote_call_id: Option<String>,
    /// Canonical blocks.
    pub blocks: BTreeMap<u32, ProjectedBlock>,
    /// Provider block index → canonical block index.
    pub provider_blocks: BTreeMap<u32, u32>,
    /// Whether the item finished.
    pub done: bool,
}

/// Shared block/item state machine for one Responses-family projection.
pub struct ResponseProjectorCore {
    ids: CanonicalIdSources,
    limits: ProjectorLimits,
    policy: ProjectorPolicy,
    events_seen: usize,
    blocks_seen: usize,
    accumulated_bytes: usize,
    started: bool,
    terminal: bool,
    failed: bool,
    response_id: Option<String>,
    response_metadata: Option<ModelResponseMetadata>,
    provider_item_ids: BTreeSet<String>,
    remote_call_ids: BTreeSet<String>,
    provider_items: BTreeMap<u32, u32>,
    items: BTreeMap<u32, ProjectedItem>,
}

impl ResponseProjectorCore {
    /// Creates an empty projector core with live/strict defaults (xAI/OR).
    #[must_use]
    pub fn new(ids: CanonicalIdSources, limits: ProjectorLimits) -> Self {
        Self::new_with_policy(ids, limits, ProjectorPolicy::live_strict())
    }

    /// Creates an empty projector core with an explicit dialect policy.
    ///
    /// OpenAI Platform must use [`ProjectorPolicy::openai_platform`]
    /// (`SoftSeedEmptyOnly` + `DeferToFinish`).
    #[must_use]
    pub fn new_with_policy(
        ids: CanonicalIdSources,
        limits: ProjectorLimits,
        policy: ProjectorPolicy,
    ) -> Self {
        Self {
            ids,
            limits,
            policy,
            events_seen: 0,
            blocks_seen: 0,
            accumulated_bytes: 0,
            started: false,
            terminal: false,
            failed: false,
            response_id: None,
            response_metadata: None,
            provider_item_ids: BTreeSet::new(),
            remote_call_ids: BTreeSet::new(),
            provider_items: BTreeMap::new(),
            items: BTreeMap::new(),
        }
    }

    /// Projector limits.
    #[must_use]
    pub const fn limits(&self) -> ProjectorLimits {
        self.limits
    }

    /// Dialect policy pinned at construction.
    #[must_use]
    pub const fn policy(&self) -> ProjectorPolicy {
        self.policy
    }

    /// Accumulated projected byte counter (content + residual reserves).
    #[must_use]
    pub const fn accumulated_bytes(&self) -> usize {
        self.accumulated_bytes
    }
    /// Whether response started.
    #[must_use]
    pub const fn started(&self) -> bool {
        self.started
    }
    /// Whether terminal.
    #[must_use]
    pub const fn terminal(&self) -> bool {
        self.terminal
    }
    /// Whether failed.
    #[must_use]
    pub const fn failed(&self) -> bool {
        self.failed
    }
    /// Mark failed.
    pub fn mark_failed(&mut self) {
        self.failed = true;
    }

    /// Retained response id.
    #[must_use]
    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }
    /// Response metadata when set.
    #[must_use]
    pub fn response_metadata(&self) -> Option<&ModelResponseMetadata> {
        self.response_metadata.as_ref()
    }
    /// Provider → canonical item map.
    #[must_use]
    pub const fn provider_items(&self) -> &BTreeMap<u32, u32> {
        &self.provider_items
    }
    /// Projected items.
    #[must_use]
    pub const fn items(&self) -> &BTreeMap<u32, ProjectedItem> {
        &self.items
    }

    /// Accept one more stream event (limits + terminal guard).
    pub fn begin_event(&mut self) -> ProjResult<()> {
        if self.terminal || self.failed {
            return Err(FamilyProjectionError::InvalidState(
                "provider event arrived after projector termination",
            ));
        }
        self.events_seen = self.events_seen.saturating_add(1);
        if self.events_seen > self.limits.max_events {
            self.failed = true;
            return Err(FamilyProjectionError::ResourceLimit("projected events"));
        }
        Ok(())
    }

    /// Start response envelope.
    pub fn start_response(
        &mut self,
        response_id: String,
        metadata: ModelResponseMetadata,
        plan_digest: Digest,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        if self.started || response_id.is_empty() {
            return Err(FamilyProjectionError::InvalidState(
                "response started more than once or without identity",
            ));
        }
        self.started = true;
        self.response_id = Some(response_id.clone());
        self.response_metadata = Some(metadata.clone());
        output.push(ModelEvent::ResponseStarted(
            ResponseStarted::new(Some(response_id), plan_digest).with_metadata(metadata),
        ));
        Ok(())
    }

    /// Register a provider item shell. When `track_ids`, enforces unique provider_id/call_id.
    pub fn register_item(
        &mut self,
        provider_index: u32,
        kind: ProjectedItemKind,
        provider_id: Option<String>,
        remote_call_id: Option<String>,
        track_ids: bool,
    ) -> ProjResult<EntryId> {
        if self.provider_items.contains_key(&provider_index) {
            return Err(FamilyProjectionError::InvalidState(
                "output item was added more than once",
            ));
        }
        if self.items.len() >= self.limits.max_items {
            return Err(FamilyProjectionError::ResourceLimit(
                "canonical output items",
            ));
        }
        if track_ids {
            if provider_id
                .as_ref()
                .is_some_and(|id| self.provider_item_ids.contains(id))
            {
                return Err(FamilyProjectionError::InvalidState(
                    "output item identity was reused",
                ));
            }
            if remote_call_id
                .as_ref()
                .is_some_and(|id| self.remote_call_ids.contains(id))
            {
                return Err(FamilyProjectionError::InvalidState(
                    "function-call correlation was reused",
                ));
            }
        }
        let canonical_index = u32::try_from(self.items.len())
            .map_err(|_| FamilyProjectionError::ResourceLimit("canonical output item index"))?;
        let entry_id = self.ids.entry_id()?;
        self.provider_items.insert(provider_index, canonical_index);
        self.items.insert(
            canonical_index,
            ProjectedItem {
                canonical_index,
                entry_id,
                kind,
                provider_id: provider_id.clone(),
                remote_call_id: remote_call_id.clone(),
                blocks: BTreeMap::new(),
                provider_blocks: BTreeMap::new(),
                done: false,
            },
        );
        if let Some(id) = provider_id {
            self.provider_item_ids.insert(id);
        }
        if let Some(id) = remote_call_id {
            self.remote_call_ids.insert(id);
        }
        Ok(entry_id)
    }

    /// Start a function-call tool block at provider block 0 (emit + Requested).
    pub fn start_tool_block(
        &mut self,
        provider_index: u32,
        entry_id: EntryId,
        call_id_remote: &str,
        name: ToolName,
        owner: ToolExecutionOwner,
        initial_arguments: &str,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        let correlation = ToolCorrelation::new(
            Some(call_id_remote.to_owned()),
            None,
            ToolCallerLineage::Direct,
        )?;
        self.start_tool_block_with(
            provider_index,
            entry_id,
            correlation,
            name,
            owner,
            ToolCallState::Requested,
            initial_arguments,
            true,
            output,
        )
    }

    /// Start a tool block with full correlation / emit control (OpenAI custom/program).
    #[allow(
        clippy::too_many_arguments,
        reason = "matches wire correlation fields at tool start"
    )]
    pub fn start_tool_block_with(
        &mut self,
        provider_index: u32,
        entry_id: EntryId,
        correlation: ToolCorrelation,
        name: ToolName,
        owner: ToolExecutionOwner,
        state: ToolCallState,
        initial_arguments: &str,
        emit_started: bool,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        let call = self.ids.call_id()?;
        let block = ProjectedBlock::Tool {
            call_id: call,
            name: name.clone(),
            owner: owner.clone(),
            correlation: correlation.clone(),
            bytes: Vec::new(),
            complete: false,
        };
        let location = self.insert_block(provider_index, 0, block)?;
        if emit_started {
            output.push(ModelEvent::BlockStarted(BlockStarted::new(
                entry_id,
                location,
                BlockStart::ToolCall {
                    call_id: call,
                    tool: name,
                    owner,
                    state,
                    correlation,
                },
            )));
        }
        if !initial_arguments.is_empty() {
            self.append_tool_with_emit(provider_index, initial_arguments, emit_started, output)?;
        }
        Ok(())
    }

    /// Insert a pre-completed block and emit BlockStarted.
    pub fn start_complete_block(
        &mut self,
        provider_index: u32,
        provider_block: u32,
        complete: ContentBlock,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        let entry_id = self.item(provider_index)?.entry_id;
        let location = self.insert_block(
            provider_index,
            provider_block,
            ProjectedBlock::Complete(complete.clone()),
        )?;
        output.push(ModelEvent::BlockStarted(BlockStarted::new(
            entry_id,
            location,
            BlockStart::Complete(complete),
        )));
        Ok(())
    }

    /// Start a reasoning summary block (part-added path).
    ///
    /// Under [`ReasoningEmitMode::DeferToFinish`], only reserves block state —
    /// no `BlockStarted` is emitted until [`Self::finish_deferred_reasoning`].
    pub fn start_reasoning_block(
        &mut self,
        provider_index: u32,
        summary_index: u32,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        self.start_reasoning_block_with_visibility(
            provider_index,
            summary_index,
            ReasoningVisibility::Summary,
            output,
        )
    }

    /// Starts a reasoning block with an explicitly selected canonical visibility.
    pub fn start_reasoning_block_with_visibility(
        &mut self,
        provider_index: u32,
        summary_index: u32,
        visibility: ReasoningVisibility,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        if self
            .item(provider_index)?
            .provider_blocks
            .contains_key(&summary_index)
        {
            return Err(FamilyProjectionError::InvalidState(
                "reasoning summary was added more than once",
            ));
        }
        let entry_id = self.item(provider_index)?.entry_id;
        let location = self.insert_block(
            provider_index,
            summary_index,
            ProjectedBlock::Reasoning {
                bytes: Vec::new(),
                chunks: Vec::new(),
                replay: None,
                complete: false,
            },
        )?;
        if self.policy.reasoning_emit == ReasoningEmitMode::Live {
            output.push(ModelEvent::BlockStarted(BlockStarted::new(
                entry_id,
                location,
                BlockStart::Reasoning {
                    visibility,
                    replay: None,
                },
            )));
        }
        Ok(())
    }

    /// Start a text or refusal block with an explicit phase (OpenAI message phase).
    pub fn start_message_content_block(
        &mut self,
        provider_index: u32,
        content_index: u32,
        phase: Option<TextPhase>,
        is_refusal: bool,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        let entry_id = self.item(provider_index)?.entry_id;
        let (block, start) = if is_refusal {
            (
                ProjectedBlock::Refusal {
                    bytes: Vec::new(),
                    complete: false,
                },
                BlockStart::Refusal,
            )
        } else {
            (
                ProjectedBlock::Text {
                    phase,
                    bytes: Vec::new(),
                    complete: false,
                },
                BlockStart::Text { phase },
            )
        };
        let location = self.insert_block(provider_index, content_index, block)?;
        output.push(ModelEvent::BlockStarted(BlockStarted::new(
            entry_id, location, start,
        )));
        Ok(())
    }

    /// Validate completed content-part against streamed fragments (xAI).
    pub fn finish_content_validate(
        &mut self,
        provider_index: u32,
        provider_block: u32,
        part: &Value,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        let kind = part.get("type").and_then(Value::as_str);
        let expected = match kind {
            Some("output_text") => part.get("text").and_then(Value::as_str),
            Some("refusal") => part.get("refusal").and_then(Value::as_str),
            _ => return Ok(()),
        }
        .ok_or(FamilyProjectionError::InvalidState(
            "completed content omitted its value",
        ))?;
        let location = self.location(provider_index, provider_block)?;
        let matches = match (kind, self.block(location)?) {
            (Some("output_text"), ProjectedBlock::Text { bytes, .. })
            | (Some("refusal"), ProjectedBlock::Refusal { bytes, .. }) => {
                bytes.as_slice() == expected.as_bytes()
            }
            _ => false,
        };
        if !matches {
            return Err(FamilyProjectionError::InvalidState(
                "completed content disagrees with its fragments",
            ));
        }
        self.complete_provider_block(provider_index, provider_block, output)
    }

    /// Append text/refusal delta.
    pub fn append_text(
        &mut self,
        provider_index: u32,
        provider_block: u32,
        delta: &str,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        if delta.is_empty() {
            return Ok(());
        }
        self.reserve_bytes(delta.len())?;
        let location = self.location(provider_index, provider_block)?;
        match self.block_mut(location)? {
            ProjectedBlock::Text {
                bytes, complete, ..
            }
            | ProjectedBlock::Refusal { bytes, complete }
                if !*complete =>
            {
                bytes.extend_from_slice(delta.as_bytes());
            }
            _ => {
                return Err(FamilyProjectionError::InvalidState(
                    "text delta targeted a non-text or complete block",
                ));
            }
        }
        output.push(ModelEvent::TextDelta(TextDelta::new(
            location,
            delta.as_bytes(),
        )));
        Ok(())
    }

    /// Append reasoning delta; auto-starts the block when missing.
    ///
    /// Under [`ReasoningEmitMode::DeferToFinish`], buffers bytes/chunks only.
    pub fn append_reasoning(
        &mut self,
        provider_index: u32,
        summary_index: u32,
        delta: &str,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        if delta.is_empty() {
            return Ok(());
        }
        if !self
            .item(provider_index)?
            .provider_blocks
            .contains_key(&summary_index)
        {
            let entry_id = self.item(provider_index)?.entry_id;
            let location = self.insert_block(
                provider_index,
                summary_index,
                ProjectedBlock::Reasoning {
                    bytes: Vec::new(),
                    chunks: Vec::new(),
                    replay: None,
                    complete: false,
                },
            )?;
            if self.policy.reasoning_emit == ReasoningEmitMode::Live {
                output.push(ModelEvent::BlockStarted(BlockStarted::new(
                    entry_id,
                    location,
                    BlockStart::Reasoning {
                        visibility: ReasoningVisibility::Summary,
                        replay: None,
                    },
                )));
            }
        }
        self.reserve_bytes(delta.len())?;
        let location = self.location(provider_index, summary_index)?;
        let defer = self.policy.reasoning_emit == ReasoningEmitMode::DeferToFinish;
        match self.block_mut(location)? {
            ProjectedBlock::Reasoning {
                bytes,
                chunks,
                complete,
                ..
            } if !*complete => {
                bytes.extend_from_slice(delta.as_bytes());
                if defer {
                    chunks.push(delta.as_bytes().to_vec());
                }
            }
            _ => {
                return Err(FamilyProjectionError::InvalidState(
                    "reasoning delta targeted a non-reasoning or complete block",
                ));
            }
        }
        if self.policy.reasoning_emit == ReasoningEmitMode::Live {
            output.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                location,
                delta.as_bytes(),
            )));
        }
        Ok(())
    }

    /// Append tool argument delta at provider block 0 (always emit).
    pub fn append_tool(
        &mut self,
        provider_index: u32,
        delta: &str,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        self.append_tool_with_emit(provider_index, delta, true, output)
    }

    /// Append tool argument delta; optionally suppress `ToolInputDelta` (custom tools).
    pub fn append_tool_with_emit(
        &mut self,
        provider_index: u32,
        delta: &str,
        emit: bool,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        if delta.is_empty() {
            return Ok(());
        }
        self.reserve_bytes(delta.len())?;
        let location = self.location(provider_index, 0)?;
        match self.block_mut(location)? {
            ProjectedBlock::Tool {
                bytes, complete, ..
            } if !*complete => bytes.extend_from_slice(delta.as_bytes()),
            _ => {
                return Err(FamilyProjectionError::InvalidState(
                    "tool delta targeted a non-tool or complete block",
                ));
            }
        }
        if emit {
            output.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
                location,
                delta.as_bytes(),
            )));
        }
        Ok(())
    }

    /// Mark provider block complete and emit `BlockCompleted`.
    pub fn complete_provider_block(
        &mut self,
        provider_index: u32,
        provider_block: u32,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        self.complete_provider_block_with_emit(provider_index, provider_block, true, output)
    }

    /// Mark provider block complete; optionally suppress `BlockCompleted` (deferred reasoning).
    pub fn complete_provider_block_with_emit(
        &mut self,
        provider_index: u32,
        provider_block: u32,
        emit: bool,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        let location = self.location(provider_index, provider_block)?;
        let completed = match self.block_mut(location)? {
            ProjectedBlock::Text { complete, .. }
            | ProjectedBlock::Refusal { complete, .. }
            | ProjectedBlock::Reasoning { complete, .. }
            | ProjectedBlock::Tool { complete, .. } => {
                if *complete {
                    return Ok(());
                }
                *complete = true;
                true
            }
            ProjectedBlock::Complete(_) => false,
        };
        if completed && emit {
            output.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        Ok(())
    }

    /// Kind/done/identity gates for finish_item. `None` means already done.
    pub fn prepare_finish_item(
        &self,
        provider_index: u32,
        expected_kind: ProjectedItemKind,
        provider_id: Option<&str>,
        remote_call_id: Option<&str>,
        check_identity: bool,
    ) -> ProjResult<Option<u32>> {
        let canonical_index = *self.provider_items.get(&provider_index).ok_or(
            FamilyProjectionError::InvalidState("output item completed before it was added"),
        )?;
        let item = self
            .items
            .get(&canonical_index)
            .ok_or(FamilyProjectionError::InvalidState(
                "canonical item disappeared",
            ))?;
        if item.kind != expected_kind {
            return Err(FamilyProjectionError::InvalidState(
                "output item type changed before completion",
            ));
        }
        if check_identity
            && (item.provider_id.as_deref() != provider_id
                || item.remote_call_id.as_deref() != remote_call_id)
        {
            return Err(FamilyProjectionError::InvalidState(
                "output item identity changed before completion",
            ));
        }
        if item.done {
            return Ok(None);
        }
        Ok(Some(canonical_index))
    }

    /// Mark item done.
    pub fn mark_item_done(&mut self, canonical_index: u32) -> ProjResult<()> {
        self.items
            .get_mut(&canonical_index)
            .ok_or(FamilyProjectionError::InvalidState(
                "canonical item disappeared",
            ))?
            .done = true;
        Ok(())
    }

    /// Complete function-call arguments using the core's pinned tool-args policy.
    pub fn finish_function_arguments(
        &mut self,
        provider_index: u32,
        arguments: &str,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        self.finish_function_arguments_with(
            provider_index,
            arguments,
            self.policy.tool_args,
            true,
            output,
        )
    }

    /// SoftSeedEmptyOnly finish with optional stream-event emission (custom tools).
    pub fn finish_function_arguments_with(
        &mut self,
        provider_index: u32,
        arguments: &str,
        policy: ToolArgsFinishPolicy,
        emit_events: bool,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        let seed = match policy {
            ToolArgsFinishPolicy::SoftSeedEmptyOnly if arguments.is_empty() => "{}",
            _ => arguments,
        };
        let location = self.location(provider_index, 0)?;
        let (known_len, known_empty, matches_seed, semantically_equivalent, complete) =
            match self.block(location)? {
                ProjectedBlock::Tool {
                    bytes, complete, ..
                } => {
                    let matches_seed = bytes.as_slice() == seed.as_bytes();
                    let semantically_equivalent = policy == ToolArgsFinishPolicy::SoftSeedEmptyOnly
                        && !bytes.is_empty()
                        && !matches_seed
                        && json_values_equivalent(std::str::from_utf8(bytes).unwrap_or(""), seed);
                    (
                        bytes.len(),
                        bytes.is_empty(),
                        matches_seed,
                        semantically_equivalent,
                        *complete,
                    )
                }
                _ => {
                    return Err(FamilyProjectionError::InvalidState(
                        "tool done targeted a non-tool or complete block",
                    ));
                }
            };
        if !matches_seed {
            if complete {
                if !semantically_equivalent {
                    return Err(FamilyProjectionError::InvalidState(
                        "tool done targeted a non-tool or complete block",
                    ));
                }
            } else if known_empty {
                self.append_tool_with_emit(provider_index, seed, emit_events, output)?;
            } else {
                let replace = match policy {
                    ToolArgsFinishPolicy::SoftSeedEmptyOnly => !semantically_equivalent,
                    ToolArgsFinishPolicy::Strict => {
                        return Err(FamilyProjectionError::InvalidState(
                            "complete tool arguments disagree with deltas",
                        ));
                    }
                };
                if replace {
                    self.replace_reserved_bytes(known_len, seed.len())?;
                    match self.block_mut(location)? {
                        ProjectedBlock::Tool {
                            bytes, complete, ..
                        } if !*complete => {
                            bytes.clear();
                            bytes.extend_from_slice(seed.as_bytes());
                        }
                        _ => {
                            return Err(FamilyProjectionError::InvalidState(
                                "tool done targeted a non-tool or complete block",
                            ));
                        }
                    }
                }
            }
        }
        self.complete_provider_block_with_emit(provider_index, 0, emit_events, output)
    }

    /// Emit deferred OpenAI-style reasoning for one item (started + deltas + completed).
    ///
    /// Requires [`ReasoningEmitMode::DeferToFinish`]. Binds `replay` onto every
    /// summary block and emits the canonical event sequence at terminal finish.
    pub fn finish_deferred_reasoning(
        &mut self,
        provider_index: u32,
        replay: &ReplayEnvelope,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        let entry_id = self.item(provider_index)?.entry_id;
        let provider_blocks: Vec<u32> = self
            .item(provider_index)?
            .provider_blocks
            .keys()
            .copied()
            .collect();
        for provider_block in provider_blocks {
            let location = self.location(provider_index, provider_block)?;
            let (chunks, complete) = match self.block_mut(location)? {
                ProjectedBlock::Reasoning {
                    chunks,
                    complete,
                    replay: slot,
                    ..
                } => {
                    *slot = Some(replay.clone());
                    (chunks.clone(), *complete)
                }
                _ => {
                    return Err(FamilyProjectionError::InvalidState(
                        "reasoning item contains a non-reasoning block",
                    ));
                }
            };
            if !complete {
                return Err(FamilyProjectionError::InvalidState(
                    "reasoning output item done before summary part completion",
                ));
            }
            output.push(ModelEvent::BlockStarted(BlockStarted::new(
                entry_id,
                location,
                BlockStart::Reasoning {
                    visibility: ReasoningVisibility::Summary,
                    replay: Some(replay.clone()),
                },
            )));
            for chunk in chunks {
                output.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                    location, chunk,
                )));
            }
            output.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)));
        }
        Ok(())
    }

    /// Bind reasoning replay onto an existing streaming block.
    pub fn bind_reasoning_replay(
        &mut self,
        location: OutputLocation,
        replay: ReplayEnvelope,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        match self.block_mut(location)? {
            ProjectedBlock::Reasoning { replay: target, .. } => {
                *target = Some(replay.clone());
            }
            _ => {
                return Err(FamilyProjectionError::InvalidState(
                    "reasoning replay targeted a non-reasoning block",
                ));
            }
        }
        output.push(ModelEvent::ReasoningReplayBound(
            plakous_model::ReasoningReplayBound::new(location, replay),
        ));
        Ok(())
    }

    /// Finish one streamed reasoning summary part (existing or complete materialize).
    pub fn finish_reasoning_summary_part(
        &mut self,
        provider_index: u32,
        provider_block: u32,
        text: &str,
        bind_replay: Option<&ReplayEnvelope>,
        output: &mut Vec<ModelEvent>,
    ) -> ProjResult<()> {
        let existing = self
            .item(provider_index)?
            .provider_blocks
            .contains_key(&provider_block);
        if !existing {
            self.reserve_bytes(text.len())?;
            let block = ContentBlock::Reasoning(ReasoningBlock::new(
                ReasoningVisibility::Summary,
                Some(text.to_owned()),
                bind_replay.cloned(),
            )?);
            return self.start_complete_block(provider_index, provider_block, block, output);
        }
        let location = self.location(provider_index, provider_block)?;
        let known = match self.block(location)? {
            ProjectedBlock::Reasoning { bytes, .. } => bytes.clone(),
            _ => Vec::new(),
        };
        if known != text.as_bytes() {
            return Err(FamilyProjectionError::InvalidState(
                "complete reasoning summary disagrees with deltas",
            ));
        }
        if let Some(replay) = bind_replay {
            self.bind_reasoning_replay(location, replay.clone(), output)?;
        }
        self.complete_provider_block(provider_index, provider_block, output)
    }

    /// Build contiguous completed conversation items.
    pub fn canonical_items(&self) -> ProjResult<Vec<ConversationItem>> {
        let mut output = Vec::with_capacity(self.items.len());
        for index in 0..self.items.len() {
            let index = u32::try_from(index)
                .map_err(|_| FamilyProjectionError::ResourceLimit("canonical item index"))?;
            let item = self
                .items
                .get(&index)
                .ok_or(FamilyProjectionError::InvalidState(
                    "canonical output item indices are not contiguous",
                ))?;
            if !item.done {
                return Err(FamilyProjectionError::InvalidState(
                    "canonical output item did not complete",
                ));
            }
            let mut blocks = Vec::with_capacity(item.blocks.len());
            for block_index in 0..item.blocks.len() {
                let block_index = u32::try_from(block_index)
                    .map_err(|_| FamilyProjectionError::ResourceLimit("canonical block index"))?;
                let block =
                    item.blocks
                        .get(&block_index)
                        .ok_or(FamilyProjectionError::InvalidState(
                            "canonical block indices are not contiguous",
                        ))?;
                blocks.push(canonical_block(block)?);
            }
            output.push(ConversationItem::new(
                item.entry_id,
                Actor::Assistant,
                blocks,
                Provenance::ModelOutput,
            )?);
        }
        Ok(output)
    }

    /// Insert a block under a provider index.
    pub fn insert_block(
        &mut self,
        provider_index: u32,
        provider_block: u32,
        block: ProjectedBlock,
    ) -> ProjResult<OutputLocation> {
        if self.blocks_seen >= self.limits.max_blocks {
            return Err(FamilyProjectionError::ResourceLimit(
                "canonical output blocks",
            ));
        }
        let item = self.item_mut(provider_index)?;
        if item.provider_blocks.contains_key(&provider_block) {
            return Err(FamilyProjectionError::InvalidState(
                "output block was added more than once",
            ));
        }
        let canonical_block = u32::try_from(item.blocks.len())
            .map_err(|_| FamilyProjectionError::ResourceLimit("canonical block index"))?;
        item.provider_blocks.insert(provider_block, canonical_block);
        item.blocks.insert(canonical_block, block);
        let canonical_index = item.canonical_index;
        self.blocks_seen = self.blocks_seen.saturating_add(1);
        Ok(OutputLocation::new(canonical_index, canonical_block))
    }

    /// Resolve provider block to canonical location.
    pub fn location(&self, provider_index: u32, provider_block: u32) -> ProjResult<OutputLocation> {
        let item = self.item(provider_index)?;
        let block = *item.provider_blocks.get(&provider_block).ok_or(
            FamilyProjectionError::InvalidState("event referenced an unknown output block"),
        )?;
        Ok(OutputLocation::new(item.canonical_index, block))
    }

    /// Item by provider index.
    pub fn item(&self, provider_index: u32) -> ProjResult<&ProjectedItem> {
        let canonical =
            self.provider_items
                .get(&provider_index)
                .ok_or(FamilyProjectionError::InvalidState(
                    "event referenced an unknown output item",
                ))?;
        self.items
            .get(canonical)
            .ok_or(FamilyProjectionError::InvalidState(
                "canonical item disappeared",
            ))
    }

    /// Mutable item by provider index.
    pub fn item_mut(&mut self, provider_index: u32) -> ProjResult<&mut ProjectedItem> {
        let canonical = *self.provider_items.get(&provider_index).ok_or(
            FamilyProjectionError::InvalidState("event referenced an unknown output item"),
        )?;
        self.items
            .get_mut(&canonical)
            .ok_or(FamilyProjectionError::InvalidState(
                "canonical item disappeared",
            ))
    }

    /// Block by location.
    pub fn block(&self, location: OutputLocation) -> ProjResult<&ProjectedBlock> {
        self.items
            .get(&location.item_index())
            .and_then(|item| item.blocks.get(&location.block_index()))
            .ok_or(FamilyProjectionError::InvalidState(
                "canonical block disappeared",
            ))
    }

    /// Mutable block by location.
    pub fn block_mut(&mut self, location: OutputLocation) -> ProjResult<&mut ProjectedBlock> {
        self.items
            .get_mut(&location.item_index())
            .and_then(|item| item.blocks.get_mut(&location.block_index()))
            .ok_or(FamilyProjectionError::InvalidState(
                "canonical block disappeared",
            ))
    }

    /// Build a replay envelope from a serialized payload.
    pub fn make_replay_envelope(
        &mut self,
        protocol: ProtocolId,
        model_family: Option<ModelFamilyId>,
        model: ModelId,
        kind: &'static str,
        version: u32,
        payload: Box<RawValue>,
    ) -> ProjResult<ReplayEnvelope> {
        self.reserve_bytes(payload.get().len())?;
        Ok(ReplayEnvelope::new(
            protocol,
            model_family,
            ReplayScope::ExactModel(model),
            ReplayKindId::new(kind)?,
            version,
            DataClassification::Sensitive,
            payload,
        )?)
    }

    /// Reserve projected byte budget.
    pub fn reserve_bytes(&mut self, bytes: usize) -> ProjResult<()> {
        let accumulated_bytes = self
            .accumulated_bytes
            .checked_add(bytes)
            .ok_or(FamilyProjectionError::ResourceLimit("projected bytes"))?;
        if accumulated_bytes > self.limits.max_accumulated_bytes {
            return Err(FamilyProjectionError::ResourceLimit("projected bytes"));
        }
        self.accumulated_bytes = accumulated_bytes;
        Ok(())
    }

    /// Replace a previously reserved byte slice with a different size (item raw swap).
    pub fn replace_reserved_bytes(
        &mut self,
        previous_bytes: usize,
        replacement_bytes: usize,
    ) -> ProjResult<()> {
        let without_previous = self
            .accumulated_bytes
            .checked_sub(previous_bytes)
            .ok_or(FamilyProjectionError::ResourceLimit("projected bytes"))?;
        let replacement_total = without_previous
            .checked_add(replacement_bytes)
            .ok_or(FamilyProjectionError::ResourceLimit("projected bytes"))?;
        if replacement_total > self.limits.max_accumulated_bytes {
            return Err(FamilyProjectionError::ResourceLimit("projected bytes"));
        }
        self.accumulated_bytes = replacement_total;
        Ok(())
    }

    /// Tool block correlation for dialect residual checks.
    pub fn tool_correlation(
        &self,
        provider_index: u32,
    ) -> ProjResult<(ToolName, ToolCorrelation, ToolExecutionOwner, CallId)> {
        let location = self.location(provider_index, 0)?;
        match self.block(location)? {
            ProjectedBlock::Tool {
                call_id,
                name,
                owner,
                correlation,
                ..
            } => Ok((name.clone(), correlation.clone(), owner.clone(), *call_id)),
            _ => Err(FamilyProjectionError::InvalidState(
                "tool output block is missing",
            )),
        }
    }

    /// Materialise a completed tool block into a [`ContentBlock`] (custom-tool finish).
    pub fn materialize_tool_block(&self, provider_index: u32) -> ProjResult<ContentBlock> {
        let location = self.location(provider_index, 0)?;
        canonical_block(self.block(location)?)
    }

    /// Require response start observed.
    pub fn require_started(&self) -> ProjResult<()> {
        if self.started {
            Ok(())
        } else {
            Err(FamilyProjectionError::InvalidState(
                "output arrived before response start",
            ))
        }
    }

    /// Text/refusal complete blocks from strings.
    pub fn text_block(text: String) -> ProjResult<ContentBlock> {
        Ok(ContentBlock::Text(TextBlock::new(
            text,
            Some(TextPhase::Final),
        )?))
    }

    /// Match streamed text/refusal bytes to expected.
    pub fn streamed_text_matches(block: &ProjectedBlock, is_text: bool, expected: &str) -> bool {
        match (is_text, block) {
            (true, ProjectedBlock::Text { bytes, .. })
            | (false, ProjectedBlock::Refusal { bytes, .. }) => {
                bytes.as_slice() == expected.as_bytes()
            }
            _ => false,
        }
    }
}

impl fmt::Debug for ResponseProjectorCore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseProjectorCore")
            .field("limits", &self.limits)
            .field("policy", &self.policy)
            .field("events_seen", &self.events_seen)
            .field("blocks_seen", &self.blocks_seen)
            .field("accumulated_bytes", &self.accumulated_bytes)
            .field("started", &self.started)
            .field("terminal", &self.terminal)
            .field("failed", &self.failed)
            .field(
                "response_id",
                &self.response_id.as_ref().map(|_| "<redacted>"),
            )
            .field("response_metadata", &self.response_metadata)
            .field("items", &self.items.len())
            .finish_non_exhaustive()
    }
}

fn json_values_equivalent(a: &str, b: &str) -> bool {
    match (
        serde_json::from_str::<Value>(a),
        serde_json::from_str::<Value>(b),
    ) {
        (Ok(va), Ok(vb)) => va == vb,
        _ => false,
    }
}

fn canonical_block(block: &ProjectedBlock) -> ProjResult<ContentBlock> {
    match block {
        ProjectedBlock::Text {
            phase,
            bytes,
            complete: true,
        } => Ok(ContentBlock::Text(TextBlock::new(
            String::from_utf8(bytes.clone())?,
            *phase,
        )?)),
        ProjectedBlock::Refusal {
            bytes,
            complete: true,
        } => Ok(ContentBlock::Refusal(RefusalBlock::new(
            String::from_utf8(bytes.clone())?,
        )?)),
        ProjectedBlock::Reasoning {
            bytes,
            replay,
            complete: true,
            ..
        } => {
            if bytes.is_empty() {
                Ok(ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::OpaqueReplay,
                    None,
                    replay.clone(),
                )?))
            } else {
                Ok(ContentBlock::Reasoning(ReasoningBlock::new(
                    ReasoningVisibility::Summary,
                    Some(String::from_utf8(bytes.clone())?),
                    replay.clone(),
                )?))
            }
        }
        ProjectedBlock::Tool {
            call_id,
            name,
            owner,
            correlation,
            bytes,
            complete: true,
        } => {
            // Raw args are retained even when not JSON (OpenAI custom tools).
            // Local function tools still receive parseable JSON from dialect
            // finish / SoftSeedEmptyOnly seeding; callers inspect
            // [`ToolCallBlock::arguments`] when parse is required.
            let state = if matches!(owner, ToolExecutionOwner::ProviderHosted { .. }) {
                ToolCallState::Pending
            } else {
                ToolCallState::Requested
            };
            Ok(ContentBlock::ToolCall(ToolCallBlock::new(
                *call_id,
                name.clone(),
                owner.clone(),
                state,
                correlation.clone(),
                String::from_utf8(bytes.clone())?,
            )?))
        }
        ProjectedBlock::Complete(block) => Ok(block.clone()),
        ProjectedBlock::Text { .. }
        | ProjectedBlock::Refusal { .. }
        | ProjectedBlock::Reasoning { .. }
        | ProjectedBlock::Tool { .. } => Err(FamilyProjectionError::InvalidState(
            "canonical block did not complete",
        )),
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
    use plakous_model::{ToolExecutionOwner, ToolName};

    fn empty_core(policy: ProjectorPolicy) -> ResponseProjectorCore {
        core_with_byte_limit(policy, ProjectorLimits::default().max_accumulated_bytes)
    }

    fn core_with_byte_limit(
        policy: ProjectorPolicy,
        max_accumulated_bytes: usize,
    ) -> ResponseProjectorCore {
        ResponseProjectorCore::new_with_policy(
            CanonicalIdSources::system(),
            ProjectorLimits {
                max_accumulated_bytes,
                ..ProjectorLimits::default()
            },
            policy,
        )
    }

    fn test_digest(label: &[u8]) -> Digest {
        Digest::sha256(plakous_model::DigestDomain::RequestPlan, 1, &[label]).expect("digest")
    }

    fn start_function(core: &mut ResponseProjectorCore) -> EntryId {
        let mut out = Vec::new();
        core.start_response(
            "resp_test".into(),
            ModelResponseMetadata::empty(),
            test_digest(b"fn"),
            &mut out,
        )
        .expect("start");
        let entry = core
            .register_item(
                0,
                ProjectedItemKind::Function,
                Some("fc_1".into()),
                Some("call_1".into()),
                true,
            )
            .expect("register");
        let name = ToolName::new("lookup").expect("name");
        core.start_tool_block(
            0,
            entry,
            "call_1",
            name,
            ToolExecutionOwner::Local,
            "",
            &mut out,
        )
        .expect("tool start");
        entry
    }

    #[test]
    fn soft_seed_empty_only_seeds_when_deltas_empty() {
        let mut core = empty_core(ProjectorPolicy::openai_platform());
        let _ = start_function(&mut core);
        let mut out = Vec::new();
        core.finish_function_arguments(0, r#"{"q":1}"#, &mut out)
            .expect("finish");
        let location = core.location(0, 0).expect("loc");
        match core.block(location).expect("block") {
            ProjectedBlock::Tool {
                bytes, complete, ..
            } => {
                assert!(*complete);
                assert_eq!(bytes.as_slice(), br#"{"q":1}"#);
            }
            _ => panic!("expected tool"),
        }
        assert!(
            out.iter()
                .any(|e| matches!(e, ModelEvent::ToolInputDelta(_))),
            "empty known must seed a ToolInputDelta"
        );
    }

    #[test]
    fn soft_seed_empty_only_keeps_non_empty_deltas() {
        let mut core = empty_core(ProjectorPolicy::openai_platform());
        let _ = start_function(&mut core);
        let mut out = Vec::new();
        core.append_tool(0, r#"{"q":1}"#, &mut out).expect("delta");
        out.clear();
        core.finish_function_arguments(0, r#"{"q": 1}"#, &mut out)
            .expect("finish keep");
        let location = core.location(0, 0).expect("loc");
        match core.block(location).expect("block") {
            ProjectedBlock::Tool {
                bytes, complete, ..
            } => {
                assert!(*complete);
                // SoftSeedEmptyOnly: keep delta form, not done re-serialization.
                assert_eq!(bytes.as_slice(), br#"{"q":1}"#);
            }
            _ => panic!("expected tool"),
        }
        assert!(
            !out.iter()
                .any(|e| matches!(e, ModelEvent::ToolInputDelta(_))),
            "must not replace or re-emit args when deltas already present"
        );
    }

    #[test]
    fn soft_seed_semantic_diverge_prefers_done_payload() {
        let mut core = empty_core(ProjectorPolicy::openai_platform());
        let _ = start_function(&mut core);
        let mut out = Vec::new();
        core.append_tool(0, r#"{"q":1}"#, &mut out).expect("delta");
        assert_eq!(core.accumulated_bytes(), r#"{"q":1}"#.len());
        out.clear();
        core.finish_function_arguments(0, r#"{"q":1,"extra":true}"#, &mut out)
            .expect("finish");
        assert_eq!(core.accumulated_bytes(), r#"{"q":1,"extra":true}"#.len());
        let location = core.location(0, 0).expect("loc");
        match core.block(location).expect("block") {
            ProjectedBlock::Tool {
                bytes, complete, ..
            } => {
                assert!(*complete);
                assert_eq!(
                    std::str::from_utf8(bytes).expect("utf8"),
                    r#"{"q":1,"extra":true}"#
                );
            }
            _ => panic!("expected tool"),
        }
    }

    #[test]
    fn soft_seed_semantically_equivalent_repeated_finish_is_idempotent() {
        let mut core = empty_core(ProjectorPolicy::openai_platform());
        let _ = start_function(&mut core);
        let mut out = Vec::new();
        core.append_tool(0, r#"{"q":1}"#, &mut out).expect("delta");
        core.finish_function_arguments(0, r#"{"q": 1}"#, &mut out)
            .expect("first finish");
        let accumulated = core.accumulated_bytes();
        out.clear();

        core.finish_function_arguments(0, r#"{ "q" : 1 }"#, &mut out)
            .expect("idempotent repeated finish");

        assert_eq!(core.accumulated_bytes(), accumulated);
        assert_eq!(out, [] as [ModelEvent; 0]);
        let location = core.location(0, 0).expect("location");
        let ProjectedBlock::Tool {
            bytes, complete, ..
        } = core.block(location).expect("block")
        else {
            panic!("expected tool")
        };
        assert_eq!(bytes, br#"{"q":1}"#);
        assert!(*complete);
    }

    #[test]
    fn soft_seed_empty_only_empty_done_becomes_object() {
        let mut core = empty_core(ProjectorPolicy::openai_platform());
        let _ = start_function(&mut core);
        let mut out = Vec::new();
        core.finish_function_arguments(0, "", &mut out)
            .expect("finish empty");
        let location = core.location(0, 0).expect("loc");
        match core.block(location).expect("block") {
            ProjectedBlock::Tool { bytes, .. } => assert_eq!(bytes.as_slice(), b"{}"),
            _ => panic!("expected tool"),
        }
    }

    #[test]
    fn done_replacement_accounting_is_exact_and_fail_closed_at_boundaries() {
        for limit in 1_usize..=16 {
            for initial_len in 1..=limit {
                for replacement_len in 1..=limit.saturating_add(1) {
                    let mut core = core_with_byte_limit(ProjectorPolicy::default(), limit);
                    let _ = start_function(&mut core);
                    let mut out = Vec::new();
                    let initial = "a".repeat(initial_len);
                    let replacement = "b".repeat(replacement_len);
                    core.append_tool(0, &initial, &mut out).expect("initial");
                    assert_eq!(core.accumulated_bytes(), initial_len);

                    let result = core.finish_function_arguments_with(
                        0,
                        &replacement,
                        ToolArgsFinishPolicy::SoftSeedEmptyOnly,
                        true,
                        &mut out,
                    );
                    let location = core.location(0, 0).expect("location");
                    let ProjectedBlock::Tool {
                        bytes, complete, ..
                    } = core.block(location).expect("block")
                    else {
                        panic!("expected tool")
                    };
                    if replacement_len <= limit {
                        result.expect("replacement within bound");
                        assert_eq!(core.accumulated_bytes(), replacement_len);
                        assert_eq!(bytes, replacement.as_bytes());
                        assert!(*complete);
                    } else {
                        assert!(matches!(
                            result,
                            Err(FamilyProjectionError::ResourceLimit("projected bytes"))
                        ));
                        assert_eq!(core.accumulated_bytes(), initial_len);
                        assert_eq!(bytes, initial.as_bytes());
                        assert!(!*complete);
                    }
                }
            }
        }
    }

    #[test]
    fn rejected_empty_seed_reservation_leaves_byte_ledger_unchanged() {
        let mut core = core_with_byte_limit(ProjectorPolicy::default(), 1);
        let _ = start_function(&mut core);
        let mut out = Vec::new();
        assert!(matches!(
            core.finish_function_arguments_with(
                0,
                "",
                ToolArgsFinishPolicy::SoftSeedEmptyOnly,
                true,
                &mut out,
            ),
            Err(FamilyProjectionError::ResourceLimit("projected bytes"))
        ));
        assert_eq!(core.accumulated_bytes(), 0);
        let location = core.location(0, 0).expect("location");
        let ProjectedBlock::Tool {
            bytes, complete, ..
        } = core.block(location).expect("block")
        else {
            panic!("expected tool")
        };
        assert_eq!(bytes.as_slice(), [] as [u8; 0]);
        assert!(!*complete);
    }

    #[test]
    fn defer_reasoning_buffers_until_finish() {
        let mut core = empty_core(ProjectorPolicy::openai_platform());
        let mut out = Vec::new();
        core.start_response(
            "resp_r".into(),
            ModelResponseMetadata::empty(),
            test_digest(b"rs"),
            &mut out,
        )
        .expect("start");
        let _ = core
            .register_item(
                0,
                ProjectedItemKind::Reasoning,
                Some("rs_1".into()),
                None,
                true,
            )
            .expect("register");
        out.clear();
        core.start_reasoning_block(0, 0, &mut out).expect("part");
        assert!(out.is_empty(), "DeferToFinish must not emit on part.added");
        core.append_reasoning(0, 0, "think", &mut out)
            .expect("delta");
        assert!(
            out.is_empty(),
            "DeferToFinish must not emit live ReasoningDelta"
        );
        core.complete_provider_block_with_emit(0, 0, false, &mut out)
            .expect("part done silent");
        assert_eq!(out, [] as [ModelEvent; 0]);

        let payload = RawValue::from_string("{}".into()).expect("raw");
        let replay = core
            .make_replay_envelope(
                ProtocolId::new("openai.responses").expect("proto"),
                None,
                ModelId::new("gpt-test").expect("model"),
                "test.reasoning",
                1,
                payload,
            )
            .expect("replay");
        core.finish_deferred_reasoning(0, &replay, &mut out)
            .expect("finish");
        assert!(
            matches!(out.first(), Some(ModelEvent::BlockStarted(_))),
            "deferred finish starts with BlockStarted"
        );
        assert!(
            out.iter()
                .any(|e| matches!(e, ModelEvent::ReasoningDelta(_))),
            "deferred finish emits buffered deltas"
        );
        assert!(
            matches!(out.last(), Some(ModelEvent::BlockCompleted(_))),
            "deferred finish ends with BlockCompleted"
        );
    }
}
