//! Host admission and detached qualification for the two revisioned stores.

use std::path::{Path, PathBuf};

use crate::home::paths::PlakPaths;
use plakous_recursive_subagents_store::{
    admission::{AdmissionError, Schema},
    database,
};

use crate::{CliFailure, CliResult};

pub(crate) const STORE_STEMS: [&str; 2] = ["product", "subagents"];

pub(crate) fn backup_count(paths: &PlakPaths) -> usize {
    std::fs::read_dir(paths.state_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry.file_name().to_str().is_some_and(|name| {
                    STORE_STEMS.into_iter().any(|stem| {
                        name.strip_prefix(&format!("{stem}.pre-revision-"))
                            .and_then(|name| name.strip_suffix(".sqlite3"))
                            .is_some_and(|revision| {
                                revision
                                    .parse::<i64>()
                                    .is_ok_and(|value| value > 0 && value.to_string() == revision)
                            })
                    })
                })
        })
        .count()
}

pub(crate) fn databases(paths: &PlakPaths) -> [(Schema, PathBuf); 2] {
    [
        (super::schema::schema(), paths.product_database()),
        (database::schema(), paths.subagents_database()),
    ]
}

/// Fence only foreign revisions before touching any store. Ordinary opens
/// still own pending-revision refusals and corruption diagnostics.
pub(crate) fn refuse_incompatible(paths: &PlakPaths) -> Result<(), String> {
    for (schema, path) in databases(paths) {
        if let Err(AdmissionError::Unsupported(message)) = schema.check_revision(&path) {
            return Err(message);
        }
    }
    Ok(())
}

#[cfg(feature = "web")]
pub(crate) mod host {
    use super::{AdmissionError, CliFailure, CliResult, PlakPaths, databases};
    use plakous_recursive_subagents_store::admission::Status;

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
