//! Agent workspaces over `git` and `jj`.
//!
//! One copy per agent: detect the launch checkout's VCS, mint a worktree or
//! workspace over a named base, seal what it changed as a candidate, land a
//! candidate three-way onto a target, and unmint what is discarded. The
//! [`occupancy_vcs::VcsOps`] port is the whole surface; [`occupancy_vcs::SystemVcs`]
//! selects the native JJ library or optional Git CLI backend.
//!
//! This crate is the product's VCS security surface: every `sh`, `git`, and
//! model-facing JJ fence is here, and
//! [`git_redirect_fence`] is what keeps a minted copy's tooling from
//! reaching the launch checkout behind the copy's back. It runs no agent, owns
//! no tree, and knows nothing about topology beyond the copy shape it is asked
//! for.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::map_unwrap_or,
    clippy::too_many_lines,
    reason = "the set this crate actually trips"
)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::unwrap_in_result,
        reason = "unit tests"
    )
)]

pub mod git_redirect_fence;
pub mod occupancy_vcs;
pub mod workspace;

/// Real-`git`/`jj` repository fixtures for the crates whose tests drive a copy.
/// Behind `test-support`; never compiled into the product.
#[cfg(any(test, feature = "test-support"))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test fixtures panic on a broken repository rather than typing an error"
)]
pub mod vcs_fixtures;

#[cfg(feature = "jj-lib")]
mod native_jj;
#[cfg(not(feature = "jj-lib"))]
#[path = "native_jj_disabled.rs"]
mod native_jj;

/// jj-lib writer version used by this build (also reported without the jj feature).
pub const JJ_LIB_VERSION: &str = "0.46.0";
/// Tested external CLI minor range, inclusive, for major zero.
pub const JJ_CLI_SUPPORTED: std::ops::RangeInclusive<u32> = 44..=46;

/// The built native writer version, or None when JJ was not compiled in.
pub fn built_jj_library_version() -> Option<&'static str> {
    native_jj::BUILT_LIBRARY_VERSION
}
