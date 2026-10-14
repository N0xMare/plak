//! Store integration contracts.

#[cfg(feature = "commit-trace")]
mod commit_trace;
mod conformance;
mod feed;
#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "sqlite")]
mod sqlite_feed;
