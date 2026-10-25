//! Store integration contracts.

#[cfg(feature = "commit-trace")]
mod commit_trace;
mod conformance;
mod feed;
mod named_state;
#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "sqlite")]
mod sqlite_feed;
#[cfg(all(feature = "sqlite", unix))]
mod sqlite_power_loss;
