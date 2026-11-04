//! Job output files on a target without a host filesystem.
//!
//! [`JobOutputRoot::open`] refuses, so no root exists, no service binds one,
//! and every query over the binding finds nothing.

use std::path::Path;
use std::time::SystemTime;

use super::job_output_types::{JobOutputFile, JobOutputSlice};
use super::{ExecutionBackendError, ExecutionBackendErrorKind, backend_error};

fn unsupported() -> ExecutionBackendError {
    backend_error(
        ExecutionBackendErrorKind::Unsupported,
        "job output files need a unix host",
    )
    .with_proven_no_effect(true)
}

/// One host-owned private directory under which job output files live.
///
/// This target has none: [`Self::open`] always refuses.
#[derive(Clone, Debug)]
pub struct JobOutputRoot {
    _unconstructible: (),
}

impl JobOutputRoot {
    /// Refuses: job output files need a unix host.
    ///
    /// # Errors
    /// Always `Unsupported`.
    pub fn open(_path: &Path) -> Result<Self, ExecutionBackendError> {
        Err(unsupported())
    }

    /// The root's absolute path. No root exists on this target.
    #[must_use]
    pub fn path(&self) -> &str {
        ""
    }

    /// Deletes stale job output files. No root exists on this target.
    pub fn sweep_stale(&self, _now: SystemTime) {}
}

#[derive(Clone)]
pub(super) enum IssuedJobOutput {}

pub(super) struct JobOutputs;

impl JobOutputs {
    pub(super) const fn new(
        _root: JobOutputRoot,
        _session: plakous_model::SessionId,
        _branch: plakous_model::BranchId,
        _owner: [u8; 8],
    ) -> Self {
        Self
    }

    pub(super) const fn scope(&self) -> JobOutputScope {
        JobOutputScope
    }

    pub(super) const fn issued(&self, _numbers: &[u32]) -> Vec<IssuedJobOutput> {
        Vec::new()
    }

    pub(super) const fn file(&self, _number: u32) -> Option<JobOutputFile> {
        None
    }

    pub(super) const fn admit(&self, _path: &str) -> Option<JobOutputAdmission> {
        None
    }

    pub(super) const fn retire(&mut self, _names: &[String], _paths: &[String]) {}

    pub(super) fn create(
        &mut self,
        _number: u32,
    ) -> Result<JobOutputWriter, ExecutionBackendError> {
        Err(unsupported())
    }
}

pub(super) struct JobOutputScope;

impl JobOutputScope {
    pub(super) const fn prepare_create(
        &self,
        _now: SystemTime,
    ) -> (Vec<String>, Vec<String>, bool) {
        (Vec::new(), Vec::new(), false)
    }

    pub(super) const fn release(
        &self,
        _numbers: &[u32],
        _own: &[IssuedJobOutput],
        _now: SystemTime,
    ) -> Vec<String> {
        Vec::new()
    }
}

#[expect(dead_code, reason = "no binding exists here to admit an issued path")]
pub(super) enum JobOutputAdmission {
    Issued(IssuedJobOutputLocation),
    Refused(ExecutionBackendError),
}

pub(super) enum IssuedJobOutputLocation {}

impl IssuedJobOutputLocation {
    pub(super) fn read(
        &self,
        _offset: u64,
        _max_bytes: u64,
    ) -> Result<JobOutputSlice, ExecutionBackendError> {
        match *self {}
    }
}

pub(super) enum JobOutputWriter {}

impl JobOutputWriter {
    pub(super) fn append(&mut self, _bytes: &[u8]) {
        match *self {}
    }
}
