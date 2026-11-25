//! Bounded process-memory implementation of the semantic session store.

#![forbid(unsafe_code)]

/// Repository-only, process-wide durable SQLite commit tracing.
#[cfg(feature = "test-support")]
pub mod commit_trace;

#[cfg(feature = "sqlite")]
mod holder;
mod lease_clock;
mod memory_feed;
pub use memory_feed::MemoryFeedLimits;

#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "sqlite")]
pub use sqlite::{
    BUNDLED_SQLITE_REQUIRED_COMPILE_OPTIONS, BUNDLED_SQLITE_SOURCE_ID, BUNDLED_SQLITE_VERSION,
    BranchHolder, PreservationReader, SQLITE_SCHEMA_VERSION, SqliteFeedUsage, SqliteFileUsage,
    SqlitePersistenceShape, SqliteRetentionUsage, SqliteSessionRetention, SqliteSessionStore,
    SqliteStoreOptions, SqliteStoreStats, branch_holder,
};
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub use sqlite::{SqliteCommitTrace, SqliteTraceCollector};

mod memory;
pub use memory::{
    DEFAULT_MAX_BRANCHES_PER_SESSION, DEFAULT_MAX_CHECKPOINTS, DEFAULT_MAX_OBJECT_BYTES,
    DEFAULT_MAX_OBJECTS, DEFAULT_MAX_PAGE_SIZE, DEFAULT_MAX_SESSIONS, MemorySessionStore,
    MemoryStoreLimits, MemoryStoreStats,
};

#[cfg(test)]
#[path = "../tests/fixture.rs"]
mod integrity_fixture;
