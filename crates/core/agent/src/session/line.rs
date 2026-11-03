//! Agent lines: the durable single-agent lineage whose head branch owns the
//! line's processes (spec 01 §5.1, §5.7).
//!
//! Every branch belongs to exactly one line, named in its checkpoint
//! topology. A line starts with the branch that creates it (a genesis or an
//! ordinary fork; its id is that branch's id) at generation 1. A rebind or a
//! same-store carry moves the head to its target and adds one to the
//! generation in the same transaction that publishes the target. A commit on
//! any branch that is not its line's head at its own generation is refused
//! with [`super::SessionStoreError::NotLineHead`].

use super::{BranchLease, CheckpointId, FeedSeq, SessionRef};
use crate::manifest::LineJobTerminal;
use plakous_model::{BranchId, LineId, SessionId};

/// Longest accepted provenance text of an imported line.
pub const MAX_LINE_PROVENANCE_BYTES: usize = 256;

/// Where a line came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LineOrigin {
    /// A new session that continues no other line.
    Created,
    /// An ordinary or recompiling fork of another line's branch.
    Forked {
        /// The source branch's line.
        line: LineId,
        /// The source branch.
        source: SessionRef,
        /// The exact source checkpoint.
        checkpoint: CheckpointId,
    },
    /// A history carried in from another store (an import); the text is
    /// provenance only and names nothing this store can resolve.
    Imported {
        /// Bounded, single-line provenance text.
        provenance: String,
    },
}

impl LineOrigin {
    /// Validates imported provenance text: 1 to 256 bytes, no control
    /// characters.
    #[must_use]
    pub fn imported(provenance: impl Into<String>) -> Option<Self> {
        let provenance = provenance.into();
        (!provenance.is_empty()
            && provenance.len() <= MAX_LINE_PROVENANCE_BYTES
            && !provenance.chars().any(char::is_control))
        .then_some(Self::Imported { provenance })
    }

    /// The canonical stored text, absent for [`Self::Created`].
    #[must_use]
    pub fn encode(&self) -> Option<String> {
        match self {
            Self::Created => None,
            Self::Forked {
                line,
                source,
                checkpoint,
            } => Some(format!(
                "fork:{line}:{}:{}:{checkpoint}",
                source.session_id(),
                source.branch_id()
            )),
            Self::Imported { provenance } => Some(format!("import:{provenance}")),
        }
    }

    /// Decodes canonical stored text.
    #[must_use]
    pub fn decode(text: Option<&str>) -> Option<Self> {
        let Some(text) = text else {
            return Some(Self::Created);
        };
        if let Some(provenance) = text.strip_prefix("import:") {
            return Self::imported(provenance);
        }
        let mut parts = text.strip_prefix("fork:")?.split(':');
        let line = parts.next()?.parse::<LineId>().ok()?;
        let session = parts.next()?.parse::<SessionId>().ok()?;
        let branch = parts.next()?.parse::<BranchId>().ok()?;
        let checkpoint = parts.next()?.parse::<CheckpointId>().ok()?;
        if parts.next().is_some() {
            return None;
        }
        let origin = Self::Forked {
            line,
            source: SessionRef::new(session, branch),
            checkpoint,
        };
        (origin.encode().as_deref() == Some(text)).then_some(origin)
    }
}

/// One line's current head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineRecord {
    line: LineId,
    head: SessionRef,
    generation: u64,
    origin: LineOrigin,
}

impl LineRecord {
    /// Creates a backend-returned line record.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(
        line: LineId,
        head: SessionRef,
        generation: u64,
        origin: LineOrigin,
    ) -> Self {
        Self {
            line,
            head,
            generation,
            origin,
        }
    }

    /// The line.
    #[must_use]
    pub const fn line(&self) -> LineId {
        self.line
    }

    /// The head branch: the only branch of the line that can commit.
    #[must_use]
    pub const fn head(&self) -> SessionRef {
        self.head
    }

    /// The head's generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Where the line came from.
    #[must_use]
    pub const fn origin(&self) -> &LineOrigin {
        &self.origin
    }
}

/// The line a new branch's checkpoint names, as its publication applies it.
///
/// The store derives this from the target checkpoint's topology and the
/// source, so it is part of the publication's exact identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineStep {
    /// The branch starts a new line at generation 1.
    NewLine,
    /// The branch becomes the next head of an existing line: generation
    /// `from + 1`, provided the line is still at generation `from`.
    Advance {
        /// The line.
        line: LineId,
        /// The generation the head must still have.
        from: u64,
    },
}

impl LineStep {
    /// Classifies a new branch's topology. `None` when it names neither a
    /// line of its own at generation 1 nor a successor generation.
    #[must_use]
    pub fn of(branch: BranchId, line: LineId, generation: u64) -> Option<Self> {
        match generation {
            0 => None,
            1 => (line.as_uuid() == branch.as_uuid()).then_some(Self::NewLine),
            next => Some(Self::Advance {
                line,
                from: next - 1,
            }),
        }
    }
}

/// Terminal evidence recorded into `line_job_terminals` under a lease-only
/// open of a line's head.
#[derive(Clone, Debug)]
pub struct RecordLineJobTerminals {
    lease: BranchLease,
    line: LineId,
    generation: u64,
    head: CheckpointId,
    terminals: Vec<LineJobTerminal>,
}

impl RecordLineJobTerminals {
    /// Creates a fenced line-terminal write under the head's exact lease.
    #[must_use]
    pub const fn new(
        lease: BranchLease,
        line: LineId,
        generation: u64,
        head: CheckpointId,
        terminals: Vec<LineJobTerminal>,
    ) -> Self {
        Self {
            lease,
            line,
            generation,
            head,
            terminals,
        }
    }

    /// The head's exact lease.
    #[must_use]
    pub const fn lease(&self) -> &BranchLease {
        &self.lease
    }

    /// The line.
    #[must_use]
    pub const fn line(&self) -> LineId {
        self.line
    }

    /// The head generation the write is fenced to.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The head checkpoint the evidence was observed against.
    #[must_use]
    pub const fn head(&self) -> CheckpointId {
        self.head
    }

    /// The evidence, one entry per job.
    #[must_use]
    pub fn terminals(&self) -> &[LineJobTerminal] {
        &self.terminals
    }
}

/// What a line-terminal write recorded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineTerminalsReceipt {
    recorded: Vec<u32>,
    feed_seq: Option<FeedSeq>,
}

impl LineTerminalsReceipt {
    /// Creates a backend-returned receipt.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_store(recorded: Vec<u32>, feed_seq: Option<FeedSeq>) -> Self {
        Self { recorded, feed_seq }
    }

    /// Jobs whose evidence this write added; a job already recorded keeps its
    /// first evidence and is not listed.
    #[must_use]
    pub fn recorded(&self) -> &[u32] {
        &self.recorded
    }

    /// The feed record of the write, absent when nothing new was recorded.
    #[must_use]
    pub const fn feed_seq(&self) -> Option<FeedSeq> {
        self.feed_seq
    }
}
