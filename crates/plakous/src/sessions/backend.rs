#[cfg(feature = "sessions-sqlite")]
use super::error::store_path_error;
use super::registry::DriverRegistry;
use super::{AgentSessionsError, error::store_error};
use plakous_agent::internal::{RuntimeDurability, SessionStore};
use plakous_agent::internal::{StoreBoundUsage, StoreUsage};
use plakous_store::{MemorySessionStore, MemoryStoreLimits};
#[cfg(feature = "sessions-sqlite")]
use plakous_store::{SqliteFeedUsage, SqliteFileUsage, SqliteSessionStore};
#[cfg(feature = "sessions-sqlite")]
use std::path::Path;
use std::{fmt, sync::Arc};

/// Opaque crate-owned semantic-session backend.
///
/// This value is the construction seam for [`AgentSessions`](super::AgentSessions). It intentionally
/// exposes no store trait, row, object graph, receipt, lease, fence, SQL
/// connection, or compare-and-swap operation.
#[derive(Clone)]
pub struct SessionBackend {
    store: Arc<dyn SessionStore>,
    durability: RuntimeDurability,
    drivers: Arc<DriverRegistry>,
}

impl SessionBackend {
    pub(super) fn into_parts(
        self,
    ) -> (
        Arc<dyn SessionStore>,
        RuntimeDurability,
        Arc<DriverRegistry>,
    ) {
        (self.store, self.durability, self.drivers)
    }

    /// Creates one bounded process-memory backend.
    ///
    /// Its exact session semantics match the durable backend, but its
    /// process-memory checkpoints cannot survive process exit.
    ///
    /// # Errors
    ///
    /// Returns a storage failure when the host lease clock cannot be established.
    pub fn memory() -> Result<Self, AgentSessionsError> {
        let store = MemorySessionStore::new(MemoryStoreLimits::default()).map_err(store_error)?;
        Ok(Self::from_store(
            Arc::new(store),
            RuntimeDurability::Disabled,
        ))
    }
}

#[cfg(feature = "sessions-sqlite")]
impl SessionBackend {
    /// Reads, for every session of one SQLite store, its retained checkpoints
    /// and the object bytes they reach, largest first, without opening the
    /// store for use. Returns an empty list when no current-shape database
    /// exists. Shared objects count toward every session that reaches them.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe refusal for unsafe, unknown, or damaged shape.
    pub fn inspect_sqlite_session_retention(
        path: impl AsRef<Path>,
    ) -> Result<Vec<SessionRetention>, AgentSessionsError> {
        let path = path.as_ref();
        let retention = SqliteSessionStore::inspect_session_retention(path)
            .map_err(|error| store_path_error(path, error))?;
        Ok(retention
            .unwrap_or_default()
            .into_iter()
            .map(|session| SessionRetention {
                session_id: session.session_id,
                checkpoints: session.checkpoints,
                object_bytes: session.object_bytes,
            })
            .collect())
    }

    /// Opens or creates one required-durability SQLite backend.
    ///
    /// Attach admits bounded shape, exact schema, and counters. It does not
    /// reconstruct every retained object. A legacy shape requires an explicit
    /// [`Self::bootstrap_sqlite`] before opening a writer.
    ///
    /// The database path and backend remain private implementation inputs;
    /// callers receive only the semantic workflow handle.
    /// Capacity waits have no overall outage deadline. Without an interrupt
    /// probe, bound a workflow by dropping or timing out its future. Dropping
    /// an agent command or result observer alone does not stop its running
    /// owner: cancel the exact turn or shut down its agent explicitly.
    ///
    /// # Errors
    ///
    /// Returns `BootstrapRequired` for the supported pre-bootstrap schema,
    /// or a redaction-safe storage or integrity failure when the database
    /// cannot be opened exactly.
    pub fn sqlite(path: impl AsRef<Path>) -> Result<Self, AgentSessionsError> {
        Self::sqlite_with_storage_probe(path, Arc::new(|_| false))
    }

    /// Upgrades the exact pre-bootstrap shape of a stopped SQLite store.
    /// Close all store handles first. An already-current or absent store is
    /// left alone. The verified transaction keeps a synced pre-change backup.
    ///
    /// # Errors
    /// Returns `BootstrapBusy` if another handle prevents exclusive ownership,
    /// or a storage or integrity failure without activating a partial shape.
    pub fn bootstrap_sqlite(path: impl AsRef<Path>) -> Result<(), AgentSessionsError> {
        let path = path.as_ref();
        SqliteSessionStore::bootstrap_stopped(path, |_| {})
            .map_err(|error| store_path_error(path, error))
    }

    /// Opens SQLite with a process-local pause probe for colocated
    /// authoritative stores. True refuses new dispatch, never a retained
    /// publication. The probe receives `true` to retry outstanding storage
    /// debt through an authoritative write, and `false` for observation only.
    /// Only the store that failed may clear its own pause. Storage waits are
    /// interrupted per handle
    /// ([`AgentSessions::with_storage_interrupt`](super::AgentSessions::with_storage_interrupt)) and
    /// per agent, never through the shared backend.
    ///
    /// # Errors
    /// Returns the same admission failures as [`Self::sqlite`].
    pub fn sqlite_with_storage_probe(
        path: impl AsRef<Path>,
        probe: Arc<dyn Fn(bool) -> bool + Send + Sync>,
    ) -> Result<Self, AgentSessionsError> {
        let path = path.as_ref();
        let store =
            SqliteSessionStore::open(path).map_err(|error| store_path_error(path, error))?;
        Ok(Self::from_store(
            Arc::new(store.with_storage_probe(probe)),
            RuntimeDurability::Required,
        ))
    }

    /// Opens or creates one required-durability SQLite backend whose
    /// aggregate retention bounds are `checkpoints`, `objects` and
    /// `object_bytes` instead of the defaults; every other bound keeps its
    /// default. The admission reserve and its class thresholds follow these
    /// bounds. Usage inspection ([`Self::inspect_sqlite_usage`]) still reads
    /// against the defaults.
    ///
    /// # Errors
    /// Returns the same admission failures as [`Self::sqlite`].
    pub fn sqlite_with_retention_bounds(
        path: impl AsRef<Path>,
        checkpoints: std::num::NonZeroUsize,
        objects: std::num::NonZeroUsize,
        object_bytes: std::num::NonZeroUsize,
    ) -> Result<Self, AgentSessionsError> {
        Self::sqlite_with_bounds_and_storage_probe(
            path,
            [
                checkpoints,
                objects,
                object_bytes,
                Self::default_sqlite_retention_bounds()[3],
            ],
            Arc::new(|_| false),
        )
    }

    /// Default SQLite caps in checkpoint, object, accounted-byte, session order.
    #[must_use]
    pub fn default_sqlite_retention_bounds() -> [std::num::NonZeroUsize; 4] {
        let limits = plakous_store::SqliteStoreOptions::default().limits();
        [
            limits.max_checkpoints(),
            limits.max_objects(),
            limits.max_object_bytes(),
            limits.max_sessions(),
        ]
    }

    /// Opens with explicit aggregate bounds and the colocated-store pause probe.
    ///
    /// # Errors
    /// Returns the same admission failures as [`Self::sqlite`].
    pub fn sqlite_with_bounds_and_storage_probe(
        path: impl AsRef<Path>,
        bounds: [std::num::NonZeroUsize; 4],
        probe: Arc<dyn Fn(bool) -> bool + Send + Sync>,
    ) -> Result<Self, AgentSessionsError> {
        let defaults = plakous_store::SqliteStoreOptions::default();
        let options = plakous_store::SqliteStoreOptions::new(
            sqlite_limits(bounds),
            defaults.queue_capacity(),
        );
        let path = path.as_ref();
        let store = SqliteSessionStore::open_with_options(path, options)
            .map_err(|error| store_path_error(path, error))?;
        Ok(Self::from_store(
            Arc::new(store.with_storage_probe(probe)),
            RuntimeDurability::Required,
        ))
    }
    /// Classifies bounded SQLite persistence shape without authorizing use.
    ///
    /// `Current` is only startup-routing evidence. Call [`Self::sqlite`] before
    /// resume, mutation, or recovery.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe refusal for unsafe, unknown, or damaged shape.
    pub fn classify_sqlite(
        path: impl AsRef<Path>,
    ) -> Result<SessionPersistenceShape, AgentSessionsError> {
        use plakous_store::SqlitePersistenceShape;

        let path = path.as_ref();
        SqliteSessionStore::classify_persistence(path)
            .map(|shape| match shape {
                SqlitePersistenceShape::MissingOrFresh => SessionPersistenceShape::MissingOrFresh,
                SqlitePersistenceShape::Current => SessionPersistenceShape::Current,
            })
            .map_err(|error| store_path_error(path, error))
    }

    /// Reads aggregate retention usage of one SQLite store without opening it
    /// for use. Returns `None` when no current-shape database exists.
    ///
    /// Usage is measured against the default limits [`Self::sqlite`] opens with.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe refusal for unsafe, unknown, or damaged shape.
    pub fn inspect_sqlite_usage(
        path: impl AsRef<Path>,
    ) -> Result<Option<SessionStoreUsage>, AgentSessionsError> {
        Self::inspect_sqlite_usage_with_bounds(path, Self::default_sqlite_retention_bounds())
    }

    /// Reads aggregate SQLite usage against explicitly configured bounds.
    ///
    /// # Errors
    /// Returns the same read-only refusals as [`Self::inspect_sqlite_usage`].
    pub fn inspect_sqlite_usage_with_bounds(
        path: impl AsRef<Path>,
        bounds: [std::num::NonZeroUsize; 4],
    ) -> Result<Option<SessionStoreUsage>, AgentSessionsError> {
        let path = path.as_ref();
        let Some(usage) = SqliteSessionStore::inspect_retention_usage(path, sqlite_limits(bounds))
            .map_err(|error| store_path_error(path, error))?
        else {
            return Ok(None);
        };
        Ok(Some(SessionStoreUsage::from_store(
            usage.aggregate,
            (usage.sessions, bound64(bounds[3])),
            usage.file,
            usage.feed,
        )))
    }

    /// Zero usage with the same admission thresholds as an opened SQLite store.
    #[must_use]
    pub fn empty_sqlite_usage(bounds: [std::num::NonZeroUsize; 4]) -> SessionStoreUsage {
        let [checkpoints, objects, bytes, sessions] = bounds.map(|limit| (0, bound64(limit)));
        SessionStoreUsage::from_store(
            StoreUsage::new(checkpoints, objects, bytes),
            sessions,
            SqliteFileUsage {
                file_bytes: 0,
                free_bytes: 0,
            },
            SqliteFeedUsage {
                records: 0,
                bytes: 0,
            },
        )
    }
}

impl SessionBackend {
    pub(super) fn from_store(store: Arc<dyn SessionStore>, durability: RuntimeDurability) -> Self {
        Self {
            store,
            durability,
            drivers: Arc::new(DriverRegistry::default()),
        }
    }
}

#[cfg(feature = "sessions-sqlite")]
fn bound64(limit: std::num::NonZeroUsize) -> u64 {
    u64::try_from(limit.get()).unwrap_or(u64::MAX)
}

#[cfg(feature = "sessions-sqlite")]
fn sqlite_limits(
    [checkpoints, objects, bytes, sessions]: [std::num::NonZeroUsize; 4],
) -> MemoryStoreLimits {
    let defaults = plakous_store::SqliteStoreOptions::default().limits();
    MemoryStoreLimits::new(
        sessions,
        defaults.max_branches_per_session(),
        checkpoints,
        objects,
        bytes,
        defaults.max_page_size(),
    )
}

/// Non-authoritative bounded SQLite persistence-shape classification.
#[cfg(feature = "sessions-sqlite")]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPersistenceShape {
    /// No runtime database exists, or its private file has no `SQLite` objects.
    MissingOrFresh,
    /// Exact readable shape, possibly requiring bootstrap on authoritative open.
    Current,
}

impl fmt::Debug for SessionBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionBackend")
            .field("store", &"<semantic session store>")
            .field("durability", &self.durability)
            .finish_non_exhaustive()
    }
}

/// Where ordinary writes through a subordinate handle stop on a bound of
/// `limit`: one reserve before the ordinary threshold, so that the handles
/// without the class keep that reserve for their own new work
/// ([`AgentSessions::with_subordinate_admission`](super::AgentSessions::with_subordinate_admission)).
pub(crate) const fn subordinate_threshold(limit: u64) -> u64 {
    limit.saturating_sub(plakous_agent::internal::StoreAdmission::reserve(limit).saturating_mul(2))
}

/// Which aggregate retention bound a usage reading or refusal is for.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStoreBound {
    /// Retained checkpoints across every session.
    Checkpoints,
    /// Retained content-addressed objects.
    Objects,
    /// Accounted bytes of retained objects.
    ObjectBytes,
}

impl fmt::Display for SessionStoreBound {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Checkpoints => "checkpoints",
            Self::Objects => "objects",
            Self::ObjectBytes => "object bytes",
        })
    }
}

/// Used and configured amount of one aggregate retention bound.
///
/// New work stops at [`Self::ordinary_threshold`]; recording the outcome of
/// running work stops at [`Self::settlement_threshold`]; only recovery may
/// fill the bound to [`Self::limit`]. Deletion is never refused for capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionStoreBoundUsage {
    bound: SessionStoreBound,
    usage: StoreBoundUsage,
}

impl SessionStoreBoundUsage {
    /// Every aggregate bound of `usage`: checkpoints, objects, object bytes.
    pub(super) const fn readings(usage: StoreUsage) -> [Self; 3] {
        let [checkpoints, objects, object_bytes] = usage.bounds();
        [
            Self {
                bound: SessionStoreBound::Checkpoints,
                usage: checkpoints,
            },
            Self {
                bound: SessionStoreBound::Objects,
                usage: objects,
            },
            Self {
                bound: SessionStoreBound::ObjectBytes,
                usage: object_bytes,
            },
        ]
    }

    /// Returns which bound this reading is for.
    #[must_use]
    pub const fn bound(&self) -> SessionStoreBound {
        self.bound
    }

    /// Returns the retained amount.
    #[must_use]
    pub const fn used(&self) -> u64 {
        self.usage.used()
    }

    /// Returns the configured bound.
    #[must_use]
    pub const fn limit(&self) -> u64 {
        self.usage.limit()
    }

    /// Returns the reserve kept for settlement and recovery.
    #[must_use]
    pub const fn reserve(&self) -> u64 {
        self.usage.reserve()
    }

    /// Returns the amount at which new work stops.
    #[must_use]
    pub const fn ordinary_threshold(&self) -> u64 {
        self.usage.ordinary_threshold()
    }

    /// Returns the amount at which settlement of running work stops.
    #[must_use]
    pub const fn settlement_threshold(&self) -> u64 {
        self.usage.settlement_threshold()
    }

    /// Returns the amount at which new work through a subordinate handle
    /// stops, one reserve below [`Self::ordinary_threshold`].
    #[must_use]
    pub const fn subordinate_threshold(&self) -> u64 {
        subordinate_threshold(self.usage.limit())
    }

    /// Whether new work is refused on this bound.
    #[must_use]
    pub const fn ordinary_exhausted(&self) -> bool {
        self.usage.ordinary_exhausted()
    }
}

/// One session's retained checkpoints and the object bytes they reach
/// (`SessionBackend::inspect_sqlite_session_retention`, with `sessions-sqlite`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionRetention {
    session_id: plakous_model::SessionId,
    checkpoints: u64,
    object_bytes: u64,
}

impl SessionRetention {
    /// Returns the session.
    #[must_use]
    pub const fn session_id(&self) -> plakous_model::SessionId {
        self.session_id
    }

    /// Returns the retained checkpoints of every branch of the session.
    #[must_use]
    pub const fn checkpoints(&self) -> u64 {
        self.checkpoints
    }

    /// Returns the accounted bytes of the objects those checkpoints reach,
    /// shared objects included.
    #[must_use]
    pub const fn object_bytes(&self) -> u64 {
        self.object_bytes
    }
}

#[cfg(feature = "sessions-sqlite")]
/// Aggregate retention usage of one whole session store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionStoreUsage {
    bounds: [SessionStoreBoundUsage; 3],
    sessions: (u64, u64),
    file_bytes: u64,
    free_bytes: u64,
    feed_records: u64,
    feed_bytes: u64,
}

#[cfg(feature = "sessions-sqlite")]
impl SessionStoreUsage {
    fn from_store(
        usage: StoreUsage,
        sessions: (u64, u64),
        file: SqliteFileUsage,
        feed: SqliteFeedUsage,
    ) -> Self {
        Self {
            bounds: SessionStoreBoundUsage::readings(usage),
            sessions,
            file_bytes: file.file_bytes,
            free_bytes: file.free_bytes,
            feed_records: feed.records,
            feed_bytes: feed.bytes,
        }
    }

    /// Returns retained sessions and their hard limit. There is no reserve for this bound.
    #[must_use]
    pub const fn session_capacity(&self) -> (u64, u64) {
        self.sessions
    }

    /// Returns the change feed's retained records across every session.
    /// Informational: feed records never count against the retention bounds.
    #[must_use]
    pub const fn feed_records(&self) -> u64 {
        self.feed_records
    }

    /// Returns the encoded bytes of the change feed's retained records.
    #[must_use]
    pub const fn feed_bytes(&self) -> u64 {
        self.feed_bytes
    }

    /// Returns the database file size in bytes.
    #[must_use]
    pub const fn file_bytes(&self) -> u64 {
        self.file_bytes
    }

    /// Returns database file bytes on the free list, reusable without growth
    /// and returned to the filesystem by later incremental vacuum.
    #[must_use]
    pub const fn free_bytes(&self) -> u64 {
        self.free_bytes
    }

    /// Returns every aggregate bound: checkpoints, objects, object bytes.
    #[must_use]
    pub const fn bounds(&self) -> &[SessionStoreBoundUsage; 3] {
        &self.bounds
    }
}
