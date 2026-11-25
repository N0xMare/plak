//! One private checkpoint slot per home. The requester serializes the slot and
//! the source coordinator serializes surrender. The slot survives requester
//! loss so a previous coordinator can still hand off to a gated destination.

use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, BufWriter, Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use plakous_recursive_subagents::tree::HostInstance;
use rustix::fs::{Mode, OFlags};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use super::{Identity, preservation::Snapshot};
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliFailureKind, CliResult};

const LIMIT: u64 = 512 << 20;
const FRAME: u64 = 1024;

#[derive(Deserialize, Serialize)]
pub(in crate::host) struct Ticket {
    nonce: Uuid,
    source: String,
    destination: String,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(in crate::host) enum Failure {
    Preservation,
    Health,
    Startup,
}

impl Failure {
    pub(in crate::host) fn fix_forward(self) -> CliFailure {
        let phase = match self {
            Self::Preservation => "preservation",
            Self::Health => "health",
            Self::Startup => "startup",
        };
        CliFailure::classified(
            CliFailureKind::RecoveryRequired,
            format!(
                "host_upgrade_fix_forward_required: destination {phase} failed after surrender; inspect the destination host log and fix forward without restoring a store backup"
            ),
        )
    }
}

#[derive(Deserialize, Serialize)]
struct Refused {
    nonce: Uuid,
    failure: Failure,
}

pub(super) fn claim(paths: &PlakPaths) -> CliResult<File> {
    let descriptor = rustix::fs::open(
        root(paths).join("upgrade-checkpoint.lock"),
        OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(error)?;
    let file = File::from(descriptor);
    private(&file, 0)?;
    file.try_lock().map_err(|_| {
        CliFailure::classified(
            CliFailureKind::Retryable,
            "host_upgrade_checkpoint_busy: another requester owns this home's upgrade",
        )
    })?;
    Ok(file)
}

impl Ticket {
    pub(super) fn new(identity: &Identity, destination: &str) -> Self {
        Self {
            nonce: Uuid::new_v4(),
            source: identity.instance.clone(),
            destination: destination.to_owned(),
        }
    }

    pub(super) fn stage(
        &self,
        paths: &PlakPaths,
        snapshot: &Snapshot,
    ) -> CliResult<tempfile::NamedTempFile> {
        stage(&baseline(paths), LIMIT, |out| {
            serde_json::to_writer(&mut *out, self).map_err(error)?;
            out.write_all(b"\n").map_err(error)?;
            serde_json::to_writer(out, snapshot).map_err(error)
        })
    }

    pub(super) fn publish(paths: &PlakPaths, staging: tempfile::NamedTempFile) -> CliResult<()> {
        publish(staging, &baseline(paths))
    }

    pub(super) fn clear(paths: &PlakPaths) {
        let _ = fs::remove_file(baseline(paths));
        let _ = fs::remove_file(refusal(paths));
    }

    pub(super) fn failure(&self, paths: &PlakPaths) -> CliResult<Option<Failure>> {
        let Some(file) = open(&refusal(paths), FRAME)? else {
            return Ok(None);
        };
        let refusal: Refused = decode(file, FRAME)?;
        Ok((refusal.nonce == self.nonce).then_some(refusal.failure))
    }

    pub(super) fn successor(&self, identity: &Identity) -> bool {
        identity.instance != self.source
            && HostInstance::parse(&identity.instance)
                .ok()
                .map(HostInstance::pid)
                == HostInstance::parse(&self.source)
                    .ok()
                    .map(HostInstance::pid)
    }

    pub(in crate::host) fn record(&self, paths: &PlakPaths, failure: Failure) {
        let path = refusal(paths);
        let saved = stage(&path, FRAME, |out| {
            serde_json::to_writer(
                out,
                &Refused {
                    nonce: self.nonce,
                    failure,
                },
            )
            .map_err(error)
        })
        .and_then(|file| publish(file, &path));
        if let Err(error) = saved {
            eprintln!("{}", error.message());
        }
    }
}

pub(in crate::host) struct Checkpoint {
    pub(in crate::host) ticket: Ticket,
    body: BufReader<File>,
}

impl Checkpoint {
    fn load(
        paths: &PlakPaths,
        binary: &str,
        matches: impl FnOnce(&str) -> bool,
    ) -> CliResult<Option<Self>> {
        let Some(file) = open(&baseline(paths), LIMIT)? else {
            return Ok(None);
        };
        let mut body = BufReader::new(file);
        let mut header = Vec::new();
        (&mut body)
            .take(FRAME)
            .read_until(b'\n', &mut header)
            .map_err(error)?;
        if header.last() != Some(&b'\n') {
            return Err(error("missing checkpoint identity"));
        }
        let ticket: Ticket = serde_json::from_slice(&header).map_err(error)?;
        Ok((ticket.destination == binary && matches(&ticket.source))
            .then_some(Self { ticket, body }))
    }

    pub(super) fn held(paths: &PlakPaths, source: &str, binary: &str) -> CliResult<()> {
        if let Some(mut checkpoint) = Self::load(paths, binary, |instance| instance == source)? {
            let held = checkpoint.verify(paths)?;
            Ticket::publish(paths, checkpoint.ticket.stage(paths, &held)?)?;
        }
        Ok(())
    }

    pub(in crate::host) fn inherited(paths: &PlakPaths, binary: &str) -> CliResult<Option<Self>> {
        Self::load(paths, binary, |instance| {
            HostInstance::parse(instance).is_ok_and(|source| source.pid() == std::process::id())
        })
    }

    pub(in crate::host) fn verify_destination(&mut self, paths: &PlakPaths) -> CliResult<()> {
        self.verify(paths).map(drop)
    }

    fn verify(&mut self, paths: &PlakPaths) -> CliResult<Snapshot> {
        let before: Snapshot = decode(&mut self.body, LIMIT)?;
        let (after, accepted) = Snapshot::capture(paths, Some(&before))?;
        before.compare(&after, &accepted, paths)?;
        Ok(after)
    }
}

fn root(paths: &PlakPaths) -> PathBuf {
    paths.home().join("host")
}

fn baseline(paths: &PlakPaths) -> PathBuf {
    root(paths).join("upgrade-checkpoint")
}

fn refusal(paths: &PlakPaths) -> PathBuf {
    root(paths).join("upgrade-checkpoint-refused")
}

fn open(path: &Path, limit: u64) -> CliResult<Option<File>> {
    let descriptor = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(failure) => return Err(error(failure)),
    };
    let file = File::from(descriptor);
    private(&file, limit)?;
    Ok(Some(file))
}

fn private(file: &File, limit: u64) -> CliResult<()> {
    let meta = file.metadata().map_err(error)?;
    if !meta.is_file()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.len() > limit
    {
        return Err(error("checkpoint is not a bounded, private regular file"));
    }
    Ok(())
}

fn stage(
    path: &Path,
    limit: u64,
    write: impl FnOnce(&mut BufWriter<&mut File>) -> CliResult<()>,
) -> CliResult<tempfile::NamedTempFile> {
    let parent = path
        .parent()
        .ok_or_else(|| error("missing checkpoint directory"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(error)?;
    {
        let mut buffered = BufWriter::new(temp.as_file_mut());
        write(&mut buffered)?;
        buffered.flush().map_err(error)?;
    }
    private(temp.as_file(), limit)?;
    Ok(temp)
}

fn publish(temp: tempfile::NamedTempFile, path: &Path) -> CliResult<()> {
    // The checkpoint tracks a running process: after a machine crash no
    // inherited upgrade exists. Only the optional backup requires fsync.
    temp.persist(path).map_err(|failure| error(failure.error))?;
    Ok(())
}

fn decode<T: DeserializeOwned>(read: impl std::io::Read, limit: u64) -> CliResult<T> {
    serde_json::from_reader(read.take(limit)).map_err(error)
}

fn error(failure: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(format!("host_upgrade_checkpoint_refused: {failure}"))
}
