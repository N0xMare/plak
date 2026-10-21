//! Cut-point × route × face crash matrix for the public durable-session facade.
//!
//! The existing `crash_matrix.rs` cuts one binding at the boundaries the
//! loopback controller announces. This file widens that in three directions.
//!
//! **Cut points.** Every seam a process death can land on: before the turn,
//! inside the first model request, at its terminal event, inside a running
//! tool (the loopback effect, and a real child process), after the tool result
//! before the second round, inside the second request, at the terminal result
//! before publication, after publication, inside a compaction fold, around the
//! placeholder rung, and inside an effect-decision frame. Seams with no public hook
//! (the interval between a model response and its tool-batch journal, the
//! interval between a tool result and the next admission, the placeholder pass
//! transformation itself, a running Code Cell) are bracketed by the boundary on
//! either side and said so in the cell table, never faked.
//!
//! **Routes.** For a custom `LanguageModel` the durable layer is route-inert: a
//! protocol family id enters the binding identity and nothing else, so running
//! the same cut three times over three family labels would prove nothing. The
//! place a wire shape really matters after a crash is the *next request*: the
//! recovered history, abandonment note included, has to lower on every wire
//! the product speaks. So every cell lowers its recovered canonical history
//! through the three real request builders (Anthropic Messages, `OpenAI`
//! Responses, Chat Completions) and asserts the note reaches each body. Code
//! Mode composition is a genuinely different binding and gets its own cuts.
//!
//! **Faces.** The in-process durable session (this file's child mode, which
//! re-executes the test binary itself so a scenario can be selected by data),
//! and the real `plak` binary on its JSONL face through the scripted model,
//! where the harness can reach a cut deterministically.
//!
//! Every cell asserts the reopen outcome class, the canonical history
//! (byte-identical, identities aside, to the prefix a complete run produces,
//! plus at most the abandonment note), that the physical evidence log is
//! identical before and after the probe (no redispatch), and the wire
//! lowering. Assertions are matched on exact typed reports, never on prose.

#![cfg(unix)]
#![forbid(unsafe_code)]
#![allow(
    clippy::too_many_lines,
    reason = "the matrix keeps its scenarios, classifier and cell table visibly together"
)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use plakous::agent::{AgentEventKind, TurnResultOutcome};
use plakous::model::{
    Actor, Capability, CapabilityConstraints, CapabilitySet, ConfiguredTool, ContentBlock,
    ConversationItem, ConversationView, Digest, DigestDomain, EffectiveProviderConfigurationId,
    FactProvenance, FactSource, FeatureSupport, ModelAdapterProfileId, ModelFamilyId, ModelId,
    ModelRequest, ModelResponse, ProtocolId, Provenance, ReasoningPolicy, RequestMetadata,
    ResolvedModelDescriptor, RouteId, SelectedProviderConfigurationId, Support, Timestamp,
    ToolCallerDisposition, ToolConcurrency, ToolDefinition, ToolEffect, ToolExecutionDisposition,
    ToolInputContract, ToolOutputContract, ValidationMode,
};
use plakous::sessions::{
    AgentSessions, AgentSessionsErrorKind, CompactReason, CompactSessionOutcome,
    CompactSessionRequest, ContextQuery, ContextStatusRequest, CreateSessionOutcome,
    CreateSessionRequest, ForkSessionOutcome, ForkSessionRequest, OpenSessionOutcome,
    OpenSessionRequest, RebindSessionOutcome, RebindSessionRequest, RecoverSessionOutcome,
    RecoverSessionRequest, RecoveryRequiredReport, SessionBackend, SessionContextStatusKind,
    SessionLocator, SessionOperationId, SessionRecoveryAction, SessionRecoveryPhase,
    SessionRuntimeBinding, SessionWorkflowUnavailable,
};
use plakous_chat_completions::{
    ChatOutputLimitField, ChatReasoningReplay, ChatRequestPolicy, ReasoningAliasPolicy,
    lower_request as lower_chat_request,
};
use plakous_messages::{
    MessagesRequestPolicy, ToolOwnership, lower_request as lower_messages_request,
};
use plakous_offline_crash_harness::evidence::{EvidenceKind, EvidenceRecord, read_evidence};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::script::ScriptPlan;
use plakous_offline_crash_harness::{
    BoundaryController, scripted_code_mode_runtime_binding, scripted_pi_runtime_binding,
    scripted_runtime_binding_with_cursor,
};
use plakous_responses_dialect::{
    ResponsesRequestConfig, ResponsesRequestPolicy, ResponsesStoragePolicy, lower_responses_request,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
type ChildResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

/// How long a child may take to reach a boundary or to answer a probe.
const CHILD_DEADLINE: Duration = Duration::from_secs(30);

/// The environment variable that turns the re-executed test binary into a child.
const COMMAND_ENV: &str = "CRASH_MATRIX_ROUTES_COMMAND";

/// The provider note the product writes when it abandons a lost model request.
const PROVIDER_NOTE: &str = "the model request for this turn was abandoned after its outcome \
                             became uncertain";
/// The tool note the product writes when it abandons a turn cut inside a tool.
const TOOL_NOTE: &str = "this turn was abandoned after the process was lost while a tool was \
                         running";
/// The marker the placeholder rung leaves on a body it removed.
const PLACEHOLDER_MARKER: &str = "[plak: tool result replaced]";
/// The fixture's ordinary final assistant text.
const FINAL_TEXT: &str = "loopback turn committed";

// ---------------------------------------------------------------------------
// Scenarios, cut points and the child command
// ---------------------------------------------------------------------------

/// One binding shape the durable session is exercised with.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scenario {
    /// The scripted loopback model with one mutating loopback effect.
    Direct,
    /// The standard coding packs over a real POSIX shell: the tool starts a
    /// child process and joins it, so a cut lands inside a running process.
    Process,
    /// Two large reads either side of the context fence, so the scheduled
    /// placeholder rung runs between rounds.
    Growth,
    /// A policy that parks every mutating call in an effect-decision frame.
    Decision,
    /// Two quiescent turns and then a manual fold through the summary binding.
    Fold,
    /// A real Code Mode attachment: the model emits a cell, the sidecar runs it.
    CodeMode,
}

/// Where the child is parked for `SIGKILL`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Cut {
    /// Run to completion.
    None,
    /// One controller boundary of the main binding.
    Boundary(CrashBoundary),
    /// The tool's child process has started (it created a marker file).
    ProcessRunning,
    /// The turn published `EffectDecisionRequired` and is waiting.
    DecisionPending,
    /// One controller boundary of the *summary* binding, during a manual fold.
    Fold(CrashBoundary),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Run,
    Probe,
}

/// Everything a child needs, written to a file the child reads by path.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChildCommand {
    mode: Mode,
    scenario: Scenario,
    cut: Cut,
    database: PathBuf,
    workspace: PathBuf,
    evidence: PathBuf,
    fold_evidence: PathBuf,
    state: PathBuf,
    reply: PathBuf,
    marker: PathBuf,
    sidecar: Option<PathBuf>,
}

/// What a child writes back, fsynced, as its last act.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChildReply {
    /// The decision frame was reached; the child is parked.
    Reached {
        what: String,
    },
    /// A no-cut run finished.
    Finished {
        locator: String,
        items: Vec<Value>,
    },
    /// A probe classified the persisted head.
    Probed {
        report: Report,
    },
    Failed {
        message: String,
    },
}

/// The closed reopen outcome classes.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Report {
    /// The reopen adopted a quiescent head and found no abandoned turn.
    Exact {
        source: String,
        locator: String,
        items: Vec<Value>,
    },
    /// The reopen closed a lost turn by abandoning it.
    Abandoned {
        source: String,
        locator: String,
        items: Vec<Value>,
        operations: Vec<u64>,
        effects: Vec<u64>,
        note: String,
    },
    /// The reopen restored the cut turn at its decision barrier: the turn is
    /// live again, waiting for the decision the lost process never got.
    RestoredWorking {
        source: String,
        locator: String,
        items: Vec<Value>,
        pending_decision: bool,
    },
    /// Persisted working authority that forbids ordinary resume.
    RecoveryRequired {
        source: String,
        locator: String,
        phase: String,
        obligations: Vec<(u64, String)>,
        /// Every public bypass workflow returned the same recovery report.
        bypass_blocked: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LocatorState {
    source: String,
    latest: String,
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

fn direct_script() -> Value {
    json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": FINAL_TEXT}}
        ]
    })
}

// Orphans expire on their own, including on assertion failures. A historical
// pid marker is observation evidence, never authority to signal a process:
// even a live pid may have been recycled after the fixture child exited.
// Keep the lifetime longer than the reopen deadlines and survival assertions.
const PROCESS_ORPHAN_COMMAND: &str =
    "printf '%s\\n' \"$$\" > process-started.marker; exec sleep 120";
const FACE_ORPHAN_COMMAND: &str = "printf '%s\\n' \"$$\" > face-process.pid; exec sleep 120";

fn process_script(workspace: &Path) -> Value {
    json!({
        "version": 1,
        "repeat_last_step": true,
        "process_tools": true,
        "steps": [
            {"output": {"tool": {"name": "process_session_start", "arguments": {
                "command": PROCESS_ORPHAN_COMMAND,
                "workdir": ".",
                "login": false,
                "tty": false,
                "keep_stdin_open": true,
                "yield_time_ms": 5000,
                "max_output_bytes": 8192
            }}}, "finish": "tool_calls"},
            {"output": {"text": FINAL_TEXT}, "gate": {"file": workspace.join("release-process-answer")}}
        ]
    })
}

fn growth_script() -> Value {
    // Every round reports its projected size as the provider input, so the
    // measure anchors and the fence can act (an unanchored estimate never
    // schedules a compaction).
    json!({
        "version": 1,
        "repeat_last_step": true,
        "maximum_projected_input": 20_000,
        "scheduled_compaction": true,
        "content_sensitive_projection": true,
        "tool_result_bytes": 7_500,
        "fold": "refuse",
        "steps": [
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls", "usage": {}},
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls", "usage": {}},
            {"output": {"text": FINAL_TEXT}, "usage": {}}
        ]
    })
}

fn decision_script() -> Value {
    let mut script = direct_script();
    script["tool_policy"] = Value::from("decision_required");
    script
}

fn fold_script() -> Value {
    let mut script = direct_script();
    script["fold"] = Value::from("accept");
    // The manual fold reuses this turn script, and its proof measures the same
    // counter. A flat count cannot show that the successor is smaller.
    script["content_sensitive_projection"] = Value::from(true);
    script
}

/// A cell that calls `exec_command` twice and emits both results.
const TWO_NESTED_CALLS: &str = r#"
const one = await tools.exec_command({ cmd: "printf nested-one" });
const two = await tools.exec_command({ cmd: "printf nested-two" });
if (one.output !== "nested-one" || one.exit_code !== 0) throw new Error("bad first process object");
if (two.output !== "nested-two" || two.exit_code !== 0) throw new Error("bad second process object");
text(one.output + "|" + two.output);
"#;

fn code_mode_script() -> Value {
    json!({
        "version": 1,
        "code_mode": true,
        "repeat_last_step": true,
        "steps": [
            {"output": {"cell": TWO_NESTED_CALLS}, "finish": "tool_calls"},
            {"output": {"text": FINAL_TEXT}}
        ]
    })
}

fn script_for(scenario: Scenario, workspace: &Path) -> ChildResult<Arc<ScriptPlan>> {
    let document = match scenario {
        Scenario::Direct => direct_script(),
        Scenario::Process => process_script(workspace),
        Scenario::Growth => growth_script(),
        Scenario::Decision => decision_script(),
        Scenario::Fold => fold_script(),
        Scenario::CodeMode => code_mode_script(),
    };
    let plan: ScriptPlan = serde_json::from_value(document)?;
    plan.validate()?;
    Ok(Arc::new(plan))
}

// ---------------------------------------------------------------------------
// Child: bindings, run and probe
// ---------------------------------------------------------------------------

/// The re-executed child. Selected by `CRASH_MATRIX_ROUTES_COMMAND`, which
/// names the command file; without it this test is a no-op so an ordinary
/// `cargo test` run never spawns anything from here.
#[test]
#[ignore = "child entry point of crash_matrix_routes; spawned by the matrix itself"]
fn crash_matrix_routes_child() {
    let Some(path) = std::env::var_os(COMMAND_ENV) else {
        return;
    };
    let command: ChildCommand = match File::open(&path)
        .map_err(|error| error.to_string())
        .and_then(|file| serde_json::from_reader(file).map_err(|error| error.to_string()))
    {
        Ok(command) => command,
        Err(message) => {
            eprintln!("crash matrix child command unreadable: {message}");
            std::process::exit(2);
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("crash matrix child could not start a runtime: {error}");
            std::process::exit(2);
        }
    };
    let reply = runtime.block_on(async {
        match command.mode {
            Mode::Run => run_child(&command).await,
            Mode::Probe => probe_child(&command).await,
        }
    });
    let reply = match reply {
        Ok(reply) => reply,
        Err(error) => ChildReply::Failed {
            message: error.to_string().chars().take(1024).collect(),
        },
    };
    if let Err(error) = write_reply(&command.reply, &reply) {
        eprintln!("crash matrix child could not write its reply: {error}");
        std::process::exit(2);
    }
}

fn write_reply(path: &Path, reply: &ChildReply) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(reply).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn write_locators(path: &Path, source: SessionLocator, latest: SessionLocator) -> ChildResult {
    let temporary = path.with_extension("tmp");
    let mut bytes = serde_json::to_vec(&LocatorState {
        source: source.to_string(),
        latest: latest.to_string(),
    })?;
    bytes.push(b'\n');
    let mut file = File::create(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    File::open(path.parent().ok_or("state path has no parent")?)?.sync_all()?;
    Ok(())
}

/// The main binding of one scenario, cut at `boundary`.
fn main_binding(
    command: &ChildCommand,
    boundary: CrashBoundary,
    cursor: &Arc<AtomicU64>,
) -> ChildResult<SessionRuntimeBinding> {
    let controller = BoundaryController::quiet_from_evidence_path(boundary, &command.evidence)?;
    let script = script_for(command.scenario, &command.workspace)?;
    match command.scenario {
        Scenario::CodeMode => {
            let sidecar = command
                .sidecar
                .as_ref()
                .ok_or("the Code Mode scenario needs a sidecar path")?;
            scripted_code_mode_runtime_binding(
                &command.workspace,
                controller,
                script,
                Arc::clone(cursor),
                sidecar,
                None,
            )
        }
        // One script position per process, shared by every binding built for
        // the session: a second turn is round three, not round one again.
        _ => scripted_runtime_binding_with_cursor(
            &command.workspace,
            controller,
            script,
            Arc::clone(cursor),
        ),
    }
}

/// The binding a manual fold goes through, cut at `boundary`.
///
/// What the product does (`hosted/src/runtime/startup.rs`): the session's own
/// provider configuration with the compaction generation limit, because the
/// fold operation must be exact for its source. The script cursor starts past
/// the tool step so the summary request is answered with text, and the fold's
/// physical evidence goes to its own file so its rows never interleave with
/// the turn controller's sequence.
fn summary_binding(
    command: &ChildCommand,
    boundary: CrashBoundary,
) -> ChildResult<SessionRuntimeBinding> {
    let controller =
        BoundaryController::quiet_from_evidence_path(boundary, &command.fold_evidence)?;
    Ok(scripted_runtime_binding_with_cursor(
        &command.workspace,
        controller,
        script_for(command.scenario, &command.workspace)?,
        Arc::new(AtomicU64::new(3)),
    )?
    .with_compaction_output_limit()?)
}

fn sessions(command: &ChildCommand) -> ChildResult<AgentSessions> {
    let backend = SessionBackend::sqlite(&command.database)
        .map_err(|error| format!("backend open failed with {:?}", error.kind()))?;
    AgentSessions::new(backend)
        .map_err(|error| format!("service create failed with {:?}", error.kind()).into())
}

fn items_of(view: &ConversationView) -> ChildResult<Vec<Value>> {
    view.items()
        .into_iter()
        .map(|item| serde_json::to_value(item).map_err(Into::into))
        .collect()
}

async fn run_child(command: &ChildCommand) -> ChildResult<ChildReply> {
    std::fs::create_dir_all(&command.workspace)?;
    let main_cut = match command.cut {
        Cut::Boundary(boundary) => boundary,
        _ => CrashBoundary::None,
    };
    let fold_cut = match command.cut {
        Cut::Fold(boundary) => boundary,
        _ => CrashBoundary::None,
    };
    let cursor = Arc::new(AtomicU64::new(1));
    let sessions = sessions(command)?;
    let created = Box::pin(sessions.create(CreateSessionRequest::new(
        SessionOperationId::new("crash-matrix-routes:create")?,
        main_binding(command, main_cut, &cursor)?,
    )))
    .await
    .map_err(|error| format!("session create failed with {:?}: {error}", error.kind()))?;
    let CreateSessionOutcome::Created { session, .. } = created else {
        return Err("session create returned an unknown outcome".into());
    };
    let old_locator = session.release().await?;
    write_locators(&command.state, old_locator, old_locator)?;
    // The genesis boundary is announced by the controller that created the
    // session; this scenario's controllers are per binding, so announce it
    // through one built for exactly that.
    BoundaryController::quiet_from_evidence_path(CrashBoundary::None, &command.evidence)?;
    if main_cut == CrashBoundary::GenesisReleased {
        append_genesis_row(command)?;
        std::future::pending::<()>().await;
    }
    append_genesis_row(command)?;

    let mut latest = old_locator;
    let prompts: &[&str] = match command.scenario {
        Scenario::Fold => &["first intent: perform the loopback effect", "second intent"],
        _ => &["perform the loopback effect"],
    };
    for prompt in prompts {
        let opened = Box::pin(sessions.open(OpenSessionRequest::new(
            SessionOperationId::new(format!(
                "crash-matrix-routes:open:{}",
                latest.checkpoint_id()
            ))?,
            latest,
            main_binding(command, main_cut, &cursor)?,
        )))
        .await
        .map_err(|error| format!("turn open failed with {:?}: {error}", error.kind()))?;
        let OpenSessionOutcome::Opened { session, .. } = opened else {
            return Err("a released quiescent head did not reopen as quiescent".into());
        };
        let turn = session.agent().prompt(*prompt).await?;
        if command.cut == Cut::DecisionPending {
            let mut events = turn.subscribe()?;
            loop {
                let event = events.recv().await?;
                if matches!(event.kind(), AgentEventKind::EffectDecisionRequired { .. }) {
                    write_reply(
                        &command.reply,
                        &ChildReply::Reached {
                            what: "effect_decision".to_owned(),
                        },
                    )?;
                    std::future::pending::<()>().await;
                }
            }
        }
        let result = turn.result().await?;
        if !matches!(result.outcome(), TurnResultOutcome::Completed(_))
            || result.response().map(ModelResponse::output_text).as_deref() != Some(FINAL_TEXT)
        {
            return Err(format!(
                "the turn did not complete with the exact final text: outcome={:?}, text={:?}",
                result.outcome(),
                result.response().map(ModelResponse::output_text)
            )
            .into());
        }
        if command.scenario == Scenario::Fold && Some(prompt) == prompts.last() {
            // A manual fold is a workflow on the attached driver, so it runs
            // while the session is still open, on the head the last turn
            // published.
            let compacted = sessions
                .compact(CompactSessionRequest::new(
                    SessionOperationId::new("crash-matrix-routes:compact")?,
                    session.locator(),
                    CompactReason::Manual,
                    summary_binding(command, fold_cut)?,
                ))
                .await?;
            let CompactSessionOutcome::Compacted { .. } = compacted else {
                return Err(format!("the manual fold did not compact: {compacted:?}").into());
            };
        }
        latest = session.release().await?;
        write_locators(&command.state, old_locator, latest)?;
    }

    append_released_row(command)?;
    if main_cut == CrashBoundary::CheckpointReleased {
        std::future::pending::<()>().await;
    }
    let items = {
        let opened = Box::pin(sessions.open(OpenSessionRequest::new(
            SessionOperationId::new("crash-matrix-routes:read-back")?,
            latest,
            main_binding(command, CrashBoundary::None, &cursor)?,
        )))
        .await?;
        let OpenSessionOutcome::Opened { session, .. } = opened else {
            return Err("the finished head did not reopen for the read-back".into());
        };
        let items = items_of(&session.agent().conversation())?;
        latest = session.release().await?;
        write_locators(&command.state, old_locator, latest)?;
        items
    };
    Ok(ChildReply::Finished {
        locator: latest.to_string(),
        items,
    })
}

/// The controller's own genesis/checkpoint rows are announced by the fixture's
/// `run_fixture`, not by a binding; this child appends them the same way so the
/// evidence log has the same shape the existing matrix reads.
fn append_genesis_row(command: &ChildCommand) -> ChildResult {
    append_row(
        command,
        EvidenceKind::OldCheckpointReleased,
        CrashBoundary::GenesisReleased,
    )
}

fn append_released_row(command: &ChildCommand) -> ChildResult {
    append_row(
        command,
        EvidenceKind::NewCheckpointReleased,
        CrashBoundary::CheckpointReleased,
    )
}

fn append_row(command: &ChildCommand, kind: EvidenceKind, boundary: CrashBoundary) -> ChildResult {
    let existing = read_evidence(&command.evidence)?;
    let sequence = existing
        .last()
        .map_or(1, |record| record.sequence.saturating_add(1));
    let row = EvidenceRecord {
        version: plakous_offline_crash_harness::protocol::PROTOCOL_VERSION,
        sequence,
        kind,
        round: None,
        operation_id: None,
        boundary,
    };
    let mut bytes = serde_json::to_vec(&row)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&command.evidence)?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_data()?;
    Ok(())
}

async fn probe_child(command: &ChildCommand) -> ChildResult<ChildReply> {
    let state: LocatorState = serde_json::from_reader(BufReader::new(File::open(&command.state)?))?;
    let source = state.source.parse::<SessionLocator>()?;
    let mut locator = state.latest.parse::<SessionLocator>()?;
    let cursor = Arc::new(AtomicU64::new(1));
    let sessions = sessions(command)?;
    for attempt in 1_u8..=2 {
        let opened = Box::pin(sessions.open(OpenSessionRequest::new(
            SessionOperationId::new(format!("crash-matrix-routes:probe-open-{attempt}"))?,
            locator,
            main_binding(command, CrashBoundary::None, &cursor)?,
        )))
        .await;
        match opened {
            Err(error) if error.kind() == AgentSessionsErrorKind::StaleLocator => {
                locator = error
                    .current_locator()
                    .ok_or("stale locator did not expose the current exact head")?;
            }
            Err(error) => return Err(error.into()),
            Ok(OpenSessionOutcome::RecoveryRequired { recovery, .. }) => {
                let bypass_blocked = Box::pin(bypass_workflows_stay_blocked(
                    command, &sessions, &recovery, &cursor,
                ))
                .await?;
                return Ok(ChildReply::Probed {
                    report: recovery_report(source, &recovery, bypass_blocked),
                });
            }
            Ok(OpenSessionOutcome::Opened { session, .. }) => {
                let view = session.agent().conversation();
                let items = items_of(&view)?;
                let report = match session.agent().abandoned_turn() {
                    Some(abandoned) => {
                        let note = view
                            .items()
                            .into_iter()
                            .filter_map(|item| match item.blocks().first() {
                                Some(ContentBlock::Text(text)) => Some(text.text().to_owned()),
                                _ => None,
                            })
                            .find(|text| text.contains("abandoned"))
                            .ok_or("an abandoned turn left no note in its canonical history")?;
                        Report::Abandoned {
                            source: source.to_string(),
                            locator: session.locator().to_string(),
                            items,
                            operations: abandoned.operations().to_vec(),
                            effects: abandoned.effects().to_vec(),
                            note,
                        }
                    }
                    None => Report::Exact {
                        source: source.to_string(),
                        locator: session.locator().to_string(),
                        items,
                    },
                };
                session.release().await?;
                return Ok(ChildReply::Probed { report });
            }
            Ok(OpenSessionOutcome::RestoredWorking { session, turn, .. }) => {
                let items = items_of(&session.agent().conversation())?;
                let report = Report::RestoredWorking {
                    source: source.to_string(),
                    locator: session.locator().to_string(),
                    items,
                    pending_decision: turn.pending_decision().is_some(),
                };
                session.release().await?;
                return Ok(ChildReply::Probed { report });
            }
            Ok(_) => return Err("session open returned an unknown outcome".into()),
        }
    }
    Err("stale locator retry did not reach the current exact head".into())
}

/// Recovery, context status, compaction, fork and rebind must all return the
/// same recovery authority rather than bypass it, and none may dispatch.
async fn bypass_workflows_stay_blocked(
    command: &ChildCommand,
    sessions: &AgentSessions,
    recovery: &RecoveryRequiredReport,
    cursor: &Arc<AtomicU64>,
) -> ChildResult<bool> {
    let recover_operation = SessionOperationId::new("crash-matrix-routes:probe-recover")?;
    let outcome = Box::pin(sessions.recover(RecoverSessionRequest::new(
        recover_operation.clone(),
        recovery.locator(),
        main_binding(command, CrashBoundary::None, cursor)?,
    )))
    .await?;
    let recover_blocked = matches!(
        &outcome,
        RecoverSessionOutcome::Unavailable {
            operation_id,
            recovery: still_required,
            reason: SessionWorkflowUnavailable::WorkingRecovery,
        } if operation_id == &recover_operation && still_required == recovery
    );

    let context_operation = SessionOperationId::new("crash-matrix-routes:probe-context")?;
    let context = sessions
        .context_status(ContextStatusRequest::new(
            context_operation.clone(),
            recovery.locator(),
            ContextQuery::Current,
        ))
        .await?;
    let context_blocked = context.operation_id() == &context_operation
        && context.locator() == recovery.locator()
        && matches!(
            context.kind(),
            SessionContextStatusKind::RecoveryRequired(still_required) if still_required == recovery
        );

    let compact_operation = SessionOperationId::new("crash-matrix-routes:probe-compact")?;
    let compact = sessions
        .compact(CompactSessionRequest::new(
            compact_operation.clone(),
            recovery.locator(),
            CompactReason::Manual,
            main_binding(command, CrashBoundary::None, cursor)?,
        ))
        .await?;
    let compact_blocked = matches!(
        compact,
        CompactSessionOutcome::RecoveryRequired {
            operation_id,
            recovery: still_required,
            ..
        } if operation_id == compact_operation && still_required == *recovery
    );

    let fork_operation = SessionOperationId::new("crash-matrix-routes:probe-fork")?;
    let fork = sessions
        .fork(ForkSessionRequest::new(
            fork_operation.clone(),
            recovery.locator(),
        ))
        .await?;
    let fork_blocked = matches!(
        fork,
        ForkSessionOutcome::RecoveryRequired {
            operation_id,
            recovery: still_required,
        } if operation_id == fork_operation && still_required == *recovery
    );

    let runtime = main_binding(command, CrashBoundary::None, cursor)?;
    let target = runtime.target_binding();
    let rebind_operation = SessionOperationId::new("crash-matrix-routes:probe-rebind")?;
    let rebound = Box::pin(sessions.rebind(RebindSessionRequest::new(
        rebind_operation.clone(),
        recovery.locator(),
        target,
        runtime,
    )))
    .await?;
    let rebind_blocked = matches!(
        rebound,
        RebindSessionOutcome::RecoveryRequired {
            operation_id,
            recovery: still_required,
        } if operation_id == rebind_operation && still_required == *recovery
    );

    let reopened = Box::pin(sessions.open(OpenSessionRequest::new(
        SessionOperationId::new("crash-matrix-routes:probe-reopen-working")?,
        recovery.locator(),
        main_binding(command, CrashBoundary::None, cursor)?,
    )))
    .await?;
    let reopen_blocked = matches!(
        reopened,
        OpenSessionOutcome::RecoveryRequired { recovery: still_required, .. }
            if still_required == *recovery
    );
    Ok(recover_blocked
        && context_blocked
        && compact_blocked
        && fork_blocked
        && rebind_blocked
        && reopen_blocked)
}

fn recovery_report(
    source: SessionLocator,
    recovery: &RecoveryRequiredReport,
    bypass_blocked: bool,
) -> Report {
    let phase = match recovery.phase() {
        SessionRecoveryPhase::DispatchEvidence => "dispatch_evidence",
        SessionRecoveryPhase::EffectReconciliation => "effect_reconciliation",
        SessionRecoveryPhase::CommitRecovery => "commit_recovery",
        SessionRecoveryPhase::Finalization => "finalization",
        _ => "unknown",
    };
    let obligations = recovery
        .obligations()
        .iter()
        .map(|obligation| {
            let action = match obligation.action() {
                SessionRecoveryAction::CommitRecovery => "commit_recovery",
                SessionRecoveryAction::ProviderEvidenceRequired => "provider_evidence_required",
                SessionRecoveryAction::EffectEvidenceRequired => "effect_evidence_required",
                SessionRecoveryAction::ProvenNotDispatched => "proven_not_dispatched",
                SessionRecoveryAction::FinalizationRequired => "finalization_required",
                _ => "unknown",
            };
            (obligation.operation_id(), action.to_owned())
        })
        .collect();
    Report::RecoveryRequired {
        source: source.to_string(),
        locator: recovery.locator().to_string(),
        phase: phase.to_owned(),
        obligations,
        bypass_blocked,
    }
}

// ---------------------------------------------------------------------------
// Parent: fixture, child control, cut waiting
// ---------------------------------------------------------------------------

struct Fixture {
    root: TempDir,
    command: ChildCommand,
}

impl Fixture {
    fn new(scenario: Scenario) -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let sidecar = (scenario == Scenario::CodeMode)
            .then(sidecar_path)
            .flatten();
        if scenario == Scenario::CodeMode && sidecar.is_none() {
            return Err("plakous-js-host is not built beside this test binary; \
                        run `cargo build -p plakous-js-host` first"
                .into());
        }
        Ok(Self {
            command: ChildCommand {
                mode: Mode::Run,
                scenario,
                cut: Cut::None,
                database: root.path().join("session.sqlite3"),
                marker: workspace.join("process-started.marker"),
                workspace,
                evidence: root.path().join("physical-evidence.jsonl"),
                fold_evidence: root.path().join("fold-evidence.jsonl"),
                state: root.path().join("locator.json"),
                reply: root.path().join("reply.jsonl"),
                sidecar,
            },
            root,
        })
    }

    fn evidence(&self) -> TestResult<(Vec<EvidenceRecord>, Vec<EvidenceRecord>)> {
        Ok((
            read_evidence(&self.command.evidence)?,
            read_evidence(&self.command.fold_evidence)?,
        ))
    }
}

/// The JavaScript sidecar, when it has been built into the same target dir.
fn sidecar_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PLAK_JS_HOST") {
        return Some(PathBuf::from(explicit));
    }
    let deps = std::env::current_exe().ok()?;
    let target = deps.parent()?.parent()?;
    let candidate = target.join("plakous-js-host");
    candidate.is_file().then_some(candidate)
}

struct CrashChild {
    child: Option<Child>,
    _command_file: PathBuf,
}

impl CrashChild {
    fn spawn(fixture: &Fixture, mode: Mode, cut: Cut) -> TestResult<Self> {
        let command = ChildCommand {
            mode,
            cut,
            ..fixture.command.clone()
        };
        let _ = std::fs::remove_file(&command.reply);
        let command_file = fixture
            .root
            .path()
            .join(format!("command-{}.json", u8::from(mode == Mode::Probe)));
        std::fs::write(&command_file, serde_json::to_vec(&command)?)?;
        let child = Command::new(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                "crash_matrix_routes_child",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env(COMMAND_ENV, &command_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        Ok(Self {
            child: Some(child),
            _command_file: command_file,
        })
    }

    fn sigkill(&mut self) -> TestResult<i32> {
        let child = self.child.as_mut().ok_or("child is no longer live")?;
        child.kill()?;
        let status = child.wait()?;
        self.child = None;
        Ok(status.signal().unwrap_or_default())
    }

    fn exited(&mut self) -> TestResult<Option<bool>> {
        let child = self.child.as_mut().ok_or("child is no longer live")?;
        Ok(child.try_wait()?.map(|status| status.success()))
    }

    fn wait_success(&mut self, deadline: Instant) -> TestResult {
        loop {
            match self.exited()? {
                Some(true) => return Ok(()),
                Some(false) => return Err("child exited unsuccessfully".into()),
                None if Instant::now() >= deadline => {
                    return Err("child exit deadline elapsed".into());
                }
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

impl Drop for CrashChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn read_reply(path: &Path) -> TestResult<Option<ChildReply>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut last = None;
    for line in BufReader::new(File::open(path)?).lines() {
        last = Some(serde_json::from_str(&line?)?);
    }
    Ok(last)
}

/// Blocks until the child is parked at `cut`, then returns.
///
/// A boundary is observed through the fsynced evidence row the controller
/// writes immediately before it parks; nothing durable happens in between, so
/// a kill after the row is a kill inside the boundary. The process cut is
/// observed through the marker file the child command creates, and the
/// decision cut through the reply the child writes on the event.
fn await_cut(fixture: &Fixture, child: &mut CrashChild, cut: Cut) -> TestResult {
    let deadline = Instant::now() + CHILD_DEADLINE;
    loop {
        let reached = match cut {
            Cut::None => return Err("a no-cut run has nothing to wait for".into()),
            Cut::Boundary(boundary) => read_evidence(&fixture.command.evidence)?
                .iter()
                .any(|record| record.boundary == boundary),
            Cut::Fold(boundary) => read_evidence(&fixture.command.fold_evidence)?
                .iter()
                .any(|record| record.boundary == boundary),
            Cut::ProcessRunning => fixture.command.marker.exists(),
            Cut::DecisionPending => matches!(
                read_reply(&fixture.command.reply)?,
                Some(ChildReply::Reached { .. })
            ),
        };
        if reached {
            return Ok(());
        }
        if let Some(success) = child.exited()? {
            return Err(format!(
                "child exited (success={success}) before reaching {cut:?}; reply: {:?}",
                read_reply(&fixture.command.reply)?
            )
            .into());
        }
        if Instant::now() >= deadline {
            return Err(format!("child never reached {cut:?}").into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn probe(fixture: &Fixture) -> TestResult<Report> {
    let mut child = CrashChild::spawn(fixture, Mode::Probe, Cut::None)?;
    child.wait_success(Instant::now() + CHILD_DEADLINE)?;
    match read_reply(&fixture.command.reply)? {
        Some(ChildReply::Probed { report }) => Ok(report),
        Some(ChildReply::Failed { message }) => Err(format!("probe failed: {message}").into()),
        other => Err(format!("probe returned an unexpected reply: {other:?}").into()),
    }
}

fn complete_run(fixture: &Fixture) -> TestResult<(String, Vec<Value>)> {
    if fixture.command.scenario == Scenario::Process {
        std::fs::write(
            fixture.command.workspace.join("release-process-answer"),
            b"release",
        )?;
    }
    let mut child = CrashChild::spawn(fixture, Mode::Run, Cut::None)?;
    child.wait_success(Instant::now() + CHILD_DEADLINE)?;
    match read_reply(&fixture.command.reply)? {
        Some(ChildReply::Finished { locator, items }) => Ok((locator, items)),
        Some(ChildReply::Failed { message }) => Err(format!("run failed: {message}").into()),
        other => Err(format!("run returned an unexpected reply: {other:?}").into()),
    }
}

// ---------------------------------------------------------------------------
// Parent: assertions
// ---------------------------------------------------------------------------

/// What a cell expects of its reopen.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// Exact quiescent head with this many canonical items and no note.
    Exact(usize),
    /// Exact quiescent head after the open closed an admitted session-workflow
    /// compaction without applying it: the uncompacted source history, with
    /// this many items and no note, so not a prefix of the compacted run.
    CompactionAbandoned(usize),
    /// Recovered by abandoning a model request: these operations, this many
    /// items including the note.
    AbandonedRequest(&'static [u64], usize),
    /// Recovered by abandoning a tool: these operations, of which these
    /// effects, this many items including the note.
    AbandonedTool(&'static [u64], &'static [u64], usize),
    /// Restored at a decision barrier with this many items and a pending
    /// decision.
    Restored(usize),
}

/// One cell of the in-process matrix.
struct Cell {
    scenario: Scenario,
    cut: Cut,
    expect: Expect,
    /// The mutation this cell was seen to fail on (see the report).
    #[allow(dead_code, reason = "documentation carried in the table")]
    killed_by: &'static str,
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
    if provider_rounds.values().any(|count| *count > 1) {
        return Err("physical provider evidence contains a blind redispatch".into());
    }
    if effects > 2 {
        return Err("physical effect evidence contains a blind redispatch".into());
    }
    Ok(())
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

/// Strips every identity that is not stable across processes (entry ids are
/// minted at random; the scripted call ids are derived from a projected size
/// that moves by a few bytes between runs), so two runs of one scenario
/// compare byte-for-byte on what the model and the tools said.
fn without_identities(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| !matches!(key.as_str(), "id" | "call_id"))
                .map(|(key, field)| (key.clone(), without_identities(field)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_identities).collect()),
        Value::String(text) => Value::String(scrub_text(text)),
        other => other.clone(),
    }
}

/// Replaces what a tool result body legitimately carries per process: the
/// call id a placeholder pass placeholder names, and the wall time a Code Cell
/// summary reports.
fn scrub_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let rest = &text[index..];
        if rest.len() >= 36
            && rest.is_char_boundary(36)
            && rest[..36].bytes().enumerate().all(|(at, byte)| match at {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            })
        {
            out.push_str("<id>");
            index += 36;
            continue;
        }
        if let Some(after) = rest.strip_prefix("Wall time ") {
            let digits = after
                .bytes()
                .take_while(|byte| byte.is_ascii_digit() || *byte == b'.')
                .count();
            out.push_str("Wall time <t>");
            index += "Wall time ".len() + digits;
            continue;
        }
        let ch = rest.chars().next().unwrap_or_default();
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn note_text(item: &Value) -> Option<&str> {
    item["blocks"][0]["text"]
        .as_str()
        .or_else(|| item["blocks"][0]["Text"]["text"].as_str())
}

/// The record a placeholder pass or a fold leaves in the history.
fn is_compaction(item: &Value) -> bool {
    item["blocks"][0].get("Compaction").is_some()
}

fn is_note(item: &Value) -> bool {
    note_text(item).is_some_and(|text| text.contains(PROVIDER_NOTE) || text.contains(TOOL_NOTE))
}

/// The recovered history must be the prefix a complete run produced, plus at
/// most the abandonment note as its last item.
fn assert_history_is_a_prefix_of(complete: &[Value], recovered: &[Value]) -> TestResult {
    let (body, tail) = match recovered.split_last() {
        Some((last, body)) if is_note(last) => (body, Some(last)),
        _ => (recovered, None),
    };
    let complete: Vec<Value> = complete
        .iter()
        .filter(|item| !is_compaction(item))
        .map(without_identities)
        .collect();
    let body: Vec<Value> = body
        .iter()
        .filter(|item| !is_compaction(item))
        .map(without_identities)
        .collect();
    if body.len() > complete.len() {
        return Err(format!(
            "recovered history ({}) is longer than a complete run ({})",
            body.len(),
            complete.len()
        )
        .into());
    }
    for (index, (got, want)) in body.iter().zip(&complete).enumerate() {
        if serde_json::to_vec(got)? != serde_json::to_vec(want)? {
            return Err(format!(
                "recovered item {index} differs from the complete run:\n got: {got}\nwant: {want}"
            )
            .into());
        }
    }
    if let Some(note) = tail {
        let text = note_text(note).unwrap_or_default();
        if note["actor"] != json!("User") || note["provenance"] != json!("Instruction") {
            return Err(format!(
                "the abandonment note is not spelled User/Instruction, the pair every wire can \
                 carry: {note}"
            )
            .into());
        }
        if !(text.contains(PROVIDER_NOTE) || text.contains(TOOL_NOTE)) {
            return Err(format!("unexpected note text: {text}").into());
        }
    }
    Ok(())
}

fn assert_report(cell: &Cell, report: &Report, complete: &[Value]) -> TestResult {
    let label = format!("{:?}/{:?}", cell.scenario, cell.cut);
    match (cell.expect, report) {
        (Expect::CompactionAbandoned(count), Report::Exact { items, .. }) => {
            if items.len() != count || items.iter().any(is_note) {
                return Err(format!(
                    "{label}: expected {count} unchanged source items, saw {items:?}"
                )
                .into());
            }
            Ok(())
        }
        (
            Expect::Exact(count),
            Report::Exact {
                source,
                locator,
                items,
            },
        ) => {
            if items.len() != count {
                return Err(format!(
                    "{label}: expected {count} exact items, saw {}: {items:?}",
                    items.len()
                )
                .into());
            }
            if items.iter().any(is_note) {
                return Err(format!("{label}: an exact head carries an abandonment note").into());
            }
            if cell.scenario == Scenario::Direct {
                if count == 0 && source != locator {
                    return Err(format!("{label}: the old exact locator changed").into());
                }
                if count != 0 && !same_branch_new_checkpoint(source, locator) {
                    return Err(format!(
                        "{label}: the new exact head changed branch or kept its checkpoint"
                    )
                    .into());
                }
            }
            assert_history_is_a_prefix_of(complete, items)
        }
        (
            Expect::AbandonedRequest(operations, count),
            Report::Abandoned {
                source,
                locator,
                items,
                operations: got,
                effects,
                note,
            },
        ) => {
            if got != operations || !effects.is_empty() {
                return Err(format!(
                    "{label}: expected abandoned request operations {operations:?}, saw {got:?} \
                     with effects {effects:?}"
                )
                .into());
            }
            if items.len() != count {
                return Err(format!(
                    "{label}: expected {count} items, saw {}: {items:?}",
                    items.len()
                )
                .into());
            }
            // Exactly one claim applies, and the useful assertions are the
            // negative ones: a lost request must not warn about the workspace
            // and must not claim that nothing started.
            if !note.contains(PROVIDER_NOTE)
                || !note.contains("may have been dispatched and billed")
                || !note.contains("No response from it is part of this history")
                || note.contains("changed the workspace")
                || note.contains("never started")
            {
                return Err(format!("{label}: wrong provider note: {note}").into());
            }
            if !same_branch_new_checkpoint(source, locator) {
                return Err(
                    format!("{label}: abandonment did not publish a new checkpoint").into(),
                );
            }
            assert_history_is_a_prefix_of(complete, items)
        }
        (
            Expect::AbandonedTool(operations, effects, count),
            Report::Abandoned {
                source,
                locator,
                items,
                operations: got,
                effects: got_effects,
                note,
            },
        ) => {
            if got != operations || got_effects != effects {
                return Err(format!(
                    "{label}: expected abandoned operations {operations:?} / effects {effects:?}, \
                     saw {got:?} / {got_effects:?}"
                )
                .into());
            }
            if items.len() != count {
                return Err(format!(
                    "{label}: expected {count} items, saw {}: {items:?}",
                    items.len()
                )
                .into());
            }
            // A running tool outranks a lost request: the note sends the
            // operator to the workspace and does not claim a charge or that
            // nothing started.
            if !note.contains(TOOL_NOTE)
                || !note.contains("may have run to completion and changed the workspace")
                || !note.contains("Check the workspace")
                || note.contains("dispatched and billed")
                || note.contains("never started")
            {
                return Err(format!("{label}: wrong tool note: {note}").into());
            }
            if !same_branch_new_checkpoint(source, locator) {
                return Err(
                    format!("{label}: abandonment did not publish a new checkpoint").into(),
                );
            }
            assert_history_is_a_prefix_of(complete, items)
        }
        (
            Expect::Restored(count),
            Report::RestoredWorking {
                items,
                pending_decision,
                ..
            },
        ) => {
            if !pending_decision {
                return Err(format!("{label}: the restored turn has no pending decision").into());
            }
            if items.len() != count {
                return Err(format!(
                    "{label}: expected {count} items, saw {}: {items:?}",
                    items.len()
                )
                .into());
            }
            if items.iter().any(is_note) {
                return Err(format!("{label}: a restored turn carries an abandonment note").into());
            }
            assert_history_is_a_prefix_of(complete, items)
        }
        _ => Err(format!("{label}: expected {:?}, saw {report:?}", cell.expect).into()),
    }
}

// ---------------------------------------------------------------------------
// Parent: lowering the recovered history on every wire
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Wire {
    Messages,
    Responses,
    ChatCompletions,
}

const WIRES: [Wire; 3] = [Wire::Messages, Wire::Responses, Wire::ChatCompletions];

fn configuration_id<T: serde::de::DeserializeOwned>(
    domain: DigestDomain,
    label: &[u8],
) -> TestResult<T> {
    Ok(serde_json::from_value(serde_json::to_value(
        Digest::sha256(domain, 5, &[label])?,
    )?)?)
}

/// Every tool name the history calls, declared so the lowering can pair calls.
fn declared_tools(items: &[ConversationItem]) -> TestResult<Vec<ConfiguredTool>> {
    let mut names = Vec::new();
    for item in items {
        for block in item.blocks() {
            if let ContentBlock::ToolCall(call) = block
                && !names.iter().any(|(name, _)| name == call.exposed_name())
            {
                // A Code Mode marker takes its program as a JSON string; every
                // other tool takes an object.
                let free_form = serde_json::from_str::<Value>(call.raw_arguments())
                    .is_ok_and(|arguments| arguments.is_string());
                names.push((call.exposed_name().clone(), free_form));
            }
        }
    }
    names
        .into_iter()
        .map(|(name, free_form)| {
            Ok(ConfiguredTool::new(
                ToolDefinition::new(
                    name,
                    "1.0.0",
                    "A tool the recovered history called.",
                    if free_form {
                        ToolInputContract::utf8(65_536)?
                    } else {
                        ToolInputContract::structured(json!({
                            "$schema": "https://json-schema.org/draft/2020-12/schema",
                            "type": "object",
                            "properties": {},
                            "required": [],
                            "additionalProperties": false
                        }))?
                    },
                    ToolOutputContract::portable_model_content(),
                    ToolConcurrency::Serial,
                    ToolEffect::Mutating,
                )?,
                ToolExecutionDisposition::Local,
                ToolCallerDisposition::DirectOnly,
            ))
        })
        .collect()
}

/// Lowers `items` as the next request on `wire` and returns the body.
fn lower_on(wire: Wire, items: &[ConversationItem]) -> TestResult<Vec<u8>> {
    let (protocol, family, route, model) = match wire {
        Wire::Messages => (
            "anthropic.messages",
            "claude",
            "anthropic.messages",
            "claude-exact",
        ),
        Wire::Responses => ("openai.responses", "gpt", "openai.responses", "gpt-exact"),
        Wire::ChatCompletions => (
            "openai.chat-completions",
            "chat",
            "openai.chat",
            "chat-exact",
        ),
    };
    let selected: SelectedProviderConfigurationId = configuration_id(
        DigestDomain::SelectedProviderConfiguration,
        protocol.as_bytes(),
    )?;
    let effective: EffectiveProviderConfigurationId = configuration_id(
        DigestDomain::EffectiveProviderConfiguration,
        protocol.as_bytes(),
    )?;
    let provenance = FactProvenance::new(FactSource::TestFixture, Timestamp::new(10, 0)?, None);
    let features = [Capability::Streaming, Capability::Tools]
        .into_iter()
        .map(|capability| {
            (
                capability,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            )
        })
        .collect();
    let protocol_id = ProtocolId::new(protocol)?;
    let family_id = ModelFamilyId::new(family)?;
    let model_id = ModelId::new(model)?;
    let descriptor = ResolvedModelDescriptor::new_custom(
        protocol_id.clone(),
        RouteId::new(route)?,
        model_id.clone(),
        None,
        Some(family_id.clone()),
        ModelAdapterProfileId::new(format!("{route}.adapter"))?,
        CapabilitySet::new(features, BTreeMap::new()),
    );
    let request = ModelRequest::builder(
        selected,
        effective,
        ConversationView::from_items(items.to_vec())?,
        RequestMetadata::for_turn(
            uuid::Uuid::from_u128(1).try_into()?,
            uuid::Uuid::from_u128(2).try_into()?,
            uuid::Uuid::from_u128(3).try_into()?,
            Timestamp::new(11, 0)?,
        ),
    )
    .tools(declared_tools(items)?)
    .build()?;
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"crash-matrix-routes"])?;
    match wire {
        Wire::Messages => {
            let policy = MessagesRequestPolicy::new(
                protocol_id,
                model_id.clone(),
                Some(model_id),
                plan.binding_digest(),
                descriptor.semantic_binding_digest()?,
                account,
                selected,
                effective,
                8_192,
                None,
                "2023-06-01",
                "",
            );
            Ok(
                lower_messages_request(&plan, &policy, ToolOwnership::default())?
                    .body()
                    .to_vec(),
            )
        }
        Wire::ChatCompletions => {
            let policy = ChatRequestPolicy::new(
                protocol_id,
                model_id,
                plan.binding_digest(),
                account,
                selected,
                effective,
                8_192,
                ChatOutputLimitField::MaxCompletionTokens,
                ChatReasoningReplay::VisibleOnly,
                None,
                ReasoningPolicy::Disabled,
                ReasoningAliasPolicy::EitherBindFirst,
            )?;
            Ok(lower_chat_request(&plan, &policy)?.body().to_vec())
        }
        Wire::Responses => {
            let config = ResponsesRequestConfig::new(
                &protocol_id,
                Some(&family_id),
                &model_id,
                ResponsesRequestPolicy::openai_platform(
                    ResponsesStoragePolicy::Stateless,
                    false,
                    false,
                ),
                None,
            );
            Ok(lower_responses_request(&request, config)?.body_bytes()?)
        }
    }
}

/// Gives every tool call and result the remote correlation a real route's
/// model output carries.
///
/// The loopback model mints calls with no provider-side id, and the Messages
/// lowering refuses a historical `tool_use` without one ("tool call requires
/// exact remote correlation"). A history produced on a live route always has
/// it, so the lowering is asked the question it would be asked in production:
/// does this shape, with its ids, go on the wire.
fn with_remote_correlation(item: &Value) -> Value {
    let mut item = item.clone();
    if let Some(blocks) = item["blocks"].as_array_mut() {
        for block in blocks.iter_mut().filter_map(Value::as_object_mut) {
            // Indexing a `Value` mutably inserts, so only a block that already
            // is a tool call or result is touched.
            for kind in ["ToolCall", "ToolResult"] {
                let Some(body) = block.get_mut(kind) else {
                    continue;
                };
                let call_id = body["call_id"].as_str().map(str::to_owned);
                if let (Some(call_id), Some(correlation)) =
                    (call_id, body["correlation"].as_object_mut())
                {
                    correlation.insert("remote_call_id".to_owned(), Value::from(call_id));
                }
            }
        }
    }
    item
}

/// The recovered history lowers on every wire, and the note reaches each body.
///
/// A Code Mode history carries the free-form `exec` marker, which neither the
/// Messages route ("does not admit free-form local tools") nor Chat V1
/// ("free-form tools are outside Chat V1") admits as a local tool; both
/// refusals are the routes' contract and are asserted as such, and the
/// history is required to lower on Responses, the one wire that carries
/// composition.
fn assert_lowers_on_every_wire(label: &str, items: &[Value]) -> TestResult {
    let items: Vec<ConversationItem> = items
        .iter()
        .map(|item| serde_json::from_value(with_remote_correlation(item)))
        .collect::<Result<_, _>>()?;
    let note = items.iter().find_map(|item| match item.blocks().first() {
        Some(ContentBlock::Text(text))
            if item.actor() == Actor::User && item.provenance() == Provenance::Instruction =>
        {
            Some(text.text().to_owned())
        }
        _ => None,
    });
    let free_form = items.iter().any(|item| {
        item.blocks().iter().any(|block| {
            matches!(block, ContentBlock::ToolCall(call)
                if serde_json::from_str::<Value>(call.raw_arguments())
                    .is_ok_and(|arguments| arguments.is_string()))
        })
    });
    for wire in WIRES {
        let refusal = match wire {
            Wire::Messages if free_form => Some("does not admit free-form local tools"),
            Wire::ChatCompletions if free_form => Some("free-form tools are outside Chat V1"),
            _ => None,
        };
        if let Some(refusal) = refusal {
            match lower_on(wire, &items) {
                Err(error) if error.to_string().contains(refusal) => continue,
                other => {
                    return Err(format!(
                        "{label}: {wire:?} must refuse a free-form marker in history with \
                         {refusal:?}, got {other:?}"
                    )
                    .into());
                }
            }
        }
        let body = lower_on(wire, &items).map_err(|error| {
            format!("{label}: recovered history does not lower on {wire:?}: {error}")
        })?;
        if let Some(note) = &note {
            let body_text: Value = serde_json::from_slice(&body)?;
            let encoded = serde_json::to_string(&body_text)?;
            let needle = serde_json::to_string(note)?;
            let needle = needle.trim_matches('"');
            if !encoded.contains(needle) {
                return Err(format!(
                    "{label}: the abandonment note did not reach the {wire:?} body:\n{encoded}"
                )
                .into());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The in-process matrix
// ---------------------------------------------------------------------------

fn run_cell(cell: &Cell, complete: &[Value]) -> TestResult {
    let fixture = Fixture::new(cell.scenario)?;
    let mut child = CrashChild::spawn(&fixture, Mode::Run, cell.cut)?;
    await_cut(&fixture, &mut child, cell.cut)?;
    let signal = child.sigkill()?;
    if signal != 9 {
        return Err(format!("{:?}/{:?} was not cut by SIGKILL", cell.scenario, cell.cut).into());
    }
    let before = fixture.evidence()?;
    assert_no_duplicate_dispatch(&before.0)?;
    assert_no_duplicate_dispatch(&before.1)?;
    let orphan = cell.scenario == Scenario::Process;
    if orphan && !owned_orphan_is_running(&fixture.command.marker) {
        return Err("the child did not outlive the killed agent; not the orphan case".into());
    }
    let started = Instant::now();
    let report = probe(&fixture)?;
    if orphan {
        if started.elapsed() > Duration::from_secs(15) {
            return Err("the reopen waited on the orphan".into());
        }
        if let Report::Abandoned { note, .. } = &report
            && (note.contains("pid") || note.contains(PROCESS_ORPHAN_COMMAND))
        {
            return Err(format!("the reopen named a process it cannot identify: {note}").into());
        }
        if !owned_orphan_is_running(&fixture.command.marker) {
            return Err("the reopen signalled a process it cannot identify".into());
        }
    }
    let after = fixture.evidence()?;
    if after != before {
        return Err(format!(
            "{:?}/{:?}: the probe appended physical dispatch evidence",
            cell.scenario, cell.cut
        )
        .into());
    }
    assert_report(cell, &report, complete)?;
    let items = match &report {
        Report::Exact { items, .. }
        | Report::Abandoned { items, .. }
        | Report::RestoredWorking { items, .. } => items.as_slice(),
        Report::RecoveryRequired { .. } => &[],
    };
    if !items.is_empty() {
        assert_lowers_on_every_wire(&format!("{:?}/{:?}", cell.scenario, cell.cut), items)?;
    }
    Ok(())
}

/// The exact pid written by this fixture's shell immediately before it execs
/// the long-lived sleep. `exec` preserves the pid even on Alpine, where it
/// replaces the shell argv and therefore defeats marker-based `pgrep`.
fn owned_orphan_pid(marker: &Path) -> Option<u32> {
    std::fs::read_to_string(marker)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

fn owned_orphan_is_running(marker: &Path) -> bool {
    let Some(pid) = owned_orphan_pid(marker) else {
        return false;
    };
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output();
    output.is_ok_and(|output| {
        output.status.success()
            && output
                .stdout
                .iter()
                .copied()
                .find(|byte| !byte.is_ascii_whitespace())
                .is_some_and(|state| state != b'Z')
    })
}

#[test]
fn owned_orphan_liveness_distinguishes_a_live_child_from_an_exited_zombie() -> TestResult {
    let directory = tempfile::tempdir()?;
    let live_marker = directory.path().join("live.pid");
    let mut live = Command::new("sleep").arg("120").spawn()?;
    std::fs::write(&live_marker, live.id().to_string())?;
    if !owned_orphan_is_running(&live_marker) {
        let _ = live.kill();
        let _ = live.wait();
        return Err("a running fixture-owned child was reported dead".into());
    }
    // This unreaped Child, unlike a pid read from a marker, retains ownership.
    live.kill()?;
    live.wait()?;

    let dead_marker = directory.path().join("dead.pid");
    let mut dead = Command::new("sh").args(["-c", "exit 0"]).spawn()?;
    std::fs::write(&dead_marker, dead.id().to_string())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while owned_orphan_is_running(&dead_marker) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let reported_running = owned_orphan_is_running(&dead_marker);
    dead.wait()?;
    if reported_running {
        return Err("an exited fixture-owned zombie was reported running".into());
    }
    Ok(())
}

fn complete_history(scenario: Scenario) -> TestResult<Vec<Value>> {
    // A decision-required policy never completes without a decision; the
    // shape it is compared against is the same turn under the allowing policy.
    let scenario = if scenario == Scenario::Decision {
        Scenario::Direct
    } else {
        scenario
    };
    let fixture = Fixture::new(scenario)?;
    let (_, items) = complete_run(&fixture)?;
    let (evidence, fold) = fixture.evidence()?;
    assert_no_duplicate_dispatch(&evidence)?;
    assert_no_duplicate_dispatch(&fold)?;
    Ok(items)
}

fn run_cells(cells: &[Cell]) -> TestResult {
    let mut complete: BTreeMap<Scenario, Vec<Value>> = BTreeMap::new();
    let mut failures = Vec::new();
    for cell in cells {
        if let std::collections::btree_map::Entry::Vacant(entry) = complete.entry(cell.scenario) {
            entry.insert(complete_history(cell.scenario)?);
        }
        if let Err(error) = run_cell(cell, &complete[&cell.scenario]) {
            failures.push(format!("{:?}/{:?}: {error}", cell.scenario, cell.cut));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n").into())
    }
}

/// The direct loopback binding: every controller boundary plus a complete run.
#[test]
fn direct_route_cuts_have_one_deterministic_reopen_contract_each() -> TestResult {
    run_cells(&[
        Cell {
            scenario: Scenario::Direct,
            cut: Cut::Boundary(CrashBoundary::GenesisReleased),
            expect: Expect::Exact(0),
            killed_by: "M-probe-prompt",
        },
        Cell {
            scenario: Scenario::Direct,
            cut: Cut::Boundary(CrashBoundary::ProviderDispatchOne),
            expect: Expect::AbandonedRequest(&[2], 2),
            killed_by: "M-note-actor, M-note-text",
        },
        Cell {
            scenario: Scenario::Direct,
            cut: Cut::Boundary(CrashBoundary::ProviderOutcomeOne),
            expect: Expect::AbandonedRequest(&[2], 2),
            killed_by: "M-note-actor, M-note-text",
        },
        Cell {
            scenario: Scenario::Direct,
            cut: Cut::Boundary(CrashBoundary::EffectDispatch),
            // Two, not three: the round that asked for the tool is still in
            // the working projection when the tool batch is cut, so the
            // abandonment keeps the prompt and adds the note, and the call the
            // note describes is not in the history (reported as a finding).
            expect: Expect::AbandonedTool(&[3], &[3], 2),
            killed_by: "M-no-effect-fold",
        },
        Cell {
            scenario: Scenario::Direct,
            cut: Cut::Boundary(CrashBoundary::EffectOutcome),
            expect: Expect::AbandonedTool(&[3], &[3], 2),
            killed_by: "M-no-effect-fold",
        },
        Cell {
            scenario: Scenario::Direct,
            cut: Cut::Boundary(CrashBoundary::ProviderDispatchTwo),
            expect: Expect::AbandonedRequest(&[4], 4),
            killed_by: "M-note-actor, M-note-text",
        },
        Cell {
            scenario: Scenario::Direct,
            cut: Cut::Boundary(CrashBoundary::ProviderOutcomeTwo),
            expect: Expect::AbandonedRequest(&[4], 4),
            killed_by: "M-note-actor, M-note-text",
        },
        Cell {
            scenario: Scenario::Direct,
            cut: Cut::Boundary(CrashBoundary::CheckpointReleased),
            expect: Expect::Exact(4),
            killed_by: "M-probe-prompt",
        },
    ])
}

/// A cut inside a real process reports abandonment without waiting on,
/// naming, or signalling the orphan. The durable identity is not a pid.
#[test]
fn a_cut_inside_a_running_process_session_is_abandoned_and_the_orphan_is_reported() -> TestResult {
    run_cells(&[Cell {
        scenario: Scenario::Process,
        cut: Cut::ProcessRunning,
        expect: Expect::AbandonedTool(&[3], &[3], 2),
        killed_by: "M-no-effect-fold",
    }])?;
    Ok(())
}

/// The placeholder rung has no physical boundary of its own. The cut after the
/// turn proves the replaced checkpoint is the durable head that survives a
/// crash, lowers on every wire, and carries the placeholder pass marker; the cut inside
/// the first read is the bracket before it.
#[test]
fn placeholders_survive_a_cut_after_publication_and_the_bracket_before_it_abandons() -> TestResult {
    run_cells(&[
        Cell {
            scenario: Scenario::Growth,
            cut: Cut::Boundary(CrashBoundary::EffectOutcome),
            expect: Expect::AbandonedTool(&[3], &[3], 2),
            killed_by: "M-no-effect-fold",
        },
        Cell {
            scenario: Scenario::Growth,
            cut: Cut::Boundary(CrashBoundary::CheckpointReleased),
            // Prompt, two tool exchanges, the final text, and the placeholder pass's
            // own compaction record.
            expect: Expect::Exact(8),
            killed_by: "M-probe-prompt",
        },
    ])?;
    let fixture = Fixture::new(Scenario::Growth)?;
    let (_, items) = complete_run(&fixture)?;
    let replaced = items.iter().any(|item| {
        serde_json::to_string(item).is_ok_and(|text| text.contains(PLACEHOLDER_MARKER))
    });
    if !replaced {
        return Err(format!("the growth turn did not replace anything: {items:?}").into());
    }
    Ok(())
}

#[test]
fn clipped_growth_at_twenty_thousand_tokens_is_byte_identical_after_crash() -> TestResult {
    let fixture = Fixture::new(Scenario::Growth)?;
    let (_, items) = complete_run(&fixture)?;
    let text = serde_json::to_string(&items)?;
    if !text.contains("bytes omitted from the middle of this output") {
        return Err("the 20k crash cell must actually clip an observation".into());
    }
    run_cells(&[Cell {
        scenario: Scenario::Growth,
        cut: Cut::Boundary(CrashBoundary::CheckpointReleased),
        expect: Expect::Exact(8),
        killed_by: "M-clipped-observation",
    }])
}

/// A cut inside a manual fold, at the summary request's dispatch and at its
/// terminal event. Compaction keeps its own recovery rule: never abandoned by
/// a plain reopen.
#[test]
fn a_cut_inside_a_compaction_fold_closes_at_open() -> TestResult {
    run_cells(&[
        Cell {
            scenario: Scenario::Fold,
            cut: Cut::Fold(CrashBoundary::ProviderDispatchTwo),
            // The compaction operation and the provider attempt it dispatched,
            // both awaiting evidence nobody can produce: the next open closes
            // the admitted workflow itself (abandoned, never redispatched) and
            // reopens the unchanged history.
            expect: Expect::CompactionAbandoned(6),
            killed_by: "M-probe-prompt",
        },
        Cell {
            scenario: Scenario::Fold,
            cut: Cut::Fold(CrashBoundary::ProviderOutcomeTwo),
            // The compaction operation and the provider attempt it dispatched,
            // both awaiting evidence nobody can produce: the next open closes
            // the admitted workflow itself (abandoned, never redispatched) and
            // reopens the unchanged history.
            expect: Expect::CompactionAbandoned(6),
            killed_by: "M-probe-prompt",
        },
    ])
}

/// A cut inside an effect-decision frame: the tool was never dispatched.
#[test]
fn a_cut_inside_an_effect_decision_frame_has_one_contract() -> TestResult {
    run_cells(&[Cell {
        scenario: Scenario::Decision,
        cut: Cut::DecisionPending,
        // The frame is restored, not abandoned and not recovery-only: the
        // tool was never dispatched, so nothing is uncertain, and the reopen
        // resumes the turn at the decision it was waiting for. The prompt
        // is the only canonical item; the round that asked is still working.
        expect: Expect::Restored(1),
        killed_by: "M-no-restore",
    }])
}

/// The Code Mode composition route: cuts around the cell. A cut inside a
/// running cell has no boundary hook; the expected contract there, from the
/// protocol vocabulary, is recovery-only with `EvaluationAbandonmentRequired`.
#[test]
fn code_mode_route_cuts_have_one_deterministic_reopen_contract_each() -> TestResult {
    run_cells(&[
        Cell {
            scenario: Scenario::CodeMode,
            cut: Cut::Boundary(CrashBoundary::ProviderDispatchOne),
            expect: Expect::AbandonedRequest(&[2], 2),
            killed_by: "M-note-actor, M-note-text",
        },
        Cell {
            scenario: Scenario::CodeMode,
            cut: Cut::Boundary(CrashBoundary::ProviderOutcomeOne),
            expect: Expect::AbandonedRequest(&[2], 2),
            killed_by: "M-note-actor, M-note-text",
        },
        Cell {
            scenario: Scenario::CodeMode,
            cut: Cut::Boundary(CrashBoundary::ProviderDispatchTwo),
            // Operation 8, not 4: the cell, its evaluation and its two
            // nested calls each take a ledger operation before the second
            // round is admitted.
            expect: Expect::AbandonedRequest(&[8], 4),
            killed_by: "M-note-actor, M-note-text",
        },
        Cell {
            scenario: Scenario::CodeMode,
            cut: Cut::Boundary(CrashBoundary::CheckpointReleased),
            expect: Expect::Exact(4),
            killed_by: "M-probe-prompt",
        },
    ])
}

// ---------------------------------------------------------------------------
// pi's aliases of one canonical tool
// ---------------------------------------------------------------------------

/// pi's `edit` and `write` called in one round. Both are `apply_patch` under
/// pi's presentation.
fn pi_alias_script() -> Value {
    json!({
        "version": 1,
        "steps": [
            {"output": {"tools": [
                {"name": "edit", "arguments": {
                    "path": "edited.txt",
                    "edits": [{"oldText": "before", "newText": "after"}]
                }},
                {"name": "write", "arguments": {"path": "written.txt", "content": "written\n"}}
            ]}, "finish": "tool_calls"},
            {"output": {"text": FINAL_TEXT}}
        ]
    })
}

/// Runs the pi alias script as one turn on a fresh durable session and
/// returns the committed history.
async fn run_pi_alias_turn(root: &Path, workspace: &Path) -> ChildResult<Vec<Value>> {
    let plan: ScriptPlan = serde_json::from_value(pi_alias_script())?;
    plan.validate()?;
    let script = Arc::new(plan);
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.join("evidence.jsonl"),
    )?;
    let backend = SessionBackend::sqlite(root.join("sessions.sqlite3"))
        .map_err(|error| format!("backend open failed with {:?}", error.kind()))?;
    let sessions = AgentSessions::new(backend)
        .map_err(|error| format!("service create failed with {:?}", error.kind()))?;
    let created = Box::pin(sessions.create(CreateSessionRequest::new(
        SessionOperationId::new("pi-alias:create")?,
        scripted_pi_runtime_binding(workspace, controller, &script)?,
    )))
    .await
    .map_err(|error| format!("session create failed with {:?}: {error}", error.kind()))?;
    let CreateSessionOutcome::Created { session, .. } = created else {
        return Err("session create returned an unknown outcome".into());
    };
    let turn = session
        .agent()
        .prompt("edit one file and write another")
        .await?;
    let result = turn.result().await?;
    if !matches!(result.outcome(), TurnResultOutcome::Completed(_))
        || result.response().map(ModelResponse::output_text).as_deref() != Some(FINAL_TEXT)
    {
        return Err(format!(
            "the pi alias turn did not complete with the exact final text: outcome={:?}, \
             text={:?}",
            result.outcome(),
            result.response().map(ModelResponse::output_text)
        )
        .into());
    }
    let items = items_of(&session.agent().conversation())?;
    session.release().await?;
    Ok(items)
}

/// pi's `edit` and `write` in one turn commit two calls of the one canonical
/// `apply_patch`, each under the name the model called, and every wire's next
/// request names them as the model did, never by the canonical name.
#[test]
fn pi_edit_and_write_in_one_turn_commit_canonical_apply_patch_and_lower_by_exposed_name()
-> TestResult {
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace)?;
    std::fs::write(workspace.join("edited.txt"), "before\n")?;
    let items = tokio::runtime::Runtime::new()?
        .block_on(run_pi_alias_turn(root.path(), &workspace))
        .map_err(|error| error.to_string())?;

    assert_eq!(
        std::fs::read_to_string(workspace.join("edited.txt"))?,
        "after\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("written.txt"))?,
        "written\n"
    );

    let items: Vec<ConversationItem> = items
        .iter()
        .map(|item| serde_json::from_value(with_remote_correlation(item)))
        .collect::<Result<_, _>>()?;
    let calls = items
        .iter()
        .flat_map(ConversationItem::blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) => {
                Some((call.canonical_name().as_str(), call.exposed_name().as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls, [("apply_patch", "edit"), ("apply_patch", "write")]);
    let rounds = items
        .iter()
        .filter(|item| {
            item.blocks()
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall(_)))
        })
        .count();
    assert_eq!(rounds, 1, "both calls belong to one assistant round");

    for wire in WIRES {
        let body: Value = serde_json::from_slice(&lower_on(wire, &items)?)?;
        let encoded = serde_json::to_string(&body)?;
        for name in ["edit", "write"] {
            let needle = format!("\"name\":\"{name}\"");
            assert!(
                encoded.contains(&needle),
                "{wire:?} body does not name {name}:\n{encoded}"
            );
        }
        assert!(
            !encoded.contains("apply_patch"),
            "{wire:?} body names the canonical apply_patch:\n{encoded}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The real binary on its JSONL face
// ---------------------------------------------------------------------------
//
// These cells drive `plak agent` through the scripted offline model (the
// `test-hooks` feature). They are ignored by default because this package's
// gate does not build the binary; run them after
// `cargo build -p plak --features test-hooks` (or point `PLAK_TEST_BINARY` at
// one) with `cargo test -p plakous-offline-crash-harness --test
// crash_matrix_routes -- --ignored jsonl`. Cut points on this face are reached
// through fsynced evidence rows, stream records and script stalls; the fold
// and the effect-outcome interval have no deterministic hook on this face and
// are covered in-process only.

/// The note the face prints once when a reopen recovered a turn.
const FACE_RECOVERY_NOTE: &str = "recovered an interrupted turn";
const FACE_WAIT: Duration = Duration::from_secs(45);

struct FaceFixture {
    root: TempDir,
    home: PathBuf,
    workspace: PathBuf,
    script: PathBuf,
    evidence: PathBuf,
    binary: PathBuf,
}

/// The `plak` binary carrying the test hooks, when one is available.
fn plak_binary() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PLAK_TEST_BINARY") {
        return Some(PathBuf::from(explicit));
    }
    let deps = std::env::current_exe().ok()?;
    let candidate = deps.parent()?.parent()?.join("plak");
    candidate.is_file().then_some(candidate)
}

impl FaceFixture {
    fn new(script: &Value) -> TestResult<Self> {
        use std::os::unix::fs::PermissionsExt;
        let binary = plak_binary().ok_or(
            "no plak binary with test hooks; build one with `cargo build -p plak --features \
             test-hooks` or set PLAK_TEST_BINARY",
        )?;
        let root = tempfile::tempdir()?;
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(home.join("agents"))?;
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(root.path().join("tmp"))?;
        std::fs::write(
            home.join("config.toml"),
            "[root]\nagent = \"lead\"\n\n[orchestration]\nsubagents = false\n\n[compaction]\nenabled = true\n",
        )?;
        std::fs::write(
            home.join("agents").join("lead.toml"),
            "account = \"codex\"\nmodel = \"offline-loopback-v1\"\neffort = \"medium\"\ncomposition = \"direct\"\ndescription = \"offline loopback lead\"\n",
        )?;
        for path in [
            home.join("config.toml"),
            home.join("agents").join("lead.toml"),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(home.join("agents"), std::fs::Permissions::from_mode(0o700))?;
        let fixture = Self {
            script: root.path().join("script.json"),
            evidence: root.path().join("evidence.jsonl"),
            home,
            workspace,
            binary,
            root,
        };
        fixture.write_script(script)?;
        Ok(fixture)
    }

    fn write_script(&self, script: &Value) -> TestResult {
        std::fs::write(&self.script, serde_json::to_vec_pretty(script)?)?;
        Ok(())
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .current_dir(&self.workspace)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.path().join("fake-home"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("PLAK_HOME", &self.home)
            .env("PLAK_TEST_MODEL", "offline-loopback-v1")
            .env("PLAK_TEST_SCRIPT", &self.script)
            .env("PLAK_TEST_EVIDENCE", &self.evidence);
        command
    }

    /// Blocks until the offline model has entered a provider operation.
    fn await_provider_dispatch(&self) -> TestResult {
        let deadline = Instant::now() + FACE_WAIT;
        loop {
            if read_evidence(&self.evidence)?
                .iter()
                .any(|record| record.kind == EvidenceKind::ProviderDispatched)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("the offline model never entered its provider operation".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Every record kind the session log holds.
    fn session_log_kinds(&self) -> TestResult<Vec<String>> {
        let version = self.command().args(["version", "--state-key"]).output()?;
        if !version.status.success() {
            return Err("the fixture binary did not report its state key".into());
        }
        let key = String::from_utf8(version.stdout)?;
        let directory = self.home.join("state").join(key.trim()).join("sessions");
        let mut kinds = Vec::new();
        let entries = std::fs::read_dir(directory)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in body.lines() {
                if let Ok(record) = serde_json::from_str::<Value>(line)
                    && let Some(kind) = record["kind"].as_str()
                {
                    kinds.push(kind.to_owned());
                }
            }
        }
        Ok(kinds)
    }
}

struct FaceAgent {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: Arc<std::sync::Mutex<Vec<String>>>,
    stderr: Arc<std::sync::Mutex<String>>,
}

impl FaceAgent {
    fn start(fixture: &FaceFixture, arguments: &[&str]) -> TestResult<Self> {
        let mut child = fixture
            .command()
            .arg("agent")
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take();
        let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stderr = Arc::new(std::sync::Mutex::new(String::new()));
        let mut stderr_pipe = child.stderr.take().ok_or("stderr pipe")?;
        let stderr_sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut stderr_pipe, &mut text);
            if let Ok(mut sink) = stderr_sink.lock() {
                *sink = text;
            }
        });
        let stdout = child.stdout.take().ok_or("stdout pipe")?;
        let sink = Arc::clone(&lines);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(mut sink) = sink.lock() {
                    sink.push(line);
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
            stderr,
        })
    }

    fn send(&mut self, frame: &Value) -> TestResult {
        let stdin = self.stdin.as_mut().ok_or("stdin closed")?;
        writeln!(stdin, "{frame}")?;
        stdin.flush()?;
        Ok(())
    }

    fn records(&self) -> Vec<Value> {
        self.lines
            .lock()
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn stderr(&self) -> String {
        self.stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    /// Waits until a record satisfies `predicate`, or fails.
    fn wait_for(&self, what: &str, predicate: impl Fn(&Value) -> bool) -> TestResult<Value> {
        let deadline = Instant::now() + FACE_WAIT;
        loop {
            if let Some(record) = self.records().into_iter().find(&predicate) {
                return Ok(record);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{what} never arrived; stream was:\n{}\nstderr:\n{}",
                    self.records()
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join("\n"),
                    self.stderr()
                )
                .into());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn sigkill(&mut self) -> TestResult {
        self.child.kill()?;
        let status = self.child.wait()?;
        if status.signal() != Some(9) {
            return Err(format!("plak agent was not cut by SIGKILL: {status:?}").into());
        }
        Ok(())
    }

    fn finish(mut self) -> TestResult<(bool, Vec<Value>, String)> {
        self.stdin = None;
        let deadline = Instant::now() + FACE_WAIT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok((status.success(), self.records(), self.stderr()));
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                return Err("plak agent never exited".into());
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

fn prompt_frame(text: &str) -> Value {
    json!({"v": 1, "kind": "prompt", "text": text})
}

fn quit_frame() -> Value {
    json!({"v": 1, "kind": "quit"})
}

fn face_stalled_answer() -> Value {
    json!({
        "version": 1,
        "repeat_last_step": true,
        "steps": [{"output": {"text": "the stalled answer"}, "gate": {"file": "release-face-answer"}}]
    })
}

fn face_answer(process_tools: bool) -> Value {
    json!({
        "version": 1,
        "process_tools": process_tools,
        "repeat_last_step": true,
        "steps": [{"output": {"text": "recovered and answered"}}]
    })
}

/// Reopens `session` until it is ready; a dead holder's branch is reclaimed
/// at once on this host, so this is tolerance rather than a wait.
fn face_reopen(fixture: &FaceFixture, session: &str) -> TestResult<FaceAgent> {
    let deadline = Instant::now() + FACE_WAIT;
    loop {
        let agent = FaceAgent::start(fixture, &["--session", session])?;
        let limit = Instant::now() + Duration::from_secs(10);
        let ready = loop {
            let records = agent.records();
            if records.iter().any(|record| record["kind"] == "ready") {
                break true;
            }
            if records.iter().any(|record| record["kind"] == "bye") || Instant::now() > limit {
                break false;
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        if ready {
            return Ok(agent);
        }
        let (_, records, stderr) = agent.finish()?;
        if Instant::now() >= deadline {
            let why = records
                .iter()
                .filter(|record| record["kind"] == "error")
                .filter_map(|record| record["message"].as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!(
                "the session never reopened; last refusal: {why}; stderr:\n{stderr}"
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The recovery note the reopened face said, if any, before `ready`.
fn face_recovery_note(agent: &FaceAgent) -> Option<String> {
    agent.records().iter().find_map(|record| {
        (record["kind"] == "status")
            .then(|| record["detail"].as_str())
            .flatten()
            .filter(|detail| detail.contains(FACE_RECOVERY_NOTE))
            .map(str::to_owned)
    })
}

/// What the face said and did after the reopen.
struct FaceOutcome {
    note: Option<String>,
    answered: bool,
    abandoned_logged: bool,
}

fn face_reopen_and_continue(fixture: &FaceFixture, session: &str) -> TestResult<FaceOutcome> {
    let mut agent = face_reopen(fixture, session)?;
    // The note, when there is one, precedes `ready`.
    let note = face_recovery_note(&agent);
    agent.send(&prompt_frame("carry on after the cut"))?;
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    })?;
    agent.send(&quit_frame())?;
    let (success, records, stderr) = agent.finish()?;
    if !success {
        return Err(format!("the reopened agent failed; stderr:\n{stderr}").into());
    }
    let answered = records
        .iter()
        .any(|record| record["kind"] == "assistant" && record["text"] == "recovered and answered");
    let notes = records
        .iter()
        .filter_map(|record| record["detail"].as_str())
        .filter(|detail| detail.contains(FACE_RECOVERY_NOTE))
        .count();
    if notes > 1 {
        return Err(format!("the recovery note was said {notes} times").into());
    }
    Ok(FaceOutcome {
        note,
        answered,
        abandoned_logged: fixture
            .session_log_kinds()?
            .iter()
            .any(|kind| kind == "turn.abandoned"),
    })
}

/// Before the turn: a kill while idle leaves an exact head and nothing to say.
#[test]
#[ignore = "needs the plak binary with test hooks; see the module note"]
fn jsonl_face_cut_before_the_turn_reopens_exactly() -> TestResult {
    let fixture = FaceFixture::new(&face_answer(false))?;
    let mut agent = FaceAgent::start(&fixture, &[])?;
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello")?;
    let session = hello["session_id"].as_str().ok_or("session id")?.to_owned();
    agent.wait_for("ready", |record| record["kind"] == "ready")?;
    agent.sigkill()?;
    let outcome = face_reopen_and_continue(&fixture, &session)?;
    if outcome.note.is_some() || outcome.abandoned_logged || !outcome.answered {
        return Err(format!(
            "expected a silent exact reopen; note={:?} logged={} answered={}",
            outcome.note, outcome.abandoned_logged, outcome.answered
        )
        .into());
    }
    Ok(())
}

/// Inside the first model request: abandoned, with the billing warning.
#[test]
#[ignore = "needs the plak binary with test hooks; see the module note"]
fn jsonl_face_cut_inside_the_model_request_is_abandoned_with_the_billing_note() -> TestResult {
    let fixture = FaceFixture::new(&face_stalled_answer())?;
    let mut agent = FaceAgent::start(&fixture, &[])?;
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello")?;
    let session = hello["session_id"].as_str().ok_or("session id")?.to_owned();
    agent.send(&prompt_frame("stall please"))?;
    fixture.await_provider_dispatch()?;
    agent.sigkill()?;
    fixture.write_script(&face_answer(false))?;
    let outcome = face_reopen_and_continue(&fixture, &session)?;
    let note = outcome
        .note
        .ok_or("the reopen said nothing about the cut turn")?;
    if !note.contains("model request may have been processed and billed")
        || !note.contains("nothing from it was kept")
        || note.contains("check the workspace")
    {
        return Err(format!("wrong note: {note}").into());
    }
    if !outcome.abandoned_logged || !outcome.answered {
        return Err("the abandonment was not logged, or the session did not answer".into());
    }
    Ok(())
}

/// Inside a running child process: abandoned, sent to look at the workspace.
#[test]
#[ignore = "needs the plak binary with test hooks; see the module note"]
fn jsonl_face_cut_inside_a_running_tool_is_abandoned_with_the_workspace_note() -> TestResult {
    let fixture = FaceFixture::new(&json!({
        "version": 1,
        "process_tools": true,
        "steps": [
            {"output": {"tool": {"name": "process_session_start", "arguments": {
                "command": FACE_ORPHAN_COMMAND,
                "workdir": ".",
                "login": false,
                "tty": false,
                "keep_stdin_open": true,
                "yield_time_ms": 30000,
                "max_output_bytes": 8192
            }}}, "finish": "tool_calls"},
            {"output": {"text": "never reached"}}
        ]
    }))?;
    let mut agent = FaceAgent::start(&fixture, &[])?;
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello")?;
    let session = hello["session_id"].as_str().ok_or("session id")?.to_owned();
    agent.send(&prompt_frame("start a long command"))?;
    agent.wait_for("the tool to start running", |record| {
        record["kind"] == "tool" && record["name"] == "process_session_start"
    })?;
    let marker = fixture.workspace.join("face-process.pid");
    let deadline = Instant::now() + FACE_WAIT;
    while !marker.exists() {
        if Instant::now() >= deadline {
            return Err("the fixture-owned process did not publish its pid".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    agent.sigkill()?;
    let orphaned = owned_orphan_is_running(&marker);
    fixture.write_script(&face_answer(true))?;
    let outcome = face_reopen_and_continue(&fixture, &session)?;
    if !orphaned {
        return Err("the child did not outlive the killed agent; not the orphan case".into());
    }
    let note = outcome
        .note
        .ok_or("the reopen said nothing about the cut turn")?;
    if !note.contains("a tool may have run")
        || !note.contains("workspace")
        || !note.contains("not retried")
        || note.contains("billed")
    {
        return Err(format!("wrong note: {note}").into());
    }
    if !outcome.abandoned_logged || !outcome.answered {
        return Err("the abandonment was not logged, or the session did not answer".into());
    }
    Ok(())
}

/// After the terminal result was published: exact, nothing to say.
#[test]
#[ignore = "needs the plak binary with test hooks; see the module note"]
fn jsonl_face_cut_after_the_answer_reopens_exactly() -> TestResult {
    let fixture = FaceFixture::new(&face_answer(false))?;
    let mut agent = FaceAgent::start(&fixture, &[])?;
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello")?;
    let session = hello["session_id"].as_str().ok_or("session id")?.to_owned();
    agent.send(&prompt_frame("answer now"))?;
    agent.wait_for("assistant_done", |record| {
        record["kind"] == "assistant_done"
    })?;
    // The terminal record is published before `assistant_done` reaches the
    // stream, and the face is idle again once its status says so.
    agent.wait_for("idle again", |record| {
        record["kind"] == "status" && record["phase"] == "idle"
    })?;
    agent.sigkill()?;
    let outcome = face_reopen_and_continue(&fixture, &session)?;
    if outcome.note.is_some() || outcome.abandoned_logged || !outcome.answered {
        return Err(format!(
            "expected a silent exact reopen; note={:?} logged={} answered={}",
            outcome.note, outcome.abandoned_logged, outcome.answered
        )
        .into());
    }
    Ok(())
}

/// Inside an effect-decision frame: the face asked for an approval and the
/// process died waiting.
///
/// PENDING (product defect, reported): the library restores this head
/// (`OpenSessionOutcome::RestoredWorking` with the decision pending, see the
/// in-process cell), but the product's reopen refuses it: the session record's
/// locator lags the `DecisionRequired` checkpoint, and adoption of a lagging
/// locator is gated on the head being abandonable, which a restorable head is
/// not. Until that lands this cell pins the refusal text so the day it changes
/// is visible. Expected once fixed: the reopen says nothing about recovery,
/// asks for the same approval again, and writes no `turn.abandoned` record.
#[test]
#[ignore = "needs the plak binary with test hooks; see the module note"]
fn jsonl_face_cut_inside_an_approval_is_asked_again_on_reopen() -> TestResult {
    let fixture = FaceFixture::new(&decision_script())?;
    let mut agent = FaceAgent::start(&fixture, &[])?;
    let hello = agent.wait_for("hello", |record| record["kind"] == "hello")?;
    let session = hello["session_id"].as_str().ok_or("session id")?.to_owned();
    agent.send(&prompt_frame("perform the loopback effect"))?;
    agent.wait_for("the approval request", |record| {
        record["kind"] == "approval"
    })?;
    agent.sigkill()?;
    let agent = FaceAgent::start(&fixture, &["--session", &session])?;
    let answer = agent.wait_for("the reopen's answer", |record| {
        record["kind"] == "ready" || record["kind"] == "error"
    })?;
    let (_, _, stderr) = agent.finish()?;
    if answer["kind"] == "ready" {
        return Err(
            "the product now reopens a decision frame; flip this cell to the \
                    expected contract in its doc comment"
                .into(),
        );
    }
    let message = answer["message"].as_str().unwrap_or_default();
    if !message.contains("session locator is not the current exact branch head")
        || !message.contains("a retry will not change that")
    {
        return Err(format!("unexpected refusal: {message}; stderr:\n{stderr}").into());
    }
    if fixture
        .session_log_kinds()?
        .iter()
        .any(|kind| kind == "turn.abandoned")
    {
        return Err("a decision frame was logged as an abandoned turn".into());
    }
    Ok(())
}
