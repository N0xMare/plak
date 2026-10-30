use super::{
    Arc, BoundaryController, ChildReply, CrashBoundary, EvidenceKind, FsyncEvidenceLog,
    PROTOCOL_VERSION, PathBuf, future, io, write_reply,
};

impl BoundaryController {
    /// Creates a loopback boundary controller backed by one fsynced evidence file.
    ///
    /// # Errors
    ///
    /// Returns an error when the evidence file cannot be opened and fsynced.
    pub fn from_evidence_path(
        cut: CrashBoundary,
        evidence_path: impl Into<PathBuf>,
    ) -> io::Result<Self> {
        Ok(Self::new(
            cut,
            Arc::new(FsyncEvidenceLog::open(evidence_path)?),
        ))
    }

    /// Creates a loopback controller that records the same fsynced physical
    /// evidence without writing process-protocol frames to stdout.
    ///
    /// This is intended for repository tests whose stdout is an application
    /// terminal rather than the crash harness control channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the evidence file cannot be opened and fsynced.
    pub fn quiet_from_evidence_path(
        cut: CrashBoundary,
        evidence_path: impl Into<PathBuf>,
    ) -> io::Result<Self> {
        Ok(Self {
            cut,
            evidence: Arc::new(FsyncEvidenceLog::open(evidence_path)?),
            announce_stdout: false,
            release_file: None,
        })
    }

    /// Creates a quiet controller whose selected provider/effect boundary can
    /// be released by atomically creating `release_file` from another process.
    ///
    /// This retains the crash harness's physical dispatch evidence while
    /// allowing a real-terminal test to deterministically exercise UI actions
    /// during a long-running turn.
    ///
    /// # Errors
    ///
    /// Returns an error when the evidence file cannot be opened and fsynced.
    pub fn quiet_gated_from_evidence_path(
        cut: CrashBoundary,
        evidence_path: impl Into<PathBuf>,
        release_file: impl Into<PathBuf>,
    ) -> io::Result<Self> {
        Ok(Self {
            cut,
            evidence: Arc::new(FsyncEvidenceLog::open(evidence_path)?),
            announce_stdout: false,
            release_file: Some(Arc::new(release_file.into())),
        })
    }

    pub(crate) fn new(cut: CrashBoundary, evidence: Arc<FsyncEvidenceLog>) -> Self {
        Self {
            cut,
            evidence,
            announce_stdout: true,
            release_file: None,
        }
    }

    pub(super) async fn pause_at_selected_boundary(&self, selected: bool) {
        if !selected {
            return;
        }
        let Some(release_file) = &self.release_file else {
            future::pending::<()>().await;
            return;
        };
        while !release_file.exists() {
            std::thread::sleep(std::time::Duration::from_millis(1));
            tokio::task::yield_now().await;
        }
    }

    /// A gated pause that a cancelled model call leaves immediately.
    ///
    /// A provider that ignores its cancellation token is an *unjoinable*
    /// operation, and plakous correctly reports an unjoinable cancel as
    /// recovery-required rather than as an interrupt. Modelling the ordinary
    /// §6.1 interrupt therefore needs a gate that a real provider's
    /// cancellation would also unblock. Only the gated form observes the token:
    /// an ungated crash boundary still parks forever, because the child process
    /// is meant to be killed there.
    pub(super) async fn pause_at_selected_boundary_until_cancelled(
        &self,
        selected: bool,
        cancellation: &plakous::model::CancellationToken,
    ) {
        if !selected {
            return;
        }
        let Some(release_file) = &self.release_file else {
            future::pending::<()>().await;
            return;
        };
        while !release_file.exists() && !cancellation.is_cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(1));
            tokio::task::yield_now().await;
        }
    }

    pub(super) fn announce(
        &self,
        boundary: CrashBoundary,
        kind: EvidenceKind,
        round: Option<u64>,
    ) -> io::Result<bool> {
        self.announce_operation(boundary, kind, round, None)
    }

    pub(super) fn announce_operation(
        &self,
        boundary: CrashBoundary,
        kind: EvidenceKind,
        round: Option<u64>,
        operation_id: Option<String>,
    ) -> io::Result<bool> {
        let evidence_sequence = self.evidence.append(kind, round, boundary, operation_id)?;
        if self.announce_stdout {
            write_reply(&ChildReply::Boundary {
                version: PROTOCOL_VERSION,
                boundary,
                evidence_kind: boundary.evidence_kind(),
                evidence_sequence,
            })?;
        }
        Ok(self.cut == boundary)
    }
}

pub(super) async fn pause_if(should_pause: bool) {
    if should_pause {
        future::pending::<()>().await;
    }
}
