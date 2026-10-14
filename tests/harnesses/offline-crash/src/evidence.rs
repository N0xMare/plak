use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::protocol::{CrashBoundary, PROTOCOL_VERSION};

/// One fsynced loopback-provider/effect evidence row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceRecord {
    /// Evidence schema version.
    pub version: u16,
    /// Monotonic sequence within one evidence file.
    pub sequence: u64,
    /// Stable physical operation class.
    pub kind: EvidenceKind,
    /// Provider round, when applicable.
    pub round: Option<u64>,
    /// Stable physical request/effect identity at the injected seam.
    pub operation_id: Option<String>,
    /// Public boundary certified by this row.
    pub boundary: CrashBoundary,
}

/// Closed physical evidence classes emitted by loopback provider/effect seams.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// The facade returned an exact old checkpoint after release.
    OldCheckpointReleased,
    /// A prepared provider call crossed first-poll dispatch admission.
    ProviderDispatched,
    /// The loopback provider produced its terminal aggregate.
    ProviderOutcome,
    /// The registered mutating tool was invoked.
    EffectDispatched,
    /// The registered mutating tool produced its typed outcome.
    EffectOutcome,
    /// The facade returned an exact new checkpoint after release.
    NewCheckpointReleased,
}

#[derive(Debug)]
pub(crate) struct FsyncEvidenceLog {
    path: PathBuf,
    next: AtomicU64,
}

impl FsyncEvidenceLog {
    pub(crate) fn append_summary_dispatch(
        &self,
        round: u64,
        boundary: CrashBoundary,
        operation_id: String,
    ) -> io::Result<()> {
        Self::open(self.path.with_extension("summary.jsonl"))?.append(
            EvidenceKind::ProviderDispatched,
            Some(round),
            boundary,
            Some(operation_id),
        )?;
        Ok(())
    }

    pub(crate) fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.sync_all()?;
        sync_parent(&path)?;
        let existing = read_evidence(&path)?;
        let next = existing
            .last()
            .map_or(1, |record| record.sequence.saturating_add(1));
        Ok(Self {
            path,
            next: AtomicU64::new(next),
        })
    }

    pub(crate) fn append(
        &self,
        kind: EvidenceKind,
        round: Option<u64>,
        boundary: CrashBoundary,
        operation_id: Option<String>,
    ) -> io::Result<u64> {
        let sequence = self.next.fetch_add(1, Ordering::AcqRel);
        let row = EvidenceRecord {
            version: PROTOCOL_VERSION,
            sequence,
            kind,
            round,
            operation_id,
            boundary,
        };
        let mut encoded = serde_json::to_vec(&row).map_err(io::Error::other)?;
        encoded.push(b'\n');
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        file.write_all(&encoded)?;
        file.flush()?;
        file.sync_data()?;
        Ok(sequence)
    }
}

/// Reads and strictly validates a complete durable evidence file.
///
/// # Errors
///
/// Returns an I/O or strict codec/sequence error for missing durability or a
/// malformed, unsupported, or non-contiguous evidence row.
pub fn read_evidence(path: impl AsRef<Path>) -> io::Result<Vec<EvidenceRecord>> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let record: EvidenceRecord = serde_json::from_str(&line).map_err(io::Error::other)?;
        if record.version != PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported evidence protocol version",
            ));
        }
        let expected = u64::try_from(records.len())
            .map_err(io::Error::other)?
            .saturating_add(1);
        if record.sequence != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "non-contiguous evidence sequence",
            ));
        }
        records.push(record);
    }
    Ok(records)
}

pub(crate) fn write_fsynced_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("tmp");
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_requests_append_across_provider_relaunches() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempfile::tempdir()?;
        let path = root.path().join("evidence.jsonl");
        let first = FsyncEvidenceLog::open(&path)?;
        first.append_summary_dispatch(
            1,
            CrashBoundary::ProviderDispatchOne,
            "summary-one".to_owned(),
        )?;
        drop(first);
        let second = FsyncEvidenceLog::open(&path)?;
        second.append_summary_dispatch(
            1,
            CrashBoundary::ProviderDispatchOne,
            "summary-two".to_owned(),
        )?;
        let summaries = read_evidence(path.with_extension("summary.jsonl"))?;
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].operation_id.as_deref(), Some("summary-one"));
        assert_eq!(summaries[1].operation_id.as_deref(), Some("summary-two"));
        assert!(read_evidence(&path)?.is_empty());
        Ok(())
    }
}
