#[cfg(feature = "sessions-sqlite")]
use super::error::store_path_error;
use super::registry::DriverRegistry;
use super::{AgentSessionsError, error::store_error};
use plakous_agent::internal::{RuntimeDurability, SessionStore};
#[cfg(feature = "sessions-sqlite")]
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

    /// Opens or creates one required-durability SQLite backend.
    ///
    /// Ordinary attach admits bounded shape, exact schema, and counters. It
    /// does not reconstruct every retained object. Call
    /// [`Self::sqlite_verified`] before any
    /// integrity-clean claim.
    ///
    /// The database path and backend remain private implementation inputs;
    /// callers receive only the semantic workflow handle.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe storage or integrity failure when the database
    /// cannot be opened exactly.
    #[cfg(feature = "sessions-sqlite")]
    pub fn sqlite(path: impl AsRef<Path>) -> Result<Self, AgentSessionsError> {
        let path = path.as_ref();
        let store =
            SqliteSessionStore::open(path).map_err(|error| store_path_error(path, error))?;
        Ok(Self::from_store(
            Arc::new(store),
            RuntimeDurability::Required,
        ))
    }

    /// Opens one SQLite backend after complete object and receipt verification.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe storage or integrity failure when the database
    /// cannot be opened exactly.
    #[cfg(feature = "sessions-sqlite")]
    pub fn sqlite_verified(path: impl AsRef<Path>) -> Result<Self, AgentSessionsError> {
        let path = path.as_ref();
        let store = SqliteSessionStore::open_verified(path)
            .map_err(|error| store_path_error(path, error))?;
        Ok(Self::from_store(
            Arc::new(store),
            RuntimeDurability::Required,
        ))
    }

    /// Classifies bounded SQLite persistence shape without authorizing use.
    ///
    /// `Current` is only startup-routing evidence. Call [`Self::sqlite`] before
    /// resume, mutation, or recovery. Call [`Self::sqlite_verified`] before any
    /// integrity-clean claim.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe refusal for unsafe, unknown, or damaged shape.
    #[cfg(feature = "sessions-sqlite")]
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
    #[cfg(feature = "sessions-sqlite")]
    pub fn inspect_sqlite_usage(
        path: impl AsRef<Path>,
    ) -> Result<Option<SessionStoreUsage>, AgentSessionsError> {
        let path = path.as_ref();
        let Some(usage) = SqliteSessionStore::inspect_usage(path)
            .map_err(|error| store_path_error(path, error))?
        else {
            return Ok(None);
        };
        let file = SqliteSessionStore::inspect_file_usage(path).map_err(store_error)?;
        let feed = SqliteSessionStore::inspect_feed_usage(path).map_err(store_error)?;
        Ok(Some(SessionStoreUsage::from_store(usage, file, feed)))
    }

    pub(super) fn from_store(store: Arc<dyn SessionStore>, durability: RuntimeDurability) -> Self {
        Self {
            store,
            durability,
            drivers: Arc::new(DriverRegistry::default()),
        }
    }
}

/// Non-authoritative bounded SQLite persistence-shape classification.
#[cfg(feature = "sessions-sqlite")]
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPersistenceShape {
    /// No runtime database exists, or its private file has no `SQLite` objects.
    MissingOrFresh,
    /// Current bounded shape; full authoritative open is still required.
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

#[cfg(feature = "sessions-sqlite")]
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

#[cfg(feature = "sessions-sqlite")]
impl SessionStoreBoundUsage {
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

    /// Whether new work is refused on this bound.
    #[must_use]
    pub const fn ordinary_exhausted(&self) -> bool {
        self.usage.ordinary_exhausted()
    }
}

#[cfg(feature = "sessions-sqlite")]
/// Aggregate retention usage of one whole session store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionStoreUsage {
    bounds: [SessionStoreBoundUsage; 3],
    file_bytes: u64,
    free_bytes: u64,
    feed_records: u64,
    feed_bytes: u64,
}

#[cfg(feature = "sessions-sqlite")]
impl SessionStoreUsage {
    fn from_store(
        usage: StoreUsage,
        file: Option<SqliteFileUsage>,
        feed: Option<SqliteFeedUsage>,
    ) -> Self {
        let [checkpoints, objects, object_bytes] = usage.bounds();
        let reading = |bound, usage| SessionStoreBoundUsage { bound, usage };
        let file = file.unwrap_or(SqliteFileUsage {
            file_bytes: 0,
            free_bytes: 0,
        });
        let feed = feed.unwrap_or(SqliteFeedUsage {
            records: 0,
            bytes: 0,
        });
        Self {
            bounds: [
                reading(SessionStoreBound::Checkpoints, checkpoints),
                reading(SessionStoreBound::Objects, objects),
                reading(SessionStoreBound::ObjectBytes, object_bytes),
            ],
            file_bytes: file.file_bytes,
            free_bytes: file.free_bytes,
            feed_records: feed.records,
            feed_bytes: feed.bytes,
        }
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
