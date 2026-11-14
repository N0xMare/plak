//! Unsupported host composition seams; these values are never a cross-build codec.
#![allow(
    missing_docs,
    reason = "hidden host-only semantic reconstruction vocabulary"
)]

use super::{
    BranchLease, BranchRevision, SessionHead, SessionRef, SessionStoreError, SessionWorkflowId,
};
pub use crate::manifest::{
    ImportedCompactionFacts, ImportedCompactionSelection, ProcessSessionId,
    ProcessSessionOwnerGeneration,
};
use crate::session::LineRecord;
use crate::{RuntimeCheckpoint, RuntimeCheckpointKind};
use plakous_model::{CheckpointId, Digest, DigestDomain, LineId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveDisposition {
    Commit,
    Abort,
    Publish,
    Abandon,
}

/// Host-only guarded withdrawal of an installed, never-executed destination batch.
/// The coordinator must hold its never-Ready receipt lock across this call.
#[derive(Clone, Debug)]
pub struct AbandonUnopenedMove {
    pub operation: SessionWorkflowId,
    pub digest: Digest,
    pub leases: Vec<BranchLease>,
}

#[derive(Clone, Debug)]
pub struct MoveBranch {
    pub session: SessionRef,
    pub revision: BranchRevision,
    pub checkpoint: CheckpointId,
    pub fence_floor: u64,
}

#[derive(Clone, Debug)]
pub struct MoveSession {
    pub session: SessionHead,
    pub feed_tail_floor: u64,
    pub branches: Vec<MoveBranch>,
    pub checkpoints: Vec<RuntimeCheckpoint>,
}

#[derive(Clone, Debug)]
pub struct MoveLine {
    pub line: LineRecord,
    pub terminals: Vec<crate::manifest::LineJobTerminal>,
}

/// Source-only inspection data. This is not an installation or retirement grant.
#[derive(Clone, Debug)]
pub struct MoveSource {
    pub sessions: Vec<MoveSession>,
    pub lines: Vec<MoveLine>,
}

impl MoveSource {
    pub fn settled(self) -> Result<MoveInstall, SessionStoreError> {
        MoveInstall::new(self.sessions, self.lines)
    }
}

/// Target-owned compilation inputs, not a source checkpoint or execution grant.
#[derive(Clone, Debug)]
pub struct MoveHeadSeed {
    pub operation: SessionWorkflowId,
    pub source: SessionRef,
    pub source_checkpoint: CheckpointId,
    pub source_fork: Option<crate::manifest::ForkLineage>,
    pub source_revision: BranchRevision,
    pub fence_floor: u64,
    pub line: LineId,
    pub generation: u64,
    pub history: plakous_model::CanonicalHistory,
    pub named_state: crate::manifest::NamedState,
    pub jobs: crate::manifest::JobLedger,
    pub compaction: Option<ImportedCompactionFacts>,
    pub interpreter: Option<MoveInterpreter>,
}

/// Reopen floor and affected cell, never a live interpreter or source to replay.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveInterpreter {
    pub generation: u64,
    pub affected_cell: Option<String>,
}

impl MoveInterpreter {
    pub(crate) fn observation(
        &self,
    ) -> Result<crate::manifest::InterpreterObservation, crate::manifest::RuntimeManifestError>
    {
        crate::manifest::InterpreterObservation::new(
            self.generation,
            crate::manifest::InterpreterAvailability::Reset,
            Some(crate::manifest::InterpreterResetReason::ProcessReopen),
            self.affected_cell.clone(),
            None,
        )
    }
}

#[derive(Clone, Debug)]
pub struct ReconstructedMoveHead {
    pub branch: MoveBranch,
    pub checkpoint: RuntimeCheckpoint,
}

/// An opaque, checked installation batch reconstructed from semantic values.
/// No source lease, runtime object address or serialized checkpoint is accepted.
#[derive(Clone, Debug)]
pub struct MoveInstall {
    sessions: Vec<MoveSession>,
    lines: Vec<MoveLine>,
    digest: Digest,
}

impl MoveInstall {
    pub fn new(
        mut sessions: Vec<MoveSession>,
        mut lines: Vec<MoveLine>,
    ) -> Result<Self, SessionStoreError> {
        if sessions.is_empty() || sessions.len() > 1024 || lines.is_empty() || lines.len() > 16384 {
            return Err(SessionStoreError::IdentityConflict);
        }
        sessions.sort_by_key(|s| s.session.session_id());
        lines.sort_by_key(|l| l.line.line());
        let mut branches = BTreeMap::new();
        let mut checkpoints = BTreeMap::new();
        let mut session_ids = BTreeSet::new();
        let mut bytes = Vec::new();
        fingerprint_part(&mut bytes, &(sessions.len() as u64).to_be_bytes());
        fingerprint_part(&mut bytes, &(lines.len() as u64).to_be_bytes());
        for session in &mut sessions {
            if !session_ids.insert(session.session.session_id())
                || session.feed_tail_floor == u64::MAX
                || session.session.is_archived()
                || session.branches.is_empty()
                || session.branches.len() > 1024
                || session.checkpoints.len() > 131_072
                || !session
                    .branches
                    .iter()
                    .any(|b| b.session.branch_id() == session.session.initial_branch())
            {
                return Err(SessionStoreError::IdentityConflict);
            }
            session.branches.sort_by_key(|b| b.session);
            session.checkpoints.sort_by_key(RuntimeCheckpoint::id);
            session.fingerprint_into(&mut bytes);
            for checkpoint in &session.checkpoints {
                if checkpoint.manifest().topology().session_id != session.session.session_id()
                    || checkpoints.insert(checkpoint.id(), checkpoint).is_some()
                {
                    return Err(SessionStoreError::IdentityConflict);
                }
                fingerprint_part(&mut bytes, checkpoint.integrity().bytes());
            }
            for branch in &session.branches {
                let checkpoint = checkpoints
                    .get(&branch.checkpoint)
                    .ok_or(SessionStoreError::IdentityConflict)?;
                let topology = checkpoint.manifest().topology();
                if branch.session.session_id() != session.session.session_id()
                    || topology.branch_id != branch.session.branch_id()
                    || topology.branch_revision != branch.revision.get()
                    || topology.lease_fence > branch.fence_floor
                    || branch.fence_floor == u64::MAX
                    || checkpoint.kind() != RuntimeCheckpointKind::Quiescent
                    || !jobs_settled(checkpoint, &lines)?
                    || branches
                        .insert(branch.session, (topology.line, topology.line_generation))
                        .is_some()
                {
                    return Err(SessionStoreError::IdentityConflict);
                }
                branch.fingerprint_into(&mut bytes);
            }
        }
        let mut line_ids = BTreeMap::new();
        for line in &mut lines {
            if line_ids
                .insert(line.line.line(), line.line.generation())
                .is_some()
                || line.line.generation() == u64::MAX
                || branches.get(&line.line.head())
                    != Some(&(line.line.line(), line.line.generation()))
            {
                return Err(SessionStoreError::IdentityConflict);
            }
            line.fingerprint_into(&mut bytes)?;
        }
        if branches
            .values()
            .any(|(line, generation)| line_ids.get(line).is_none_or(|head| generation > head))
        {
            return Err(SessionStoreError::IdentityConflict);
        }
        let digest = Digest::sha256(DigestDomain::Blob, 1, &[b"host-move/semantic/v1", &bytes])
            .map_err(|_| SessionStoreError::corrupt())?;
        Ok(Self {
            sessions,
            lines,
            digest,
        })
    }
    #[must_use]
    pub fn sessions(&self) -> &[MoveSession] {
        &self.sessions
    }
    #[must_use]
    pub fn lines(&self) -> &[MoveLine] {
        &self.lines
    }
    #[must_use]
    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

fn jobs_settled(
    checkpoint: &RuntimeCheckpoint,
    lines: &[MoveLine],
) -> Result<bool, SessionStoreError> {
    let line = checkpoint.manifest().topology().line;
    let index = lines
        .binary_search_by_key(&line, |entry| entry.line.line())
        .map_err(|_| SessionStoreError::IdentityConflict)?;
    let terminals = &lines[index].terminals;
    Ok(checkpoint
        .manifest()
        .jobs()
        .with_line_terminals(terminals)
        .map_err(|_| SessionStoreError::IdentityConflict)?
        .rows()
        .iter()
        .all(|row| row.terminal.is_some()))
}

fn fingerprint_part(bytes: &mut Vec<u8>, value: &[u8]) {
    bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(value);
}

impl MoveSession {
    fn fingerprint_into(&self, bytes: &mut Vec<u8>) {
        fingerprint_part(bytes, self.session.session_id().to_string().as_bytes());
        fingerprint_part(bytes, &self.feed_tail_floor.to_be_bytes());
        fingerprint_part(bytes, self.session.initial_branch().to_string().as_bytes());
        fingerprint_part(bytes, &self.session.revision().get().to_be_bytes());
        fingerprint_part(bytes, &self.session.created_at().seconds().to_be_bytes());
        fingerprint_part(
            bytes,
            &self.session.created_at().nanoseconds().to_be_bytes(),
        );
        fingerprint_part(
            bytes,
            &[match self.session.durability() {
                crate::RuntimeDurability::Disabled => 0,
                crate::RuntimeDurability::Required => 1,
                crate::RuntimeDurability::BestEffort => 2,
            }],
        );
        fingerprint_part(bytes, &(self.branches.len() as u64).to_be_bytes());
        fingerprint_part(bytes, &(self.checkpoints.len() as u64).to_be_bytes());
    }
}

impl MoveBranch {
    fn fingerprint_into(&self, bytes: &mut Vec<u8>) {
        fingerprint_part(bytes, self.session.session_id().to_string().as_bytes());
        fingerprint_part(bytes, self.session.branch_id().to_string().as_bytes());
        fingerprint_part(bytes, self.checkpoint.to_string().as_bytes());
        fingerprint_part(bytes, &self.revision.get().to_be_bytes());
        fingerprint_part(bytes, &self.fence_floor.to_be_bytes());
    }
}

impl MoveLine {
    fn fingerprint_into(&mut self, bytes: &mut Vec<u8>) -> Result<(), SessionStoreError> {
        fingerprint_part(bytes, self.line.line().to_string().as_bytes());
        fingerprint_part(bytes, self.line.head().session_id().to_string().as_bytes());
        fingerprint_part(bytes, self.line.head().branch_id().to_string().as_bytes());
        fingerprint_part(bytes, &self.line.generation().to_be_bytes());
        fingerprint_part(
            bytes,
            self.line.origin().encode().unwrap_or_default().as_bytes(),
        );
        self.terminals.sort_by_key(|t| t.number);
        if self
            .terminals
            .windows(2)
            .any(|t| t[0].number == t[1].number)
        {
            return Err(SessionStoreError::IdentityConflict);
        }
        fingerprint_part(bytes, &(self.terminals.len() as u64).to_be_bytes());
        for terminal in &self.terminals {
            fingerprint_part(
                bytes,
                &terminal
                    .encode()
                    .map_err(|_| SessionStoreError::corrupt())?,
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct PrepareMove {
    pub operation: SessionWorkflowId,
    pub leases: Vec<BranchLease>,
}

/// Persisted states, independent of host/product visibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveState {
    Prepared = 0,
    Retired = 1,
    Staged = 2,
    Published = 3,
    Abandoned = 4,
    Aborted = 5,
}

impl MoveState {
    pub fn transition(self, disposition: MoveDisposition) -> Result<Self, SessionStoreError> {
        use MoveDisposition::{Abandon, Abort, Commit, Publish};
        match (self, disposition) {
            (Self::Prepared | Self::Retired, Commit) => Ok(Self::Retired),
            (Self::Prepared | Self::Aborted, Abort) => Ok(Self::Aborted),
            (Self::Staged | Self::Published, Publish) => Ok(Self::Published),
            (Self::Staged | Self::Abandoned, Abandon) => Ok(Self::Abandoned),
            _ => Err(SessionStoreError::Conflict),
        }
    }
    #[must_use]
    pub const fn visible(self) -> bool {
        matches!(self, Self::Published | Self::Aborted)
    }
    pub fn decode(value: i64) -> Result<Self, SessionStoreError> {
        match value {
            0 => Ok(Self::Prepared),
            1 => Ok(Self::Retired),
            2 => Ok(Self::Staged),
            3 => Ok(Self::Published),
            4 => Ok(Self::Abandoned),
            5 => Ok(Self::Aborted),
            _ => Err(SessionStoreError::corrupt()),
        }
    }
}

#[must_use]
pub fn blocked() -> SessionStoreError {
    SessionStoreError::NotLineHead
}

#[must_use]
pub fn blocked_state(state: MoveState) -> SessionStoreError {
    if state == MoveState::Retired {
        SessionStoreError::OwnershipMoved
    } else {
        SessionStoreError::NotLineHead
    }
}
pub fn line_ids(install: &MoveInstall) -> impl Iterator<Item = LineId> + '_ {
    install.lines().iter().map(|l| l.line.line())
}
