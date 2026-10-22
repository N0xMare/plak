//! Unavailable native JJ operations for builds without the backend.
pub(super) const ENABLED: bool = false;

use super::occupancy_vcs::{AdoptFailure, AdoptLanding, MintedCopy, VcsError, VcsSnapshot};
use std::path::Path;
fn unavailable() -> VcsError {
    VcsError::mint_unavailable("this build has no jj support; use a build with the jj feature")
}
pub(super) fn snapshot(_: &Path) -> Result<VcsSnapshot, VcsError> {
    Err(unavailable())
}
pub(super) fn mint(
    _: &Path,
    _: &str,
    _: &Path,
    _: Option<&VcsSnapshot>,
) -> Result<MintedCopy, VcsError> {
    Err(unavailable())
}
pub(super) fn adopt(
    _: &Path,
    _: &Path,
    _: &str,
    _: &VcsSnapshot,
    _: Option<&str>,
) -> Result<AdoptLanding, AdoptFailure> {
    Err(AdoptFailure::Unavailable(unavailable().detail))
}
pub(super) fn diverged(_: &Path, _: &str) -> Result<bool, VcsError> {
    Err(unavailable())
}
pub(super) fn forget(_: &Path, _: &str) -> Result<(), VcsError> {
    Err(unavailable())
}
pub(super) fn over(_: &Path, _: &str, _: &VcsSnapshot) -> bool {
    false
}
pub(super) fn dirty(_: &Path) -> bool {
    true
}
pub(super) fn ahead_behind(_: &Path, _: &Path) -> Option<(u64, u64)> {
    None
}
#[cfg(test)]
pub(super) fn resolve(_: &Path, _: &str) -> Option<String> {
    None
}

pub(super) fn pin(_: &Path, _: &str, _: &str, _: Option<&str>) -> Result<(), VcsError> {
    Err(unavailable())
}
