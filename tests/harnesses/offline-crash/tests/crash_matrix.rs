//! Process-level crash-cut matrix for the public durable-session facade.

#![cfg(unix)]
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use plakous::sessions::SessionLocator;
use plakous_offline_crash_harness::evidence::{EvidenceKind, EvidenceRecord, read_evidence};
use plakous_offline_crash_harness::protocol::{
    ChildReply, ControlCommand, CrashBoundary, PROTOCOL_VERSION, ProbeReport,
};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const REPLY_DEADLINE: Duration = Duration::from_secs(10);

struct Fixture {
    _root: TempDir,
    database: PathBuf,
    workspace: PathBuf,
    evidence: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let database = root.path().join("session.sqlite3");
        let workspace = root.path().join("workspace");
        let evidence = root.path().join("physical-evidence.jsonl");
        let state = root.path().join("locator.json");
        std::fs::create_dir(&workspace)?;
        Ok(Self {
            _root: root,
            database,
            workspace,
            evidence,
            state,
        })
    }

    fn run_command(&self, cut: CrashBoundary) -> ControlCommand {
        ControlCommand::Run {
            version: PROTOCOL_VERSION,
            database: self.database.clone(),
            workspace: self.workspace.clone(),
            evidence: self.evidence.clone(),
            state: self.state.clone(),
            cut,
        }
    }

    fn probe_command(&self) -> ControlCommand {
        ControlCommand::Probe {
            version: PROTOCOL_VERSION,
            database: self.database.clone(),
            workspace: self.workspace.clone(),
            evidence: self.evidence.clone(),
            state: self.state.clone(),
        }
    }
}

#[test]
fn complete_run_is_one_exact_new_state_and_probe_is_inert() -> TestResult {
    let fixture = Fixture::new()?;
    let mut child = spawn_child(&fixture.run_command(CrashBoundary::None))?;
    await_started(&mut child)?;
    let finished = await_finished(&mut child)?;
    assert!(!finished.is_empty());
    assert!(
        child
            .wait_before(Instant::now() + REPLY_DEADLINE)?
            .success()
    );

    let before = read_evidence(&fixture.evidence)?;
    assert_no_duplicate_dispatch(&before)?;
    assert_eq!(count(&before, EvidenceKind::ProviderDispatched), 2);
    assert_eq!(count(&before, EvidenceKind::ProviderOutcome), 2);
    assert_eq!(count(&before, EvidenceKind::EffectDispatched), 1);
    assert_eq!(count(&before, EvidenceKind::EffectOutcome), 1);
    assert_eq!(count(&before, EvidenceKind::NewCheckpointReleased), 1);

    let report = probe(&fixture)?;
    let ProbeReport::NewExact {
        canonical_items,
        final_text,
        source,
        locator,
    } = &report
    else {
        return Err(format!("expected exact new state: {report:?}").into());
    };
    assert_eq!(*canonical_items, 4);
    assert_eq!(final_text, "loopback turn committed");
    assert!(same_branch_new_checkpoint(source, locator));
    assert_eq!(read_evidence(&fixture.evidence)?, before);
    Ok(())
}

/// A `SIGKILL` after every runtime commit of the loopback turn (spec 01
/// §3.5: each transition is one commit, so each commit is a cut): every cut
/// reopens into a classified state without redispatching anything, the probe
/// is inert, and the sweep reaches the uncut run.
#[test]
fn every_runtime_commit_cut_reopens_without_redispatch() -> TestResult {
    let mut observations = Vec::new();
    for ordinal in 1..=128_u64 {
        let fixture = Fixture::new()?;
        let trace = fixture.state.with_extension("trace.jsonl");
        let mut child = spawn_child_with_env(
            &fixture.run_command(CrashBoundary::None),
            &[
                (
                    plakous_offline_crash_harness::commit_trace::TRACE_ENV,
                    trace.as_os_str(),
                ),
                (
                    plakous_offline_crash_harness::commit_trace::KILL_ENV,
                    std::ffi::OsStr::new(&format!("runtime:{ordinal}")),
                ),
            ],
        )?;
        await_started(&mut child)?;
        let status = child.wait_before(Instant::now() + REPLY_DEADLINE)?;
        if status.success() {
            assert!(ordinal > 1, "the sweep never cut a commit");
            let expected = EXPECTED_RUNTIME_CUTS
                .iter()
                .map(|(cut, outcome)| (*cut, (*outcome).to_owned()))
                .collect::<Vec<_>>();
            assert_eq!(
                observations, expected,
                "a runtime cut reopened unexpectedly"
            );
            return Ok(());
        }
        assert_eq!(
            status.signal(),
            Some(9),
            "commit {ordinal} was not cut by SIGKILL"
        );
        let before = read_evidence(&fixture.evidence)?;
        assert_no_duplicate_dispatch(&before)?;
        if !fixture.state.exists() {
            // Cut before the child recorded any locator: nothing ran.
            assert_eq!(count(&before, EvidenceKind::ProviderDispatched), 0);
            continue;
        }
        // Every cut reopens into its closure-table outcome (spec 01 §5.5).
        let report = probe(&fixture).map_err(|error| format!("commit {ordinal}: {error}"))?;
        let observed = cut_outcome(&report);
        eprintln!("commit {ordinal}: {observed}");
        observations.push((ordinal, observed));
        assert_eq!(
            read_evidence(&fixture.evidence)?,
            before,
            "commit {ordinal}: probe appended physical dispatch evidence"
        );
    }
    Err("the loopback turn did not finish within the commit sweep".into())
}

const NOTHING_RAN: &str = "abandoned[]:this turn was abandoned after its process was lost, before any model request or tool started";
const REQUEST_ONE: &str = "abandoned[2]:the model request for this turn was abandoned after its outcome became uncertain: the process was lost before any response was observed";
const REQUEST_TWO: &str = "abandoned[4]:the model request for this turn was abandoned after its outcome became uncertain: the process was lost before any response was observed";
const TOOL_RUNNING: &str =
    "abandoned[3]:this turn was abandoned after the process was lost while a tool was running";

/// The outcome each runtime commit cut of the loopback turn reopens into:
/// cuts 1-4 precede any recorded locator; 19-21 follow the final round's
/// decision commit and finish as the live driver would have.
const EXPECTED_RUNTIME_CUTS: &[(u64, &str)] = &[
    (5, "old"),
    (6, NOTHING_RAN),
    (7, NOTHING_RAN),
    (8, NOTHING_RAN),
    (9, REQUEST_ONE),
    (10, REQUEST_ONE),
    (
        11,
        "abandoned[]:this turn was abandoned after the process was lost",
    ),
    (12, TOOL_RUNNING),
    (13, TOOL_RUNNING),
    (14, TOOL_RUNNING),
    (
        15,
        "abandoned[3]:this turn was abandoned after a tool finished, but its result was not added to the conversation",
    ),
    (
        16,
        "abandoned[]:this turn was abandoned after its tool round completed",
    ),
    (17, REQUEST_TWO),
    (18, REQUEST_TWO),
    (19, "new"),
    (20, "new"),
    (21, "new"),
];

fn cut_outcome(report: &ProbeReport) -> String {
    match report {
        ProbeReport::OldExact { .. } => "old".to_owned(),
        ProbeReport::NewExact { .. } => "new".to_owned(),
        ProbeReport::RecoveredByAbandonment {
            abandoned_operations,
            note,
            ..
        } => format!(
            "abandoned{abandoned_operations:?}:{}",
            note.split('.').next().unwrap_or_default()
        ),
        ProbeReport::RecoveryRequired { .. } => "recovery-required".to_owned(),
    }
}

fn same_branch_new_checkpoint(source: &str, target: &str) -> bool {
    let (Ok(source), Ok(target)) = (
        source.parse::<SessionLocator>(),
        target.parse::<SessionLocator>(),
    ) else {
        return false;
    };
    source.session_id() == target.session_id()
        && source.branch_id() == target.branch_id()
        && source.checkpoint_id() != target.checkpoint_id()
}

fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_plakous-offline-crash-harness"))
}

struct CrashChild {
    child: Option<Child>,
    replies: Receiver<Result<ChildReply, String>>,
    reader: Option<JoinHandle<()>>,
}

impl CrashChild {
    fn wait_before(&mut self, deadline: Instant) -> TestResult<ExitStatus> {
        loop {
            if let Some(status) = self.child_mut()?.try_wait()? {
                self.child = None;
                self.join_reader();
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err("child exit deadline elapsed".into());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn child_mut(&mut self) -> std::io::Result<&mut Child> {
        self.child.as_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "crash harness child is no longer live",
            )
        })
    }

    fn join_reader(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for CrashChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
        self.join_reader();
    }
}

fn spawn_child(command: &ControlCommand) -> TestResult<CrashChild> {
    spawn_child_with_env(command, &[])
}

fn spawn_child_with_env(
    command: &ControlCommand,
    env: &[(&str, &std::ffi::OsStr)],
) -> TestResult<CrashChild> {
    let mut child = Command::new(binary())
        .envs(env.iter().copied())
        .arg("--child")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut input = child.stdin.take().ok_or("child stdin was not piped")?;
    let output = child.stdout.take().ok_or("child stdout was not piped")?;
    let (sender, replies) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let reply = line
                .map_err(|error| format!("child control stream read failed: {error}"))
                .and_then(|line| {
                    serde_json::from_str(&line)
                        .map_err(|error| format!("child control reply was invalid: {error}"))
                });
            let failed = reply.is_err();
            if sender.send(reply).is_err() || failed {
                return;
            }
        }
    });
    let child = CrashChild {
        child: Some(child),
        replies,
        reader: Some(reader),
    };
    serde_json::to_writer(&mut input, command)?;
    input.write_all(b"\n")?;
    input.flush()?;
    drop(input);
    Ok(child)
}

fn await_started(child: &mut CrashChild) -> TestResult {
    match read_reply_before(child, Instant::now() + REPLY_DEADLINE)? {
        ChildReply::Started {
            version: PROTOCOL_VERSION,
        } => Ok(()),
        other => Err(format!("child did not start: {other:?}").into()),
    }
}

fn await_finished(child: &mut CrashChild) -> TestResult<String> {
    let deadline = Instant::now() + REPLY_DEADLINE;
    loop {
        match read_reply_before(child, deadline)? {
            ChildReply::Boundary { .. } => {}
            ChildReply::Finished { locator, .. } => return Ok(locator),
            ChildReply::Failed { message, .. } => return Err(message.into()),
            other => return Err(format!("child did not finish: {other:?}").into()),
        }
    }
}

fn probe(fixture: &Fixture) -> TestResult<ProbeReport> {
    let mut child = spawn_child(&fixture.probe_command())?;
    await_started(&mut child)?;
    let report = match read_reply_before(&mut child, Instant::now() + REPLY_DEADLINE)? {
        ChildReply::Probed { report, .. } => report,
        ChildReply::Failed { message, .. } => return Err(message.into()),
        other => return Err(format!("probe returned an unexpected reply: {other:?}").into()),
    };
    if !child
        .wait_before(Instant::now() + REPLY_DEADLINE)?
        .success()
    {
        return Err("probe child failed after reporting state".into());
    }
    Ok(report)
}

fn read_reply_before(child: &mut CrashChild, deadline: Instant) -> TestResult<ChildReply> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match child.replies.recv_timeout(remaining) {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(message)) => Err(message.into()),
        Err(RecvTimeoutError::Disconnected) => {
            let status = child.child_mut()?.try_wait()?;
            Err(format!("child control stream ended; child status: {status:?}").into())
        }
        Err(RecvTimeoutError::Timeout) => {
            let status = child.child_mut()?.try_wait()?;
            Err(format!("child reply deadline elapsed; child status: {status:?}").into())
        }
    }
}

fn count(records: &[EvidenceRecord], kind: EvidenceKind) -> usize {
    records.iter().filter(|record| record.kind == kind).count()
}

fn assert_no_duplicate_dispatch(records: &[EvidenceRecord]) -> TestResult {
    let mut provider_rounds = BTreeMap::<u64, usize>::new();
    let mut effects = 0_usize;
    for record in records {
        match record.kind {
            EvidenceKind::ProviderDispatched => {
                let round = record.round.ok_or("provider dispatch lacked a round")?;
                *provider_rounds.entry(round).or_default() += 1;
            }
            EvidenceKind::EffectDispatched => effects = effects.saturating_add(1),
            _ => {}
        }
    }
    if provider_rounds.values().any(|count| *count > 1) || effects > 1 {
        return Err("physical provider/effect evidence contains a blind redispatch".into());
    }
    Ok(())
}
