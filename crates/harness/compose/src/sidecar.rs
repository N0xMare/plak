//! Where a composition's evaluator worker lives.
//!
//! A JavaScript or Python cell runs its code in a separate process, and
//! something has to say which executable that is. Finding it is a *product*
//! act — an environment override, an install layout, a sibling of the running
//! binary — and a composition that performed it would be deciding for its
//! embedder how their program is installed.
//!
//! The embedder supplies locations through [`SidecarLocator`].

use std::path::PathBuf;

/// The evaluator workers a composition may need.
///
/// Each answer is resolved when it is asked for, not cached here: an embedder
/// whose layout changes under it should be able to say so, and a locator that
/// froze its answer at construction could not.
pub trait SidecarLocator: Send + Sync {
    /// The JavaScript evaluator worker.
    ///
    /// # Errors
    ///
    /// Returns an operator-facing message naming what the embedder looked for
    /// and how to point it somewhere else. It contains no secrets.
    fn javascript_host(&self) -> Result<PathBuf, String>;

    /// The Python evaluator worker.
    ///
    /// # Errors
    ///
    /// As [`Self::javascript_host`].
    fn python_host(&self) -> Result<PathBuf, String>;
}
