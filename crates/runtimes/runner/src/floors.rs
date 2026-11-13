//! The owner floor a runner keeps across restarts: the newest owner it
//! accepted for each host's line, and the host instance that holds it (spec
//! 01 §8.4). A restarted runner refuses an owner older than its floor, so a
//! host that lost a line to a newer head cannot act on it through a runner
//! restart, and accepts the floor owner itself only from its holder, so a
//! sibling process of the host cannot take the line by presenting it.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use plakous_runner_protocol::LineOwner;
use serde::{Deserialize, Serialize};

/// One line's floor: the newest owner accepted for it and the host instance
/// whose transfer it accepted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Floor {
    /// The accepted owner.
    pub owner: LineOwner,
    /// The host instance holding the line.
    pub holder: String,
    /// Irreversible native-control ban after job-only retention transfer.
    pub retention_only: bool,
}

/// Where a runner records accepted owners.
pub trait OwnerFloors: Send + Sync {
    /// Every recorded floor, with its host.
    fn load(&self) -> Vec<(String, Floor)>;

    /// Records `floor` as `host`'s floor for its line. It returns only once
    /// the record is durable; the runner accepts the owner after that.
    ///
    /// # Errors
    ///
    /// The record cannot be written.
    fn record(&self, host: &str, floor: &Floor) -> io::Result<()>;
}

/// Owner floors in one JSON file, replaced atomically on every change.
pub struct FileOwnerFloors {
    path: PathBuf,
    floors: Mutex<BTreeMap<String, BTreeMap<String, Floor>>>,
}

impl FileOwnerFloors {
    /// The floors recorded at `path`; a missing file records none.
    ///
    /// # Errors
    ///
    /// The file exists but cannot be read or decoded, including a file of
    /// an earlier format: the error names the file and the remedy. There is
    /// no fallback, since dropping floors silently would drop the fence they
    /// keep.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let floors = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "the runner's owner floors cannot be read ({error}); keep {} intact \
                         and use the retained matching build to finish, consume or disown \
                         its jobs before repairing the record without discarding owner fences",
                        path.display()
                    ),
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            floors: Mutex::new(floors),
        })
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl OwnerFloors for FileOwnerFloors {
    fn load(&self) -> Vec<(String, Floor)> {
        let floors = self.floors.lock().unwrap_or_else(PoisonError::into_inner);
        floors
            .iter()
            .flat_map(|(host, lines)| {
                lines
                    .values()
                    .map(move |floor| (host.clone(), floor.clone()))
            })
            .collect()
    }

    fn record(&self, host: &str, floor: &Floor) -> io::Result<()> {
        let mut floors = self.floors.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = floors.clone();
        next.entry(host.to_owned())
            .or_default()
            .insert(floor.owner.line.clone(), floor.clone());
        write_atomically(
            &self.path,
            &serde_json::to_vec(&next).map_err(io::Error::other)?,
        )?;
        *floors = next;
        Ok(())
    }
}

/// Writes `bytes` to `path` through a synced temporary file in the same
/// directory, then syncs the directory, so a crash leaves the old file or
/// the new one.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)?;
    std::fs::File::open(directory)?.sync_all()
}
