//! Explicit semantic product DTO; embedded in the versioned export, never a native row image.
use super::DormantSession;
use super::MoveIdentityClaim;
use crate::binding::{CompositionSelection, TargetSelection};
use crate::sessions::projector::{ProductIntents, RowKey, SessionViewState, SessionViews};
use crate::sessions::{
    ProductTranscript, QueueCommands, QueueDrainState, QueueItem, RootProfile, RunnerDisplay,
    SessionNode, SessionRecord, TranscriptRow,
};
use plakous::sessions::SessionLocator;
use plakous::sessions::feed::FeedSeq;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Closed export-v13 product cut. No process-local draft registry is represented.
#[doc(hidden)]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DormantSessionDocument {
    id: String,
    node: SessionNode,
    create_operation_id: String,
    workspace_root: PathBuf,
    created_at_unix: u64,
    updated_at_unix: u64,
    account: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    composition: CompositionSelection,
    target_selection: TargetSelection,
    title: Option<String>,
    title_override: Option<String>,
    source_product_session_id: Option<String>,
    source_workflow_operation_id: Option<String>,
    #[serde(with = "super::locator_serde")]
    locator: SessionLocator,
    frozen_tools: Option<String>,
    prompt_history: Vec<String>,
    queue: Vec<QueueEntry>,
    profile: Option<RootProfile>,
    runner: Option<RunnerDisplay>,
    floor: u64,
    state: SessionViewState,
    rows: Vec<(RowKey, TranscriptRow)>,
    intents: ProductIntents,
    claims: Vec<MoveIdentityClaim>,
    plan_authority: Option<String>,
    acknowledgements: Vec<super::MoveAgentAcknowledgement>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct QueueEntry {
    id: String,
    revision: u64,
    text: String,
    submitted_at_ms: u64,
    images: Vec<Image>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Image {
    name: String,
    media_type: String,
    bytes: Vec<u8>,
}

impl DormantSessionDocument {
    /// The existing source namespace; not a new plan grant or configuration selection.
    #[must_use]
    pub fn plan_authority(&self) -> Option<&str> {
        self.plan_authority.as_deref()
    }
    /// The product identity at both ends of the move.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
    /// The immutable source checkpoint, for provenance and head reconstruction.
    #[must_use]
    pub const fn source(&self) -> SessionLocator {
        self.locator
    }
    /// The semantic projection cut, not a destination admission fact.
    #[must_use]
    pub const fn floor(&self) -> u64 {
        self.floor
    }

    /// Decodes semantic data and validates it before any native write.
    ///
    /// # Errors
    /// Refuses malformed records, frozen tools, duplicate rows or unsettled work.
    pub fn into_cut(self) -> Result<DormantSession, String> {
        let mut rows = BTreeMap::new();
        for (key, value) in self.rows {
            if rows.insert(key, value).is_some() {
                return Err("duplicate move transcript row".into());
            }
        }
        let views = SessionViews {
            cursor: FeedSeq::new(self.floor),
            state: self.state,
            rows,
        };
        let record = SessionRecord {
            store_revision: 0,
            id: self.id,
            node: self.node,
            create_operation_id: self.create_operation_id,
            workspace_root: self.workspace_root,
            created_at_unix: self.created_at_unix,
            updated_at_unix: self.updated_at_unix,
            account: self.account,
            model: self.model,
            effort: self.effort,
            composition: self.composition,
            target_selection: self.target_selection,
            title: self.title,
            title_override: self.title_override,
            archived: false,
            source_product_session_id: self.source_product_session_id,
            source_workflow_operation_id: self.source_workflow_operation_id,
            plakous_locator: Some(self.locator),
            frozen_tools: self
                .frozen_tools
                .as_deref()
                .map(crate::frozen_tools::FrozenRootTools::decode)
                .transpose()?,
            transcript: ProductTranscript::stored(views.transcript(&self.intents)),
            first_prompt: None,
            prompt_history: self.prompt_history,
            pending_queue: self
                .queue
                .into_iter()
                .map(|item| QueueItem {
                    id: item.id,
                    revision: item.revision,
                    text: item.text,
                    submitted_at_ms: item.submitted_at_ms,
                    images: item
                        .images
                        .into_iter()
                        .map(|image| plakous_host_protocol::DraftImage {
                            name: image.name,
                            media_type: image.media_type,
                            bytes: image.bytes,
                            source_dimensions: None,
                        })
                        .collect::<Vec<_>>()
                        .into(),
                })
                .collect(),
            queue_commands: QueueCommands::default(),
            queue_projection_debt: false,
            queue_drain_state: QueueDrainState::RestoredPaused,
            root_turn_count: views.state.counters.root_turns,
            child_wake_suppressed: false,
            tool_call_count: views.state.counters.tool_calls,
            compaction_count: views.state.counters.compactions,
            carry_intent: None,
            root_profile: self.profile,
            runner: self.runner,
            account_choices: BTreeMap::new(),
        };
        let mut cut =
            DormantSession::prepare(record, views, self.intents, FeedSeq::new(self.floor))?
                .with_identity_claims(self.claims)?
                .with_acknowledgements(self.acknowledgements)?;
        if let Some(authority) = self.plan_authority {
            cut = cut.with_plan_authority(authority)?;
        }
        cut.validate_identity_claims()?;
        Ok(cut)
    }
}

impl DormantSession {
    /// Projects only explicit durable semantic data into the export DTO.
    ///
    /// # Errors
    /// Refuses invalid frozen tool data or an absent runtime checkpoint.
    pub fn document(&self) -> Result<DormantSessionDocument, String> {
        self.validate_identity_claims()?;
        let record = &self.record;
        Ok(DormantSessionDocument {
            id: record.id.clone(),
            node: record.node.clone(),
            create_operation_id: record.create_operation_id.clone(),
            workspace_root: record.workspace_root.clone(),
            created_at_unix: record.created_at_unix,
            updated_at_unix: record.updated_at_unix,
            account: record.account.clone(),
            model: record.model.clone(),
            effort: record.effort.clone(),
            composition: record.composition,
            target_selection: record.target_selection.clone(),
            title: record.title.clone(),
            title_override: record.title_override.clone(),
            source_product_session_id: record.source_product_session_id.clone(),
            source_workflow_operation_id: record.source_workflow_operation_id.clone(),
            locator: record.plakous_locator.ok_or("move record has no runtime")?,
            frozen_tools: record
                .frozen_tools
                .as_ref()
                .map(crate::frozen_tools::FrozenRootTools::encode)
                .transpose()?,
            prompt_history: record.prompt_history.clone(),
            queue: record
                .pending_queue
                .iter()
                .map(|item| QueueEntry {
                    id: item.id.clone(),
                    revision: item.revision,
                    text: item.text.clone(),
                    submitted_at_ms: item.submitted_at_ms,
                    images: item
                        .images
                        .iter()
                        .map(|image| Image {
                            name: image.name.clone(),
                            media_type: image.media_type.clone(),
                            bytes: image.bytes.clone(),
                        })
                        .collect(),
                })
                .collect(),
            profile: record.root_profile.clone(),
            runner: record.runner.clone(),
            floor: self.floor.get(),
            // The last root turn is this build's continue bookkeeping, not
            // part of export v13: a moved session continues after its next
            // failure.
            state: SessionViewState {
                last_root_turn: None,
                ..self.views.state.clone()
            },
            rows: self
                .views
                .rows
                .iter()
                .map(|(key, value)| (*key, value.clone()))
                .collect(),
            intents: self.intents.clone(),
            claims: self.claims.clone(),
            plan_authority: self.plan_authority.clone(),
            acknowledgements: self.acknowledgements.clone(),
        })
    }
}
