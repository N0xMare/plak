use super::{Error, fmt};

/// Closed semantic reason for refusing a session-store request.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum StoreInvalidReason {
    /// Branch revision exhausted.
    #[serde(rename = "branch revision exhausted")]
    #[error("branch revision exhausted")]
    BranchRevisionExhausted,
    /// Checkpoint commit topology.
    #[serde(rename = "checkpoint commit topology")]
    #[error("checkpoint commit topology")]
    CheckpointCommitTopology,
    /// Checkpoint object binding.
    #[serde(rename = "checkpoint object binding")]
    #[error("checkpoint object binding")]
    CheckpointObjectBinding,
    /// Checkpoint object bundle.
    #[serde(rename = "checkpoint object bundle")]
    #[error("checkpoint object bundle")]
    CheckpointObjectBundle,
    /// Checkpoint objects are not canonical.
    #[serde(rename = "checkpoint objects are not canonical")]
    #[error("checkpoint objects are not canonical")]
    NoncanonicalCheckpointObjects,
    /// Disabled durability requires an in-memory session store.
    #[serde(rename = "disabled durability requires an in-memory session store")]
    #[error("disabled durability requires an in-memory session store")]
    DisabledDurabilityRequiresMemoryStore,
    /// Fork checkpoint semantics.
    #[serde(rename = "fork checkpoint semantics")]
    #[error("fork checkpoint semantics")]
    ForkCheckpointSemantics,
    /// Fork semantic request digest domain.
    #[serde(rename = "fork semantic request digest domain")]
    #[error("fork semantic request digest domain")]
    ForkRequestDigestDomain,
    /// Fork source is not quiescent.
    #[serde(rename = "fork source is not quiescent")]
    #[error("fork source is not quiescent")]
    ForkSourceNotQuiescent,
    /// Genesis checkpoint topology.
    #[serde(rename = "genesis checkpoint topology")]
    #[error("genesis checkpoint topology")]
    GenesisCheckpointTopology,
    /// Invalid typed branch context transition.
    #[serde(rename = "invalid typed branch context transition")]
    #[error("invalid typed branch context transition")]
    InvalidBranchContextTransition,
    /// Lease expiry.
    #[serde(rename = "lease expiry")]
    #[error("lease expiry")]
    LeaseExpiry,
    /// Lease expiry overflow.
    #[serde(rename = "lease expiry overflow")]
    #[error("lease expiry overflow")]
    LeaseExpiryOverflow,
    /// Manifest root delta.
    #[serde(rename = "manifest root delta")]
    #[error("manifest root delta")]
    ManifestRootDelta,
    /// Ordinary fork changed source semantics.
    #[serde(rename = "ordinary fork changed source semantics")]
    #[error("ordinary fork changed source semantics")]
    OrdinaryForkChangedSemantics,
    /// Rebind did not change a bound configuration fact.
    #[serde(rename = "rebind did not change a bound configuration fact")]
    #[error("rebind did not change a bound configuration fact")]
    RebindConfigurationUnchanged,
    /// Prune head is not quiescent.
    #[serde(rename = "prune head is not quiescent")]
    #[error("prune head is not quiescent")]
    PruneHeadNotQuiescent,
    /// Session is not archived.
    #[serde(rename = "session is not archived")]
    #[error("session is not archived")]
    SessionNotArchived,
    /// Unauthorized branch manifest transition.
    #[serde(rename = "unauthorized branch manifest transition")]
    #[error("unauthorized branch manifest transition")]
    UnauthorizedBranchManifestTransition,
}

/// Closed resource or numeric bound enforced by a session store.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum StoreCapacityBound {
    /// Branch revision.
    #[serde(rename = "branch revision")]
    #[error("branch revision")]
    BranchRevision,
    /// Session revision.
    #[serde(rename = "session revision")]
    #[error("session revision")]
    SessionRevision,
    /// Checkpoints.
    #[serde(rename = "checkpoints")]
    #[error("checkpoints")]
    Checkpoints,
    /// Lease fence.
    #[serde(rename = "lease fence")]
    #[error("lease fence")]
    LeaseFence,
    /// Object bytes.
    #[serde(rename = "object bytes")]
    #[error("object bytes")]
    ObjectBytes,
    /// Objects.
    #[serde(rename = "objects")]
    #[error("objects")]
    Objects,
    /// Page size.
    #[serde(rename = "page size")]
    #[error("page size")]
    PageSize,
    /// Sessions.
    #[serde(rename = "sessions")]
    #[error("sessions")]
    Sessions,
    /// Branches per session.
    #[serde(rename = "branches per session")]
    #[error("branches per session")]
    BranchesPerSession,
    /// `SQLite` integer.
    #[serde(rename = "SQLite integer")]
    #[error("SQLite integer")]
    SqliteInteger,
    /// Encoded bytes of one change-feed record.
    #[serde(rename = "feed record bytes")]
    #[error("feed record bytes")]
    FeedRecordBytes,
}

/// Which share of an aggregate retention bound one store write may consume.
///
/// Every aggregate retention bound (checkpoints, objects, object bytes) keeps
/// a reserve of `ceil(limit / 8)`. Work that begins something new stops before
/// the reserve; recording the outcome of work already dispatched may use half
/// of it; recovery may use all of it. The caller that owns the write's meaning
/// chooses the class; it is never a user setting.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StoreAdmission {
    /// Begins new work: a root turn, a prompt, a prepare record, the first
    /// fence of an operation out of `Prepared` (later transitions of an
    /// operation that crossed it are settlement), a session creation or fork.
    #[default]
    Ordinary,
    /// Records the outcome of already-dispatched work or publishes a turn's
    /// terminal result.
    Settlement,
    /// Open-time abandonment, explicit recovery and recovery evidence.
    Recovery,
}

impl StoreAdmission {
    /// Returns the reserve kept behind an aggregate `limit`.
    #[must_use]
    pub const fn reserve(limit: u64) -> u64 {
        limit.div_ceil(8)
    }

    /// Returns how much of an aggregate `limit` this class may fill.
    #[must_use]
    pub const fn threshold(self, limit: u64) -> u64 {
        let reserve = Self::reserve(limit);
        match self {
            Self::Ordinary => limit - reserve,
            Self::Settlement => limit - reserve / 2,
            Self::Recovery => limit,
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Ordinary => "ordinary",
            Self::Settlement => "settlement",
            Self::Recovery => "recovery",
        }
    }
}

impl fmt::Display for StoreAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Which bounded store resource was exhausted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionStoreCapacity {
    /// Stable redaction-safe bound name.
    pub bound: StoreCapacityBound,
    /// Current or attempted count.
    pub current: u64,
    /// The maximum this request was held to, or zero for numeric exhaustion.
    /// For an admission-classed aggregate bound it is the class threshold.
    pub max: u64,
    /// Configured bound; equal to `max` unless an admission class applied.
    pub limit: u64,
    /// The admission class held to `max`, when an aggregate reserve applied.
    pub admission: Option<StoreAdmission>,
}

impl fmt::Display for SessionStoreCapacity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.max == 0 {
            return write!(formatter, "{} overflow", self.bound);
        }
        match self.admission {
            Some(admission) if self.max != self.limit => write!(
                formatter,
                "{} {}/{} ({admission} writes stop at {})",
                self.bound, self.current, self.limit, self.max
            ),
            _ => write!(formatter, "{} {}/{}", self.bound, self.current, self.max),
        }
    }
}

/// Used and configured amount of one aggregate retention bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreBoundUsage {
    bound: StoreCapacityBound,
    used: u64,
    limit: u64,
}

impl StoreBoundUsage {
    /// Creates one backend-reported bound reading.
    #[must_use]
    pub const fn new(bound: StoreCapacityBound, used: u64, limit: u64) -> Self {
        Self { bound, used, limit }
    }

    /// Returns which bound this reading is for.
    #[must_use]
    pub const fn bound(self) -> StoreCapacityBound {
        self.bound
    }

    /// Returns the retained amount.
    #[must_use]
    pub const fn used(self) -> u64 {
        self.used
    }

    /// Returns the configured bound.
    #[must_use]
    pub const fn limit(self) -> u64 {
        self.limit
    }

    /// Returns the reserve ordinary work may not consume.
    #[must_use]
    pub const fn reserve(self) -> u64 {
        StoreAdmission::reserve(self.limit)
    }

    /// Returns the amount at which ordinary work stops.
    #[must_use]
    pub const fn ordinary_threshold(self) -> u64 {
        StoreAdmission::Ordinary.threshold(self.limit)
    }

    /// Returns the amount at which settlement stops.
    #[must_use]
    pub const fn settlement_threshold(self) -> u64 {
        StoreAdmission::Settlement.threshold(self.limit)
    }

    /// Whether ordinary work has no headroom left on this bound.
    #[must_use]
    pub const fn ordinary_exhausted(self) -> bool {
        self.used >= self.ordinary_threshold()
    }

    /// Returns the capacity refusal ordinary work meets on this bound.
    #[must_use]
    pub const fn ordinary_refusal(self) -> SessionStoreCapacity {
        SessionStoreCapacity {
            bound: self.bound,
            current: self.used,
            max: self.ordinary_threshold(),
            limit: self.limit,
            admission: Some(StoreAdmission::Ordinary),
        }
    }
}

/// Aggregate retention usage of one whole store, across every session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreUsage {
    checkpoints: StoreBoundUsage,
    objects: StoreBoundUsage,
    object_bytes: StoreBoundUsage,
}

impl StoreUsage {
    /// Creates a backend-reported usage reading.
    #[must_use]
    pub const fn new(
        checkpoints: (u64, u64),
        objects: (u64, u64),
        object_bytes: (u64, u64),
    ) -> Self {
        Self {
            checkpoints: StoreBoundUsage::new(
                StoreCapacityBound::Checkpoints,
                checkpoints.0,
                checkpoints.1,
            ),
            objects: StoreBoundUsage::new(StoreCapacityBound::Objects, objects.0, objects.1),
            object_bytes: StoreBoundUsage::new(
                StoreCapacityBound::ObjectBytes,
                object_bytes.0,
                object_bytes.1,
            ),
        }
    }

    /// Returns every reserved aggregate bound in a stable order.
    #[must_use]
    pub const fn bounds(self) -> [StoreBoundUsage; 3] {
        [self.checkpoints, self.objects, self.object_bytes]
    }

    /// Returns the first bound on which ordinary work has no headroom left.
    #[must_use]
    pub fn ordinary_exhausted(self) -> Option<StoreBoundUsage> {
        self.bounds()
            .into_iter()
            .find(|bound| bound.ordinary_exhausted())
    }
}

/// Content-free classification of a store integrity failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreIntegrityReason {
    /// An incremental publication's base is absent.
    #[error("missing_base")]
    MissingBase,
    /// A checkpoint's root is absent.
    #[error("missing_root")]
    MissingRoot,
    /// An object references an absent child.
    #[error("missing_child")]
    MissingChild,
    /// Other stored semantics or backend integrity are inconsistent.
    #[error("inconsistent")]
    Inconsistent,
}

/// Typed redaction-safe semantic store failure.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum SessionStoreError {
    /// Caller input violates a closed semantic invariant.
    #[error("invalid session-store request: {0}")]
    Invalid(StoreInvalidReason),
    /// The selected session, branch, checkpoint, or receipt is absent.
    #[error("selected session-store record does not exist")]
    NotFound,
    /// An operation identity is occupied by different exact semantic bytes.
    #[error("session-store operation identity conflicts with accepted bytes")]
    IdentityConflict,
    /// Expected session/branch revision or head disagrees.
    #[error("session-store optimistic concurrency conflict")]
    Conflict,
    /// Another unexpired owner holds the branch.
    #[error("session branch is leased by another owner")]
    LeaseHeld,
    /// The supplied lease has expired.
    #[error("session branch lease expired")]
    LeaseExpired,
    /// The supplied reading or grant belongs to a different clock domain.
    #[error("session branch lease clock domain differs")]
    LeaseDomainMismatch,
    /// A newer or renewed fence value superseded the supplied token.
    #[error("session branch fence is superseded")]
    FenceSuperseded,
    /// The branch is not its line's head at its generation: a rebind or
    /// carry moved the head on, so the branch is read-only. Nothing was
    /// written; continue on the head or fork the branch.
    #[error("session branch is not its line's head")]
    NotLineHead,
    /// The session is archived and closed to ordinary mutation/load.
    #[error("session is archived")]
    Archived,
    /// A configured hard bound was reached.
    #[error("session store capacity was reached: {0}")]
    Capacity(SessionStoreCapacity),
    /// Durable backend I/O or authoritative in-process state is unavailable.
    #[error("session store backend is unavailable")]
    BackendUnavailable,
    /// A bounded backend queue or lock wait is exhausted.
    #[error("session store backend is busy")]
    BackendBusy,
    /// Physical storage is exhausted, independently of logical retention limits.
    #[error("session store disk is full")]
    StorageFull,
    /// The selected checkpoint or receipt was reclaimed by history retention.
    #[error("selected checkpoint history was reclaimed by retention")]
    Pruned,
    /// Stored object bytes, graph, receipt, or topology are inconsistent.
    #[error("session store integrity check failed: {0}")]
    Corrupt(StoreIntegrityReason),
    /// Persistent data uses an unsupported schema.
    #[error("session store schema is unsupported")]
    UnsupportedSchema,
    /// Persistent semantic/object bytes use an unsupported codec.
    #[error("session store codec is unsupported")]
    UnsupportedCodec,
    /// The store has entered its terminal shutdown state.
    #[error("session store is shut down")]
    Shutdown,
}

impl SessionStoreError {
    /// Creates an integrity failure without a more specific graph reason.
    #[must_use]
    pub const fn corrupt() -> Self {
        Self::Corrupt(StoreIntegrityReason::Inconsistent)
    }

    /// Creates a structured capacity error.
    #[must_use]
    pub const fn capacity(bound: StoreCapacityBound, current: u64, max: u64) -> Self {
        Self::Capacity(SessionStoreCapacity {
            bound,
            current,
            max,
            limit: max,
            admission: None,
        })
    }

    /// Creates a capacity error for an aggregate bound held to `admission`'s
    /// share of the configured `limit`.
    #[must_use]
    pub const fn admission_capacity(
        bound: StoreCapacityBound,
        current: u64,
        limit: u64,
        admission: StoreAdmission,
    ) -> Self {
        Self::Capacity(SessionStoreCapacity {
            bound,
            current,
            max: admission.threshold(limit),
            limit,
            admission: Some(admission),
        })
    }

    /// Returns the capacity refusal this error carries, if any.
    #[must_use]
    pub const fn capacity_refusal(&self) -> Option<SessionStoreCapacity> {
        match self {
            Self::Capacity(capacity) => Some(*capacity),
            _ => None,
        }
    }
}
