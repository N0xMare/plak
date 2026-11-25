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

pub(in crate::host) use super::preservation::Evidence as EvidenceLevel;
use super::{
    Identity, bindings,
    preservation::{Evidence, Snapshot},
};
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliFailureKind, CliResult};

const LIMIT: u64 = 512 << 20;
const FRAME: u64 = 1024;

#[derive(Deserialize, Serialize)]
pub(in crate::host) struct Ticket {
    nonce: Uuid,
    source: String,
    destination: String,
    process_birth: String,
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
struct Verdict {
    nonce: Uuid,
    outcome: Outcome,
}

#[derive(Deserialize, Serialize)]
pub(super) enum Outcome {
    Pending,
    Ready(String, Evidence),
    Failed(Failure),
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
    pub(super) fn new(identity: &Identity, destination: &str) -> CliResult<Self> {
        let source = HostInstance::parse(&identity.instance).map_err(error)?;
        Ok(Self {
            nonce: Uuid::new_v4(),
            source: identity.instance.clone(),
            destination: destination.to_owned(),
            process_birth: process_birth(source.pid())?,
        })
    }

    pub(super) fn stage(
        &self,
        paths: &PlakPaths,
        snapshot: &Snapshot,
        bindings: &bindings::Baseline,
    ) -> CliResult<tempfile::NamedTempFile> {
        stage(&baseline(paths), LIMIT, |out| {
            serde_json::to_writer(&mut *out, self).map_err(error)?;
            out.write_all(b"\n").map_err(error)?;
            serde_json::to_writer(out, &Payload { snapshot, bindings }).map_err(error)
        })
    }

    pub(super) fn publish(paths: &PlakPaths, staging: tempfile::NamedTempFile) -> CliResult<()> {
        publish(staging, &baseline(paths))
    }

    pub(super) fn clear(paths: &PlakPaths) {
        let _ = fs::remove_file(baseline(paths));
        let _ = fs::remove_file(verdict_path(paths));
    }

    pub(super) fn clear_stale(paths: &PlakPaths) -> CliResult<()> {
        refuse_cold_start(paths)?;
        Self::clear(paths);
        Ok(())
    }

    pub(super) fn outcome(&self, paths: &PlakPaths) -> CliResult<Option<Outcome>> {
        let Some(file) = open(&verdict_path(paths), FRAME)? else {
            return Ok(None);
        };
        let verdict: Verdict = decode(file, FRAME)?;
        Ok((verdict.nonce == self.nonce).then_some(verdict.outcome))
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

    pub(in crate::host) fn record_pending(&self, paths: &PlakPaths) -> CliResult<()> {
        self.record(paths, Outcome::Pending)
    }

    pub(in crate::host) fn record_ready(
        &self,
        paths: &PlakPaths,
        evidence: Evidence,
    ) -> CliResult<()> {
        let instance = plakous_recursive_subagents::tree::custody_host()
            .instance
            .to_string();
        self.record(paths, Outcome::Ready(instance, evidence))
    }

    pub(in crate::host) fn record_failure(&self, paths: &PlakPaths, failure: Failure) {
        if let Err(error) = self.record(paths, Outcome::Failed(failure)) {
            eprintln!("{}", error.message());
        }
    }

    fn record(&self, paths: &PlakPaths, outcome: Outcome) -> CliResult<()> {
        let path = verdict_path(paths);
        stage(&path, FRAME, |out| {
            serde_json::to_writer(
                out,
                &Verdict {
                    nonce: self.nonce,
                    outcome,
                },
            )
            .map_err(error)
        })
        .and_then(|file| {
            file.as_file().sync_all().map_err(error)?;
            publish(file, &path)?;
            File::open(root(paths))
                .and_then(|dir| dir.sync_all())
                .map_err(error)
        })
    }
}

#[derive(Deserialize, Serialize)]
struct Payload<S, B> {
    snapshot: S,
    bindings: B,
}

fn refuse_cold_start(paths: &PlakPaths) -> CliResult<()> {
    if let Some(file) = open(&verdict_path(paths), FRAME)?
        && !matches!(decode::<Verdict>(file, FRAME)?.outcome, Outcome::Ready(..))
    {
        return Err(CliFailure::classified(
            CliFailureKind::RecoveryRequired,
            "host_upgrade_fix_forward_required: an inherited upgrade did not reach Ready; inspect the destination logs, repair the failure, then clear the stopped home's upgrade-checkpoint-verdict before restarting",
        ));
    }
    Ok(())
}

pub(in crate::host) fn clear_cold_start(paths: &PlakPaths) -> CliResult<()> {
    // The cold owner already holds the home lock: a checkpoint with no
    // unfinished verdict cannot describe a still-running surrender.
    Ticket::clear_stale(paths)
}

pub(in crate::host) struct Checkpoint {
    pub(in crate::host) ticket: Ticket,
    body: Option<BufReader<File>>,
}

impl Checkpoint {
    fn load(
        paths: &PlakPaths,
        binary: &str,
        matches: impl FnOnce(&Ticket) -> CliResult<bool>,
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
        Ok(
            (ticket.destination == binary && matches(&ticket)?).then_some(Self {
                ticket,
                body: Some(body),
            }),
        )
    }

    pub(super) fn held(
        paths: &PlakPaths,
        source: &str,
        binary: &str,
        bindings: &bindings::Baseline,
    ) -> CliResult<()> {
        if let Some(checkpoint) = Self::load(paths, binary, |ticket| Ok(ticket.source == source))? {
            let held = Snapshot::capture(paths, None)?.0;
            Ticket::publish(paths, checkpoint.ticket.stage(paths, &held, bindings)?)?;
        }
        Ok(())
    }

    pub(in crate::host) fn inherited(
        paths: &PlakPaths,
        binary: &str,
        source_instance: Option<&str>,
    ) -> CliResult<Option<Self>> {
        Self::load(paths, binary, |ticket| {
            let pid = std::process::id();
            let source = HostInstance::parse(&ticket.source).map_err(error)?;
            // The previous descriptor speaker did not include the instance
            // nonce. Its same-PID exec must still match the staged process birth.
            Ok(source.pid() == pid
                && source_instance.is_none_or(|instance| instance == ticket.source)
                && process_birth(pid)? == ticket.process_birth)
        })
    }

    pub(in crate::host) fn verify_destination(
        &mut self,
        paths: &PlakPaths,
    ) -> CliResult<(Evidence, bindings::Baseline)> {
        let body = self
            .body
            .take()
            .ok_or_else(|| error("checkpoint already verified"))?;
        let before: Payload<Snapshot, bindings::Baseline> = decode(body, LIMIT)?;
        let (after, accepted) = Snapshot::capture(paths, Some(&before.snapshot))?;
        before.snapshot.compare(&after, &accepted, paths)?;
        Ok((before.snapshot.evidence, before.bindings))
    }
}

#[cfg(target_os = "linux")]
fn process_birth(pid: u32) -> CliResult<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).map_err(error)?;
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .filter(|birth| birth.bytes().all(|byte| byte.is_ascii_digit()))
        .map(str::to_owned)
        .ok_or_else(|| error("cannot read the source process birth"))
}

#[cfg(target_os = "macos")]
fn process_birth(pid: u32) -> CliResult<String> {
    let birth = super::process::ps_field(pid, "lstart=", super::PROBE_BOUND)?;
    if birth.trim().is_empty() {
        return Err(error("cannot read the source process birth"));
    }
    Ok(birth.trim().to_owned())
}

fn root(paths: &PlakPaths) -> PathBuf {
    paths.home().join("host")
}

fn baseline(paths: &PlakPaths) -> PathBuf {
    root(paths).join("upgrade-checkpoint")
}

fn verdict_path(paths: &PlakPaths) -> PathBuf {
    root(paths).join("upgrade-checkpoint-verdict")
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
    write: impl FnOnce(&mut BufWriter<Capped<'_>>) -> CliResult<()>,
) -> CliResult<tempfile::NamedTempFile> {
    let parent = path
        .parent()
        .ok_or_else(|| error("missing checkpoint directory"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(error)?;
    {
        let mut buffered = BufWriter::new(Capped {
            file: temp.as_file_mut(),
            left: limit,
        });
        write(&mut buffered)?;
        buffered.flush().map_err(error)?;
    }
    private(temp.as_file(), limit)?;
    Ok(temp)
}

struct Capped<'a> {
    file: &'a mut File,
    left: u64,
}

impl std::io::Write for Capped<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.left {
            return Err(std::io::Error::other("checkpoint exceeds its byte bound"));
        }
        let written = self.file.write(bytes)?;
        self.left -= written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn publish(temp: tempfile::NamedTempFile, path: &Path) -> CliResult<()> {
    // Only the verdict and detached backup survive a machine crash.
    temp.persist(path).map_err(|failure| error(failure.error))?;
    Ok(())
}

fn decode<T: DeserializeOwned>(read: impl std::io::Read, limit: u64) -> CliResult<T> {
    serde_json::from_reader(read.take(limit)).map_err(error)
}

fn error(failure: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(format!("host_upgrade_checkpoint_refused: {failure}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn inherited_checkpoint_rejects_a_different_instance_on_the_same_pid() {
        let (_root, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        crate::commands::runner::private_directory(&root(&paths)).unwrap();
        let binary = "0123456789ab";
        let instance = HostInstance::generate().to_string();
        let other = HostInstance::generate().to_string();
        let identity = Identity {
            protocol: crate::carry::upgrade::PROTOCOL,
            key: crate::home::state::state_key(),
            instance: instance.clone(),
            binary: binary.to_owned(),
            deploy_idle: Some(true),
        };
        let mut ticket = Ticket::new(&identity, binary).unwrap();
        let publish_ticket = |ticket: &Ticket| {
            let scratch = stage(&baseline(&paths), LIMIT, |out| {
                serde_json::to_writer(&mut *out, ticket).unwrap();
                out.write_all(b"\n").map_err(error)
            })
            .unwrap();
            Ticket::publish(&paths, scratch).unwrap();
        };
        publish_ticket(&ticket);
        assert!(
            Checkpoint::inherited(&paths, binary, Some(&instance))
                .unwrap()
                .is_some()
        );
        assert!(
            Checkpoint::inherited(&paths, binary, Some(&other))
                .unwrap()
                .is_none()
        );
        assert!(
            Checkpoint::inherited(&paths, binary, None)
                .unwrap()
                .is_some()
        );
        ticket.process_birth = "recycled".into();
        publish_ticket(&ticket);
        assert!(
            Checkpoint::inherited(&paths, binary, None)
                .unwrap()
                .is_none()
        );
        clear_cold_start(&paths).unwrap();
        assert!(!baseline(&paths).exists());
    }
}
