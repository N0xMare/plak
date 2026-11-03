//! The host's record of the keyed runner calls it sends (spec 01 §8.4): each
//! is claimed, with the runner instance it is dispatched to, before it is
//! sent, so a host that restarts replays each by key and digest and learns
//! whether it ran rather than guessing.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use plakous_runner_protocol::Request;
use serde::{Deserialize, Serialize};

/// One keyed call the host sent or was about to send.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEntry {
    /// The request as sent; a replay sends it again under a new request id.
    pub request: Request,
    /// When the host claimed it, in milliseconds since the Unix epoch on
    /// the host's clock. Only the host reads it, to age a replay.
    pub claimed_at_ms: u64,
    /// The runner instance the call is dispatched to. A replay to another
    /// instance proves nothing, so it is not sent.
    pub dispatched_to: String,
}

/// Where a host records the keyed calls it sends to one runner.
pub trait CallJournal: Send + Sync {
    /// Records `entry` before its call is sent.
    ///
    /// # Errors
    ///
    /// The record cannot be written; the call is then not sent.
    fn claim(&self, entry: &JournalEntry) -> io::Result<()>;

    /// Forgets `request_id`: its answer was received.
    ///
    /// # Errors
    ///
    /// The record cannot be removed.
    fn settle(&self, request_id: &str) -> io::Result<()>;

    /// Every call claimed and not settled.
    ///
    /// # Errors
    ///
    /// The record cannot be read or does not decode.
    fn pending(&self) -> io::Result<Vec<JournalEntry>>;
}

/// A journal of one file per call in a directory.
pub struct FileCallJournal {
    directory: PathBuf,
}

impl FileCallJournal {
    /// The journal in `directory`, created with mode 0700 when missing.
    ///
    /// # Errors
    ///
    /// The directory cannot be created.
    pub fn open(directory: impl Into<PathBuf>) -> io::Result<Self> {
        let directory = directory.into();
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&directory)?;
        Ok(Self { directory })
    }

    fn path(&self, request_id: &str) -> PathBuf {
        // Request ids are a client's uuid, a dot and a counter.
        let name: String = request_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        self.directory.join(format!("{name}.json"))
    }

    fn write(&self, entry: &JournalEntry) -> io::Result<()> {
        let path = self.path(&entry.request.request_id);
        let temporary = path.with_extension("tmp");
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(&temporary)?;
            file.write_all(&serde_json::to_vec(entry).map_err(io::Error::other)?)?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &path)?;
        sync_directory(&self.directory)
    }
}

impl CallJournal for FileCallJournal {
    fn claim(&self, entry: &JournalEntry) -> io::Result<()> {
        self.write(entry)
    }

    fn settle(&self, request_id: &str) -> io::Result<()> {
        match std::fs::remove_file(self.path(request_id)) {
            Ok(()) => sync_directory(&self.directory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn pending(&self) -> io::Result<Vec<JournalEntry>> {
        let mut entries = Vec::new();
        for item in std::fs::read_dir(&self.directory)? {
            let path = item?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            entries.push(serde_json::from_slice(&std::fs::read(&path)?).map_err(io::Error::other)?);
        }
        entries.sort_by_key(|entry: &JournalEntry| entry.claimed_at_ms);
        Ok(entries)
    }
}

fn sync_directory(directory: &Path) -> io::Result<()> {
    std::fs::File::open(directory)?.sync_all()
}
