//! Process-level SIGKILL evidence over Plak's real host loop and startup reconcilers.

#![cfg(unix)]
#![forbid(unsafe_code)]

use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use super::protocol::{
    ChildReply, ControlCommand, PROTOCOL_VERSION, ProductCrashBoundary, ProductProbeReport,
    ProductWorkflow,
};
use crate::product_paths::PlakPaths;
use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptRow};
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> TestResult<Self> {
        let root = PlakPaths::isolated_tempdir();
        let home = root.path().join("plak-home");
        let workspace = root.path().join("workspace");
        let state = root.path().join("actual-host-state.json");
        std::fs::create_dir(&workspace)?;
        Ok(Self {
            _root: root,
            home,
            workspace,
            state,
        })
    }

    fn run(&self, workflow: ProductWorkflow, cut: ProductCrashBoundary) -> ControlCommand {
        ControlCommand::ActualHostRun {
            version: PROTOCOL_VERSION,
            home: self.home.clone(),
            workspace: self.workspace.clone(),
            state: self.state.clone(),
            workflow,
            cut,
        }
    }

    fn recover(&self, next_prompt: bool) -> ControlCommand {
        ControlCommand::ActualHostRecover {
            version: PROTOCOL_VERSION,
            home: self.home.clone(),
            workspace: self.workspace.clone(),
            state: self.state.clone(),
            next_prompt,
        }
    }

    fn probe(&self) -> ControlCommand {
        ControlCommand::ProductProbe {
            version: PROTOCOL_VERSION,
            home: self.home.clone(),
            state: self.state.clone(),
        }
    }
}

#[test]
fn actual_host_sigkill_relaunch_matrix_uses_real_handlers_and_reconcilers() -> TestResult {
    let mut interrupted = Vec::new();
    for (workflow, cuts) in [
        (
            ProductWorkflow::RootDelivery,
            &[
                ProductCrashBoundary::Prepared,
                ProductCrashBoundary::RuntimeReturned,
                ProductCrashBoundary::ProductPublished,
                ProductCrashBoundary::Finalization,
                ProductCrashBoundary::FinalizationPendingTerminal,
                ProductCrashBoundary::CompletedToolRound,
            ][..],
        ),
        (
            ProductWorkflow::SendNow,
            &[
                ProductCrashBoundary::Prepared,
                ProductCrashBoundary::RuntimeReturned,
                ProductCrashBoundary::ProductPublished,
                ProductCrashBoundary::ProductCommitted,
            ][..],
        ),
        (
            ProductWorkflow::Compact,
            &[
                ProductCrashBoundary::Prepared,
                ProductCrashBoundary::RuntimeReturned,
                ProductCrashBoundary::ProductPublished,
                ProductCrashBoundary::ProductCommitted,
            ][..],
        ),
    ] {
        for &cut in cuts {
            eprintln!("actual-host case {workflow:?}/{cut:?}");
            let fixture = Fixture::new()?;
            kill_at(&fixture.run(workflow, cut), workflow, cut)?;
            if matches!(
                workflow,
                ProductWorkflow::RootDelivery | ProductWorkflow::SendNow
            ) {
                let report = request_report(&fixture.probe())?;
                let paths = PlakPaths::explicit(fixture.home.clone());
                let mut store = crate::store::ProductStore::open(&paths)?;
                let kind = if workflow == ProductWorkflow::RootDelivery {
                    plakous_hosted::sessions::projector::SubmissionKind::RootStart
                } else {
                    plakous_hosted::sessions::projector::SubmissionKind::SendNowSteer
                };
                assert_eq!(
                    store
                        .load_intents(&report.product_session_id)?
                        .submissions
                        .iter()
                        .filter(|submission| submission.kind == kind)
                        .count(),
                    1,
                    "{workflow:?}/{cut:?}: exactly one tested submission"
                );
                let obsolete: i64 = rusqlite::Connection::open(paths.product_database())?.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name GLOB '*delivery*'", [], |row| row.get(0),
                )?;
                assert_eq!(
                    obsolete, 0,
                    "{workflow:?}/{cut:?}: legacy delivery table exists"
                );
            }
            let before = evidence(&fixture)?;
            interrupted.push((workflow, cut, fixture, before));
        }
    }

    let reclaimed = Fixture::new()?;
    kill_at(
        &reclaimed.run(
            ProductWorkflow::RootDelivery,
            ProductCrashBoundary::Prepared,
        ),
        ProductWorkflow::RootDelivery,
        ProductCrashBoundary::Prepared,
    )?;

    // A real SIGKILL cannot release the facade's 30-second branch lease. Cut
    // every independent fixture first so one bounded wait expires all leases
    // without weakening the production ownership fence.
    std::thread::sleep(Duration::from_secs(31));

    for (workflow, cut, fixture, before) in interrupted {
        eprintln!("actual-host recovery {workflow:?}/{cut:?}");
        let (converged, startup) = relaunch_report(&fixture.recover(false), &fixture.probe())?;
        if cut == ProductCrashBoundary::CompletedToolRound {
            assert_eq!(startup, None, "completed-tool boundary was refused");
            let paths = PlakPaths::explicit(fixture.home.clone());
            let log =
                std::fs::read_to_string(paths.session_events_file(&converged.product_session_id))?;
            let abandoned = log
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|event| event["kind"] == "turn.abandoned")
                .collect::<Vec<_>>();
            assert_eq!(abandoned.len(), 1);
            assert_eq!(abandoned[0]["claim"], "tool_round_completed");
            let record =
                crate::product_sessions::load_session(&paths, &converged.product_session_id)?;
            let completed_notices = record
                .transcript
                .iter()
                .filter(|row| {
                    matches!(row, TranscriptRow::Notice(notice)
                    if notice.scope == NoticeScope::Recovery
                        && notice.text == "plak: recovered an interrupted turn; its tool round completed, but the turn was interrupted before the next model request; completed tool results were kept")
                })
                .count();
            assert_eq!(
                completed_notices, 1,
                "exactly one honest completed-round notice must be published"
            );
            assert!(
                !record
                    .transcript
                    .iter()
                    .any(|row| row.text() == Some("loopback turn committed")),
                "a final answer was invented"
            );
        }
        if matches!(
            cut,
            ProductCrashBoundary::Finalization
                | ProductCrashBoundary::FinalizationPendingTerminal
                | ProductCrashBoundary::CompletedToolRound
        ) {
            let lost_jobs: Vec<String> = serde_json::from_slice(&std::fs::read(
                fixture.home.join("actual-host-lost-jobs.json"),
            )?)?;
            assert_eq!(lost_jobs, ["p1"], "lost-job notice was not emitted");
        }
        assert!(
            converged.locator.is_some(),
            "startup reconciler lost the locator after {workflow:?}/{cut:?}"
        );
        let after = evidence(&fixture)?;
        assert_no_duplicate_physical_ids(&after);
        assert!(
            after.starts_with(&before),
            "startup reconciliation rewrote physical evidence for {workflow:?}/{cut:?}"
        );

        let (stable, repeated_startup) =
            relaunch_report(&fixture.recover(false), &fixture.probe())?;
        assert_eq!(repeated_startup, startup);
        assert_eq!(
            stable, converged,
            "a second startup did not converge after {workflow:?}/{cut:?}"
        );
        assert_eq!(
            evidence(&fixture)?,
            after,
            "a second startup dispatched physical work for {workflow:?}/{cut:?}"
        );
        if matches!(
            cut,
            ProductCrashBoundary::Finalization | ProductCrashBoundary::FinalizationPendingTerminal
        ) {
            assert_eq!(startup, None, "the unpublished terminal head was refused");
            assert_terminal_published_once(&fixture, &stable)?;
        }
        if matches!(
            cut,
            ProductCrashBoundary::Finalization
                | ProductCrashBoundary::CompletedToolRound
                | ProductCrashBoundary::FinalizationPendingTerminal
        ) {
            // The follow-up runtime does not replay the initial process tool.
            std::fs::write(
                fixture.workspace.join("finalization-script.json"),
                serde_json::to_vec(
                    &serde_json::json!({"version": 1, "process_tools": true, "steps": [{"output": {"text": "loopback turn committed"}}]}),
                )?,
            )?;
            let before_followup = evidence(&fixture)?;
            let (followup, failure) = relaunch_report(&fixture.recover(true), &fixture.probe())?;
            assert_eq!(failure, None);
            assert_ne!(
                followup.locator, stable.locator,
                "{workflow:?}/{cut:?}: follow-up dispatched {} provider attempts, previously {}",
                followup.provider_dispatches, stable.provider_dispatches
            );
            let physical = evidence(&fixture)?;
            assert_no_duplicate_physical_ids(&physical);
            assert_eq!(physical.len(), before_followup.len() + 1);
            let paths = PlakPaths::explicit(fixture.home.clone());
            let record =
                crate::product_sessions::load_session(&paths, &followup.product_session_id)?;
            assert_eq!(
                record
                    .transcript
                    .iter()
                    .filter(|row| row.text() == Some("loopback turn committed"))
                    .count(),
                if cut == ProductCrashBoundary::CompletedToolRound {
                    1
                } else {
                    2
                }
            );
            let pid = std::fs::read_to_string(fixture.workspace.join("finalization-job.pid"))?;
            // Recovery never signals the orphan; the fixture owns its cleanup.
            let _ = Command::new("kill").args(["-KILL", pid.trim()]).status()?;
        }
    }
    reclaimed_delivery_after_another_face_advances(&reclaimed)?;
    // Reopen never re-dispatches a fork/rebind intent or selects its successor.
    // It withdraws an unadmitted request, or projects an already-created successor,
    // and the source remains available for the next prompt.
    for (workflow, cuts) in [
        (
            ProductWorkflow::Fork,
            &[
                ProductCrashBoundary::Prepared,
                ProductCrashBoundary::RuntimeReturned,
                ProductCrashBoundary::ProductPublished,
            ][..],
        ),
        (
            ProductWorkflow::Rebind,
            &[
                ProductCrashBoundary::RuntimeReturned,
                ProductCrashBoundary::ProductPublished,
            ][..],
        ),
    ] {
        for &cut in cuts {
            eprintln!("actual-host reconciler case {workflow:?}/{cut:?}");
            let fixture = Fixture::new()?;
            let component_seed = ControlCommand::ProductRun {
                version: PROTOCOL_VERSION,
                home: fixture.home.clone(),
                workspace: fixture.workspace.clone(),
                state: fixture.state.clone(),
                workflow,
                cut,
            };
            kill_at(&component_seed, workflow, cut)?;
            let before = evidence(&fixture)?;
            kill_at_physical(&fixture.recover(true))?;
            let report = request_report(&fixture.probe())?;
            assert_eq!(
                report.target_locator.is_some(),
                cut != ProductCrashBoundary::Prepared,
                "{workflow:?}/{cut:?}: {report:?}"
            );
            let paths = PlakPaths::explicit(fixture.home.clone());
            let source = crate::product_sessions::load_session(&paths, &report.product_session_id)?;
            assert!(source.source_product_session_id.is_none());
            let mut store = crate::store::ProductStore::open(&paths)?;
            let intents = store.load_intents(&source.id)?;
            if cut == ProductCrashBoundary::Prepared {
                assert_eq!(
                    intents.workflow_requests,
                    [] as [plakous_hosted::sessions::projector::WorkflowRequest; 0]
                );
                assert_eq!(
                    intents
                        .notices
                        .iter()
                        .filter(|notice| {
                            notice.row.text() == Some("fork did not start; run /fork again")
                        })
                        .count(),
                    1
                );
            } else {
                let request = intents
                    .workflow_requests
                    .first()
                    .ok_or("branch request missing")?;
                assert_eq!(
                    report.target_product_session_id.as_deref(),
                    Some(request.successor_product_id().as_str())
                );
                assert_eq!(
                    report.source_product_session_id.as_deref(),
                    Some(source.id.as_str())
                );
            }
            let after = evidence(&fixture)?;
            assert!(
                after.len() > before.len(),
                "next source prompt did not dispatch"
            );
            assert_no_duplicate_physical_ids(&after);
        }
    }
    Ok(())
}

fn reclaimed_delivery_after_another_face_advances(fixture: &Fixture) -> TestResult {
    use plakous::agent::{CommandAckDisposition, CommandId, ControlErrorKind};
    use plakous::sessions::{
        AgentSessions, OpenSessionOutcome, OpenSessionRequest, SessionBackend, SessionOperationId,
    };
    use plakous_offline_crash_harness::{BoundaryController, runtime_binding};

    let paths = PlakPaths::explicit(fixture.home.clone());
    let report = request_report(&fixture.probe())?;
    let record = crate::product_sessions::load_session(&paths, &report.product_session_id)?;
    let intents = crate::store::ProductStore::open(&paths)?.load_intents(&record.id)?;
    let intent = intents.submissions.first().ok_or("submission missing")?;
    let command = CommandId::new(&intent.command_id)?;
    let controller = BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        fixture.home.join("product-physical.jsonl"),
    )?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let executor = plakous::application::ApplicationExecutor::new()?;
    let OpenSessionOutcome::Opened { session, .. } = executor.block_on(
        sessions.open(OpenSessionRequest::new(
            SessionOperationId::new("second-face:open")?,
            record.plakous_locator.ok_or("recorded locator missing")?,
            runtime_binding(&fixture.workspace, controller.clone())
                .map_err(|error| -> Box<dyn Error> { error })?,
        )),
    )??
    else {
        return Err("second face did not open".into());
    };
    executor.block_on(async {
        let submitted = session
            .agent()
            .start(command.clone(), intent.canonical_command.clone())
            .await?;
        assert_eq!(
            submitted.acknowledgement().disposition(),
            CommandAckDisposition::Accepted
        );
        let _ = submitted
            .into_turn()
            .ok_or("accepted delivery omitted turn")?
            .result()
            .await?;
        for index in 0..16 {
            let _ = session
                .agent()
                .prompt(format!("second face root turn {index}"))
                .await
                .map_err(|error| format!("second face prompt {index}: {error}"))?
                .result()
                .await
                .map_err(|error| format!("second face result {index}: {error}"))?;
        }
        Ok::<_, Box<dyn Error>>(())
    })??;
    // A live driver's result cache can mask durable reclamation.
    let current = executor.block_on(session.release())??;
    let _ = executor.block_on(sessions.prune_history(current))??;
    let OpenSessionOutcome::Opened { session, .. } = executor.block_on(
        sessions.open(OpenSessionRequest::new(
            SessionOperationId::new("second-face:verify-reclaimed")?,
            current,
            runtime_binding(&fixture.workspace, controller)
                .map_err(|error| -> Box<dyn Error> { error })?,
        )),
    )??
    else {
        return Err("pruned second face did not reopen".into());
    };
    executor.block_on(async {
        let duplicate = session
            .agent()
            .command(plakous::agent::TurnCommand::Start {
                command_id: command,
                input: intent.canonical_command.clone().into(),
            })
            .await
            .map_err(|error| format!("pruned duplicate lookup: {error}"))?;
        assert_eq!(
            duplicate
                .acknowledgement()
                .rejection()
                .map(plakous::agent::ControlError::kind),
            Some(ControlErrorKind::Reclaimed)
        );
        Ok::<_, Box<dyn Error>>(())
    })??;
    let _ = executor.block_on(session.release())??;
    let before = evidence(fixture)?;
    let (reopened, _) = relaunch_report(&fixture.recover(false), &fixture.probe())?;
    assert_eq!(reopened.queue_items, 0);
    let mut product = crate::store::ProductStore::open(&paths)?;
    assert!(
        product
            .load_intents(&report.product_session_id)?
            .submissions
            .iter()
            .any(|submission| submission.command_id == intent.command_id)
    );
    assert!(matches!(
        product
            .load_views(&report.product_session_id)?
            .ok_or("missing projected session")?
            .state
            .deliveries
            .get(&intent.command_id),
        Some(crate::store::sessions::projector::Delivery::Accepted)
    ));
    let stored = crate::product_sessions::load_session(&paths, &report.product_session_id)?;
    assert_eq!(
        stored
            .transcript
            .iter()
            .filter(|line| line.text()
                == Some("queued prompt already ran; its result is no longer kept"))
            .count(),
        0
    );
    assert_eq!(
        evidence(fixture)?,
        before,
        "reopening must not rerun the reclaimed command"
    );
    let (stable, _) = relaunch_report(&fixture.recover(false), &fixture.probe())?;
    assert_eq!(stable, reopened);
    assert_eq!(evidence(fixture)?, before);
    Ok(())
}

/// The runtime committed the turn's terminal and the host died before the
/// product recorded it: the reopen adopts exactly that head and publishes its
/// result, once, rather than refusing the locator.
fn assert_terminal_published_once(fixture: &Fixture, report: &ProductProbeReport) -> TestResult {
    use plakous::sessions::AgentSessions;

    let paths = PlakPaths::explicit(fixture.home.clone());
    let record = crate::product_sessions::load_session(&paths, &report.product_session_id)?;
    assert!(
        crate::store::ProductStore::open(&paths)?
            .load_views(&record.id)?
            .ok_or("missing projected session")?
            .state
            .deliveries
            .values()
            .any(|delivery| matches!(
                delivery,
                crate::store::sessions::projector::Delivery::Accepted
            ))
    );
    let locator = record
        .plakous_locator
        .ok_or("the adopted head was not recorded")?;
    let recovery = AgentSessions::inspect_recovery_sqlite(paths.runtime_database(), locator)?;
    assert!(
        recovery.is_none(),
        "the recorded locator is not the quiescent runtime head"
    );
    assert_eq!(
        record
            .transcript
            .iter()
            .filter(|row| row.text() == Some("loopback turn committed"))
            .count(),
        1,
        "the adopted terminal was not published exactly once"
    );
    Ok(())
}

fn evidence(fixture: &Fixture) -> TestResult<Vec<(EvidenceKind, String)>> {
    let mut physical = Vec::new();
    for row in read_evidence(fixture.home.join("product-physical.jsonl"))? {
        if !matches!(
            row.kind,
            EvidenceKind::ProviderDispatched | EvidenceKind::EffectDispatched
        ) {
            continue;
        }
        let operation_id = row
            .operation_id
            .ok_or("physical evidence omitted operation id")?;
        physical.push((row.kind, operation_id));
    }
    Ok(physical)
}

fn assert_no_duplicate_physical_ids(rows: &[(EvidenceKind, String)]) {
    let mut provider_ids = rows
        .iter()
        .filter_map(|(kind, id)| (*kind == EvidenceKind::ProviderDispatched).then_some(id))
        .collect::<Vec<_>>();
    provider_ids.sort_unstable();
    provider_ids.dedup();
    assert_eq!(
        provider_ids.len(),
        rows.iter()
            .filter(|(kind, _)| *kind == EvidenceKind::ProviderDispatched)
            .count(),
        "provider dispatch operation ids repeated"
    );
    assert!(
        rows.iter()
            .filter(|(kind, _)| *kind == EvidenceKind::EffectDispatched)
            .count()
            <= 1
    );
}

struct TestChild {
    child: Child,
    replies: Receiver<Result<ChildReply, String>>,
}

impl Drop for TestChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl TestChild {
    fn read(&mut self, scenario: &str) -> TestResult<ChildReply> {
        // This process exercises the real host, SQLite stores, and crash
        // reconcilers. Parallel nextest load can delay scheduling well beyond
        // the normal sub-second path, so keep the deadline bounded without
        // confusing host contention with a semantic recovery failure.
        match self.replies.recv_timeout(Duration::from_secs(45)) {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(error)) => Err(format!("{scenario}: child reply failed: {error}").into()),
            Err(error) => {
                let _ = self.child.kill();
                let status = self.child.wait()?;
                Err(
                    format!("{scenario}: child reply deadline elapsed ({error}); reaped {status}")
                        .into(),
                )
            }
        }
    }
}

fn spawn(command: &ControlCommand) -> TestResult<TestChild> {
    let mut child = Command::new(std::env::current_exe()?)
        .args(super::CHILD_TEST_ARGS)
        .env(super::CHILD_SENTINEL, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("child stdin unavailable")?;
    serde_json::to_writer(&mut stdin, command)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;
    drop(stdin);
    let stdout = child.stdout.take().ok_or("child stdout unavailable")?;
    let (sender, replies) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let reply = line.map_err(|error| error.to_string()).and_then(|line| {
                if let Some((_, frame)) = line.split_once(super::REPLY_PREFIX) {
                    super::protocol::decode_reply(frame)
                } else if line.starts_with('{') {
                    super::protocol::decode_reply(&line)
                } else {
                    Err("unframed libtest output".to_owned())
                }
            });
            if matches!(&reply, Err(error) if error == "unframed libtest output") {
                continue;
            }
            if sender.send(reply).is_err() {
                break;
            }
        }
    });
    Ok(TestChild { child, replies })
}

fn await_started(child: &mut TestChild, scenario: &str) -> TestResult {
    match child.read(scenario)? {
        ChildReply::Started {
            version: PROTOCOL_VERSION,
        } => Ok(()),
        other => Err(format!("unexpected start reply: {other:?}").into()),
    }
}

fn kill_at(
    command: &ControlCommand,
    workflow: ProductWorkflow,
    boundary: ProductCrashBoundary,
) -> TestResult {
    let scenario = format!("{workflow:?}/{boundary:?}");
    let mut process = spawn(command)?;
    await_started(&mut process, &scenario)?;
    loop {
        match process.read(&scenario)? {
            ChildReply::ProductBoundary {
                version: PROTOCOL_VERSION,
                workflow: observed_workflow,
                boundary: observed_boundary,
            } if observed_workflow == workflow && observed_boundary == boundary => break,
            ChildReply::Boundary { .. } | ChildReply::ProductBoundary { .. } => {}
            ChildReply::Failed { message, .. } => return Err(message.into()),
            other => return Err(format!("unexpected boundary reply: {other:?}").into()),
        }
    }
    if matches!(
        boundary,
        ProductCrashBoundary::Finalization | ProductCrashBoundary::FinalizationPendingTerminal
    ) {
        let ControlCommand::ActualHostRun {
            home, workspace, ..
        } = command
        else {
            return Err("wrong finalization command".into());
        };
        let paths = PlakPaths::explicit(home.clone());
        let connection = rusqlite::Connection::open(paths.runtime_database())?;
        let current: String = connection.query_row(
            "SELECT session_id || '/' || branch_id || '/' || checkpoint_id FROM branches",
            [],
            |row| row.get(0),
        )?;
        let head = current.parse()?;
        connection.execute_batch("DROP TRIGGER stop_terminal;")?;
        let report = plakous::sessions::AgentSessions::inspect_recovery_sqlite(
            paths.runtime_database(),
            head,
        )?
        .ok_or("cut is not Working")?;
        assert_eq!(
            report.phase(),
            plakous::sessions::SessionRecoveryPhase::Finalization
        );
        assert_eq!(
            report
                .obligations()
                .iter()
                .map(|item| item.operation_id())
                .collect::<Vec<_>>(),
            [1, 4]
        );
        assert!(
            report.obligations().iter().all(|item| item.action()
                == plakous::sessions::SessionRecoveryAction::FinalizationRequired)
        );
        let operation_bytes = |id: u64| -> TestResult<Vec<u8>> {
            Ok(connection.query_row(
                "WITH RECURSIVE graph(kind,codec,digest) AS (SELECT root_kind,root_codec,root_digest FROM checkpoints WHERE checkpoint_id=(SELECT checkpoint_id FROM branches) UNION SELECT child_kind,child_codec,child_digest FROM runtime_object_edges JOIN graph ON parent_kind=graph.kind AND parent_codec=graph.codec AND parent_digest=graph.digest) SELECT p.bytes FROM graph JOIN runtime_objects AS o USING(kind,codec,digest) JOIN runtime_object_bytes AS p ON p.id=o.id WHERE kind=14 AND substr(p.bytes,1,8)=?1",
                [id.to_be_bytes().as_slice()],
                |row| row.get(0),
            )?)
        };
        // Operation codec: Dispatched has no evidence; Completed ends with a
        // 38-byte component digest. Check the actual cut, not a pending root.
        let root = operation_bytes(1)?;
        assert_eq!(
            root.last(),
            Some(&2),
            "the consequence has not closed the root"
        );
        let prior = operation_bytes(2)?;
        assert_eq!(prior[prior.len() - 39], 6);
        let provider = operation_bytes(4)?;
        assert_eq!(provider[provider.len() - 39], 3);
        let recorded: Vec<u8> = connection.query_row(
            "SELECT p.bytes FROM runtime_objects AS o JOIN runtime_object_bytes AS p ON p.id=o.id WHERE kind=8 AND digest=?1",
            [&provider[provider.len() - 32..]],
            |row| row.get(0),
        )?;
        // Component wire: kind (1), codec (4), payload length (4), payload,
        // then the integrity digest (38).
        let recorded: serde_json::Value =
            serde_json::from_slice(&recorded[9..recorded.len() - 38])?;
        assert_eq!(recorded[0], "provider-outcome-v2");
        assert_eq!(recorded[1]["decision"], "Terminal");
        for operation in [&prior, &provider] {
            let evidence: Vec<u8> = connection.query_row(
                "SELECT p.bytes FROM runtime_objects AS o JOIN runtime_object_bytes AS p ON p.id=o.id WHERE kind=8 AND digest=?1",
                [&operation[operation.len() - 32..]],
                |row| row.get(0),
            )?;
            let candidate = b"\"family_artifact_candidate\":{";
            assert!(
                evidence
                    .windows(candidate.len())
                    .any(|part| part == candidate),
                "the selected provider lacks its own sealed candidate"
            );
        }
        let native_finals: i64 = connection.query_row(
            "WITH RECURSIVE graph(kind,codec,digest) AS (SELECT root_kind,root_codec,root_digest FROM checkpoints WHERE checkpoint_id=(SELECT checkpoint_id FROM branches) UNION SELECT child_kind,child_codec,child_digest FROM runtime_object_edges JOIN graph ON parent_kind=graph.kind AND parent_codec=graph.codec AND parent_digest=graph.digest) SELECT count(*) FROM graph JOIN runtime_objects AS o USING(kind,codec,digest) JOIN runtime_object_bytes AS p ON p.id=o.id WHERE instr(p.bytes, CAST('\"family_artifact_candidate\":{' AS BLOB)) > 0 AND instr(p.bytes, CAST('encrypted_content' AS BLOB)) > 0",
            [],
            |row| row.get(0),
        )?;
        assert!(
            native_finals >= 2,
            "the live cut must carry prior and final sealed candidates"
        );
        let product: String = rusqlite::Connection::open(paths.product_database())?.query_row(
            "SELECT id FROM sessions",
            [],
            |row| row.get(0),
        )?;
        let record = crate::product_sessions::load_session(&paths, &product)?;
        let before = std::fs::read(paths.runtime_database())?;
        crate::commands::reload_rehearsal::assert_finalization_rehearsal(&paths, &record, head);
        assert_eq!(
            before,
            std::fs::read(paths.runtime_database())?,
            "rehearsal wrote the runtime store"
        );
        let pid = std::fs::read_to_string(workspace.join("finalization-job.pid"))?;
        assert!(
            Command::new("kill")
                .args(["-0", pid.trim()])
                .status()?
                .success(),
            "promoted process is not live at the cut"
        );
    }
    if boundary == ProductCrashBoundary::CompletedToolRound {
        let ControlCommand::ActualHostRun {
            home, workspace, ..
        } = command
        else {
            return Err("wrong tool-round command".into());
        };
        let paths = PlakPaths::explicit(home.clone());
        let connection = rusqlite::Connection::open(paths.runtime_database())?;
        let current: String = connection.query_row(
            "SELECT session_id || '/' || branch_id || '/' || checkpoint_id FROM branches",
            [],
            |row| row.get(0),
        )?;
        let head = current.parse()?;
        connection.execute_batch("DROP TRIGGER stop_next_model;")?;
        let report = plakous::sessions::AgentSessions::inspect_recovery_sqlite(
            paths.runtime_database(),
            head,
        )?
        .ok_or("cut is not Working")?;
        assert_eq!(
            report
                .obligations()
                .iter()
                .map(|item| item.operation_id())
                .collect::<Vec<_>>(),
            [1]
        );
        let operations = {
            let mut statement = connection.prepare("WITH RECURSIVE graph(kind,codec,digest) AS (SELECT root_kind,root_codec,root_digest FROM checkpoints WHERE checkpoint_id=(SELECT checkpoint_id FROM branches) UNION SELECT child_kind,child_codec,child_digest FROM runtime_object_edges JOIN graph ON parent_kind=graph.kind AND parent_codec=graph.codec AND parent_digest=graph.digest) SELECT p.bytes FROM graph JOIN runtime_objects AS o USING(kind,codec,digest) JOIN runtime_object_bytes AS p ON p.id=o.id WHERE kind=14 ORDER BY substr(p.bytes,1,8)")?;
            statement
                .query_map([], |row| row.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        assert_eq!(
            operations.len(),
            3,
            "a next provider or unrelated operation exists"
        );
        assert_eq!(operations[0].last(), Some(&2), "root is not Dispatched");
        let recorded: Vec<u8> = connection.query_row(
            "SELECT p.bytes FROM runtime_objects AS o JOIN runtime_object_bytes AS p ON p.id=o.id WHERE kind=8 AND digest=?1",
            [&operations[1][operations[1].len() - 32..]],
            |row| row.get(0),
        )?;
        let recorded: serde_json::Value =
            serde_json::from_slice(&recorded[9..recorded.len() - 38])?;
        assert_eq!(recorded[0], "provider-outcome-v2");
        assert_eq!(recorded[1]["decision"], "ToolBatch");
        for bytes in &operations[1..] {
            assert_eq!(
                bytes[bytes.len() - 39],
                6,
                "provider/effect is not Finalized"
            );
        }
        let product: String = rusqlite::Connection::open(paths.product_database())?.query_row(
            "SELECT id FROM sessions",
            [],
            |row| row.get(0),
        )?;
        let record = crate::product_sessions::load_session(&paths, &product)?;
        let before = std::fs::read(paths.runtime_database())?;
        crate::commands::reload_rehearsal::assert_abandonment_rehearsal(&paths, &record, head);
        assert_eq!(
            before,
            std::fs::read(paths.runtime_database())?,
            "rehearsal wrote the runtime store"
        );
        let pid = std::fs::read_to_string(workspace.join("finalization-job.pid"))?;
        assert!(
            Command::new("kill")
                .args(["-0", pid.trim()])
                .status()?
                .success(),
            "promoted process is not live"
        );
    }
    process.child.kill()?;
    assert_eq!(process.child.wait()?.signal(), Some(9));
    Ok(())
}

fn request_report(command: &ControlCommand) -> TestResult<ProductProbeReport> {
    let mut process = spawn(command)?;
    await_started(&mut process, "report")?;
    let report = loop {
        match process.read("report")? {
            ChildReply::ProductProbed {
                version: PROTOCOL_VERSION,
                report,
            } => break *report,
            ChildReply::Boundary { .. } => {}
            ChildReply::Failed { message, .. } => return Err(message.into()),
            other => return Err(format!("unexpected report reply: {other:?}").into()),
        }
    };
    if !process.child.wait()?.success() {
        return Err("report child failed".into());
    }
    Ok(report)
}

fn relaunch_report(
    command: &ControlCommand,
    probe: &ControlCommand,
) -> TestResult<(ProductProbeReport, Option<String>)> {
    let mut process = spawn(command)?;
    await_started(&mut process, "startup-reconciler")?;
    let outcome = loop {
        match process.read("startup-reconciler")? {
            ChildReply::ProductProbed {
                version: PROTOCOL_VERSION,
                report,
            } => break (Some(*report), None),
            ChildReply::Boundary { .. } | ChildReply::ProductBoundary { .. } => {}
            ChildReply::Failed { message, .. }
                if message.contains("requires recovery")
                    || message.contains("unresolved working turn")
                    || message.contains("not the current exact branch head") =>
            {
                let classification = if message.contains("not the current exact branch head") {
                    "stale_head"
                } else {
                    "recovery_required"
                };
                break (None, Some(classification.to_owned()));
            }
            ChildReply::Failed { message, .. } => return Err(message.into()),
            other => return Err(format!("unexpected startup reply: {other:?}").into()),
        }
    };
    let status = process.child.wait()?;
    match outcome {
        (Some(report), None) if status.success() => Ok((report, None)),
        (None, Some(classification)) if !status.success() => {
            Ok((request_report(probe)?, Some(classification)))
        }
        _ => Err(format!("startup reconciler exit contradicted its reply: {status}").into()),
    }
}

fn kill_at_physical(command: &ControlCommand) -> TestResult {
    let mut process = spawn(command)?;
    await_started(&mut process, "next-prompt-provider-dispatch")?;
    loop {
        match process.read("next-prompt-provider-dispatch")? {
            ChildReply::Boundary {
                version: PROTOCOL_VERSION,
                boundary: super::protocol::CrashBoundary::ProviderDispatchOne,
                ..
            } => break,
            ChildReply::Boundary { .. } | ChildReply::ProductBoundary { .. } => {}
            ChildReply::Failed { message, .. } => return Err(message.into()),
            other => return Err(format!("unexpected physical reply: {other:?}").into()),
        }
    }
    process.child.kill()?;
    assert_eq!(process.child.wait()?.signal(), Some(9));
    Ok(())
}
