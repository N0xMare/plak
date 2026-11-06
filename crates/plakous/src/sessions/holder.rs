//! The process that holds a session's head branch in the SQLite runtime
//! store, unless it is this process or is proven gone by its free process
//! lock: what a host names when another process has a session open.

use super::SessionLocator;

pub use plakous_store::BranchHolder;

/// The [`BranchHolder`] of `locator`'s branch in the SQLite runtime store at
/// `database`: the process whose holder record names that branch, on another
/// machine (never proven gone from here) or on this one with its process
/// lock held. `None` when no record names one, the record names this
/// process, or its lock is free.
#[must_use]
pub fn sqlite_branch_holder(
    database: &std::path::Path,
    locator: SessionLocator,
) -> Option<BranchHolder> {
    plakous_store::branch_holder(
        database,
        &locator.session_id.to_string(),
        &locator.branch_id.to_string(),
    )
}
