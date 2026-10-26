//! Where Plak installs its evaluator workers.
//!
//! The product side of `plakous::tools::SidecarLocator`: an explicit
//! environment override, else a sibling of the running binary. The composition
//! library asks through its locator port; the product resolves installation paths.

use std::path::PathBuf;

/// The JavaScript evaluator worker's install layout.
pub mod js_host;
/// The Python evaluator worker's install layout.
#[cfg(feature = "python")]
pub mod python_bundle;
#[cfg(not(feature = "python"))]
#[path = "sidecars/python_unavailable.rs"]
pub mod python_bundle;

/// Plak's locator: `PLAK_JS_HOST` / `PLAK_PY_HOST`, else a sibling.
pub(crate) struct ProductSidecarLocator;

impl plakous::tools::SidecarLocator for ProductSidecarLocator {
    fn javascript_host(&self) -> Result<PathBuf, String> {
        js_host::locate()
    }

    fn python_host(&self) -> Result<PathBuf, String> {
        python_bundle::locate()
    }
}
