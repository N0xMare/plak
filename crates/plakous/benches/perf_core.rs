//! Offline `perf-core` workload for the public Plakous facade.
//!
//! The workload emits JSONL and deliberately keeps all timing/probe code in a
//! bench target. It sweeps retained canonical history to expose proportional-
//! growth regressions and separately proves prompt ingress reaches a bounded
//! backpressure result. It performs no provider, filesystem-effect, or store I/O.

#![allow(
    clippy::expect_used,
    clippy::print_stdout,
    reason = "benchmark fixture fails fast and emits machine-readable JSONL"
)]

use std::collections::{BTreeMap, VecDeque};
use std::hint::black_box;
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_core::Stream;
use plakous::agent::{AgentBuilder, AgentConfig, AgentErrorKind, TurnResultOutcome, ViewRequest};
use plakous::model::{
    Actor, BlockCompleted, BlockStart, BlockStarted, CachePolicy, CanonicalHistory, Capability,
    CapabilityConstraints, CapabilitySet, CapacityCertificate, CapacityRule,
    CompiledProviderConfiguration, ConfiguredLanguageModel, ContentBlock,
    ContextAdmissionRequirement, ContextContract, ContextEvidenceSource, CountCertificateKind,
    CountObservation, CredentialSelectionId, Digest, DigestDomain, EffectiveProviderConfiguration,
    EntryId, EvidenceFreshness, FactProvenance, FactSource, FamilyPolicyEnvelope, FeatureSupport,
    FinishDetails, FinishReason, LanguageModel, LocalDurabilityDisposition, MeasurementCertificate,
    ModelAdapterProfileId, ModelError, ModelEvent, ModelFamilyId, ModelId,
    ModelImplementationIdentity, ModelPreparationContext, ModelPreparationError,
    ModelPreparationErrorKind, ModelResponse, ModelStream, OutputBoundDisposition, OutputLocation,
    OutputPolicy, ParallelToolDisposition, PreparedCacheUse, PreparedCallRecord, PreparedModelCall,
    PreparedModelProjection, PreparedOutputLimit, PreparedTokenProjection, ProtocolId, Provenance,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ProviderStorageDisposition,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningSummaryDisposition,
    ReplayDisposition, ResolvedModelDescriptor, ResolvedProviderAuthorityBinding, ResponseStarted,
    ResponseStateDisposition, RetentionDisposition, RouteId, RuntimeModelAuthority,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, SafeLimit,
    SelectedProviderConfiguration, StableRequestPrefix, Support, TextBlock, TextDelta, TextPhase,
    Timestamp, ToolChoice, ValidatedRequestPlan,
};
use plakous::tools::{ToolRegistryBuilder, WorkspacePolicy};
use plakous_model::StandardIntegrationAuthority;
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::sync::Notify;
use uuid::Uuid;

const SCHEMA: &str = "plakous.perf-core.v4";
const FIXTURE: &str = "public-facade-dynamic-text-v3";
const EVENT_FIXTURE: &str = "public-content-event-history-instrumented-v1";
const FIXTURE_REVISION: u32 = 3;
const RESPONSE: &str = "ok";
const HISTORY_SWEEP: &[usize] = &[0, 8, 32, 64, 128];
const SAMPLES: usize = 3;
const PROMPT_CAPACITY: usize = 8;
const PROTOCOL_FAMILY: &str = "perf.core-synthetic-family";
const LOWERING_REVISION: &str = "perf-core-synthetic-lowering-v2";
const PROJECTION_REVISION: &str = "perf-core-synthetic-projection-v2";
const ACCOUNTING_REVISION: &str = "perf-core-synthetic-byte-accounting-v1";
const RESERVATION_REVISION: &str = "perf-core-synthetic-reservation-v1";
const MEASUREMENT_CERTIFICATE_ID: &str = "perf-core-synthetic-byte-counter";
const MEASUREMENT_CERTIFICATE_REVISION: &str = "perf-core-synthetic-byte-counter-v1";

#[tokio::main]
async fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_some_and(|value| value == "--isolated-probe")
    {
        run_isolated_probe(&arguments[1..]).await;
        return;
    }
    emit_metadata();
    for &history_turns in HISTORY_SWEEP {
        for sample in 0..SAMPLES {
            measure_turn(history_turns, sample).await;
        }
    }
    measure_prompt_bound().await;
    for &history_turns in HISTORY_SWEEP {
        measure_event_history(history_turns).await;
    }
}

async fn run_isolated_probe(arguments: &[String]) {
    let [operation, history_turns, iterations, trailing @ ..] = arguments else {
        panic!("usage: perf_core --isolated-probe event-round <history-turns> <iterations>");
    };
    assert!(
        trailing.iter().all(|argument| argument == "--bench"),
        "unknown trailing isolated perf-core probe argument"
    );
    assert_eq!(operation, "event-round", "unknown isolated perf-core probe");
    let history_turns = history_turns.parse::<usize>().expect("history turns");
    let iterations = iterations.parse::<usize>().expect("iterations");
    let directory = tempdir().expect("isolated event workspace");
    let workspace =
        Arc::new(WorkspacePolicy::host(directory.path(), ".").expect("event workspace policy"));
    let agent = AgentBuilder::new(
        configured_model(Arc::new(DynamicTextModel::new()), &descriptor()),
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )
    .config(AgentConfig::default().with_event_capacity(65_536))
    .build()
    .expect("event agent");
    for index in 0..history_turns {
        run_turn(&agent, format!("isolated-history-{index:04}")).await;
    }
    let attachment = agent
        .attach(ViewRequest::content())
        .await
        .expect("event content projection");
    let (snapshot, mut events) = attachment.into_parts();
    let mut cursor = cursor_sequence(snapshot.cursor());
    let mut projected_events = 0_u64;
    let mut projected_event_serialized_bytes = 0_usize;
    for iteration in 0..iterations {
        run_turn(&agent, format!("isolated-measured-{iteration:04}")).await;
        let after = agent
            .attach(ViewRequest::content())
            .await
            .expect("post-event projection");
        let next = cursor_sequence(after.snapshot().cursor());
        let event_count = next.saturating_sub(cursor);
        for _ in 0..event_count {
            let event = events.recv().await.expect("isolated event retained");
            projected_event_serialized_bytes = projected_event_serialized_bytes.saturating_add(
                serde_json::to_vec(event.as_ref())
                    .expect("serialize isolated projected event")
                    .len(),
            );
        }
        projected_events = projected_events.saturating_add(event_count);
        cursor = next;
    }
    println!(
        "{}",
        json!({
            "schema": "plakous.perf-isolated-workload.v1",
            "bench": "perf_core",
            "operation": operation,
            "history_turns": history_turns,
            "iterations": iterations,
            "completed_iterations": iterations,
            "projected_events": projected_events,
            "projected_event_serialized_bytes": projected_event_serialized_bytes,
        })
    );
    agent.shutdown().await.expect("isolated event shutdown");
}

fn cursor_sequence(cursor: plakous::agent::EventCursor) -> u64 {
    serde_json::to_value(cursor.sequence())
        .expect("serialize event sequence")
        .as_u64()
        .expect("numeric event sequence")
}

fn emit_metadata() {
    let features = enabled_features();
    let git_status = git_output(&["status", "--porcelain=v1"]);
    let host_id = perf_host_id();
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "metadata",
            "fixture": FIXTURE,
            "fixture_revision": FIXTURE_REVISION,
            "workload": "unperturbed public facade prompt-to-terminal result, exact prompt-queue saturation, then a separate public event-history instrumentation fixture",
            "host": {
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "family": std::env::consts::FAMILY,
                "kernel": command_output("uname", &["-srvmo"]),
                "cpu": cpu_metadata(),
                "logical_parallelism": std::thread::available_parallelism().ok().map(usize::from),
                "host_id": host_id,
            },
            "toolchain": {
                "rustc_vv": command_output("rustc", &["-Vv"]),
                "cargo_v": command_output("cargo", &["-V"]),
                "target": target_metadata(),
            },
            "cargo": {
                "package": "plakous",
                "bench": "perf_core",
                "command": "cargo bench --locked -p plakous --bench perf_core",
                "enabled_feature_closure": features,
                "default_feature_closure": "empty",
                "requested_feature_flags": std::env::var("PLAKOUS_PERF_FEATURE_FLAGS").ok(),
            },
            "codegen": codegen_metadata(),
            "source": {
                "commit": git_output(&["rev-parse", "HEAD"]),
                "worktree_dirty": git_status.as_ref().is_some_and(|status| !status.is_empty()),
                "worktree_status_digest": git_status.as_deref().and_then(command_bytes_digest),
                "tracked_worktree_patch_digest": git_stdout_digest(&["diff", "--binary", "HEAD"]),
                "non_ignored_worktree_content_closure": worktree_content_closure_metadata(),
                "benchmark_source_git_blob": git_output(&["hash-object", "crates/plakous/benches/perf_core.rs"]),
                "workspace_manifest_git_blob": git_output(&["hash-object", "Cargo.toml"]),
                "package_manifest_git_blob": git_output(&["hash-object", "crates/plakous/Cargo.toml"]),
                "cargo_lock_git_blob": git_output(&["hash-object", "Cargo.lock"]),
                "baseline_identity": Value::Null,
                "baseline_status": "no ratified numeric baseline; emit raw samples",
            },
            "history_sweep_turns": HISTORY_SWEEP,
            "event_history_instrumented_fixture": EVENT_FIXTURE,
            "event_history_samples_per_point": 1,
            "samples_per_point": SAMPLES,
            "prompt_capacity": PROMPT_CAPACITY,
            "cold_warm_rule": {
                "classification": "not claimed",
                "sample_fixture_state": "each sample creates a fresh agent and temporary workspace and rebuilds its history prefill",
                "process_state": "all samples run serially in one process, so code, allocator, and OS cache warmth are uncontrolled",
            },
            "turn_timing_scope": "primary turn rows time agent.prompt call through authoritative TurnResult readiness only; agent/workspace construction, history prefill, event subscribers/projections, conversation serialization, and shutdown are excluded",
            "allocation_probe": {
                "available": false,
                "reason": "ordinary timing rows have no allocator instrumentation; tests/performance/baselines/perf_allocations.py runs dedicated event entry points under process-isolated glibc memusage",
            },
            "synthetic_model_preparation": {
                "token_unit": "one byte of the complete identity-only synthetic model wire",
                "output_reservation": "fixture-only certified hard cap",
                "claim": "offline model-fixture admission only; not provider/tokenizer/capacity evidence",
            },
        })
    );
}

fn perf_host_id() -> String {
    let value = std::env::var("PLAKOUS_PERF_HOST_ID")
        .expect("PLAKOUS_PERF_HOST_ID must identify the benchmark host");
    assert!(
        is_stable_host_id(&value),
        "PLAKOUS_PERF_HOST_ID must be a stable, non-placeholder host identifier"
    );
    value
}

fn is_stable_host_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (3..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "localhost" | "unknown" | "host" | "stable-host-id" | "<stable-host-id>"
        )
}

async fn measure_turn(history_turns: usize, sample: usize) {
    let directory = tempdir().expect("perf workspace");
    let workspace =
        Arc::new(WorkspacePolicy::host(directory.path(), ".").expect("perf workspace policy"));
    let model = Arc::new(DynamicTextModel::new());
    let agent = AgentBuilder::new(
        configured_model(model.clone(), &descriptor()),
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )
    .build()
    .expect("perf agent");

    for index in 0..history_turns {
        run_turn(&agent, format!("history-{index:04}")).await;
    }
    let before = agent.conversation();
    model.expect_measured_history(before.history());
    let before_bytes = serde_json::to_vec(&before)
        .expect("serialize pre-turn conversation")
        .len();
    let model_calls_before = model.calls();
    let started = Instant::now();
    let result = agent
        .prompt("measured")
        .await
        .expect("measured prompt")
        .result()
        .await
        .expect("measured result");
    let prompt_to_terminal_result_elapsed_ns = started.elapsed().as_nanos();
    assert!(matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    assert_eq!(
        result.response().expect("completed response").output_text(),
        RESPONSE
    );
    let after = result.conversation();
    let after_bytes = serde_json::to_vec(result.conversation())
        .expect("serialize post-turn conversation")
        .len();
    let prior_segments_shared = before
        .history()
        .segments()
        .iter()
        .zip(after.history().segments())
        .all(|(prior, next)| prior.shares_storage_with(next));
    assert!(after.history().segments().len() >= before.history().segments().len());
    assert!(prior_segments_shared);
    let prior_segment_pairs = before.history().segments().len();
    let sharing = model.sharing_evidence();
    assert_eq!(sharing.pre_turn_history_is_exact_ancestor, Some(true));
    assert_eq!(sharing.measured_history_segment_pairs, prior_segment_pairs);
    assert_eq!(
        sharing.all_measured_history_segment_payloads_shared,
        (prior_segment_pairs > 0).then_some(true)
    );
    assert!(sharing.all_prefixes_shared);
    let measured_task_model_calls = model.calls().saturating_sub(model_calls_before);
    assert_eq!(measured_task_model_calls, 1);
    assert_eq!(result.model_rounds(), 1);
    assert_eq!(result.tool_calls(), 0);

    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "turn",
            "fixture": FIXTURE,
            "thermal_classification": "uncontrolled-not-claimed",
            "process_sample_order": if sample == 0 { "first-for-history-point" } else { "repeat-for-history-point" },
            "sample": sample,
            "history_prefill_turns": history_turns,
            "history_prefill_excluded_from_timing": true,
            "history_items_before": before.items().len(),
            "conversation_serialized_bytes_before": before_bytes,
            "history_items_after": result.conversation().items().len(),
            "conversation_serialized_bytes_after": after_bytes,
            "terminal_outcome": "completed",
            "prompt_to_terminal_result_elapsed_ns": prompt_to_terminal_result_elapsed_ns,
            "model_calls_total_in_fresh_agent": model.calls(),
            "model_rounds": result.model_rounds(),
            "tool_calls": result.tool_calls(),
            "structural_sharing": {
                "prepared_request_pre_turn_history_is_exact_persistent_ancestor": sharing.pre_turn_history_is_exact_ancestor,
                "prepared_request_appended_history_segments": sharing.measured_history_appended_segments,
                "prepared_request_history_root_allocation_shared": sharing.measured_history_root_shared,
                "prepared_request_prior_segment_payload_pairs_observed": sharing.measured_history_segment_pairs,
                "all_pre_turn_segment_payload_pairs_shared_into_prepared_request": sharing.all_measured_history_segment_payloads_shared,
                "all_observed_stable_prefix_pairs_shared_exact_allocation": (sharing.prefix_observations >= 2).then_some(sharing.all_prefixes_shared),
                "stable_prefix_reuse_pairs": sharing.prefix_observations.saturating_sub(1),
                "stable_prefix_observations": sharing.prefix_observations,
                "prior_segment_payload_pairs_observed": prior_segment_pairs,
                "all_pre_turn_segment_payload_pairs_shared_into_result": (prior_segment_pairs > 0).then_some(prior_segments_shared),
                "assertion_scope": "Arc allocation identity exposed by public sharing predicates; not an allocation-count measurement",
            },
            "allocation_count": Value::Null,
            "allocated_bytes": Value::Null,
            "measured_task_model_calls": measured_task_model_calls,
        })
    );
    agent.shutdown().await.expect("perf shutdown");
}

async fn measure_event_history(history_turns: usize) {
    let directory = tempdir().expect("event workspace");
    let workspace =
        Arc::new(WorkspacePolicy::host(directory.path(), ".").expect("event workspace policy"));
    let agent = AgentBuilder::new(
        configured_model(Arc::new(DynamicTextModel::new()), &descriptor()),
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )
    .config(AgentConfig::default().with_event_capacity(65_536))
    .build()
    .expect("event agent");
    for index in 0..history_turns {
        run_turn(&agent, format!("event-history-{index:04}")).await;
    }
    let canonical_items_before = agent.conversation().items().len();
    let attach_started = Instant::now();
    let attachment = agent
        .attach(ViewRequest::content())
        .await
        .expect("event content projection");
    let attach_elapsed_ns = attach_started.elapsed().as_nanos();
    let (snapshot, mut events) = attachment.into_parts();
    let snapshot_clone_started = Instant::now();
    let snapshot_clone = snapshot.clone();
    let snapshot_clone_elapsed_ns = snapshot_clone_started.elapsed().as_nanos();
    assert_eq!(snapshot_clone, snapshot);
    let snapshot_projection_serialized_bytes =
        serde_json::to_vec(snapshot.conversation().unwrap_or_default())
            .expect("serialize event snapshot projection")
            .len();
    let pre_turn_sequence = snapshot.cursor().sequence();
    run_turn(&agent, "event-measured".to_owned()).await;
    let after = agent
        .attach(ViewRequest::content())
        .await
        .expect("post-event content projection");
    let post_turn_sequence = after.snapshot().cursor().sequence();
    let post_snapshot_projection_serialized_bytes =
        serde_json::to_vec(after.snapshot().conversation().unwrap_or_default())
            .expect("serialize post-event snapshot projection")
            .len();
    let event_count = serde_json::to_value(post_turn_sequence)
        .expect("serialize post-event sequence")
        .as_u64()
        .expect("numeric post-event sequence")
        .saturating_sub(
            serde_json::to_value(pre_turn_sequence)
                .expect("serialize pre-event sequence")
                .as_u64()
                .expect("numeric pre-event sequence"),
        );
    let delivery_started = Instant::now();
    let mut event_serialized_bytes = 0_usize;
    for _ in 0..event_count {
        let event = events.recv().await.expect("instrumented event retained");
        event_serialized_bytes = event_serialized_bytes.saturating_add(
            serde_json::to_vec(event.as_ref())
                .expect("serialize projected event")
                .len(),
        );
    }
    let delivery_and_serialization_elapsed_ns = delivery_started.elapsed().as_nanos();
    assert_eq!(events.last_delivered().sequence(), post_turn_sequence);
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "event-history-instrumented-sample",
            "fixture": EVENT_FIXTURE,
            "history_prefill_turns": history_turns,
            "canonical_items_before": canonical_items_before,
            "pre_turn_cursor_sequence": pre_turn_sequence,
            "post_turn_cursor_sequence": post_turn_sequence,
            "projected_events": event_count,
            "projected_event_serialized_bytes": event_serialized_bytes,
            "projected_event_serialized_bytes_per_event": event_serialized_bytes.checked_div(usize::try_from(event_count).expect("event count fits usize")),
            "event_delivery_and_serialization_elapsed_ns": delivery_and_serialization_elapsed_ns,
            "snapshot_attach_elapsed_ns": attach_elapsed_ns,
            "snapshot_clone_elapsed_ns": snapshot_clone_elapsed_ns,
            "snapshot_clone_value_equal": true,
            "snapshot_committed_conversation_items": snapshot.committed_conversation_items(),
            "snapshot_projection_serialized_bytes": snapshot_projection_serialized_bytes,
            "post_snapshot_projection_serialized_bytes": post_snapshot_projection_serialized_bytes,
            "snapshot_projection_serialized_growth_bytes": post_snapshot_projection_serialized_bytes.saturating_sub(snapshot_projection_serialized_bytes),
            "retained_bytes": Value::Null,
            "retained_bytes_external_probe": "tests/performance/baselines/perf_allocations.py process-isolated glibc memusage trace; ordinary timing remains uninstrumented",
            "instrumentation_scope": "separate fresh-agent public projection/subscriber workload executed after all primary prompt latency rows",
        })
    );
    agent.shutdown().await.expect("event shutdown");
}

async fn run_turn(agent: &plakous::agent::Agent, prompt: String) {
    let result = agent
        .prompt(prompt)
        .await
        .expect("history prompt")
        .result()
        .await
        .expect("history result");
    assert_eq!(
        result.response().expect("completed response").output_text(),
        RESPONSE
    );
}

async fn measure_prompt_bound() {
    let directory = tempdir().expect("queue workspace");
    let workspace =
        Arc::new(WorkspacePolicy::host(directory.path(), ".").expect("queue workspace policy"));
    let model = Arc::new(BlockingModel::new());
    let agent = AgentBuilder::new(
        configured_model(model.clone(), &descriptor()),
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )
    .config(AgentConfig::default().with_prompt_queue_capacity(PROMPT_CAPACITY))
    .build()
    .expect("queue agent");

    let mut turns = vec![agent.prompt("active").await.expect("active prompt")];
    tokio::time::timeout(Duration::from_secs(10), model.wait_started())
        .await
        .expect("blocking model did not start within the fixture timeout");
    assert!(
        model.started(),
        "blocking model did not start deterministically"
    );
    for index in 0..PROMPT_CAPACITY {
        let turn = agent
            .prompt(format!("queued-{index}"))
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "queue admission {index} below configured capacity failed as {:?}: {}",
                    error.kind(),
                    error.message()
                )
            });
        turns.push(turn);
    }
    let rejected = agent
        .prompt(format!("queued-{PROMPT_CAPACITY}"))
        .await
        .expect_err("first prompt beyond exact queue capacity must be rejected");
    assert_eq!(rejected.kind(), AgentErrorKind::Backpressure);
    assert_eq!(turns.len(), PROMPT_CAPACITY + 1);
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "prompt-bound",
            "fixture": "blocked-model-prompt-ingress-v2",
            "configured_capacity": PROMPT_CAPACITY,
            "active_turns": 1,
            "admitted_queued_turns": turns.len() - 1,
            "admitted_turns_including_active": turns.len(),
            "first_rejected_queue_index": PROMPT_CAPACITY,
            "rejection": format!("{:?}", rejected.kind()),
            "exact_bound_proved": turns.len() == PROMPT_CAPACITY + 1,
        })
    );
    agent.shutdown().await.expect("queue shutdown");
}

fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    Command::new(program)
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_output(arguments: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(workspace_root())
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_stdout_digest(arguments: &[&str]) -> Option<Digest> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root())
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| command_bytes_digest(&output.stdout))?
}

fn worktree_content_closure_metadata() -> Value {
    let root = workspace_root();
    let output = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output();
    let Ok(output) = output else {
        return Value::Null;
    };
    if !output.status.success() {
        return Value::Null;
    }
    let mut framed = Vec::new();
    let mut file_count = 0_usize;
    let mut content_bytes = 0_u64;
    let mut missing_files = 0_usize;
    for raw_path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let Ok(path) = std::str::from_utf8(raw_path) else {
            return Value::Null;
        };
        let Ok(path_len) = u64::try_from(raw_path.len()) else {
            return Value::Null;
        };
        framed.extend_from_slice(&path_len.to_be_bytes());
        framed.extend_from_slice(raw_path);
        match std::fs::read(root.join(path)) {
            Ok(bytes) => {
                let Ok(byte_len) = u64::try_from(bytes.len()) else {
                    return Value::Null;
                };
                framed.push(b'F');
                framed.extend_from_slice(&byte_len.to_be_bytes());
                framed.extend_from_slice(&bytes);
                content_bytes = content_bytes.saturating_add(byte_len);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                framed.push(b'M');
                missing_files = missing_files.saturating_add(1);
            }
            Err(_) => return Value::Null,
        }
        file_count = file_count.saturating_add(1);
    }
    json!({
        "identity": Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"plakous/non-ignored-worktree-content-closure-v1", &framed],
        ).ok(),
        "file_count": file_count,
        "content_bytes": content_bytes,
        "missing_tracked_files": missing_files,
        "rule": "ordered git ls-files cached plus non-ignored untracked paths, framed with each current regular-file content or a missing marker; file modes are covered separately by commit/diff provenance",
    })
}

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn command_bytes_digest(bytes: impl AsRef<[u8]>) -> Option<Digest> {
    Digest::sha256(
        DigestDomain::Blob,
        1,
        &[b"plakous/perf-command-stdout-v1", bytes.as_ref()],
    )
    .ok()
}

fn cpu_metadata() -> Value {
    let Some(output) = command_output("lscpu", &[]) else {
        return Value::Null;
    };
    let fields = [
        "Architecture",
        "Vendor ID",
        "Model name",
        "CPU(s)",
        "Thread(s) per core",
        "Core(s) per socket",
        "Socket(s)",
        "NUMA node(s)",
        "CPU max MHz",
        "CPU min MHz",
    ];
    Value::Object(
        fields
            .into_iter()
            .filter_map(|field| {
                output.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    (name.trim() == field)
                        .then(|| (field.to_owned(), Value::String(value.trim().to_owned())))
                })
            })
            .collect(),
    )
}

fn target_metadata() -> Value {
    json!({
        "triple": target_triple(),
        "arch": std::env::consts::ARCH,
        "os": std::env::consts::OS,
        "family": std::env::consts::FAMILY,
        "environment": target_environment(),
        "pointer_width": if cfg!(target_pointer_width = "64") { 64 } else if cfg!(target_pointer_width = "32") { 32 } else { 16 },
        "endian": if cfg!(target_endian = "little") { "little" } else { "big" },
    })
}

fn target_triple() -> &'static str {
    if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "musl"
    )) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "gnu"
    )) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "musl"
    )) {
        "aarch64-unknown-linux-musl"
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(
        target_arch = "x86_64",
        target_os = "windows",
        target_env = "msvc"
    )) {
        "x86_64-pc-windows-msvc"
    } else {
        "unmapped-see-target-axes"
    }
}

fn target_environment() -> &'static str {
    if cfg!(target_env = "gnu") {
        "gnu"
    } else if cfg!(target_env = "musl") {
        "musl"
    } else if cfg!(target_env = "msvc") {
        "msvc"
    } else {
        "none-or-unmapped"
    }
}

/// Reports the linker configuration the workspace actually ships.
///
/// Evidence is read from `.cargo/config.toml` at run time rather than assumed:
/// the repository declares no linker or `rustflags` section, so this records
/// `"none"` instead of a linker the build never used.
fn workspace_target_config() -> String {
    let config = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(".cargo")
        .join("config.toml");
    let Ok(text) = std::fs::read_to_string(&config) else {
        return "unreadable".to_owned();
    };
    let declared = text
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("rustflags") || line.starts_with("linker"))
        .collect::<Vec<_>>();
    if declared.is_empty() {
        "none".to_owned()
    } else {
        declared.join("; ")
    }
}

fn codegen_metadata() -> Value {
    json!({
        "profile": "bench",
        "profile_inheritance": "Cargo bench defaults inherited from release; workspace overrides release codegen-units and lto",
        "workspace_declared_not_effective": {
            "opt_level": 3,
            "debug": false,
            "debug_assertions": cfg!(debug_assertions),
            "overflow_checks": false,
            "lto": "thin",
            "codegen_units": 1,
            "incremental": false,
            "panic": if cfg!(panic = "abort") { "abort" } else { "unwind" },
        },
        "rustflags": {
            "workspace_target_config": workspace_target_config(),
            "RUSTFLAGS_at_execution": std::env::var("RUSTFLAGS").ok(),
            "CARGO_ENCODED_RUSTFLAGS_at_execution": std::env::var("CARGO_ENCODED_RUSTFLAGS").ok(),
            "additional_flags_id": std::env::var("PLAKOUS_PERF_RUSTFLAGS_ID").ok(),
            "effective_command_claimed": false,
        },
    })
}

fn enabled_features() -> Vec<&'static str> {
    [
        ("anthropic", cfg!(feature = "anthropic")),
        ("auth", cfg!(feature = "auth")),
        ("full", cfg!(feature = "full")),
        (
            "native-subscriptions",
            cfg!(feature = "native-subscriptions"),
        ),
        ("openai", cfg!(feature = "openai")),
        ("openrouter", cfg!(feature = "openrouter")),
        ("sessions-sqlite", cfg!(feature = "sessions-sqlite")),
        ("xai", cfg!(feature = "xai")),
    ]
    .into_iter()
    .filter_map(|(name, enabled)| enabled.then_some(name))
    .collect()
}

struct DynamicTextModel {
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    entries: AtomicU64,
    calls: AtomicU64,
    sharing: Mutex<ModelSharingProbe>,
}

#[derive(Clone, Copy)]
struct ModelSharingEvidence {
    pre_turn_history_is_exact_ancestor: Option<bool>,
    measured_history_appended_segments: usize,
    measured_history_root_shared: Option<bool>,
    all_measured_history_segment_payloads_shared: Option<bool>,
    measured_history_segment_pairs: usize,
    all_prefixes_shared: bool,
    prefix_observations: u64,
}

#[derive(Default)]
struct ModelSharingProbe {
    expected_history: Option<CanonicalHistory>,
    first_prefix: Option<StableRequestPrefix>,
    pre_turn_history_is_exact_ancestor: Option<bool>,
    measured_history_appended_segments: usize,
    measured_history_root_shared: Option<bool>,
    all_measured_history_segment_payloads_shared: Option<bool>,
    measured_history_segment_pairs: usize,
    all_prefixes_shared: bool,
    prefix_observations: u64,
}

impl DynamicTextModel {
    fn new() -> Self {
        let descriptor = descriptor();
        Self {
            authority: fixture_authority(&descriptor),
            descriptor,
            entries: AtomicU64::new(1),
            calls: AtomicU64::new(0),
            sharing: Mutex::new(ModelSharingProbe {
                all_prefixes_shared: true,
                ..ModelSharingProbe::default()
            }),
        }
    }

    fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    fn expect_measured_history(&self, history: &CanonicalHistory) {
        self.sharing
            .lock()
            .expect("sharing probe lock")
            .expected_history = Some(history.clone());
    }

    fn observe_request_sharing(&self, plan: &ValidatedRequestPlan) {
        let mut probe = self.sharing.lock().expect("sharing probe lock");
        let prefix = plan.request().stable_prefix();
        if let Some(first) = &probe.first_prefix {
            probe.all_prefixes_shared &= first.shares_storage_with(prefix);
        } else {
            probe.first_prefix = Some(prefix.clone());
        }
        probe.prefix_observations = probe.prefix_observations.saturating_add(1);
        if let Some(expected) = probe.expected_history.take() {
            let measured = plan.request().input().history();
            let segment_pairs = expected.segments().len();
            let appended_segments = measured
                .append_delta_since(&expected)
                .expect("prepared request retains exact pre-turn history ancestry")
                .len();
            assert_eq!(
                measured.segments().len(),
                expected.segments().len() + appended_segments
            );
            let all_segment_payloads_shared = expected
                .segments()
                .iter()
                .zip(measured.segments())
                .all(|(prior, next)| prior.shares_storage_with(next));
            if segment_pairs > 0 {
                assert!(all_segment_payloads_shared);
            }
            probe.pre_turn_history_is_exact_ancestor = Some(true);
            probe.measured_history_appended_segments = appended_segments;
            probe.measured_history_root_shared =
                (segment_pairs > 0).then_some(measured.shares_storage_with(&expected));
            probe.all_measured_history_segment_payloads_shared =
                (segment_pairs > 0).then_some(all_segment_payloads_shared);
            probe.measured_history_segment_pairs = segment_pairs;
        }
    }

    fn sharing_evidence(&self) -> ModelSharingEvidence {
        let probe = self.sharing.lock().expect("sharing probe lock");
        ModelSharingEvidence {
            pre_turn_history_is_exact_ancestor: probe.pre_turn_history_is_exact_ancestor,
            measured_history_appended_segments: probe.measured_history_appended_segments,
            measured_history_root_shared: probe.measured_history_root_shared,
            all_measured_history_segment_payloads_shared: probe
                .all_measured_history_segment_payloads_shared,
            measured_history_segment_pairs: probe.measured_history_segment_pairs,
            all_prefixes_shared: probe.all_prefixes_shared,
            prefix_observations: probe.prefix_observations,
        }
    }
}

impl LanguageModel for DynamicTextModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        fresh_fixture_authority(&self.descriptor, &self.authority, checked_at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.observe_request_sharing(&plan);
        let (record, prepared_wire) = fixture_prepared_record(
            &self.descriptor,
            &self.authority,
            &plan,
            &context,
            "dynamic-text",
        )?;
        Ok(PreparedModelCall::new(record, move |_call_context| {
            Box::pin(async move {
                black_box(prepared_wire);
                let entry = self.entries.fetch_add(1, Ordering::Relaxed);
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(text_stream(&plan, entry))
            })
        }))
    }
}

struct BlockingModel {
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    started: AtomicBool,
    started_notify: Notify,
}

impl BlockingModel {
    fn new() -> Self {
        let descriptor = descriptor();
        Self {
            authority: fixture_authority(&descriptor),
            descriptor,
            started: AtomicBool::new(false),
            started_notify: Notify::new(),
        }
    }

    fn started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    async fn wait_started(&self) {
        while !self.started() {
            self.started_notify.notified().await;
        }
    }
}

impl LanguageModel for BlockingModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        fresh_fixture_authority(&self.descriptor, &self.authority, checked_at)
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let (record, prepared_wire) = fixture_prepared_record(
            &self.descriptor,
            &self.authority,
            &plan,
            &context,
            "blocking",
        )?;
        Ok(PreparedModelCall::new(record, move |call_context| {
            let cancellation = call_context.cancellation().clone();
            Box::pin(async move {
                black_box(&prepared_wire);
                self.started.store(true, Ordering::Release);
                self.started_notify.notify_one();
                cancellation.cancelled().await;
                black_box(prepared_wire);
                Err(ModelError::cancelled())
            })
        }))
    }
}

fn fixture_authority(descriptor: &ResolvedModelDescriptor) -> RuntimeModelAuthority {
    let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"perf-fixture-account"])
        .expect("perf account binding");
    let credential = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"perf-fixture-credential"])
        .expect("perf credential binding");
    RuntimeModelAuthority::new(
        descriptor,
        account,
        credential,
        1,
        Timestamp::new(0, 0).expect("perf authority timestamp"),
        None,
    )
    .expect("perf runtime authority")
}

fn configured_model(
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
) -> ConfiguredLanguageModel {
    let standard = StandardIntegrationAuthority::release_owned();
    let authority = fixture_authority(descriptor);
    let behavior = ProviderBehaviorConfiguration {
        tool_mode: plakous_model::ToolPresentationMode::Direct,
        reasoning_effort: ReasoningEffortDisposition::Disabled,
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache: CachePolicy::Disabled,
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tools: Vec::new(),
        hosted_tools: Vec::new(),
        tool_choice: ToolChoice::Auto,
        parallel_tools: ParallelToolDisposition::Disabled,
        output: OutputPolicy::default(),
        output_bound: OutputBoundDisposition::NoRequestBound,
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("perf-core-retention-v1")
                .expect("retention revision"),
        },
        response_state: ResponseStateDisposition::Stateless,
        context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
        local_durability: LocalDurabilityDisposition::Ephemeral,
    };
    let selected = SelectedProviderConfiguration::new(
        ProviderRouteSelection::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.profile().clone(),
            ProviderProfileRevisionId::new("perf-core-route-v1").expect("route revision"),
            ModelAdapterProfileId::new("perf-core.auth").expect("auth adapter"),
            ProviderProfileRevisionId::new("perf-core-auth-v1").expect("auth revision"),
            ProviderAuthorityClassId::new("perf-core-synthetic").expect("authority class"),
            CredentialSelectionId::new("perf-core-credential").expect("credential selection"),
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )
    .expect("selected configuration");
    let stable = ResolvedProviderAuthorityBinding::new(
        descriptor,
        &authority,
        ProviderSemanticEvidence::new(1, b"perf-core-provider-semantics-v1".to_vec())
            .expect("provider semantics"),
    )
    .expect("stable authority");
    let family = FamilyPolicyEnvelope::new(
        descriptor.model_family().cloned().expect("model family"),
        1,
        1,
        1,
        b"perf-core-family-policy-v1".to_vec(),
    )
    .expect("family policy");
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        ModelImplementationIdentity::release_owned(&standard, descriptor, 1)
            .expect("implementation identity"),
        stable,
        behavior,
        family,
    )
    .expect("effective configuration");
    ConfiguredLanguageModel::new(
        &standard,
        model,
        CompiledProviderConfiguration::new(selected, effective).expect("compiled configuration"),
        authority,
    )
    .expect("configured model")
}

fn fresh_fixture_authority(
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
    checked_at: Timestamp,
) -> RuntimeModelAuthorityOperation {
    let authority = authority.clone();
    let result = authority
        .validate(descriptor, checked_at)
        .map(|()| authority);
    Box::pin(async move { result })
}

fn fixture_prepared_record(
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
    plan: &ValidatedRequestPlan,
    context: &ModelPreparationContext,
    fixture: &str,
) -> Result<(PreparedCallRecord, Arc<[u8]>), ModelPreparationError> {
    descriptor.validate_plan_binding(plan)?;
    context
        .authority()
        .validate(descriptor, context.prepared_at())?;
    if !authority.same_selected_authority(context.authority()) {
        return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
    }
    let wire = serde_json::to_vec(&(
        fixture,
        plan.request().model_input_identity(),
        plan.decisions(),
    ))
    .map_err(|_| {
        ModelPreparationError::rejected(
            ModelPreparationErrorKind::Projection,
            "perf fixture wire serialization failed",
        )
    })?;
    let wire_size = u64::try_from(wire.len()).map_err(|_| {
        ModelPreparationError::rejected(
            ModelPreparationErrorKind::ResourceLimit,
            "perf fixture wire exceeds addressable size",
        )
    })?;
    let wire_digest = Digest::sha256(
        DigestDomain::Blob,
        1,
        &[b"plakous/perf-fixture-wire-v1", &wire],
    )
    .map_err(plakous::model::PreparedCallError::from)?;
    let observation = CountObservation::new(
        MEASUREMENT_CERTIFICATE_ID,
        MEASUREMENT_CERTIFICATE_REVISION,
        wire_size,
    )
    .map_err(|_| {
        ModelPreparationError::rejected(
            ModelPreparationErrorKind::Projection,
            "perf fixture count observation is invalid",
        )
    })?;
    let token_projection = PreparedTokenProjection::new(wire_digest, wire_size, Some(observation))?;
    let contract = descriptor.context_contract().ok_or_else(|| {
        ModelPreparationError::rejected(
            ModelPreparationErrorKind::Projection,
            "perf fixture descriptor omitted synthetic context evidence",
        )
    })?;
    let protocol_family = ProtocolId::new(PROTOCOL_FAMILY).map_err(|_| {
        ModelPreparationError::rejected(
            ModelPreparationErrorKind::Projection,
            "perf fixture protocol family is invalid",
        )
    })?;
    let output_limit = PreparedOutputLimit::release_verified_hard_cap(
        std::num::NonZeroU32::new(1_024).expect("nonzero fixture output cap"),
        contract.capacity().identity(),
        descriptor,
        context,
        &protocol_family,
        LOWERING_REVISION,
        PROJECTION_REVISION,
        ACCOUNTING_REVISION,
    )?;
    let projection = PreparedModelProjection::new(
        descriptor,
        plan,
        context,
        protocol_family,
        LOWERING_REVISION,
        PROJECTION_REVISION,
        ACCOUNTING_REVISION,
        wire_digest,
        wire_size,
        token_projection,
        PreparedCacheUse::None,
        Some(output_limit),
    )?;
    let record = PreparedCallRecord::for_prepared(descriptor, plan, projection, context)?;
    Ok((record, wire.into()))
}

fn descriptor() -> ResolvedModelDescriptor {
    let standard = StandardIntegrationAuthority::release_owned();
    let provenance = FactProvenance::new(
        FactSource::TestFixture,
        Timestamp::new(1, 0).expect("timestamp"),
        None,
    );
    let features = BTreeMap::from([(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    )]);
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0).expect("evidence timestamp"), None)
        .expect("evidence freshness");
    let context = ContextContract::new(
        ProtocolId::new(PROTOCOL_FAMILY).expect("protocol family"),
        ACCOUNTING_REVISION,
        PROJECTION_REVISION,
        RESERVATION_REVISION,
        CapacityCertificate::new(
            &standard,
            "perf-core-synthetic-capacity",
            "perf-core-synthetic-capacity-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::Separate {
                input: SafeLimit::Exact(1024 * 1024),
                output: SafeLimit::Exact(1_024),
            },
            None,
            freshness,
        )
        .expect("synthetic capacity"),
        vec![
            MeasurementCertificate::standard(
                &standard,
                MEASUREMENT_CERTIFICATE_ID,
                MEASUREMENT_CERTIFICATE_REVISION,
                "perf-core-complete-synthetic-wire-byte-counter",
                ACCOUNTING_REVISION,
                ContextEvidenceSource::BuiltInCatalog,
                CountCertificateKind::ExactTokenizer,
                None,
                freshness,
            )
            .expect("synthetic measurement"),
        ],
    )
    .expect("synthetic context contract");
    ResolvedModelDescriptor::new(
        &standard,
        ProtocolId::new("perf.protocol").expect("protocol"),
        RouteId::new("perf.route").expect("route"),
        ModelId::new("perf.model").expect("model"),
        None,
        Some(ModelFamilyId::new(PROTOCOL_FAMILY).expect("model family")),
        ModelAdapterProfileId::new("perf.profile").expect("profile"),
        CapabilitySet::new(features, BTreeMap::new()),
    )
    .with_context_contract(context)
}

fn text_stream(plan: &ValidatedRequestPlan, entry: u64) -> ModelStream {
    let digest = plan.digest();
    let entry_id =
        EntryId::try_from(Uuid::from_u128(u128::MAX - u128::from(entry))).expect("assistant entry");
    let item = plakous::model::ConversationItem::new(
        entry_id,
        Actor::Assistant,
        vec![ContentBlock::Text(
            TextBlock::new(RESPONSE, Some(TextPhase::Final)).expect("response text"),
        )],
        Provenance::ModelOutput,
    )
    .expect("assistant item");
    let location = OutputLocation::new(0, 0);
    let response = ModelResponse::new(
        vec![item.clone()],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    Box::pin(PerfStream {
        events: VecDeque::from([
            Ok(ModelEvent::ResponseStarted(ResponseStarted::new(
                None, digest,
            ))),
            Ok(ModelEvent::BlockStarted(BlockStarted::new(
                item.id(),
                location,
                BlockStart::Text {
                    phase: Some(TextPhase::Final),
                },
            ))),
            Ok(ModelEvent::TextDelta(TextDelta::new(
                location,
                RESPONSE.as_bytes(),
            ))),
            Ok(ModelEvent::BlockCompleted(BlockCompleted::new(location))),
            Ok(ModelEvent::ResponseCompleted(response)),
        ]),
    })
}

struct PerfStream {
    events: VecDeque<Result<ModelEvent, ModelError>>,
}

impl Stream for PerfStream {
    type Item = Result<ModelEvent, ModelError>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().events.pop_front())
    }
}
