//! Deterministic JSONL workload for prompt projection, sharing, and synthetic context evidence.

#![allow(
    clippy::expect_used,
    clippy::print_stdout,
    reason = "benchmark fixture fails fast and emits machine-readable JSONL"
)]
#![recursion_limit = "256"]

use std::collections::BTreeMap;
use std::error::Error;
use std::hint::black_box;
use std::num::NonZeroU32;
use std::process::Command;
use std::time::Instant;

use plakous::model::{
    Actor, AttemptId, BranchId, CachePolicy, CanonicalHistory, Capability, CapabilityConstraints,
    CapabilityRequirement, CapabilitySet, CapacityCertificate, CapacityRule,
    CompiledProviderConfiguration, ConfiguredTool, ContentBlock, ContextAdmissionRequirement,
    ContextContract, ContextEvidenceSource, ContextStatusKind, ConversationItem, ConversationView,
    CountCertificateKind, CountObservation, CredentialSelectionId, Digest, DigestDomain,
    EffectiveProviderConfiguration, EntryId, EvidenceFreshness, FactProvenance, FactSource,
    FamilyPolicyEnvelope, FeatureSupport, GenerationLimits, HistorySegment, HistorySegmentKind,
    InstructionBlock, InstructionSource, LocalDurabilityDisposition, MeasurementCertificate,
    ModelAdapterProfileId, ModelCacheAffinity, ModelFamilyId, ModelId, ModelImplementationIdentity,
    ModelPreparationContext, ModelRequest, NamespaceId, NativeOptions, OutputBoundDisposition,
    OutputPolicy, ParallelToolDisposition, PreparedCacheUse, PreparedCallRecord,
    PreparedModelProjection, PreparedOutputLimit, PreparedTokenProjection, ProtocolId, Provenance,
    ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
    ProviderRouteSelection, ProviderSemanticEvidence, ProviderStorageDisposition, ReasoningEffort,
    ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningPolicy,
    ReasoningSummaryDisposition, ReplayDisposition, RequestMetadata, ResolvedModelDescriptor,
    ResolvedProviderAuthorityBinding, ResponseStateDisposition, RetentionDisposition, RouteId,
    RunId, RuntimeModelAuthority, SafeLimit, SelectedProviderConfiguration, SessionId,
    StableRequestPrefix, Support, TextBlock, TextPhase, Timestamp, ToolChoice, ToolConcurrency,
    ToolDefinition, ToolEffect, ToolExecutionDisposition, ToolInputContract, ToolName,
    ToolOutputContract, TurnId, ValidationMode, WorkingProjection, WorkingProjectionLimits,
};
use plakous::tools::{ArgumentLimits, SchemaLimits, ValidatedToolSchema};
use plakous_model::{ActiveFamilyArtifact, StandardIntegrationAuthority};
use serde_json::{Value, json};
use uuid::Uuid;

const SCHEMA: &str = "plakous.perf-project.v6";
const FIXTURE: &str = "provider-neutral-segmented-projection-v5";
const FIXTURE_REVISION: u32 = 5;
const HISTORY_SWEEP_ITEMS: &[usize] = &[0, 16, 64, 256];
const HISTORY_SEGMENT_ITEMS: usize = 8;
const FAMILY_ARTIFACT_SWEEP_BYTES: &[usize] = &[64, 4_096, 65_536, 1_048_576];
const SAMPLES: usize = 4;
const PROTOCOL_FAMILY: &str = "perf.synthetic-responses-family";
const LOWERING_REVISION: &str = "perf-synthetic-lowering-v2";
const PROJECTION_REVISION: &str = "perf-synthetic-projection-v2";
const ACCOUNTING_REVISION: &str = "perf-synthetic-byte-token-accounting-v1";
const RESERVATION_REVISION: &str = "perf-synthetic-output-reservation-v1";
const CAPACITY_EVIDENCE_ID: &str = "perf-synthetic-capacity";
const CAPACITY_EVIDENCE_REVISION: &str = "perf-synthetic-capacity-v1";
const CAPACITY_EVIDENCE_CORPUS_REVISION: &str = "perf-synthetic-capacity-corpus-v1";
const MEASUREMENT_CERTIFICATE_ID: &str = "perf-synthetic-byte-tokenizer";
const MEASUREMENT_CERTIFICATE_REVISION: &str = "perf-synthetic-byte-tokenizer-v1";

type BenchResult<T> = Result<T, Box<dyn Error>>;

fn main() -> BenchResult<()> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_some_and(|value| value == "--isolated-probe")
    {
        return run_isolated_probe(&arguments[1..]);
    }
    let fixture = StableFixture::new()?;
    emit_metadata();
    let mut history = CanonicalHistory::default();
    for &target_items in HISTORY_SWEEP_ITEMS {
        let (advanced, evidence) = advance_history(&history, target_items)?;
        emit_history_advance(&history, &advanced, &evidence);
        history = advanced;
        measure_append_samples(&history, target_items)?;
        let working_segment = working_segment()?;
        for sample in 0..SAMPLES {
            measure_sample(&fixture, &history, &working_segment, target_items, sample)?;
        }
    }
    measure_family_artifact_growth(&fixture)?;
    Ok(())
}

fn run_isolated_probe(arguments: &[String]) -> BenchResult<()> {
    let [operation, scale, iterations, trailing @ ..] = arguments else {
        return Err("usage: perf_project --isolated-probe <project-round|family-artifact|tool-schema> <scale> <iterations>".into());
    };
    if trailing.iter().any(|argument| argument != "--bench") {
        return Err("unknown trailing isolated perf-project probe argument".into());
    }
    let scale = scale.parse::<usize>()?;
    let iterations = iterations.parse::<usize>()?;
    let fixture = StableFixture::new()?;
    let mut completed = 0_usize;
    let mut exact_bytes = 0_usize;
    match operation.as_str() {
        "project-round" => {
            let (history, _) = advance_history(&CanonicalHistory::default(), scale)?;
            let working_segment = working_segment()?;
            for _ in 0..iterations {
                let working = WorkingProjection::new(
                    vec![working_segment.clone()],
                    WorkingProjectionLimits::new(4, 32, 1024 * 1024)?,
                )?;
                let projection = ConversationView::from_shared(history.clone(), working);
                let request =
                    ModelRequest::from_parts(fixture.prefix.clone(), projection, fixture.metadata)?;
                let round = request.for_round(fixture.next_round_metadata)?;
                let serialized = serde_json::to_vec(&round)?;
                exact_bytes = exact_bytes.saturating_add(serialized.len());
                black_box(fixture.synthetic_prepared_evidence(&round)?);
                completed = completed.saturating_add(1);
            }
        }
        "family-artifact" => {
            for _ in 0..iterations {
                let artifact = ActiveFamilyArtifact::new(
                    PROTOCOL_FAMILY,
                    1,
                    1,
                    fixture.descriptor.binding_digest()?,
                    vec![0xA5; scale],
                )?;
                exact_bytes = exact_bytes.saturating_add(serde_json::to_vec(&artifact)?.len());
                black_box(artifact.integrity());
                completed = completed.saturating_add(1);
            }
        }
        "tool-schema" => {
            let definition = generated_tool_definition(scale)?;
            for _ in 0..iterations {
                let validator = ValidatedToolSchema::compile(
                    &definition,
                    SchemaLimits::default(),
                    ArgumentLimits::default(),
                )?;
                let arguments = generated_tool_arguments(scale)?;
                exact_bytes = exact_bytes.saturating_add(arguments.len());
                black_box(validator.validate_arguments(&arguments)?);
                completed = completed.saturating_add(1);
            }
        }
        _ => return Err("unknown isolated perf-project probe operation".into()),
    }
    println!(
        "{}",
        json!({
            "schema": "plakous.perf-isolated-workload.v1",
            "bench": "perf_project",
            "operation": operation,
            "scale": scale,
            "iterations": iterations,
            "completed_iterations": completed,
            "exact_serialized_or_validated_bytes_total": exact_bytes,
        })
    );
    Ok(())
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
            "workload": "persistent history append, structurally verified handle clones, shared-prefix ModelRequest::from_parts, shared round update, full builder rebuild, and synthetic prepared context admission",
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
                "bench": "perf_project",
                "command": "cargo bench --locked -p plakous --bench perf_project",
                "enabled_feature_closure": features,
                "default_feature_definition": "empty",
                "requested_feature_flags": std::env::var("PLAKOUS_PERF_FEATURE_FLAGS").ok(),
            },
            "codegen": codegen_metadata(),
            "source": {
                "commit": git_output(&["rev-parse", "HEAD"]),
                "worktree_dirty": git_status.as_ref().is_some_and(|status| !status.is_empty()),
                "worktree_status_digest": git_status.as_deref().and_then(command_bytes_digest),
                "tracked_worktree_patch_digest": git_stdout_digest(&["diff", "--binary", "HEAD"]),
                "non_ignored_worktree_content_closure": worktree_content_closure_metadata(),
                "benchmark_source_git_blob": git_output(&["hash-object", "crates/plakous/benches/perf_project.rs"]),
                "workspace_manifest_git_blob": git_output(&["hash-object", "Cargo.toml"]),
                "package_manifest_git_blob": git_output(&["hash-object", "crates/plakous/Cargo.toml"]),
                "cargo_lock_git_blob": git_output(&["hash-object", "Cargo.lock"]),
                "baseline_identity": Value::Null,
                "baseline_status": "no ratified numeric baseline; emit raw samples",
            },
            "history_sweep_items": HISTORY_SWEEP_ITEMS,
            "history_segment_items": HISTORY_SEGMENT_ITEMS,
            "family_artifact_sweep_bytes": FAMILY_ARTIFACT_SWEEP_BYTES,
            "samples_per_point": SAMPLES,
            "cold_warm_rule": {
                "classification": "not claimed",
                "history": "history advances persistently once between points and is reused by samples",
                "prefix": "normal samples reuse one prefix allocation; full_rebuild is separately timed but does not reset process or OS caches",
                "process": "all operations run serially in one process with uncontrolled allocator and OS cache warmth",
            },
            "allocation_probe": {
                "available": false,
                "reason": "ordinary timing rows have no in-process allocator instrumentation; tests/performance/baselines/perf_allocations.py runs dedicated entry points under process-isolated glibc memusage",
            },
            "synthetic_context_scope": {
                "protocol_family": PROTOCOL_FAMILY,
                "lowering_revision": LOWERING_REVISION,
                "projection_revision": PROJECTION_REVISION,
                "accounting_revision": ACCOUNTING_REVISION,
                "reservation_revision": RESERVATION_REVISION,
                "capacity_evidence_corpus": {
                    "revision": CAPACITY_EVIDENCE_CORPUS_REVISION,
                    "identity": capacity_evidence_corpus_identity().ok(),
                    "binding": "supplementary benchmark-fixture provenance; the public BuiltInCatalog capacity certificate does not carry a corpus field",
                },
                "token_unit": "one byte of the complete credential-free synthetic wire body",
                "claim": "fixture-only algebra evidence; not provider capacity, tokenizer, cache, or live-support evidence",
            },
            "explicit_non_claims": [
                "no allocation count or allocated-byte evidence",
                "no real Responses-family artifact codec or provider managed-cache measurement",
                "no allocator-wide transient-clone trace beyond the explicit public handle-clone probes",
                "no object-write, restart-memory, CAS, or provider-wire evidence"
            ],
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

fn measure_family_artifact_growth(fixture: &StableFixture) -> BenchResult<()> {
    for &opaque_bytes in FAMILY_ARTIFACT_SWEEP_BYTES {
        let started = Instant::now();
        let artifact = ActiveFamilyArtifact::new(
            PROTOCOL_FAMILY,
            1,
            1,
            fixture.descriptor.binding_digest()?,
            vec![0xA5; opaque_bytes],
        )?;
        let construction_elapsed_ns = elapsed_ns(started);
        let clone_started = Instant::now();
        let cloned = artifact.clone();
        let clone_elapsed_ns = elapsed_ns(clone_started);
        let exact_opaque_allocation_shared = std::ptr::eq(artifact.bytes(), cloned.bytes());
        assert!(exact_opaque_allocation_shared);
        assert_eq!(artifact.integrity(), cloned.integrity());
        let serialized_envelope_bytes = serde_json::to_vec(&artifact)?.len();
        let tool_schema_trace = FAMILY_ARTIFACT_SWEEP_BYTES
            .last()
            .is_some_and(|last| opaque_bytes == *last)
            .then(measure_tool_schema_trace)
            .transpose()?;
        println!(
            "{}",
            json!({
                "schema": SCHEMA,
                "kind": "family-artifact-growth-sample",
                "fixture": FIXTURE,
                "opaque_bytes": opaque_bytes,
                "serialized_envelope_bytes": serialized_envelope_bytes,
                "construction_and_integrity_hash_elapsed_ns": construction_elapsed_ns,
                "handle_clone_elapsed_ns": clone_elapsed_ns,
                "clone_shared_exact_opaque_byte_allocation": exact_opaque_allocation_shared,
                "integrity": artifact.integrity(),
                "retained_bytes": Value::Null,
                "post_primary_tool_schema_trace": tool_schema_trace,
                "scope": "public neutral ActiveFamilyArtifact envelope with synthetic opaque bytes; construction timing includes validation, Arc conversion, and integrity hashing",
                "non_claim": "not a provider-family codec, runtime artifact rotation, allocator-retained-byte, or managed-cache measurement",
            })
        );
    }
    Ok(())
}

struct HistoryAdvanceEvidence {
    source_items: usize,
    target_items: usize,
    appended_segments: usize,
    append_elapsed_ns_total: u64,
    all_prior_segment_payloads_shared: bool,
    every_append_created_new_history_handle: bool,
}

fn advance_history(
    source: &CanonicalHistory,
    target_items: usize,
) -> BenchResult<(CanonicalHistory, HistoryAdvanceEvidence)> {
    if target_items < source.item_count() {
        return Err("persistent history fixture cannot move backwards".into());
    }
    let source_items = source.item_count();
    let mut history = source.clone();
    let mut appended_segments = 0_usize;
    let mut append_elapsed_ns_total = 0_u64;
    let mut all_prior_segment_payloads_shared = true;
    let mut every_append_created_new_history_handle = true;
    for start in (source_items..target_items).step_by(HISTORY_SEGMENT_ITEMS) {
        let end = (start + HISTORY_SEGMENT_ITEMS).min(target_items);
        let mut items = Vec::with_capacity(end - start);
        for index in start..end {
            let actor = if index % 2 == 0 {
                Actor::User
            } else {
                Actor::Assistant
            };
            items.push(text_item(
                0x1_000 + index as u128,
                actor,
                format!("history-{index:04}-{}", "x".repeat(96)),
            )?);
        }
        let prior = history.clone();
        let prior_segments = prior.segments().len();
        let segment = HistorySegment::new(HistorySegmentKind::Conversation, items)?;
        let started = Instant::now();
        let next = prior.append(segment)?;
        append_elapsed_ns_total = append_elapsed_ns_total.saturating_add(elapsed_ns(started));
        assert_eq!(next.segments().len(), prior_segments + 1);
        let payloads_shared = prior
            .segments()
            .iter()
            .zip(next.segments())
            .all(|(left, right)| left.shares_storage_with(right));
        assert!(payloads_shared);
        let new_handle = !prior.shares_storage_with(&next);
        assert!(new_handle);
        all_prior_segment_payloads_shared &= payloads_shared;
        every_append_created_new_history_handle &= new_handle;
        appended_segments = appended_segments.saturating_add(1);
        history = next;
    }
    assert_eq!(history.item_count(), target_items);
    assert!(all_prior_segment_payloads_shared);
    assert!(every_append_created_new_history_handle);
    Ok((
        history,
        HistoryAdvanceEvidence {
            source_items,
            target_items,
            appended_segments,
            append_elapsed_ns_total,
            all_prior_segment_payloads_shared,
            every_append_created_new_history_handle,
        },
    ))
}

fn emit_history_advance(
    source: &CanonicalHistory,
    target: &CanonicalHistory,
    evidence: &HistoryAdvanceEvidence,
) {
    let append_observed = evidence.appended_segments > 0;
    let source_is_exact_ancestor = target
        .append_delta_since(source)
        .is_some_and(|delta| delta.len() == evidence.appended_segments);
    assert!(source_is_exact_ancestor);
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "history-advance",
            "fixture": FIXTURE,
            "operation": "CanonicalHistory::append over prebuilt complete segments",
            "source_items": evidence.source_items,
            "target_items": evidence.target_items,
            "appended_segments": evidence.appended_segments,
            "append_elapsed_ns_total": evidence.append_elapsed_ns_total,
            "source_identity": source.identity(),
            "target_identity": target.identity(),
            "source_is_exact_persistent_ancestor": source_is_exact_ancestor,
            "append_observed": append_observed,
            "all_prior_segment_payloads_shared": append_observed.then_some(evidence.all_prior_segment_payloads_shared),
            "every_append_created_new_history_handle": append_observed.then_some(evidence.every_append_created_new_history_handle),
            "timing_excludes": "item and segment construction, serialization, and all request work",
        })
    );
}

fn measure_append_samples(history: &CanonicalHistory, history_items: usize) -> BenchResult<()> {
    let segment = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![
            text_item(
                0xF0_000 + history_items as u128 * 2,
                Actor::User,
                "append-probe-user".to_owned(),
            )?,
            text_item(
                0xF0_001 + history_items as u128 * 2,
                Actor::Assistant,
                "append-probe-assistant".to_owned(),
            )?,
        ],
    )?;
    for sample in 0..SAMPLES {
        let candidate = segment.clone();
        let started = Instant::now();
        let successor = history.append(candidate)?;
        let elapsed_ns = elapsed_ns(started);
        assert_eq!(successor.segments().len(), history.segments().len() + 1);
        assert!(!successor.shares_storage_with(history));
        let prior_payloads_shared = history
            .segments()
            .iter()
            .zip(successor.segments())
            .all(|(left, right)| left.shares_storage_with(right));
        assert!(prior_payloads_shared);
        assert_eq!(
            successor
                .append_delta_since(history)
                .expect("exact append ancestor")
                .len(),
            1
        );
        println!(
            "{}",
            json!({
                "schema": SCHEMA,
                "kind": "history-append-sample",
                "fixture": FIXTURE,
                "operation": "CanonicalHistory::append of one prebuilt complete segment to the same persistent source",
                "history_items_before": history_items,
                "history_segments_before": history.segments().len(),
                "sample": sample,
                "thermal_classification": "uncontrolled-not-claimed",
                "elapsed_ns": elapsed_ns,
                "source_is_exact_ancestor": true,
                "new_history_handle": true,
                "all_prior_segment_payload_pairs_shared": (!history.segments().is_empty()).then_some(prior_payloads_shared),
                "prior_segment_payload_pairs_observed": history.segments().len(),
                "timing_excludes": "fixture item/segment construction and candidate segment-handle clone",
            })
        );
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one sample keeps related timings and their structural assertions together"
)]
fn measure_sample(
    fixture: &StableFixture,
    history: &CanonicalHistory,
    working_segment: &HistorySegment,
    history_items: usize,
    sample: usize,
) -> BenchResult<()> {
    let history_clone_started = Instant::now();
    let history_clone = history.clone();
    let history_handle_clone_elapsed_ns = elapsed_ns(history_clone_started);
    assert!(history_clone.shares_storage_with(history));
    black_box(history_clone.identity());

    let projection_started = Instant::now();
    let working = WorkingProjection::new(
        vec![working_segment.clone()],
        WorkingProjectionLimits::new(4, 32, 1024 * 1024)?,
    )?;
    let projection = ConversationView::from_shared(history.clone(), working);
    let projection_construction_elapsed_ns = elapsed_ns(projection_started);
    assert!(projection.shares_history_with(history));
    black_box(projection.identity());

    let from_parts_started = Instant::now();
    let request =
        ModelRequest::from_parts(fixture.prefix.clone(), projection.clone(), fixture.metadata)?;
    let from_parts_elapsed_ns = elapsed_ns(from_parts_started);
    black_box(request.identity());
    assert!(request.stable_prefix().shares_storage_with(&fixture.prefix));
    assert!(request.input().shares_history_with(history));

    let request_clone_started = Instant::now();
    let request_clone = request.clone();
    let request_handle_clone_elapsed_ns = elapsed_ns(request_clone_started);
    assert!(
        request_clone
            .stable_prefix()
            .shares_storage_with(request.stable_prefix())
    );
    assert!(
        request_clone
            .input()
            .shares_history_with(request.input().history())
    );
    assert_eq!(request_clone.identity(), request.identity());
    black_box(request_clone.identity());

    let for_round_started = Instant::now();
    let round = request.for_round(fixture.next_round_metadata)?;
    let for_round_elapsed_ns = elapsed_ns(for_round_started);
    black_box(round.identity());
    assert!(
        request
            .stable_prefix()
            .shares_storage_with(round.stable_prefix())
    );
    assert!(round.input().shares_history_with(request.input().history()));
    assert_eq!(request.model_input_identity(), round.model_input_identity());
    assert_ne!(request.identity(), round.identity());
    assert_ne!(request.canonical_bytes(), round.canonical_bytes());

    let full_rebuild_started = Instant::now();
    let rebuilt = fixture.build_request_full(projection)?;
    let full_rebuild_elapsed_ns = elapsed_ns(full_rebuild_started);
    black_box(rebuilt.identity());
    assert_eq!(request.identity(), rebuilt.identity());
    assert_eq!(request.canonical_bytes(), rebuilt.canonical_bytes());
    assert_eq!(
        request.stable_prefix().identity(),
        rebuilt.stable_prefix().identity()
    );
    assert_eq!(
        request.stable_prefix().canonical_bytes(),
        rebuilt.stable_prefix().canonical_bytes()
    );
    assert!(
        !request
            .stable_prefix()
            .shares_storage_with(rebuilt.stable_prefix())
    );
    assert!(rebuilt.input().shares_history_with(history));

    let prepared = fixture.synthetic_prepared_evidence(&request)?;
    let history_serialized_bytes = serde_json::to_vec(history)?.len();
    let request_serialize_started = Instant::now();
    let request_serialized_bytes = serde_json::to_vec(&request)?.len();
    let request_serialize_elapsed_ns = elapsed_ns(request_serialize_started);
    let round_serialize_started = Instant::now();
    let round_serialized_bytes = serde_json::to_vec(&round)?.len();
    let round_serialize_elapsed_ns = elapsed_ns(round_serialize_started);
    println!(
        "{}",
        json!({
            "schema": SCHEMA,
            "kind": "sample",
            "fixture": FIXTURE,
            "thermal_classification": "uncontrolled-not-claimed",
            "process_sample_order": if sample == 0 { "first-for-history-point" } else { "repeat-for-history-point" },
            "sample": sample,
            "history_items": history_items,
            "history_segments": history.segments().len(),
            "history_identity": history.identity(),
            "history_serialized_bytes": history_serialized_bytes,
            "history_retained_bytes_upper_bound": history.retained_bytes_upper_bound(),
            "working_items": request.input().working().item_count(),
            "working_retained_bytes_upper_bound": request.input().working().retained_bytes_upper_bound(),
            "projection_identity": request.input().identity(),
            "projection_retained_bytes_upper_bound": request.input().retained_bytes_upper_bound(),
            "projection_construction_elapsed_ns": projection_construction_elapsed_ns,
            "from_parts": {
                "operation": "shared prefix/projection handles plus ModelRequest::from_parts",
                "elapsed_ns": from_parts_elapsed_ns,
                "request_identity": request.identity(),
                "model_input_identity": request.model_input_identity(),
                "request_canonical_bytes": request.canonical_bytes().len(),
                "serialized_request_bytes": request_serialized_bytes,
                "retained_bytes_upper_bound": request.retained_bytes_upper_bound(),
                "exact_prefix_allocation_shared": request.stable_prefix().shares_storage_with(&fixture.prefix),
                "exact_history_allocation_shared": request.input().shares_history_with(history),
            },
            "for_round": {
                "operation": "ModelRequest::for_round with replacement attempt metadata",
                "elapsed_ns": for_round_elapsed_ns,
                "request_identity": round.identity(),
                "model_input_identity_equal": request.model_input_identity() == round.model_input_identity(),
                "request_identity_changed": request.identity() != round.identity(),
                "request_canonical_bytes_changed": request.canonical_bytes() != round.canonical_bytes(),
                "exact_prefix_allocation_shared": request.stable_prefix().shares_storage_with(round.stable_prefix()),
                "exact_history_allocation_shared": round.input().shares_history_with(request.input().history()),
            },
            "full_rebuild": {
                "operation": "ModelRequest builder reconstructing and validating the complete prefix and request in the same process",
                "elapsed_ns": full_rebuild_elapsed_ns,
                "request_identity_equal": request.identity() == rebuilt.identity(),
                "request_canonical_bytes_equal": request.canonical_bytes() == rebuilt.canonical_bytes(),
                "stable_prefix_identity_equal": request.stable_prefix().identity() == rebuilt.stable_prefix().identity(),
                "stable_prefix_canonical_bytes_equal": request.stable_prefix().canonical_bytes() == rebuilt.stable_prefix().canonical_bytes(),
                "stable_prefix_allocation_shared": request.stable_prefix().shares_storage_with(rebuilt.stable_prefix()),
                "exact_history_allocation_shared": rebuilt.input().shares_history_with(history),
            },
            "stable_prefix": {
                "identity": request.stable_prefix().identity(),
                "canonical_bytes": request.stable_prefix().canonical_bytes().len(),
                "retained_bytes_upper_bound": request.stable_prefix().retained_bytes_upper_bound(),
            },
            "separate_reserialization_trace": {
                "scope": "executed after all primary request construction and preparation timings",
                "request_serialized_bytes": request_serialized_bytes,
                "request_serialize_elapsed_ns": request_serialize_elapsed_ns,
                "next_round_serialized_bytes": round_serialized_bytes,
                "next_round_serialize_elapsed_ns": round_serialize_elapsed_ns,
                "exact_bytes_written_total": request_serialized_bytes.saturating_add(round_serialized_bytes),
                "stable_prefix_canonical_bytes": request.stable_prefix().canonical_bytes().len(),
                "stable_prefix_identity": request.stable_prefix().identity(),
                "cache_affinity_identity": fixture.cache_affinity.identity(),
                "claim_scope": "exact serde output lengths and elapsed work; not allocator bytes or provider HTTP framing",
            },
            "synthetic_prepared_context": prepared.fields,
            "synthetic_prepare_and_context_admission_elapsed_ns": prepared.elapsed_ns,
            "public_handle_clone_probes": {
                "history": {
                    "elapsed_ns": history_handle_clone_elapsed_ns,
                    "identity_equal": history_clone.identity() == history.identity(),
                    "exact_history_allocation_shared": history_clone.shares_storage_with(history),
                },
                "model_request": {
                    "elapsed_ns": request_handle_clone_elapsed_ns,
                    "identity_equal": request_clone.identity() == request.identity(),
                    "exact_prefix_allocation_shared": request_clone.stable_prefix().shares_storage_with(request.stable_prefix()),
                    "exact_history_allocation_shared": request_clone.input().shares_history_with(request.input().history()),
                },
                "scope": "explicit public Clone calls only; not an allocator-wide transient-clone trace",
            },
            "allocation_count": Value::Null,
            "allocated_bytes": Value::Null,
            "transient_full_history_clone_trace": Value::Null,
            "managed_cache_affinity_evidence": {
                "status": "measured-synthetic-runtime-affinity",
                "identity": fixture.cache_affinity.identity(),
                "prepared_projection_identity_equal": prepared.fields["cache_affinity"]["prepared_projection_identity_equal"],
                "claim_scope": "runtime-owned neutral affinity identity only; no provider cache facility, cache hit, or billed-token claim",
            },
        })
    );
    Ok(())
}

struct SyntheticPreparedEvidence {
    fields: Value,
    elapsed_ns: u64,
}

struct StableFixture {
    provider_configuration: CompiledProviderConfiguration,
    instructions: Vec<InstructionBlock>,
    tools: Vec<ToolDefinition>,
    output: OutputPolicy,
    reasoning: ReasoningPolicy,
    tool_choice: ToolChoice,
    cache: CachePolicy,
    limits: GenerationLimits,
    native: NativeOptions,
    metadata: RequestMetadata,
    next_round_metadata: RequestMetadata,
    prefix: StableRequestPrefix,
    descriptor: ResolvedModelDescriptor,
    authority: RuntimeModelAuthority,
    active_family_artifact: ActiveFamilyArtifact,
    cache_affinity: ModelCacheAffinity,
}

impl StableFixture {
    #[allow(
        clippy::too_many_lines,
        reason = "complete benchmark fixture construction"
    )]
    fn new() -> BenchResult<Self> {
        let instructions = vec![
            InstructionBlock::new(
                InstructionSource::System,
                "You are a deterministic offline projection fixture.",
            )?,
            InstructionBlock::new(
                InstructionSource::Profile,
                "Preserve tool groups, stable inputs, and recent intent.",
            )?,
        ];
        let tools = vec![
            tool("read_file", "Read one file", ToolEffect::ReadOnly)?,
            tool("write_file", "Write one file", ToolEffect::Mutating)?,
        ];
        let output = OutputPolicy::default();
        let reasoning = ReasoningPolicy::Enabled {
            effort: Some(ReasoningEffort::Medium),
            requirement: CapabilityRequirement::Preferred,
        };
        let tool_choice = ToolChoice::Auto;
        let cache = CachePolicy::Enabled {
            requirement: CapabilityRequirement::Preferred,
        };
        let limits = GenerationLimits::new(NonZeroU32::new(1_024), vec!["<stop>".to_owned()])?;
        let namespace = NamespaceId::new("perf.project")?;
        let native = NativeOptions::new(BTreeMap::from([(
            namespace.clone(),
            json!({"fixture_revision": FIXTURE_REVISION, "mode": "offline"}),
        )]));
        let descriptor = descriptor(namespace)?;
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            fixture_digest(DigestDomain::ModelBinding, b"perf-project-account")?,
            fixture_digest(DigestDomain::ModelBinding, b"perf-project-credential")?,
            1,
            timestamp(1)?,
            None,
        )?;
        let provider_configuration = provider_configuration(
            &descriptor,
            &authority,
            &tools,
            &output,
            reasoning,
            &tool_choice,
            cache,
            &limits,
        )?;
        let prefix = StableRequestPrefix::new(
            provider_configuration.selected_identity(),
            provider_configuration.effective_identity(),
            plakous_model::ProcessingTierDisposition::Standard,
            instructions.clone(),
            tools
                .iter()
                .cloned()
                .map(|definition| {
                    ConfiguredTool::new(
                        definition,
                        ToolExecutionDisposition::Local,
                        plakous_model::ToolCallerDisposition::DirectOnly,
                    )
                })
                .collect(),
            Vec::new(),
            output.clone(),
            reasoning,
            tool_choice.clone(),
            cache,
            limits.clone(),
            native.clone(),
            None,
        )?;
        let active_family_artifact = ActiveFamilyArtifact::new(
            PROTOCOL_FAMILY,
            1,
            1,
            descriptor.binding_digest()?,
            Vec::from(b"perf-project-synthetic-active-family-artifact-v1".as_slice()),
        )?;
        let cache_affinity = ModelCacheAffinity::from_runtime_entropy(
            SessionId::try_from(Uuid::from_u128(0x104))?,
            BranchId::try_from(Uuid::from_u128(0x105))?,
            7,
            [0x5A; 32],
        )?;
        Ok(Self {
            provider_configuration,
            instructions,
            tools,
            output,
            reasoning,
            tool_choice,
            cache,
            limits,
            native,
            metadata: request_metadata(0x102, 2)?,
            next_round_metadata: request_metadata(0x103, 3)?,
            prefix,
            descriptor,
            authority,
            active_family_artifact,
            cache_affinity,
        })
    }

    fn build_request_full(&self, input: ConversationView) -> BenchResult<ModelRequest> {
        Ok(ModelRequest::builder(
            self.provider_configuration.selected_identity(),
            self.provider_configuration.effective_identity(),
            input,
            self.metadata,
        )
        .instructions(self.instructions.clone())
        .tools(
            self.tools
                .iter()
                .cloned()
                .map(|definition| {
                    ConfiguredTool::new(
                        definition,
                        ToolExecutionDisposition::Local,
                        plakous_model::ToolCallerDisposition::DirectOnly,
                    )
                })
                .collect(),
        )
        .output(self.output.clone())
        .reasoning(self.reasoning)
        .tool_choice(self.tool_choice.clone())
        .cache(self.cache)
        .limits(self.limits.clone())
        .native(self.native.clone())
        .build()?)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "all synthetic exact-scope evidence is asserted and emitted together"
    )]
    fn synthetic_prepared_evidence(
        &self,
        request: &ModelRequest,
    ) -> BenchResult<SyntheticPreparedEvidence> {
        let started = Instant::now();
        let plan = self
            .descriptor
            .validate_request(request.clone(), ValidationMode::Strict)?;
        let artifact_clone = self.active_family_artifact.clone();
        assert!(std::ptr::eq(
            self.active_family_artifact.bytes(),
            artifact_clone.bytes()
        ));
        let context = ModelPreparationContext::new(self.authority.clone(), timestamp(2)?)
            .with_active_family_artifact(artifact_clone)
            .with_cache_affinity(self.cache_affinity.clone());
        let artifact_bytes_shared = std::ptr::eq(
            self.active_family_artifact.bytes(),
            context
                .active_family_artifact()
                .expect("active artifact")
                .bytes(),
        );
        assert!(artifact_bytes_shared);
        let max_output_tokens = request
            .limits()
            .max_output_tokens()
            .ok_or("synthetic wire requires an enforced output maximum")?;
        let wire = serde_json::to_vec(&(
            "plakous/perf-project-synthetic-complete-wire-v2",
            request,
            ("max_output_tokens", max_output_tokens.get()),
            ("cache_affinity", self.cache_affinity.identity()),
        ))?;
        let wire_size = u64::try_from(wire.len())?;
        let wire_digest = Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"plakous/perf-project-synthetic-wire-v2", &wire],
        )?;
        let observation = CountObservation::new(
            MEASUREMENT_CERTIFICATE_ID,
            MEASUREMENT_CERTIFICATE_REVISION,
            wire_size,
        )?;
        let token_projection =
            PreparedTokenProjection::new(wire_digest, wire_size, Some(observation))?;
        let output_limit = PreparedOutputLimit::enforced_wire_maximum(
            max_output_tokens,
            "max_output_tokens",
            wire_digest,
            LOWERING_REVISION,
        )?;
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            ProtocolId::new(PROTOCOL_FAMILY)?,
            LOWERING_REVISION,
            PROJECTION_REVISION,
            ACCOUNTING_REVISION,
            wire_digest,
            wire_size,
            token_projection,
            PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity {
                artifact: self.active_family_artifact.integrity(),
                affinity: self.cache_affinity.identity(),
            },
            Some(output_limit),
        )?;
        projection.validate_for_plan(&self.descriptor, &plan)?;
        assert_eq!(
            projection.active_artifact(),
            Some(self.active_family_artifact.integrity())
        );
        assert_eq!(
            projection.cache_use(),
            PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity {
                artifact: self.active_family_artifact.integrity(),
                affinity: self.cache_affinity.identity(),
            }
        );
        let status = self
            .descriptor
            .context_status(&plan, &projection, context.prepared_at())?;
        let ContextStatusKind::Fits { headroom } = status.kind() else {
            return Err("synthetic exact prepared projection did not fit".into());
        };
        let admission = status.admit(plan.request())?;
        let scope = status.scope().ok_or("missing synthetic context scope")?;
        let capacity = status.capacity().ok_or("missing synthetic capacity")?;
        let measurement = status
            .measurement()
            .ok_or("missing synthetic measurement")?;
        let reservation = status
            .reservation()
            .ok_or("missing synthetic output reservation")?;
        assert_eq!(scope.protocol_family().as_str(), PROTOCOL_FAMILY);
        assert_eq!(scope.lowering_revision(), LOWERING_REVISION);
        assert_eq!(scope.projection_revision(), PROJECTION_REVISION);
        assert_eq!(scope.accounting_revision(), ACCOUNTING_REVISION);
        assert_eq!(scope.model_input_identity(), request.model_input_identity());
        assert_eq!(
            projection.descriptor_binding(),
            self.authority.descriptor_binding()
        );
        assert_eq!(projection.model_input(), request.model_input_identity());
        assert_eq!(
            scope.prepared_token_projection(),
            projection.token_projection().identity()
        );
        assert_eq!(capacity.evidence_id(), CAPACITY_EVIDENCE_ID);
        assert_eq!(capacity.evidence_revision(), CAPACITY_EVIDENCE_REVISION);
        assert_eq!(measurement.certificate_id(), MEASUREMENT_CERTIFICATE_ID);
        assert_eq!(
            measurement.certificate_revision(),
            MEASUREMENT_CERTIFICATE_REVISION
        );
        assert_eq!(measurement.bound().upper_bound(), wire_size);
        assert!(measurement.bound().is_exact());
        assert_eq!(reservation.tokens(), u64::from(max_output_tokens.get()));
        assert_eq!(reservation.revision(), RESERVATION_REVISION);
        assert_eq!(
            reservation.prepared_output_limit(),
            projection.output_limit().expect("output limit").identity()
        );
        let record = PreparedCallRecord::for_prepared(
            &self.descriptor,
            &plan,
            projection.clone(),
            &context,
        )?;
        record.revalidate(&self.descriptor, &plan, context.prepared_at())?;
        assert_eq!(record.context_admission_identity(), admission.identity());
        assert_eq!(record.context_status_identity(), status.identity());
        let elapsed_ns = elapsed_ns(started);
        Ok(SyntheticPreparedEvidence {
            elapsed_ns,
            fields: json!({
                "claim_scope": "synthetic complete-wire context algebra only; not provider evidence",
                "complete_wire_digest": wire_digest,
                "complete_wire_bytes": wire_size,
                "synthetic_exact_token_unit": "one complete synthetic wire byte",
                "synthetic_exact_input_tokens": measurement.bound().upper_bound(),
                "prepared_projection_identity": projection.identity(),
                "descriptor_binding": projection.descriptor_binding(),
                "descriptor_binding_matches_authority": projection.descriptor_binding() == self.authority.descriptor_binding(),
                "prepared_token_projection_identity": projection.token_projection().identity(),
                "prepared_model_input_identity": projection.model_input(),
                "request_model_input_identity_equal": projection.model_input() == request.model_input_identity(),
                "protocol_family": projection.protocol_family(),
                "dialect": projection.dialect(),
                "route": projection.route(),
                "requested_model": projection.requested_model(),
                "effective_model": projection.effective_model(),
                "adapter_profile": projection.adapter(),
                "execution_authority_identity": projection.authority(),
                "authority_class_identity": projection.authority_class(),
                "account_binding": self.authority.account_binding(),
                "credential_authority_binding": self.authority.credential_authority_binding(),
                "credential_generation": self.authority.credential_generation(),
                "lowering_revision": projection.lowering_revision(),
                "projection_revision": projection.projection_revision(),
                "accounting_revision": projection.accounting_revision(),
                "context_scope_identity": scope.identity(),
                "context_contract_identity": scope.context_contract_identity(),
                "capacity": {
                    "identity": capacity.identity(),
                    "rule": capacity.rule(),
                    "evidence_id": capacity.evidence_id(),
                    "evidence_revision": capacity.evidence_revision(),
                    "evidence_source": capacity.evidence_source(),
                    "supplementary_fixture_evidence_corpus_revision": CAPACITY_EVIDENCE_CORPUS_REVISION,
                    "supplementary_fixture_evidence_corpus_identity": capacity_evidence_corpus_identity()?,
                    "corpus_binding_scope": "benchmark provenance only; not encoded by this BuiltInCatalog capacity-certificate API",
                },
                "measurement": {
                    "identity": measurement.identity(),
                    "certificate_identity": measurement.certificate_identity(),
                    "certificate_id": measurement.certificate_id(),
                    "certificate_revision": measurement.certificate_revision(),
                    "method": measurement.method(),
                    "method_revision": measurement.method_revision(),
                    "bound": measurement.bound(),
                },
                "output_reservation": {
                    "identity": reservation.identity(),
                    "tokens": reservation.tokens(),
                    "revision": reservation.revision(),
                    "prepared_output_limit_identity": reservation.prepared_output_limit(),
                },
                "context_status": {
                    "identity": status.identity(),
                    "kind": status.kind(),
                    "headroom": headroom,
                },
                "context_admission_identity": admission.identity(),
                "prepared_call_record": {
                    "identity": record.identity(),
                    "semantic_revision": record.semantic_revision(),
                    "context_admission_identity": record.context_admission_identity(),
                    "context_status_identity": record.context_status_identity(),
                },
                "cache_use": projection.cache_use(),
                "cache_affinity": {
                    "identity": self.cache_affinity.identity(),
                    "session": self.cache_affinity.session(),
                    "branch": self.cache_affinity.branch(),
                    "epoch": self.cache_affinity.epoch(),
                    "prepared_projection_identity_equal": projection.cache_affinity() == Some(self.cache_affinity.identity()),
                    "claim_scope": "exact synthetic runtime-owned branch affinity identity carried through the neutral prepared projection; not a provider cache-hit claim",
                },
                "active_family_artifact": {
                    "status": "measured-synthetic-active-artifact",
                    "family": self.active_family_artifact.family(),
                    "schema_revision": self.active_family_artifact.schema_revision(),
                    "codec_revision": self.active_family_artifact.codec_revision(),
                    "binding": self.active_family_artifact.binding(),
                    "integrity": self.active_family_artifact.integrity(),
                    "opaque_bytes": self.active_family_artifact.bytes().len(),
                    "serialized_envelope_bytes": serde_json::to_vec(&self.active_family_artifact)?.len(),
                    "retained_bytes": Value::Null,
                    "retained_bytes_status": "not-measured: ActiveFamilyArtifact exposes exact opaque bytes but no retained-size bound, and this benchmark has no allocator probe",
                    "clone_shared_exact_opaque_byte_allocation": artifact_bytes_shared,
                    "request_path": "ModelPreparationContext::with_active_family_artifact plus PreparedCacheUse::ActiveFamilyArtifact",
                    "claim_scope": "neutral synthetic artifact envelope only; not a real Responses artifact codec/reuse proof",
                },
            }),
        })
    }
}

fn descriptor(namespace: NamespaceId) -> BenchResult<ResolvedModelDescriptor> {
    let standard = StandardIntegrationAuthority::release_owned();
    let observed_at = timestamp(1)?;
    let provenance = FactProvenance::new(FactSource::TestFixture, observed_at, None);
    let native = FeatureSupport::new(
        Support::Native,
        CapabilityConstraints::default(),
        provenance,
    );
    let features = [
        Capability::Streaming,
        Capability::Reasoning,
        Capability::Tools,
        Capability::Continuation,
        Capability::Caching,
    ]
    .into_iter()
    .map(|capability| (capability, native.clone()))
    .collect();
    let freshness = EvidenceFreshness::new(observed_at, None)?;
    let contract = ContextContract::new(
        ProtocolId::new(PROTOCOL_FAMILY)?,
        ACCOUNTING_REVISION,
        PROJECTION_REVISION,
        RESERVATION_REVISION,
        CapacityCertificate::new(
            &standard,
            CAPACITY_EVIDENCE_ID,
            CAPACITY_EVIDENCE_REVISION,
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(8 * 1024 * 1024),
            },
            None,
            freshness,
        )?,
        vec![MeasurementCertificate::standard(
            &standard,
            MEASUREMENT_CERTIFICATE_ID,
            MEASUREMENT_CERTIFICATE_REVISION,
            "perf-synthetic-complete-wire-byte-counter",
            ACCOUNTING_REVISION,
            ContextEvidenceSource::BuiltInCatalog,
            CountCertificateKind::ExactTokenizer,
            None,
            freshness,
        )?],
    )?;
    Ok(ResolvedModelDescriptor::new(
        &standard,
        ProtocolId::new("perf.synthetic-dialect")?,
        RouteId::new("perf.synthetic-route")?,
        ModelId::new("perf.synthetic-model")?,
        Some(ModelId::new("perf.synthetic-effective-model")?),
        Some(ModelFamilyId::new(PROTOCOL_FAMILY)?),
        ModelAdapterProfileId::new("perf.synthetic-adapter")?,
        CapabilitySet::new(features, BTreeMap::from([(namespace, native)])),
    )
    .with_context_contract(contract))
}

#[allow(
    clippy::too_many_arguments,
    reason = "the benchmark compiles the exact behavior-bearing fixture request"
)]
fn provider_configuration(
    descriptor: &ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
    tools: &[ToolDefinition],
    output: &OutputPolicy,
    reasoning: ReasoningPolicy,
    tool_choice: &ToolChoice,
    cache: CachePolicy,
    limits: &GenerationLimits,
) -> BenchResult<CompiledProviderConfiguration> {
    let standard = StandardIntegrationAuthority::release_owned();
    let behavior = ProviderBehaviorConfiguration {
        tool_mode: plakous_model::ToolPresentationMode::Direct,
        reasoning_effort: match reasoning {
            ReasoningPolicy::Disabled => ReasoningEffortDisposition::Disabled,
            ReasoningPolicy::Enabled { effort, .. } => ReasoningEffortDisposition::Enabled(
                effort.ok_or("perf-project reasoning requires an exact effort")?,
            ),
            _ => return Err("unsupported perf-project reasoning policy".into()),
        },
        reasoning_summary: ReasoningSummaryDisposition::Omit,
        reasoning_include: ReasoningIncludeDisposition::Omit,
        replay: ReplayDisposition::Disabled,
        cache,
        processing_tier: plakous_model::ProcessingTierDisposition::Standard,
        tools: tools
            .iter()
            .cloned()
            .map(|definition| {
                ConfiguredTool::new(
                    definition,
                    ToolExecutionDisposition::Local,
                    plakous_model::ToolCallerDisposition::DirectOnly,
                )
            })
            .collect(),
        hosted_tools: Vec::new(),
        tool_choice: tool_choice.clone(),
        parallel_tools: ParallelToolDisposition::Disabled,
        output: output.clone(),
        output_bound: OutputBoundDisposition::Enforced(
            limits
                .max_output_tokens()
                .ok_or("perf-project requires an output bound")?,
        ),
        storage: ProviderStorageDisposition::OmittedRouteContract,
        retention: RetentionDisposition::RouteContract {
            profile: descriptor.profile().clone(),
            revision: ProviderProfileRevisionId::new("perf-project-retention-v1")?,
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
            ProviderProfileRevisionId::new("perf-project-route-v1")?,
            ModelAdapterProfileId::new("perf-project.auth")?,
            ProviderProfileRevisionId::new("perf-project-auth-v1")?,
            ProviderAuthorityClassId::new("perf-project-synthetic")?,
            CredentialSelectionId::new("perf-project-credential")?,
            descriptor.requested_model().clone(),
        ),
        behavior.clone(),
    )?;
    let stable_authority = ResolvedProviderAuthorityBinding::new(
        descriptor,
        authority,
        ProviderSemanticEvidence::new(1, b"perf-project-provider-semantics-v1".to_vec())?,
    )?;
    let family_policy = FamilyPolicyEnvelope::new(
        descriptor
            .model_family()
            .cloned()
            .ok_or("perf-project descriptor requires a model family")?,
        1,
        1,
        1,
        b"perf-project-family-policy-v1".to_vec(),
    )?;
    let effective = EffectiveProviderConfiguration::compile(
        &selected,
        descriptor.clone(),
        ModelImplementationIdentity::release_owned(&standard, descriptor, 1)?,
        stable_authority,
        behavior,
        family_policy,
    )?;
    Ok(CompiledProviderConfiguration::new(selected, effective)?)
}

fn tool(name: &str, description: &str, effect: ToolEffect) -> BenchResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new(name)?,
        "1.0.0",
        description,
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        effect,
    )?)
}

fn generated_tool_definition(properties: usize) -> BenchResult<ToolDefinition> {
    let mut schema_properties = serde_json::Map::new();
    let mut required = Vec::new();
    if properties <= 64 {
        for index in 0..properties {
            let name = format!("field_{index:04}");
            schema_properties.insert(name.clone(), json!({"type": "integer"}));
            required.push(Value::String(name));
        }
    } else {
        let groups = 4;
        let leaves = properties - groups;
        for group in 0..groups {
            let group_name = format!("group_{group:04}");
            let start = group * leaves / groups;
            let end = (group + 1) * leaves / groups;
            let mut nested_properties = serde_json::Map::new();
            let mut nested_required = Vec::with_capacity(end - start);
            for index in start..end {
                let name = format!("field_{index:04}");
                nested_properties.insert(name.clone(), json!({"type": "integer"}));
                nested_required.push(Value::String(name));
            }
            schema_properties.insert(
                group_name.clone(),
                json!({
                    "type": "object",
                    "properties": nested_properties,
                    "required": nested_required,
                    "additionalProperties": false
                }),
            );
            required.push(Value::String(group_name));
        }
    }
    Ok(ToolDefinition::new(
        ToolName::new(format!("generated_schema_{properties}"))?,
        "1.0.0",
        "Generated deterministic schema compilation fixture",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": schema_properties,
            "required": required,
            "additionalProperties": false,
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?)
}

fn generated_tool_arguments(properties: usize) -> BenchResult<String> {
    let mut arguments = serde_json::Map::new();
    if properties <= 64 {
        for index in 0..properties {
            arguments.insert(format!("field_{index:04}"), Value::from(index));
        }
    } else {
        let groups = 4;
        let leaves = properties - groups;
        for group in 0..groups {
            let start = group * leaves / groups;
            let end = (group + 1) * leaves / groups;
            let mut nested = serde_json::Map::new();
            for index in start..end {
                nested.insert(format!("field_{index:04}"), Value::from(index));
            }
            arguments.insert(format!("group_{group:04}"), Value::Object(nested));
        }
    }
    Ok(serde_json::to_string(&arguments)?)
}

fn measure_tool_schema_trace() -> BenchResult<Value> {
    const PROPERTY_SWEEP: &[usize] = &[1, 16, 64, 256];
    const VALIDATION_SAMPLES: usize = 9;
    let mut points = Vec::with_capacity(PROPERTY_SWEEP.len());
    for &properties in PROPERTY_SWEEP {
        let construction_started = Instant::now();
        let definition = generated_tool_definition(properties)?;
        let construction_elapsed_ns = elapsed_ns(construction_started);
        let ToolInputContract::Structured(schema) = definition.input() else {
            return Err("generated benchmark tool must have structured input".into());
        };
        let schema_bytes = serde_json::to_vec(schema.document())?.len();
        let compile_started = Instant::now();
        let validator = ValidatedToolSchema::compile(
            &definition,
            SchemaLimits::default(),
            ArgumentLimits::default(),
        )?;
        let compile_elapsed_ns = elapsed_ns(compile_started);
        let arguments = generated_tool_arguments(properties)?;
        let mut validation_elapsed_ns = Vec::with_capacity(VALIDATION_SAMPLES);
        for _ in 0..VALIDATION_SAMPLES {
            let started = Instant::now();
            black_box(validator.validate_arguments(&arguments)?);
            validation_elapsed_ns.push(elapsed_ns(started));
        }
        let mut ordered_validation = validation_elapsed_ns.clone();
        ordered_validation.sort_unstable();
        let median_validation_ns = ordered_validation[ordered_validation.len() / 2];
        let p95_validation_ns = ordered_validation[(ordered_validation.len() * 95)
            .div_ceil(100)
            .saturating_sub(1)];
        let invalid_arguments = if properties <= 64 {
            "{}".to_owned()
        } else {
            r#"{"unexpected":true}"#.to_owned()
        };
        let rejection_started = Instant::now();
        if validator.validate_arguments(&invalid_arguments).is_ok() {
            return Err("adversarial tool arguments unexpectedly validated".into());
        }
        let rejection_elapsed_ns = elapsed_ns(rejection_started);
        points.push(json!({
            "properties": properties,
            "maximum_legal_v1_property_graph": properties == 256,
            "schema_serialized_bytes": schema_bytes,
            "valid_arguments_bytes": arguments.len(),
            "semantic_construction_elapsed_ns": construction_elapsed_ns,
            "compile_elapsed_ns": compile_elapsed_ns,
            "validation_elapsed_ns": validation_elapsed_ns,
            "validation_median_ns": median_validation_ns,
            "validation_p95_ns": p95_validation_ns,
            "adversarial_rejection_elapsed_ns": rejection_elapsed_ns,
            "validation_samples": VALIDATION_SAMPLES,
            "schema_digest": validator.schema_digest(),
        }));
    }
    Ok(json!({
        "fixture": "portable-draft-2020-12-flat-required-properties-v1",
        "property_sweep": PROPERTY_SWEEP,
        "timing_position": "after all primary projection/request and family-artifact timings",
        "construction_scope": "ToolDefinition construction including SemanticJsonV1 canonicalization, PortableToolSchemaV1 validation, and semantic identities",
        "compile_scope": "ValidatedToolSchema::compile over an already frozen PortableToolSchemaV1",
        "validation_scope": "ValidatedToolSchema::validate_arguments including strict parse, bounded local validation, canonicalization, and digest",
        "allocation_scope": "not measured; process-isolated retained/peak memory belongs to the perf allocation harness",
        "points": points,
    }))
}

fn working_segment() -> BenchResult<HistorySegment> {
    Ok(HistorySegment::new(
        HistorySegmentKind::RecentIntent,
        vec![text_item(
            u128::MAX - 1,
            Actor::User,
            "measured working intent".to_owned(),
        )?],
    )?)
}

fn text_item(identity: u128, actor: Actor, text: String) -> BenchResult<ConversationItem> {
    let provenance = if actor == Actor::User {
        Provenance::UserInput
    } else {
        Provenance::ModelOutput
    };
    Ok(ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(identity))?,
        actor,
        vec![ContentBlock::Text(TextBlock::new(
            text,
            (actor == Actor::Assistant).then_some(TextPhase::Final),
        )?)],
        provenance,
    )?)
}

fn request_metadata(attempt: u128, second: u64) -> BenchResult<RequestMetadata> {
    Ok(RequestMetadata::for_turn(
        RunId::try_from(Uuid::from_u128(0x100))?,
        TurnId::try_from(Uuid::from_u128(0x101))?,
        AttemptId::try_from(Uuid::from_u128(attempt))?,
        Timestamp::new(second, 0)?,
    ))
}

fn timestamp(second: u64) -> BenchResult<Timestamp> {
    Ok(Timestamp::new(second, 0)?)
}

fn fixture_digest(domain: DigestDomain, bytes: &[u8]) -> BenchResult<Digest> {
    Ok(Digest::sha256(domain, 1, &[bytes])?)
}

fn capacity_evidence_corpus_identity() -> BenchResult<Digest> {
    Ok(Digest::sha256(
        DigestDomain::TokenMeasurement,
        1,
        &[
            b"plakous/perf-project-synthetic-capacity-corpus-v1",
            PROTOCOL_FAMILY.as_bytes(),
            CAPACITY_EVIDENCE_REVISION.as_bytes(),
            b"shared exact eight-mebibyte synthetic window",
            b"one complete credential-free synthetic wire byte equals one synthetic token",
        ],
    )?)
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
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
