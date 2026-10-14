//! Product intents (spec 02 §2.6): what the operator or the product asked
//! for, written before submission, keyed by an identity Plakous already
//! deduplicates on or by a product-local identity, and never moved forward by
//! a runtime outcome.

use plakous::sessions::SessionLocator;
use serde::{Deserialize, Serialize};

use super::views::{Lane, RowKey};
use crate::sessions::{ProductCompactReason, SessionWorkflowKind, TranscriptRow};

/// Which command a submission carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmissionKind {
    /// A queued item started a root turn.
    RootStart,
    /// A queued item was sent now as a steer into a running turn.
    SendNowSteer,
    /// The host woke the root for settled children or processes.
    HostWake,
}

impl SubmissionKind {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RootStart => "root_start",
            Self::SendNowSteer => "send_now_steer",
            Self::HostWake => "host_wake",
        }
    }

    /// Parses the stored spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "root_start" => Some(Self::RootStart),
            "send_now_steer" => Some(Self::SendNowSteer),
            "host_wake" => Some(Self::HostWake),
            _ => None,
        }
    }
}

/// One command submission: its exact bytes and identity, immutable once
/// written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Submission {
    /// The command id Plakous deduplicates on.
    pub command_id: String,
    /// What the command is.
    pub kind: SubmissionKind,
    /// The queue item it delivers.
    pub queue_item_id: String,
    /// The item revision it delivers.
    pub queue_item_revision: u64,
    /// The turn a steer is bound to.
    pub target_turn_id: Option<String>,
    /// The exact canonical command bytes.
    pub canonical_command: String,
    /// When the host submitted it, Unix milliseconds: the user row's send
    /// time (§2.6 row provenance, source c).
    pub submitted_at_ms: u64,
}

impl Submission {
    /// The permanent identity claim, independent of legacy delivery receipts.
    #[must_use]
    pub fn binding_identity(&self) -> String {
        format!("plak:submission:{}", self.command_id)
    }

    /// Binds every immutable field, including the host's send time.
    #[must_use]
    pub fn binding_digest(&self, session_id: &str) -> String {
        crate::sessions::digest_parts(&[
            session_id,
            &self.command_id,
            self.kind.as_str(),
            &self.queue_item_id,
            &self.queue_item_revision.to_string(),
            self.target_turn_id.as_deref().unwrap_or(""),
            &self.canonical_command,
            &self.submitted_at_ms.to_string(),
        ])
    }
}

/// One workflow request, written before submission and joined to its facts by
/// its operation id, never by the derived workflow identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowRequest {
    /// The workflow operation id.
    pub operation_id: String,
    /// What was asked for.
    pub kind: SessionWorkflowKind,
    /// The source checkpoint it was asked against.
    pub source_locator: SessionLocator,
}

impl WorkflowRequest {
    /// The successor's stable product identity, derived from this intent.
    #[must_use]
    pub fn successor_product_id(&self) -> String {
        let digest = crate::sessions::digest_parts(&["plak:successor", &self.operation_id]);
        format!(
            "{}-{}-{}-{}-{}",
            &digest[..8],
            &digest[8..12],
            &digest[12..16],
            &digest[16..20],
            &digest[20..32]
        )
    }

    /// The permanent content binding retained after a request is withdrawn.
    #[must_use]
    pub fn binding_digest(&self, session_id: &str) -> String {
        let (reason, selection) = crate::sessions::encode_workflow_kind(&self.kind);
        crate::sessions::digest_parts(&[
            session_id,
            &self.source_locator.to_string(),
            "",
            self.kind.as_str(),
            reason.unwrap_or(""),
            selection.as_deref().unwrap_or(""),
        ])
    }

    /// The one notice for a request withdrawn without an admission fact.
    #[must_use]
    pub const fn withdrawal_note(&self) -> &'static str {
        match self.kind {
            SessionWorkflowKind::Compact(_) => "compaction did not start; run /compact again",
            SessionWorkflowKind::Fork => "fork did not start; run /fork again",
            SessionWorkflowKind::Rebind(_) => "model change did not start; choose the model again",
            SessionWorkflowKind::Clear => "clear did not start; run /clear again",
        }
    }

    /// The compaction reason, for a compact request.
    #[must_use]
    pub const fn compact_reason(&self) -> Option<ProductCompactReason> {
        match self.kind {
            SessionWorkflowKind::Compact(reason) => Some(reason),
            _ => None,
        }
    }
}

/// A host-authored transcript row no feed fact implies, in lane 1 at the
/// cursor the authoring host had applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperatorNotice {
    /// The product-local notice identity.
    pub notice_id: String,
    /// The cursor the host had applied when it wrote the notice.
    pub anchor: u64,
    /// The product-local counter within that anchor.
    pub ordinal: u32,
    /// The row.
    pub row: TranscriptRow,
}

impl OperatorNotice {
    /// The notice's immutable ordering key.
    #[must_use]
    pub const fn key(&self) -> RowKey {
        RowKey::new(self.anchor, Lane::Product, self.ordinal)
    }
}

/// An anchored host-written intent: child-wake suppression or a pause cause
/// no fact carries.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AnchoredIntent<T> {
    /// The cursor the host had applied when it wrote the intent.
    pub anchor: u64,
    /// The product-local counter within that anchor.
    pub ordinal: u32,
    /// The intent.
    pub value: T,
}

/// A pause cause the host writes because no fact carries it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum HostPauseCause {
    /// A session-wide refusal the runtime did not record.
    UnrecordedRefusal {
        /// The refusal's operator text.
        reason: String,
    },
    /// A wake failed its preflight.
    WakePreflightFailed {
        /// The failure's operator text.
        reason: String,
    },
}

/// Every intent of one product session the projector reads.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProductIntents {
    /// Command submissions.
    pub submissions: Vec<Submission>,
    /// The applied cursor at the first durable write of each submission.
    pub submission_anchors: std::collections::BTreeMap<String, u64>,
    /// Workflow requests.
    pub workflow_requests: Vec<WorkflowRequest>,
    /// The applied cursor at the first durable write of each workflow request.
    pub workflow_anchors: std::collections::BTreeMap<String, u64>,
    /// Operator notices (lane 1).
    pub notices: Vec<OperatorNotice>,
    /// Child-wake suppression intents.
    pub wake_suppressions: Vec<AnchoredIntent<()>>,
    /// Pause causes no fact carries.
    pub pause_causes: Vec<AnchoredIntent<HostPauseCause>>,
    /// An imported session's transcript, anchor 0, lane 1, in document order.
    pub imported: Vec<TranscriptRow>,
}

impl ProductIntents {
    /// Whether retention consumed a submission whose facts are no longer
    /// available. This does not imply an acceptance or derive a transcript row.
    #[must_use]
    pub fn submission_consumed(&self, command_id: &str, retained_from: Option<u64>) -> bool {
        retained_from.is_some_and(|floor| {
            self.submission_anchors
                .get(command_id)
                .is_some_and(|anchor| *anchor < floor)
        })
    }

    /// Whether retention consumed a historical workflow request, including
    /// its clear reservation, without authorizing a withdrawal notice.
    #[must_use]
    pub fn workflow_consumed(&self, operation_id: &str, retained_from: Option<u64>) -> bool {
        retained_from.is_some_and(|floor| {
            self.workflow_anchors
                .get(operation_id)
                .is_some_and(|anchor| *anchor < floor)
        })
    }

    /// The submission a command id names.
    #[must_use]
    pub fn submission(&self, command_id: &str) -> Option<&Submission> {
        self.submissions
            .iter()
            .find(|submission| submission.command_id == command_id)
    }

    /// The workflow request an operation id names.
    #[must_use]
    pub fn workflow_request(&self, operation_id: &str) -> Option<&WorkflowRequest> {
        self.workflow_requests
            .iter()
            .find(|request| request.operation_id == operation_id)
    }
}
