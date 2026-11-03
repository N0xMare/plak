//! Durable host intents tree schema 10 defines ahead of their writers:
//! continuation decisions, terminal recovery refusals, VCS stages with their
//! inputs and expected outcomes, and a pending epoch's frozen bytes (spec 03
//! §14.3).
//!
//! Each is one `agent_operations` row of its own kind whose canonical
//! request is the payload below, encoded and decoded only here. The payloads
//! are frozen as complete: they refuse unknown fields, and any later field is
//! a tree schema change like any other. Today's flows write none of them.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::{
    AttemptFailure, AttemptId, CandidateId, FoldAnchor, OperationRecord, OrchestrationSnapshot,
    TopologyError, TopologyErrorCode, canonical_json, digest_text, valid_id,
};

fn invalid(kind: &str) -> TopologyError {
    TopologyError::new(
        TopologyErrorCode::OperationRecoveryRequired,
        format!("{kind} intent is invalid"),
    )
}

/// One intent row of `kind`: the canonical payload and its digest.
fn intent_operation<T: Serialize>(
    kind: &str,
    operation_id: String,
    caller_agent_id: &str,
    payload: &T,
    fold: Option<FoldAnchor>,
) -> Result<OperationRecord, TopologyError> {
    let canonical = canonical_json(payload)?;
    Ok(OperationRecord {
        operation_id,
        caller_agent_id: caller_agent_id.to_owned(),
        tool_name: kind.to_owned(),
        request_digest: digest_text(&canonical),
        canonical_request: canonical,
        fold,
        terminal_code: None,
        terminal_response: None,
    })
}

/// The payload of an intent row of `kind`, or `None` for another kind.
fn intent_payload<T: DeserializeOwned>(
    kind: &str,
    operation: &OperationRecord,
) -> Result<Option<T>, TopologyError> {
    if operation.tool_name != kind {
        return Ok(None);
    }
    if operation.request_digest != digest_text(&operation.canonical_request) {
        return Err(invalid(kind));
    }
    serde_json::from_str(&operation.canonical_request)
        .map(Some)
        .map_err(|_| invalid(kind))
}

/// A runtime command id as an intent names it: non-empty, without control
/// characters.
fn valid_command_id(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

/// Lowercase hex of exactly one of `lengths`.
fn is_hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A git object id: SHA-1 or SHA-256, lowercase hex.
fn is_commit(value: &str) -> bool {
    is_hex(value, &[40, 64])
}

/// A runner identity as an intent names it: non-empty, at most 128 bytes,
/// without whitespace or control characters.
fn valid_runner(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

/// Why a continuation turn may run after an attempt stopped short.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationCause {
    /// A committed transient model failure.
    Transient,
    /// The turn's owner was lost.
    OwnerLost,
    /// A planned handoff to another host.
    Handoff,
}

impl ContinuationCause {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::OwnerLost => "owner_lost",
            Self::Handoff => "handoff",
        }
    }
}

/// What a continuation decision follows: the exact turn that stopped, or,
/// when no turn was admitted, the unfacted command it settles.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ContinuationAnchor {
    /// The runtime turn that failed or was lost.
    Turn {
        /// Its plain turn id.
        turn_id: String,
    },
    /// The runtime command that never reached a turn.
    Command {
        /// Its command id.
        command_id: String,
    },
}

impl ContinuationAnchor {
    fn identity(&self) -> (&'static str, &str) {
        match self {
            Self::Turn { turn_id } => ("turn", turn_id),
            Self::Command { command_id } => ("command", command_id),
        }
    }
}

/// What a continuation decision does with the stopped work (spec §6.3).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ContinuationDecision {
    /// A new host-instruction turn under `command_id`, withdrawing these
    /// unfacted commands first (several unfacted commands become one fresh
    /// continuation).
    NewTurn {
        /// The continuation's runtime command id.
        command_id: String,
        /// Unfacted commands this continuation replaces, possibly none.
        withdrawn: Vec<String>,
    },
    /// The exact resubmission of the one unfacted command of the attempt.
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

impl ContinuationDecision {
    fn is_coherent(&self) -> bool {
        let distinct = |ids: &[String]| {
            let mut sorted = ids.iter().collect::<Vec<_>>();
            sorted.sort();
            sorted.dedup();
            sorted.len() == ids.len() && ids.iter().all(|id| valid_command_id(id))
        };
        match self {
            Self::NewTurn {
                command_id,
                withdrawn,
            } => {
                valid_command_id(command_id)
                    && distinct(withdrawn)
                    && !withdrawn.contains(command_id)
            }
            Self::Resubmit { command_id } => valid_command_id(command_id),
            Self::Withdrawn { command_ids } => !command_ids.is_empty() && distinct(command_ids),
        }
    }
}

/// One durable continuation decision for a node: the root (no attempt) or a
/// child's attempt. Its identity is the attempt, cause, host generation,
/// round and anchor, so decisions in different root turns, attempts or host
/// generations never collide.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuationIntent {
    /// The attempt continued; `None` for the root.
    pub attempt_id: Option<AttemptId>,
    /// The stopped turn or command this decision follows.
    pub anchor: ContinuationAnchor,
    /// The host generation that decided.
    pub host_generation: u64,
    /// Why it continues.
    pub cause: ContinuationCause,
    /// One-based round within this cause, anchor and generation.
    pub round: u32,
    /// Unix milliseconds of the decision.
    pub decided_at_ms: u64,
    /// What it does.
    pub decision: ContinuationDecision,
}

impl ContinuationIntent {
    /// The operation kind.
    pub const KIND: &'static str = "continuation";

    /// The stable identity of this decision on `agent_id`. Every component
    /// before the anchor is colon-free, so the id is unambiguous.
    #[must_use]
    pub fn operation_id(&self, agent_id: &str) -> String {
        let (anchor, id) = self.anchor.identity();
        format!(
            "plak:{}:{agent_id}:{}:{}:{}:{}:{anchor}:{id}",
            Self::KIND,
            self.attempt_id.as_ref().map_or("root", AttemptId::as_str),
            self.cause.as_str(),
            self.host_generation,
            self.round
        )
    }

    fn is_coherent(&self, agent_id: &str) -> bool {
        let (_, anchor) = self.anchor.identity();
        valid_id(agent_id)
            && self.round >= 1
            && self.host_generation >= 1
            && valid_command_id(anchor)
            && self.decision.is_coherent()
    }

    /// The intent row recording this decision for `agent_id`.
    ///
    /// # Errors
    ///
    /// Refuses round or generation 0, an empty anchor, a malformed decision
    /// and a payload that does not encode.
    pub fn to_operation(
        &self,
        agent_id: &str,
        fold: Option<FoldAnchor>,
    ) -> Result<OperationRecord, TopologyError> {
        if !self.is_coherent(agent_id) {
            return Err(invalid(Self::KIND));
        }
        intent_operation(
            Self::KIND,
            self.operation_id(agent_id),
            agent_id,
            self,
            fold,
        )
    }

    /// The decision an intent row carries, or `None` for another kind.
    ///
    /// # Errors
    ///
    /// Refuses a malformed or incoherent row, or one not under its own
    /// identity.
    pub fn from_operation(operation: &OperationRecord) -> Result<Option<Self>, TopologyError> {
        let Some(intent) = intent_payload::<Self>(Self::KIND, operation)? else {
            return Ok(None);
        };
        if !intent.is_coherent(&operation.caller_agent_id)
            || intent.operation_id(&operation.caller_agent_id) != operation.operation_id
        {
            return Err(invalid(Self::KIND));
        }
        Ok(Some(intent))
    }
}

/// A recovery refusal no retry gets past, remembered for the attempt it
/// was observed on so `close_agent` can end the node after a reopen.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalRefusalIntent {
    /// The attempt the refusal was observed on.
    pub attempt_id: AttemptId,
    /// The refusal.
    pub failure: AttemptFailure,
    /// Unix milliseconds it was observed.
    pub observed_at_ms: u64,
}

impl TerminalRefusalIntent {
    /// The operation kind.
    pub const KIND: &'static str = "terminal_refusal";

    /// The stable identity of this refusal on `agent_id`.
    #[must_use]
    pub fn operation_id(&self, agent_id: &str) -> String {
        format!("plak:{}:{agent_id}:{}", Self::KIND, self.attempt_id)
    }

    fn is_coherent(&self, agent_id: &str) -> bool {
        valid_id(agent_id) && !self.failure.kind.is_empty()
    }

    /// The intent row recording this refusal for `agent_id`.
    ///
    /// # Errors
    ///
    /// Refuses an empty failure kind and a payload that does not encode.
    pub fn to_operation(
        &self,
        agent_id: &str,
        fold: Option<FoldAnchor>,
    ) -> Result<OperationRecord, TopologyError> {
        if !self.is_coherent(agent_id) {
            return Err(invalid(Self::KIND));
        }
        intent_operation(
            Self::KIND,
            self.operation_id(agent_id),
            agent_id,
            self,
            fold,
        )
    }

    /// The refusal an intent row carries, or `None` for another kind.
    ///
    /// # Errors
    ///
    /// Refuses a malformed row or one not under its own identity.
    pub fn from_operation(operation: &OperationRecord) -> Result<Option<Self>, TopologyError> {
        let Some(intent) = intent_payload::<Self>(Self::KIND, operation)? else {
            return Ok(None);
        };
        if !intent.is_coherent(&operation.caller_agent_id)
            || intent.operation_id(&operation.caller_agent_id) != operation.operation_id
        {
            return Err(invalid(Self::KIND));
        }
        Ok(Some(intent))
    }
}

/// One VCS stage the tree runs or asks a runner to run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcsStageKind {
    /// Minting a child copy.
    Mint,
    /// Sealing a copy into a candidate pin.
    Seal,
    /// Landing a candidate onto its target.
    Adopt,
    /// Removing a copy.
    Unmint,
    /// Refreshing a copy onto a newer base.
    Refresh,
    /// Pinning the parent's HEAD and mint-time dirt as a snapshot commit.
    BaseSnapshot,
    /// Bundling commits on the source runner for the target runner.
    BundleCreate,
    /// The host relaying a bundle from the source runner to the target.
    Relay,
    /// Applying a bundle on the target runner.
    BundleApply,
}

/// How recovery treats a stage it finds unfinished.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcsStageClass {
    /// Re-run from the intent.
    Reproducible,
    /// Check the expected outcome: done, or re-run.
    Verifiable,
    /// Not provable either way: a `vcs_fault` decision.
    Ambiguous,
}

/// What a verifiable stage leaves behind, recorded before it runs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VcsExpectedOutcome {
    /// The reference the stage writes, under `refs/`.
    pub reference: String,
    /// The commit that reference names once the stage is done.
    pub commit: String,
}

/// Whether a stage field must be present, may be, or must not be.
#[derive(Clone, Copy)]
enum Field {
    Required,
    Optional,
    Absent,
}

impl Field {
    fn admits<T>(self, value: Option<&T>) -> bool {
        match self {
            Self::Required => value.is_some(),
            Self::Optional => true,
            Self::Absent => value.is_none(),
        }
    }
}

/// Which inputs one stage kind takes: candidate, source runner, target
/// runner, base commit, tip commit, bundle digest. Every stage names the
/// node it serves.
struct StageFields {
    candidate: Field,
    source_runner: Field,
    target_runner: Field,
    base: Field,
    tip: Field,
    digest: Field,
}

impl VcsStageKind {
    fn fields(self) -> StageFields {
        use Field::{Absent, Optional, Required};
        let local = |candidate, base| StageFields {
            candidate,
            source_runner: Absent,
            target_runner: Optional,
            base,
            tip: Absent,
            digest: Absent,
        };
        let moving = |digest| StageFields {
            candidate: Optional,
            source_runner: Required,
            target_runner: Required,
            base: Optional,
            tip: Required,
            digest,
        };
        match self {
            Self::Mint => local(Optional, Optional),
            Self::Seal => local(Required, Required),
            Self::Adopt => local(Required, Optional),
            Self::Unmint => local(Absent, Absent),
            Self::Refresh => local(Absent, Required),
            Self::BaseSnapshot => StageFields {
                candidate: Absent,
                source_runner: Required,
                target_runner: Absent,
                base: Required,
                tip: Absent,
                digest: Absent,
            },
            Self::BundleCreate => moving(Absent),
            Self::Relay | Self::BundleApply => moving(Required),
        }
    }

    /// Classes a stage of this kind may be recorded under.
    const fn admits_class(self, class: VcsStageClass) -> bool {
        match self {
            Self::BaseSnapshot | Self::BundleCreate | Self::Relay => {
                matches!(class, VcsStageClass::Reproducible)
            }
            Self::BundleApply => !matches!(class, VcsStageClass::Ambiguous),
            _ => true,
        }
    }
}

/// One VCS stage intent, written before the stage runs. Which inputs a
/// stage carries is fixed by its kind.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VcsStageIntent {
    /// The attempt the stage serves, when it serves one.
    pub attempt_id: Option<AttemptId>,
    /// Which stage.
    pub stage: VcsStageKind,
    /// How recovery treats it unfinished.
    pub class: VcsStageClass,
    /// The node the stage serves: the child whose copy it touches, or the
    /// adopter.
    pub agent_id: String,
    /// The candidate it seals, lands or carries.
    pub candidate_id: Option<CandidateId>,
    /// The runner commits come from; `None` is the local one.
    pub source_runner: Option<String>,
    /// The runner the stage writes on; `None` is the local one.
    pub target_runner: Option<String>,
    /// The base commit: the copy's base, a refresh's new base, or the
    /// ancestor a bundle is cut against.
    pub base_commit: Option<String>,
    /// The commit a bundle carries.
    pub tip_commit: Option<String>,
    /// The SHA-256 of the bundle bytes, lowercase hex.
    pub bundle_digest: Option<String>,
    /// The outcome a verifiable stage checks; required exactly for those.
    pub expected: Option<VcsExpectedOutcome>,
}

impl VcsStageIntent {
    /// The operation kind.
    pub const KIND: &'static str = "vcs_stage";

    fn is_coherent(&self) -> bool {
        let fields = self.stage.fields();
        let runners_distinct = match (&self.source_runner, &self.target_runner) {
            (Some(source), Some(target)) => source != target,
            _ => true,
        };
        let expected_valid = self.expected.as_ref().is_none_or(|expected| {
            expected.reference.starts_with("refs/")
                && expected.reference.len() > "refs/".len()
                && !expected.reference.chars().any(char::is_whitespace)
                && is_commit(&expected.commit)
        });
        valid_id(&self.agent_id)
            && self.stage.admits_class(self.class)
            && (self.class == VcsStageClass::Verifiable) == self.expected.is_some()
            && expected_valid
            && fields.candidate.admits(self.candidate_id.as_ref())
            && fields.source_runner.admits(self.source_runner.as_ref())
            && fields.target_runner.admits(self.target_runner.as_ref())
            && fields.base.admits(self.base_commit.as_ref())
            && fields.tip.admits(self.tip_commit.as_ref())
            && fields.digest.admits(self.bundle_digest.as_ref())
            && runners_distinct
            && [&self.source_runner, &self.target_runner]
                .into_iter()
                .flatten()
                .all(|runner| valid_runner(runner))
            && [&self.base_commit, &self.tip_commit]
                .into_iter()
                .flatten()
                .all(|commit| is_commit(commit))
            && self
                .bundle_digest
                .as_deref()
                .is_none_or(|digest| is_hex(digest, &[64]))
    }

    /// The intent row recording this stage under `operation_id`.
    ///
    /// # Errors
    ///
    /// Refuses a stage whose inputs or class do not fit its kind.
    pub fn to_operation(
        &self,
        operation_id: impl Into<String>,
        caller_agent_id: &str,
        fold: Option<FoldAnchor>,
    ) -> Result<OperationRecord, TopologyError> {
        if !self.is_coherent() {
            return Err(invalid(Self::KIND));
        }
        intent_operation(Self::KIND, operation_id.into(), caller_agent_id, self, fold)
    }

    /// The stage an intent row carries, or `None` for another kind.
    ///
    /// # Errors
    ///
    /// Refuses a malformed or incoherent row.
    pub fn from_operation(operation: &OperationRecord) -> Result<Option<Self>, TopologyError> {
        let intent = intent_payload::<Self>(Self::KIND, operation)?;
        if intent.as_ref().is_some_and(|intent| !intent.is_coherent()) {
            return Err(invalid(Self::KIND));
        }
        Ok(intent)
    }
}

/// A successor epoch's frozen snapshot, recorded byte-exact before the
/// runtime publication that compiles against it, and installed from these
/// bytes afterwards. `workflow_id` is the identity whose published
/// settlement in the root feed validates it; without one it is withdrawn.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpochPending {
    /// The epoch number the snapshot installs as, from 1.
    pub epoch_no: u64,
    /// The workflow whose publication settles it.
    pub workflow_id: String,
    /// The snapshot's exact JSON bytes.
    pub snapshot_json: String,
    /// The snapshot's digest.
    pub digest: String,
}

impl EpochPending {
    /// The operation kind.
    pub const KIND: &'static str = "epoch_pending";

    /// The pending bytes of `snapshot` as epoch `epoch_no`, settled by
    /// `workflow_id`.
    ///
    /// # Errors
    ///
    /// Refuses a snapshot whose digest does not match its contents.
    pub fn of(
        epoch_no: u64,
        workflow_id: impl Into<String>,
        snapshot: &OrchestrationSnapshot,
    ) -> Result<Self, TopologyError> {
        let pending = Self {
            epoch_no,
            workflow_id: workflow_id.into(),
            snapshot_json: canonical_json(snapshot)?,
            digest: snapshot.digest.clone(),
        };
        pending.snapshot()?;
        Ok(pending)
    }

    /// The snapshot these bytes freeze.
    ///
    /// # Errors
    ///
    /// Refuses epoch 0, an empty workflow id, and bytes that do not decode or
    /// whose digest is not `digest`.
    pub fn snapshot(&self) -> Result<OrchestrationSnapshot, TopologyError> {
        if self.epoch_no == 0 || !valid_command_id(&self.workflow_id) {
            return Err(invalid(Self::KIND));
        }
        let snapshot: OrchestrationSnapshot =
            serde_json::from_str(&self.snapshot_json).map_err(|_| invalid(Self::KIND))?;
        if snapshot.digest != self.digest || !snapshot.digest_matches() {
            return Err(invalid(Self::KIND));
        }
        Ok(snapshot)
    }

    /// The intent row recording these bytes under `operation_id`.
    ///
    /// # Errors
    ///
    /// Refuses bytes that do not freeze a coherent snapshot.
    pub fn to_operation(
        &self,
        operation_id: impl Into<String>,
        agent_id: &str,
        fold: Option<FoldAnchor>,
    ) -> Result<OperationRecord, TopologyError> {
        self.snapshot()?;
        intent_operation(Self::KIND, operation_id.into(), agent_id, self, fold)
    }

    /// The pending bytes an intent row carries, or `None` for another kind.
    ///
    /// # Errors
    ///
    /// Refuses a malformed row or bytes that do not freeze a coherent
    /// snapshot.
    pub fn from_operation(operation: &OperationRecord) -> Result<Option<Self>, TopologyError> {
        let pending = intent_payload::<Self>(Self::KIND, operation)?;
        if let Some(pending) = &pending {
            pending.snapshot()?;
        }
        Ok(pending)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
