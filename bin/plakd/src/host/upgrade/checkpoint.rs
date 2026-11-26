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
    preservation::{Evidence, Snapshot, StoreIdentities},
};
use crate::carry::child_reopen::LegacyChildBaseline;
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliFailureKind, CliResult};

mod verdict;
pub(in crate::host) use verdict::operate;

const LIMIT: u64 = 512 << 20;
const FRAME: u64 = 1024;
const CHILD_AUDIT_LIMIT: u64 = 8 << 20;

#[derive(Deserialize, Serialize)]
struct LegacyChildEvidence<A> {
    nonce: Uuid,
    audit: A,
}

#[derive(Clone, Deserialize, Serialize)]
pub(in crate::host) struct Ticket {
    nonce: Uuid,
    source: String,
    destination: String,
    process_birth: String,
    runtime_expected: Option<bool>,
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
                "host_upgrade_fix_forward_required: destination {phase} failed after surrender; inspect `plakd host upgrade-verdict` and the destination log; fix forward without restoring a store backup"
            ),
        )
    }
}

/// One timed destination step after surrender, in order of completion.
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::host) enum GatePhase {
    StoppedRevisions,
    StoreHealthProduct,
    StoreHealthTree,
    StoreHealthRuntime,
    Preservation,
    Bindings,
    Startup,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(in crate::host) struct GateTiming {
    phase: GatePhase,
    elapsed_ms: u64,
}

impl GateTiming {
    pub(in crate::host) fn new(phase: GatePhase, elapsed: std::time::Duration) -> Self {
        Self {
            phase,
            elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        }
    }

    /// The destination logs this line and the requester prints it from the verdict.
    pub(in crate::host) fn detail(&self) -> String {
        let phase = match self.phase {
            GatePhase::StoppedRevisions => "stopped_revisions",
            GatePhase::StoreHealthProduct => "store_health_product",
            GatePhase::StoreHealthTree => "store_health_tree",
            GatePhase::StoreHealthRuntime => "store_health_runtime",
            GatePhase::Preservation => "preservation",
            GatePhase::Bindings => "bindings",
            GatePhase::Startup => "startup",
            GatePhase::Unknown => "unknown",
        };
        format!("destination phase={phase} elapsed_ms={}", self.elapsed_ms)
    }
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
            runtime_expected: None,
        })
    }

    pub(super) fn stage(
        &self,
        paths: &PlakPaths,
        snapshot: &Snapshot,
        bindings: &bindings::Baseline,
    ) -> CliResult<tempfile::NamedTempFile> {
        let mut ticket = self.clone();
        ticket.runtime_expected = Some(snapshot.runtime_present());
        stage(&baseline(paths), LIMIT, |out| {
            serde_json::to_writer(&mut *out, &ticket).map_err(error)?;
            out.write_all(b"\n").map_err(error)?;
            serde_json::to_writer(out, &Payload { snapshot, bindings }).map_err(error)
        })
    }

    pub(super) fn publish(paths: &PlakPaths, staging: tempfile::NamedTempFile) -> CliResult<()> {
        publish(staging, &baseline(paths))
    }

    pub(super) fn publish_child_audit(
        &self,
        paths: &PlakPaths,
        audit: &LegacyChildBaseline,
    ) -> CliResult<()> {
        if audit.is_empty() {
            return Ok(());
        }
        let path = child_audit_path(paths);
        let evidence = LegacyChildEvidence {
            nonce: self.nonce,
            audit,
        };
        let temp = stage(&path, CHILD_AUDIT_LIMIT, |out| {
            serde_json::to_writer(out, &evidence).map_err(error)
        })?;
        publish(temp, &path)
    }

    pub(super) fn clear(paths: &PlakPaths) {
        let _ = fs::remove_file(baseline(paths));
        let _ = fs::remove_file(child_audit_path(paths));
        let _ = fs::remove_file(verdict_path(paths));
    }

    pub(super) fn clear_stale(paths: &PlakPaths) -> CliResult<()> {
        refuse_cold_start(paths)?;
        Self::clear(paths);
        Ok(())
    }

    pub(super) fn outcome(
        &self,
        paths: &PlakPaths,
    ) -> CliResult<Option<(Outcome, Vec<GateTiming>)>> {
        verdict::outcome(paths, self.nonce)
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
        self.record(paths, Outcome::Pending, &[])
    }

    pub(in crate::host) fn record_ready(
        &self,
        paths: &PlakPaths,
        evidence: Evidence,
        timings: &[GateTiming],
    ) -> CliResult<()> {
        let instance = plakous_recursive_subagents::tree::custody_host()
            .instance
            .to_string();
        self.record(paths, Outcome::Ready(instance, evidence), timings)
    }

    pub(in crate::host) fn record_failure(
        &self,
        paths: &PlakPaths,
        failure: Failure,
        timings: &[GateTiming],
    ) {
        if let Err(error) = self.record(paths, Outcome::Failed(failure), timings) {
            eprintln!("{}", error.message());
        }
    }

    fn record(&self, paths: &PlakPaths, outcome: Outcome, timings: &[GateTiming]) -> CliResult<()> {
        let path = verdict_path(paths);
        let verdict = verdict::WriteVerdict::new(
            paths,
            self.nonce,
            self.runtime_expected,
            outcome,
            timings.to_vec(),
        )?;
        stage(&path, FRAME, |out| {
            serde_json::to_writer(out, &verdict).map_err(error)
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

fn check_store_identities(paths: &PlakPaths, nonce: Uuid) -> CliResult<()> {
    let unproven = |_| {
        CliFailure::classified(
            CliFailureKind::RecoveryRequired,
            "host_upgrade_acknowledge_baseline_unproven: keep the fence; recover the original staged checkpoint before acknowledging",
        )
    };
    let (_, body) = open_checkpoint(paths)
        .and_then(|found| {
            found
                .filter(|(ticket, _)| ticket.nonce == nonce)
                .ok_or_else(|| error("no matching checkpoint"))
        })
        .map_err(unproven)?;
    let evidence: Payload<StoreIdentities, serde::de::IgnoredAny> =
        decode(body, LIMIT).map_err(unproven)?;
    evidence.snapshot.verify(paths)
}

fn open_checkpoint(paths: &PlakPaths) -> CliResult<Option<(Ticket, BufReader<File>)>> {
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
    let ticket = serde_json::from_slice(&header).map_err(error)?;
    Ok(Some((ticket, body)))
}

fn refuse_cold_start(paths: &PlakPaths) -> CliResult<()> {
    if verdict::fenced(paths)? {
        return Err(CliFailure::classified(
            CliFailureKind::RecoveryRequired,
            "host_upgrade_fix_forward_required: an inherited upgrade did not reach Ready; inspect with `plakd host upgrade-verdict`, repair the cause, stop any serving predecessor, then run `plakd host upgrade-verdict --acknowledge <id>`",
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
        let Some((ticket, body)) = open_checkpoint(paths)? else {
            return Ok(None);
        };
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

fn child_audit_path(paths: &PlakPaths) -> PathBuf {
    root(paths).join("upgrade-child-audit")
}

pub(crate) fn legacy_child_audit(paths: &PlakPaths) -> CliResult<Option<LegacyChildBaseline>> {
    let Some(file) = open(&child_audit_path(paths), CHILD_AUDIT_LIMIT)? else {
        return Ok(None);
    };
    let Some((ticket, _)) = open_checkpoint(paths)? else {
        return Ok(None);
    };
    let source = HostInstance::parse(&ticket.source).map_err(error)?;
    let caller = rustix::process::getppid().and_then(|pid| u32::try_from(pid.as_raw_pid()).ok());
    if caller != Some(source.pid())
        || ticket.destination != crate::build_identity::binary_id()
        || process_birth(source.pid())? != ticket.process_birth
    {
        return Ok(None);
    }
    let evidence: LegacyChildEvidence<LegacyChildBaseline> = decode(file, CHILD_AUDIT_LIMIT)?;
    Ok((evidence.nonce == ticket.nonce).then_some(evidence.audit))
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

    fn publish_header(paths: &PlakPaths, ticket: &Ticket) {
        let scratch = stage(&baseline(paths), LIMIT, |out| {
            serde_json::to_writer(&mut *out, ticket).unwrap();
            out.write_all(b"\n").map_err(error)
        })
        .unwrap();
        Ticket::publish(paths, scratch).unwrap();
    }

    #[test]
    fn legacy_child_audit_is_bound_to_the_ticket_nonce() {
        let (_root, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        crate::commands::runner::private_directory(&root(&paths)).unwrap();
        let parent = u32::try_from(rustix::process::getppid().unwrap().as_raw_pid()).unwrap();
        let mut ticket = Ticket {
            nonce: Uuid::from_u128(1),
            source: format!("{parent}-0000000000000001"),
            destination: crate::build_identity::binary_id().to_owned(),
            process_birth: process_birth(parent).unwrap(),
            runtime_expected: None,
        };
        let audit = LegacyChildBaseline::from(["a recorded branch fault".into()]);
        ticket.publish_child_audit(&paths, &audit).unwrap();
        publish_header(&paths, &ticket);
        assert_eq!(legacy_child_audit(&paths).unwrap(), Some(audit));

        // Only the request changes: the source process and image still match.
        ticket.nonce = Uuid::from_u128(2);
        publish_header(&paths, &ticket);
        assert!(legacy_child_audit(&paths).unwrap().is_none());
    }

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
            child_reopen_baseline: true,
        };
        let mut ticket = Ticket::new(&identity, binary).unwrap();
        publish_header(&paths, &ticket);
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
        publish_header(&paths, &ticket);
        assert!(
            Checkpoint::inherited(&paths, binary, None)
                .unwrap()
                .is_none()
        );
        clear_cold_start(&paths).unwrap();
        assert!(!baseline(&paths).exists());
    }
}
