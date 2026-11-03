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
            SessionWorkflowKind::Compact(_) => {
                "compaction did not start; request compaction again explicitly"
            }
            SessionWorkflowKind::Fork => "fork did not start; request a fork again explicitly",
            SessionWorkflowKind::Rebind(_) => "model change did not start; choose the model again",
            SessionWorkflowKind::Switch(_) => "profile switch did not start; choose it again",
            SessionWorkflowKind::New => {
                "new session did not start; request a new session again explicitly"
            }
        }
    }

    /// The semantic repair for a definitely unstarted workflow.
    #[must_use]
    pub const fn withdrawal_remedy(&self) -> plakous_host_protocol::notice_remedy::NoticeRemedy {
        use plakous_host_protocol::notice_remedy::NoticeRemedy;
        match self.kind {
            SessionWorkflowKind::Compact(_) => NoticeRemedy::RetryCompaction {},
            SessionWorkflowKind::Fork => NoticeRemedy::RetryFork {},
            SessionWorkflowKind::Rebind(_) => NoticeRemedy::Rebind {},
            SessionWorkflowKind::Switch(_) => NoticeRemedy::ChooseProfile {},
            SessionWorkflowKind::New => NoticeRemedy::StartFresh {},
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

/// What a root interrupt cause or continuation decision follows: the exact
/// root turn that stopped, or, when no turn was admitted, the unfacted
/// command it settles.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RootStopAnchor {
    /// The root turn that stopped.
    Turn {
        /// Its plain turn id.
        turn_id: String,
    },
    /// The command that never reached a turn.
    Command {
        /// Its command id.
        command_id: String,
    },
}

impl RootStopAnchor {
    fn is_coherent(&self) -> bool {
        match self {
            Self::Turn { turn_id: id } | Self::Command { command_id: id } => valid_command_id(id),
        }
    }
}

/// A runtime command or turn id as an intent names it: non-empty, without
/// control characters.
fn valid_command_id(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

/// Why a virtual tree's root stopped short (spec 02 §2.6): the root of a tree
/// with rows records its cause in the tree store instead.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootInterruptCause {
    /// The operator interrupted the root.
    Operator,
    /// The turn's owner was lost (a crash, a fence, a lease loss).
    OwnerLost,
    /// A planned handoff released the root to another host.
    Handoff,
    /// An explicit stop.
    Stopped,
}

/// A root interrupt cause intent. Decoded today; the recovery switch writes
/// it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RootInterrupt {
    /// The turn or command it stopped.
    pub anchor: RootStopAnchor,
    /// Why.
    pub cause: RootInterruptCause,
}

/// Why a root continuation turn may run after the root stopped short.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootContinuationCause {
    /// A committed transient model failure.
    Transient,
    /// The turn's owner was lost.
    OwnerLost,
    /// A planned handoff to another host.
    Handoff,
}

/// What a root continuation decision does with the stopped work.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RootContinuationDecision {
    /// A new host-instruction turn under `command_id`, withdrawing these
    /// unfacted commands first.
    NewTurn {
        /// The continuation's runtime command id.
        command_id: String,
        /// Unfacted commands it replaces, possibly none.
        withdrawn: Vec<String>,
    },
    /// The exact resubmission of the one unfacted command.
    Resubmit {
        /// The resubmitted command's id.
        command_id: String,
    },
    /// No continuation: these unfacted commands are withdrawn.
    Withdrawn {
        /// The withdrawn command ids, at least one.
        command_ids: Vec<String>,
    },
}

/// One durable continuation decision for a virtual tree's root: one per
/// cause, anchor, host generation and round. Decoded today; the recovery
/// switch writes it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RootContinuation {
    /// The stopped turn or command this decision follows.
    pub anchor: RootStopAnchor,
    /// The host generation that decided, from 1.
    pub host_generation: u64,
    /// Why it continues.
    pub cause: RootContinuationCause,
    /// One-based round within this cause, anchor and generation.
    pub round: u32,
    /// Unix milliseconds of the decision.
    pub decided_at_ms: u64,
    /// What it does.
    pub decision: RootContinuationDecision,
}

impl RootInterrupt {
    /// The stored encoding.
    ///
    /// # Errors
    /// Refuses an empty anchor.
    pub fn encode(&self) -> Result<String, String> {
        if !self.anchor.is_coherent() {
            return Err("root interrupt intent is invalid".to_owned());
        }
        serde_json::to_string(self).map_err(|_| "root interrupt intent is invalid".to_owned())
    }

    /// Decodes [`Self::encode`]'s JSON.
    ///
    /// # Errors
    /// Refuses anything this build did not write.
    pub fn decode(text: &str) -> Result<Self, String> {
        let intent: Self = serde_json::from_str(text)
            .map_err(|_| "root interrupt intent is invalid".to_owned())?;
        intent.encode()?;
        Ok(intent)
    }
}

impl RootContinuation {
    fn is_coherent(&self) -> bool {
        let distinct = |ids: &[String]| {
            let mut sorted = ids.iter().collect::<Vec<_>>();
            sorted.sort();
            sorted.dedup();
            sorted.len() == ids.len() && ids.iter().all(|id| valid_command_id(id))
        };
        let decision = match &self.decision {
            RootContinuationDecision::NewTurn {
                command_id,
                withdrawn,
            } => {
                valid_command_id(command_id)
                    && distinct(withdrawn)
                    && !withdrawn.contains(command_id)
            }
            RootContinuationDecision::Resubmit { command_id } => valid_command_id(command_id),
            RootContinuationDecision::Withdrawn { command_ids } => {
                !command_ids.is_empty() && distinct(command_ids)
            }
        };
        decision && self.round >= 1 && self.host_generation >= 1 && self.anchor.is_coherent()
    }

    /// The stored encoding.
    ///
    /// # Errors
    /// Refuses round or generation 0, an empty anchor and a malformed
    /// decision.
    pub fn encode(&self) -> Result<String, String> {
        if !self.is_coherent() {
            return Err("root continuation intent is invalid".to_owned());
        }
        serde_json::to_string(self).map_err(|_| "root continuation intent is invalid".to_owned())
    }

    /// Decodes [`Self::encode`]'s JSON.
    ///
    /// # Errors
    /// Refuses anything this build did not write.
    pub fn decode(text: &str) -> Result<Self, String> {
        let intent: Self = serde_json::from_str(text)
            .map_err(|_| "root continuation intent is invalid".to_owned())?;
        intent.encode()?;
        Ok(intent)
    }
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
    /// A virtual tree's root interrupt causes; no reader until recovery
    /// switches.
    pub root_interrupts: Vec<AnchoredIntent<RootInterrupt>>,
    /// A virtual tree's root continuation decisions; no reader until
    /// recovery switches.
    pub root_continuations: Vec<AnchoredIntent<RootContinuation>>,
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
