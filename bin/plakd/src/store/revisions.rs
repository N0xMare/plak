//! Host admission and detached qualification for the two revisioned stores.

use std::path::{Path, PathBuf};

use crate::home::paths::PlakPaths;

use crate::{CliFailure, CliResult};

#[cfg(feature = "web")]
pub(crate) mod host {
    use super::{CliFailure, CliResult, PathBuf, PlakPaths};
    use plakous_recursive_subagents_store::{
        admission::{AdmissionError, Schema, Status},
        database,
    };

    pub(crate) fn databases(paths: &PlakPaths) -> [(Schema, PathBuf); 2] {
        [
            (super::super::schema::schema(), paths.product_database()),
            (database::schema(), paths.subagents_database()),
        ]
    }

    /// Classify both stores before touching either. An older build must refuse
    /// the whole home even when only the tree or only the product has advanced.
    pub(crate) fn pending(paths: &PlakPaths) -> CliResult<bool> {
        let mut pending = false;
        for (schema, path) in databases(paths) {
            pending |= matches!(schema.inspect(&path).map_err(failure)?, Status::Pending(_));
        }
        Ok(pending)
    }

    pub(crate) fn failure(error: AdmissionError) -> CliFailure {
        if let AdmissionError::Busy(message) = error {
            return plakous_hosted::session_store::RecordStoreError::refused(
                plakous_hosted::error::StoreRefusal::Busy,
                message,
            )
            .into();
        }
        CliFailure::new(error.to_string())
    }
}

pub(crate) fn probe_stores(paths: &PlakPaths, root: &Path) -> CliResult<PathBuf> {
    let home = paths
        .home()
        .canonicalize()
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let canonical = root
        .canonicalize()
        .map_err(|error| CliFailure::new(error.to_string()))?;
    if root.file_name() != Some(std::ffi::OsStr::new("stores"))
        || canonical.parent().and_then(Path::parent) != Some(home.as_path())
        || !canonical.is_dir()
    {
        return Err(CliFailure::new(
            "invalid scratch store directory for upgrade probe",
        ));
    }
    Ok(canonical)
}
