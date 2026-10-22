//! Observation-parity oracle for one-shot, JSONL and TUI hosted sessions.
//!
//! For a fixed set of scripted, fully offline scenarios the three drivers of
//! the one hosted session
//!
//!   1. `plak run` one-shot (`plakous_hosted::runtime::OneShot::run`),
//!   2. the host loop as `plak agent` drives it (`agent_stdio` + the v1 JSONL
//!      projection in `plakous_host_protocol::jsonl`), and
//!   3. the host loop as the TUI drives it (`SessionIntent` in,
//!      `SessionObservation` out, nothing rendered)
//!
//! must produce the same observation sequence once normalized. The recorded
//! sequence for driver 3 is the golden; drivers 1 and 2 are compared against
//! the golden through an explicit, named per-driver projection (see
//! [`jsonl_projection`] and [`oneshot_projection`]) rather than through a
//! loosened comparison, so a driver that stops emitting something fails here
//! instead of silently diverging.
//!
//! One thing the oracle had to learn from the code rather than the spec: the
//! observation mailbox supersedes a latest-state observation that is still
//! unread (`plakous_host_protocol::channel::latest_state_event`), so the *count*
//! of snapshot observations a driver sees is a function of how fast it drains
//! and is not a contract. [`coalesce`] compares queue/busy final state and
//! coalesces other snapshots within snapshot runs; [`transient_workflow_status`]
//! drops intra-workflow progress statuses. Remaining observations are exact.
//!
//! Goldens live in `bin/plak/tests/parity/<scenario>.jsonl`.
//! `PLAK_UPDATE_GOLDENS=1 cargo test -p plak --lib observation_parity`
//! rewrites them.
//!
//! `plakous-hosted` owns OneShot and the host worker. The binary owns CLI
//! presentation and JSONL transport, while the host-protocol crate owns the
//! JSONL projection. This binary-internal suite can exercise all three faces
//! against the same product-store fixture.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "parity oracle test module"
)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous::agent::ReasoningEffort;
use plakous::subscriptions::NativeProvider;
use plakous_host_protocol::envelope::JsonlCorrelation;
use plakous_host_protocol::jsonl::{host_event_kind, project_host_event};
use plakous_host_protocol::observation::{TurnLifecycle, TurnPhase};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{
    BoundaryController, compaction_runtime_binding, failing_effect_runtime_binding,
    overflow_runtime_binding, placeholder_runtime_binding, runtime_binding,
};

use crate::home::paths::PlakPaths;
use crate::store::sessions::SessionRecord;
use crate::surface_protocol::{HostEvent, UserAction};
use plakous_hosted::runtime::{
    HostConfig, HostTargetAuthority, ReadyPathTiming, ScriptedRuntimeFactory, spawn_host_worker,
};

/// How long one scripted step may take before the oracle calls it a hang.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Normalized observation records
// ---------------------------------------------------------------------------

/// One normalized observation: its stable protocol kind label plus its
/// complete, scrubbed payload.
///
/// The payload is the observation's `Debug` form after [`scrub`] masks the
/// identities, timestamps, durations, token counts and temporary paths the
/// task treats as incidental. Every other field is compared byte for byte, so
/// a field that changes meaning fails the golden.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Record {
    kind: String,
    payload: String,
}

impl Record {
    fn from_event(event: &HostEvent, root: &Path) -> Self {
        let mut event = event.clone();
        if let HostEvent::Usage(usage) = &mut event
            && let Some(breakdown) = &mut usage.breakdown
        {
            breakdown
                .categories
                .retain(|row| row.label != "Live turn growth (est.)");
            // Only live refreshes carry this replaceable request inventory.
            breakdown.informational.retain(|row| {
                !matches!(
                    row.label.as_str(),
                    "Coding tools on the last request" | "Agent tools on the last request"
                )
            });
        }
        Self {
            kind: host_event_kind(&event).to_owned(),
            payload: scrub(&format!("{event:?}"), root),
        }
    }

    fn synthetic(kind: &str, payload: String) -> Self {
        Self {
            kind: kind.to_owned(),
            payload,
        }
    }

    fn line(&self) -> String {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "kind".to_owned(),
            serde_json::Value::from(self.kind.clone()),
        );
        fields.insert(
            "payload".to_owned(),
            serde_json::Value::from(self.payload.clone()),
        );
        serde_json::to_string(&serde_json::Value::Object(fields)).unwrap()
    }

    fn parse(line: &str) -> Self {
        let value: serde_json::Value = serde_json::from_str(line).expect("golden line is JSON");
        Self {
            kind: value["kind"].as_str().expect("kind").to_owned(),
            payload: value["payload"].as_str().expect("payload").to_owned(),
        }
    }
}

#[test]
fn usage_normalization_ignores_only_live_request_tool_inventory() {
    use plakous_host_protocol::views::presentation::{
        ContextBreakdownView, ContextInfoRowView, ContextUsage,
    };
    let mut usage = ContextUsage {
        breakdown: Some(ContextBreakdownView {
            informational: vec![ContextInfoRowView {
                label: "Tool definitions".into(),
                tokens: 17,
                detail: Some("one definition".into()),
            }],
            ..ContextBreakdownView::default()
        }),
        prepared_input: None,
        total_window: None,
        admitted_ceiling: None,
        gauge_percent: None,
        display_percent: None,
        ceiling_excess: None,
        evidence: "custom-envelope".into(),
        non_guarantee: None,
        status: "fits".into(),
    };
    let root = Path::new("/fixture");
    let expected = Record::from_event(&HostEvent::Usage(usage.clone()), root);
    usage.breakdown.as_mut().unwrap().categories.push(
        plakous_host_protocol::views::presentation::ContextCategoryView {
            label: "Live turn growth (est.)".into(),
            tokens: 42,
        },
    );
    for label in [
        "Coding tools on the last request",
        "Agent tools on the last request",
    ] {
        usage
            .breakdown
            .as_mut()
            .unwrap()
            .informational
            .push(ContextInfoRowView {
                label: label.into(),
                tokens: 0,
                detail: Some("loopback_effect".into()),
            });
    }
    assert_eq!(
        Record::from_event(&HostEvent::Usage(usage.clone()), root),
        expected
    );
    usage.breakdown.as_mut().unwrap().informational[0].detail = Some("different definition".into());
    assert_ne!(Record::from_event(&HostEvent::Usage(usage), root), expected);
}

/// Field-name suffixes whose integer values are incidental to observation
/// parity: wall clocks, durations, provider-reported token counters, and the
/// percentages derived from them.
const MASKED_NUMBER_SUFFIXES: [&str; 5] = ["_ms", "_tokens", "_seconds", "_unix", "_percent"];

/// Masks identities, temporary paths and incidental numbers so two runs of the
/// same script compare equal.
fn scrub(text: &str, root: &Path) -> String {
    let mut out = text.replace(&root.display().to_string(), "<root>");
    // A tempdir under macOS `/var` is reached through its `/private` twin.
    if let Ok(canonical) = root.canonicalize() {
        out = out.replace(&canonical.display().to_string(), "<root>");
    }
    mask_numbers(&mask_compact_token_figures(&mask_placeholder_tokens(
        &mask_elapsed(&mask_truncated_ids(&mask_uuids(&out))),
    )))
}

/// Masks Compact-row size subjects (`19.7k → 12.4k`).
///
/// Token figures follow request bytes the same way the placeholder pass note does, so
/// pinning them makes the golden flaky across drivers whose session identities
/// differ by a few characters.
fn mask_compact_token_figures(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index].is_ascii_digit()
            && !out
                .ends_with(|character: char| character.is_ascii_alphanumeric() || character == '_')
        {
            let start = index;
            while index < chars.len() && chars[index].is_ascii_digit() {
                index += 1;
            }
            let mut end = index;
            if end < chars.len() && chars[end] == '.' {
                let mut frac = end + 1;
                while frac < chars.len() && chars[frac].is_ascii_digit() {
                    frac += 1;
                }
                if frac > end + 1 {
                    end = frac;
                }
            }
            if end < chars.len() && chars[end] == 'k' {
                out.push_str("<n>k");
                index = end + 1;
                continue;
            }
            out.extend(chars[start..index].iter());
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// Masks the before/after token pair in a context note.
///
/// Covers both the context-placeholder note ("projected input tokens") and the
/// committed-compaction note ("prepared input tokens"). The byte count, the
/// number of results and the number of exchanges are contract; the token
/// counts are not. They follow the request's own bytes, and the two drivers'
/// session identities differ by a few characters, so they legitimately land a
/// handful of tokens apart. Pinning them makes the golden flaky, which is what
/// happened when the compaction note started carrying figures.
fn mask_placeholder_tokens(text: &str) -> String {
    const SUFFIXES: [&str; 2] = [" projected input tokens", " prepared input tokens"];
    for suffix in SUFFIXES {
        let Some(end) = text.find(suffix) else {
            continue;
        };
        let head = &text[..end];
        // The placeholder pass note separates with "; " and the compaction note with
        // ": ", and whichever comes last is the one before the pair.
        let start = head
            .rfind("; ")
            .map(|at| at.saturating_add(2))
            .max(head.rfind(": ").map(|at| at.saturating_add(2)));
        let Some(start) = start else {
            continue;
        };
        let Some(pair) = head.get(start..) else {
            continue;
        };
        if !pair.contains("->") {
            continue;
        }
        return format!("{}<n> -> <n>{}", &text[..start], &text[end..]);
    }
    text.to_owned()
}

/// Masks the ellipsised identity prefixes the host's operator notes carry
/// (`text: "new session 3019570d…"`).
fn mask_truncated_ids(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let mut end = index;
        while end < chars.len() && chars[end].is_ascii_hexdigit() {
            end += 1;
        }
        let run = end - index;
        let boundary =
            index == 0 || !(chars[index - 1].is_ascii_alphanumeric() || chars[index - 1] == '-');
        if run >= 8 && boundary && end < chars.len() && chars[end] == '\u{2026}' {
            out.push_str("<id>");
            index = end;
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// Masks the free-text elapsed stamps the workflow progress notes carry
/// (`"candidate runtime built · cumulative elapsed 132ms"`).
fn mask_elapsed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("elapsed ") {
        out.push_str(&rest[..index + "elapsed ".len()]);
        let tail = &rest[index + "elapsed ".len()..];
        let digits = tail.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            rest = tail;
            continue;
        }
        out.push_str("<n>");
        rest = &tail[digits..];
    }
    out.push_str(rest);
    out
}

fn mask_uuids(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        const COMPACTION_PREFIX: [char; 10] = ['c', 'o', 'm', 'p', 'a', 'c', 't', 'i', 'o', 'n'];
        let is_compaction_id = index + 27 <= chars.len()
            && chars[index..index + 10] == COMPACTION_PREFIX
            && chars[index + 10] == '-'
            && chars[index + 11..index + 27]
                .iter()
                .all(char::is_ascii_hexdigit);
        let is_uuid = index + 36 <= chars.len()
            && chars[index..index + 36].iter().enumerate().all(
                |(offset, character)| match offset {
                    8 | 13 | 18 | 23 => *character == '-',
                    _ => character.is_ascii_hexdigit(),
                },
            );
        if is_compaction_id {
            out.push_str("compaction-<id>");
            index += 27;
        } else if is_uuid {
            out.push_str("<uuid>");
            index += 36;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

fn mask_numbers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let starts_number = chars[index].is_ascii_digit()
            && !out.ends_with(|character: char| {
                character.is_ascii_alphanumeric() || character == '_' || character == '.'
            });
        if starts_number {
            let start = index;
            while index < chars.len() && chars[index].is_ascii_digit() {
                index += 1;
            }
            if masked_number_context(&out) {
                out.push_str("<n>");
            } else {
                out.extend(chars[start..index].iter());
            }
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

fn masked_number_context(prefix: &str) -> bool {
    let mut tail = prefix;
    if let Some(rest) = tail.strip_suffix("Some(") {
        tail = rest;
    }
    // `UpperBound(391)` / `UpperBoundExcess(391)` are projected token counts.
    if tail.ends_with('(') && tail.trim_end_matches('(').ends_with("UpperBound")
        || tail.ends_with("UpperBoundExcess(")
    {
        return true;
    }
    let Some(rest) = tail.strip_suffix(": ") else {
        return false;
    };
    // A tool line's detail carries the tree's own JSON inside the Debug form,
    // so the field name is quoted and the quote is escaped: `\"elapsed_ms\": 1`.
    let rest = rest
        .strip_suffix('"')
        .and_then(|rest| rest.strip_suffix('\\'))
        .unwrap_or(rest);
    let name: String = rest
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    name == "tokens"
        || MASKED_NUMBER_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// Whether an observation of this kind is a latest-state snapshot.
///
/// `plakous_host_protocol::channel::latest_state_event`: a latest-state
/// observation *supersedes the same kind still queued* in the bounded
/// observation mailbox — unconditionally, not only at saturation. The exact
/// number of snapshot observations a driver sees is therefore a function of how
/// fast it drains, never a contract. [`coalesce`] applies the mailbox's own rule
/// maximally so two drivers that drained at different speeds compare equal.
fn latest_state_kind(kind: &str) -> bool {
    matches!(
        kind,
        "presentation"
            | "provider_snapshot"
            | "activity"
            | "attached"
            | "status"
            | "busy"
            | "binding"
            | "replace_transcript"
            | "queue"
            | "prompt_history"
            | "resume_sessions"
            | "template_readiness"
            | "usage"
            | "turn"
            | "subscription_usage"
            | "usage_limits"
            | "session_info"
            | "reattach_ready"
            | "session_opening"
            | "session_opened"
            | "tree"
            | "tree_unavailable"
            | "context_capacity_recovery"
            | "pending_continuation"
            | "last_user_prompt_at"
    )
}

/// Queue and busy are replaceable state, even across receipts or tool lines.
/// Compare their exact final state; prompt receipts and canonical turn
/// terminals independently pin every admission and delivery. Other snapshots
/// retain their position between non-snapshot observations.
fn coalesce(records: Vec<Record>) -> Vec<Record> {
    let mut keep = vec![true; records.len()];
    for index in 0..records.len() {
        if !latest_state_kind(&records[index].kind) {
            continue;
        }
        for later in records.iter().skip(index + 1) {
            if !latest_state_kind(&later.kind)
                && !matches!(records[index].kind.as_str(), "queue" | "busy")
            {
                break;
            }
            if later.kind == records[index].kind {
                keep[index] = false;
                break;
            }
        }
    }
    let kept: Vec<Record> = records
        .into_iter()
        .zip(keep)
        .filter_map(|(record, keep)| keep.then_some(record))
        .collect();
    sort_snapshot_runs(kept)
}

/// Orders each maximal run of snapshot observations by kind.
///
/// Within one run there is no delta, note or receipt to order them against,
/// and a driver that drained once at the end of the run would have taken them
/// in mailbox order regardless of the order the host published them in. Their
/// order relative to each other is therefore not a contract; the run's position
/// between two barriers is, and that is preserved.
fn sort_snapshot_runs(records: Vec<Record>) -> Vec<Record> {
    let mut out: Vec<Record> = Vec::with_capacity(records.len());
    let mut run: Vec<Record> = Vec::new();
    for record in records {
        if latest_state_kind(&record.kind) {
            run.push(record);
            continue;
        }
        run.sort_by(|left, right| (&left.kind, &left.payload).cmp(&(&right.kind, &right.payload)));
        out.append(&mut run);
        out.push(record);
    }
    run.sort_by(|left, right| (&left.kind, &left.payload).cmp(&(&right.kind, &right.payload)));
    out.append(&mut run);
    out
}

/// Folds consecutive assistant deltas into one record.
///
/// The shared host coalesces deltas on a timer. Scheduling changes chunk
/// boundaries, so the contract compares text and its position in the sequence.
fn fold_assistant(records: Vec<Record>) -> Vec<Record> {
    let mut folded: Vec<Record> = Vec::with_capacity(records.len());
    for record in records {
        if record.kind == "assistant"
            && let Some(previous) = folded.last_mut()
            && previous.kind == "assistant"
        {
            let head = previous.payload.trim_end_matches("\")").to_owned();
            let tail = record.payload.trim_start_matches("AssistantDelta(\"");
            previous.payload = format!("{head}{tail}");
            continue;
        }
        folded.push(record);
    }
    folded
}

// ---------------------------------------------------------------------------
// Per-driver projections
// ---------------------------------------------------------------------------

/// What driver 2 (`plak agent`) keeps of the observation sequence.
///
/// Two differences from the TUI driver are legitimate and named here rather
/// than compared loosely:
///
/// 1. *The v1 JSONL wire is lossy by contract.* `crates/hosting/host-protocol/src/
///     jsonl.rs`: "Lossy on purpose: presentation chrome and product extension
///    traffic are dropped." Spec 02 (`plak agent` JSONL v1) fixes the carried
///    kinds; everything else — presentation snapshots, bindings, settings,
///    usage meters, session-open chrome — has no line. The oracle applies the
///    production `project_host_event` and keeps only what it answers `Some`
///    for, and [`jsonl_projection_matches_production`] pins that the kind
///    table below and the production function agree on every observation the
///    scenarios produce.
/// 2. *The JSONL face opens the navigator itself at attach.* `agent_stdio::
///     actions_on_successful_attach` sends `OpenTopologyNavigator` (and
///    `ResumeTopologyTree` under `--resume-tree`) as soon as
///    `AgentAttached(true)` is observed, so a `tree` / `tree_unavailable`
///    observation appears in driver 2's stream that no TUI script asked for.
///    Those are dropped here; a scenario that *scripts* the navigator would
///    have to compare them, and none does.
fn jsonl_projection(records: &[Record]) -> Vec<Record> {
    records
        .iter()
        .filter(|record| jsonl_carries(record))
        .filter(|record| !matches!(record.kind.as_str(), "tree" | "tree_unavailable"))
        .cloned()
        .collect()
}

/// The v1 JSONL kind table, mirroring the exhaustive match in
/// `plakous_host_protocol::jsonl::project_host_event`.
fn jsonl_carries(record: &Record) -> bool {
    // A mid-turn usage snapshot is meter chrome: `turn` means the drive
    // finished, so projecting a cumulative snapshot as `turn` would tell a
    // client the turn ended while the model is still running. Production
    // drops it, and so does this table.
    if record.kind == "turn" && record.payload.contains("terminal: false") {
        return false;
    }
    // Only notices reach the v1 wire, and never a session-log-only one.
    if record.kind == "row" {
        return record.payload.starts_with("Row(Notice(")
            && !record.payload.contains("scope: Diagnostic");
    }
    matches!(
        record.kind.as_str(),
        "status"
            | "busy"
            | "assistant"
            | "assistant_done"
            | "tool"
            | "error"
            | "queue"
            | "tree"
            | "turn"
            | "tree_unavailable"
            | "reasoning"
            | "reasoning_sealed"
            | "approval_required"
            | "approval_resolved"
            | "steer_ack"
            | "turn_lifecycle"
    )
}

/// CLI-visible observations from the production one-shot host.
///
/// All faces share the host's observations. The CLI renders canonical text and
/// tool names/states; binding, usage, queue and settings projections belong to
/// interactive presentation. This projection compares the CLI-visible sequence
/// with the independently recorded host and JSONL sequences.
fn oneshot_projection(records: &[Record]) -> Vec<Record> {
    fold_assistant(
        records
            .iter()
            .filter_map(|record| match record.kind.as_str() {
                "assistant" => Some(record.clone()),
                // Both drivers report that the turn ended in a typed refusal,
                // but the message is not shared: spec 02 §3.3 gives `plak run`
                // an exit code and CLI recovery guidance ("Continue with `plak
                // run --session <id>`"), while the loop publishes the operator
                // note its surface renders. Kind and position are the contract.
                "error" => Some(Record::synthetic("error", "error".to_owned())),
                "row" if is_error_notice(record) => {
                    Some(Record::synthetic("error", "error".to_owned()))
                }
                "tool" => Some(Record::synthetic(
                    "tool",
                    format!(
                        "tool name={} state={}",
                        debug_field(&record.payload, "name").unwrap_or_default(),
                        debug_field(&record.payload, "state").unwrap_or_default()
                    ),
                )),
                _ => None,
            })
            .collect(),
    )
}

/// An error-toned notice row: the loop's form of the JSONL `error` record.
fn is_error_notice(record: &Record) -> bool {
    record.kind == "row"
        && record.payload.starts_with("Row(Notice(")
        && record.payload.contains("tone: Error")
}

/// Reads one `Debug`-printed `name: "value"` field out of a payload.
fn debug_field(payload: &str, field: &str) -> Option<String> {
    if let Some(start) = payload.find(&format!("{field}: \"")) {
        let rest = &payload[start + field.len() + 3..];
        return rest.find('"').map(|end| rest[..end].to_owned());
    }
    // The one-shot driver records the two fields it can know directly.
    let start = payload.find(&format!("{field}="))? + field.len() + 1;
    let rest = &payload[start..];
    Some(
        rest.split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned(),
    )
}

// ---------------------------------------------------------------------------
// Scenario fixtures
// ---------------------------------------------------------------------------

/// Which offline model script an injected session runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Script {
    /// Round 1 asks for `loopback_effect`, round 2 answers with final text.
    Effect,
    /// The admitted input ceiling is one token, so every turn is refused with
    /// a typed context-capacity failure before any provider call is paid for.
    CapacityRefusal,
    /// A no-tools summary binding: the compaction workflow's model.
    CompactionSummary,
    /// [`Script::Effect`] with an oversized tool body and a small admitted
    /// ceiling, so the between-rounds fence reaches the placeholder rung.
    Placeholders,
    /// [`Script::Effect`], but the tool reports a bounded execution failure.
    FailingEffect,
    /// [`Script::Effect`], but the tool parks at its dispatch boundary until
    /// the turn is cancelled, so an intent can land mid-turn deterministically.
    GatedEffect,
    /// One tree call per round on a real [`ProductAgentTree`]: `spawn_agent`,
    /// `wait_agent` on the agent it minted, `adopt_candidate` on the candidate
    /// that agent sealed, then final text.
    Topology,
}

fn binding_for(
    script: Script,
    workspace: &Path,
    controller: BoundaryController,
) -> Result<plakous::sessions::SessionRuntimeBinding, String> {
    match script {
        Script::Effect | Script::GatedEffect => runtime_binding(workspace, controller),
        Script::FailingEffect => failing_effect_runtime_binding(workspace, controller),
        Script::CapacityRefusal => overflow_runtime_binding(workspace, controller),
        Script::CompactionSummary => compaction_runtime_binding(workspace, controller),
        Script::Placeholders => placeholder_runtime_binding(workspace, controller),
        Script::Topology => {
            return Err("the topology script needs its tree; use topology_binding".to_owned());
        }
    }
    .map_err(|error| error.to_string())
}

/// The tree the topology script drives, and the child that answers its spawn.
///
/// A `FakeVcs` tree over a `MemoryTreeStore`: mints, seals and lands are
/// recorded rather than run, so the observations the drivers compare carry the
/// tree's own deterministic ids instead of a repository's commit hashes.
/// [`ParityChildBinder`] is the product's `SpawnedSessionBinder` seam standing
/// in for a real child session — it binds a locator and then, once the tree has
/// recorded that bind, completes the child's one turn so the parent's
/// `wait_agent` settles and leaves a candidate to adopt.
fn topology_driver(workspace: &Path) -> plakous_hosted::agent_tree_tools::TreeDriver {
    use plakous_hosted::agent_tree::{FakeVcs, MemoryTreeStore, ProductAgentTree, TreeStore};
    use plakous_hosted::agent_tree_protocol::WriterMode;
    use plakous_hosted::occupancy_vcs::VcsOps;

    let vcs: Arc<dyn VcsOps> = Arc::new(FakeVcs::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        Arc::clone(&vcs),
        "parity-tree",
        "root",
        "owner",
        workspace.to_path_buf(),
        "offline-loopback-v1",
        WriterMode::SingleLease,
    )
    .expect("parity tree");
    let store: Arc<dyn TreeStore> = Arc::new(MemoryTreeStore::new());
    tree.acknowledge_revision(store.save(&tree).expect("parity tree saved"));
    plakous_hosted::agent_tree_tools::TreeDriver::new(
        Arc::new(std::sync::Mutex::new(tree)),
        store,
        vcs,
        None,
    )
}

/// Stands in for the product's child session: binds a locator, then completes
/// the child's turn once the tree has recorded the bind.
struct ParityChildBinder {
    tree: Arc<std::sync::Mutex<plakous_hosted::agent_tree::ProductAgentTree>>,
}

impl plakous_hosted::agent_tree_tools::SpawnedSessionBinder for ParityChildBinder {
    fn bind(
        &self,
        start: plakous_hosted::agent_tree_tools::SpawnStart,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = Result<
                        plakous_hosted::agent_tree_tools::SpawnBound,
                        plakous_hosted::agent_tree_tools::SpawnBindError,
                    >,
                > + Send
                + '_,
        >,
    > {
        let tree = Arc::clone(&self.tree);
        let agent_id = start.spawned.agent_id.clone();
        Box::pin(async move {
            tokio::spawn(async move {
                // Finish the fixture's admitted turn only after the spawn tool
                // has published its session locator.
                for _ in 0..STEP_TIMEOUT.as_millis() {
                    {
                        let mut tree = tree.lock().expect("parity tree lock");
                        let bound = tree
                            .node(&agent_id)
                            .map(|node| node.session_locator.is_some())
                            .unwrap_or(false);
                        if bound {
                            tree.bind_attempt_turn(&agent_id, "parity-child-turn")
                                .expect("child turn admitted");
                            std::fs::write(
                                tree.node(&agent_id).expect("child node").path.join("H.txt"),
                                "child\n",
                            )
                            .ok();
                            let turn_token = tree
                                .attempt(&agent_id)
                                .map_or(1, |attempt| attempt.turn_token);
                            tree.submit_result(
                                &agent_id,
                                &plakous_hosted::agent_tree_protocol::SubmitResultRequest {
                                    turn_token,
                                    output_json: "{\"summary\":\"offline parity child done\"}"
                                        .to_owned(),
                                },
                                None,
                            )
                            .expect("child submitted its result");
                            tree.release_attempt_turn(&agent_id)
                                .expect("child turn ended");
                            tree.record_turn_completed(&agent_id)
                                .expect("child turn completed");
                            tree.settle_for_test();
                            return;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                panic!("the parity child was never bound");
            });
            Ok(plakous_hosted::agent_tree_tools::SpawnBound {
                durably_registered: false,
                session_locator: "00000000-0000-4000-8000-0000000000c1/00000000-0000-4000-8000-00000000000b/00000000-0000-4000-8000-000000003001".to_owned(),
                turn_id: None,
            })
        })
    }
}

/// The Ready catalog every parity fixture binds against: the one exact row
/// the fixture session is bound to.
fn parity_ready_catalog() -> plakous_host_protocol::views::ReadyCatalog {
    plakous_host_protocol::views::ReadyCatalog::new(vec![
        plakous_host_protocol::views::ProviderSection {
            account: "codex".into(),
            label: "Codex".into(),
            ready: true,
            entries: vec![plakous_host_protocol::views::ModelEntry {
                id: "offline-loopback-v1".into(),
                tag: "offline".into(),
                reasoning: true,
                effort_options: vec!["low".into(), "medium".into()],
                extras: Vec::new(),
                unavailable_reason: None,
            }],
        },
    ])
}

/// The runtime binding one role asks for: the harness's own scripts, or the
/// topology script with this fixture's tree compiled into its tool set.
fn script_binding(
    script: Script,
    tree: Option<plakous_hosted::agent_tree_tools::TreeDriver>,
    workspace: &Path,
    controller: BoundaryController,
) -> Result<plakous::sessions::SessionRuntimeBinding, String> {
    let Some(driver) = tree else {
        return binding_for(script, workspace, controller);
    };
    let caller_id = driver
        .tree_handle_for_test()
        .lock()
        .map_err(|_| "parity tree lock".to_owned())?
        .root_agent_id
        .clone();
    let binder = Arc::new(ParityChildBinder {
        tree: Arc::clone(driver.tree_handle_for_test()),
    }) as Arc<dyn plakous_hosted::agent_tree_tools::SpawnedSessionBinder>;
    let registrations = plakous_hosted::agent_tree_tools::topology_tool_registrations_on(
        driver,
        caller_id,
        plakous_hosted::agent_tree_tools::TopologyHostPorts {
            binder: Some(binder),
            ..Default::default()
        },
    )
    .map_err(|error| error.message().to_owned())?;
    plakous_offline_crash_harness::topology_runtime_binding(workspace, controller, registrations)
        .map_err(|error| error.to_string())
}

pub(crate) struct Fixture {
    root: tempfile::TempDir,
    script: Script,
    /// The tree the topology script drives, kept on the fixture so every
    /// runtime role the host asks for compiles its tools against one tree.
    tree: std::sync::OnceLock<plakous_hosted::agent_tree_tools::TreeDriver>,
}

impl Fixture {
    pub(crate) fn new(script: Script) -> Self {
        Self {
            root: PlakPaths::isolated_tempdir(),
            script,
            tree: std::sync::OnceLock::new(),
        }
    }

    /// This fixture's one tree, created on first use.
    fn tree_driver(&self, workspace: &Path) -> plakous_hosted::agent_tree_tools::TreeDriver {
        self.tree.get_or_init(|| topology_driver(workspace)).clone()
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    pub(crate) fn host_config(&self) -> HostConfig<crate::surface_protocol::PlakExtension> {
        let paths = PlakPaths::explicit(self.path().join("home"));
        let workspace = self.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        paths.ensure_layout().unwrap();
        let controller = if matches!(self.script, Script::GatedEffect) {
            // The effect parks at its dispatch boundary. Nothing ever creates
            // the release file, so only a cancellation leaves the pause — the
            // gate a real provider's cancellation token would also open.
            BoundaryController::quiet_gated_from_evidence_path(
                CrashBoundary::EffectDispatch,
                self.path().join("evidence.jsonl"),
                self.path().join("never-released"),
            )
        } else {
            BoundaryController::quiet_from_evidence_path(
                CrashBoundary::None,
                self.path().join("evidence.jsonl"),
            )
        }
        .unwrap();
        let script = self.script;
        let tree = (script == Script::Topology).then(|| self.tree_driver(&workspace));
        let target = script_binding(script, tree.clone(), &workspace, controller.clone())
            .unwrap()
            .target_binding();
        // A rebind must land on a *different* model script's binding so the
        // scenario proves the loop rebound; the compaction workflow asks for
        // its own runtime role.
        let primary_workspace = workspace.clone();
        let injection = Arc::new(ScriptedRuntimeFactory {
            runtime: Arc::new(move |role| {
                let script = match role {
                    plakous_hosted::runtime::RuntimeRole::Compact => Script::CompactionSummary,
                    _ => script,
                };
                script_binding(script, tree.clone(), &primary_workspace, controller.clone())
            }),
            rebind_target: target,
            observe: Arc::new(|_, _| {}),
            child_runtime: None,
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        });
        let mut session = SessionRecord::new(workspace.clone());
        session.account = Some("codex".into());
        session.model = Some("offline-loopback-v1".into());
        session.effort = Some("medium".into());
        session.composition = crate::product_config::ProductCompositionSelection::JsPtc;
        HostConfig {
            surface: Some(crate::shell::settings::ProductSurface::boxed(&paths)),
            auto_continue: true,
            topology_admission: None,
            state_roots: crate::shell::host::product_state_roots(&paths),
            home: paths.session_home(),
            store: Arc::new(crate::store::session_records::SqliteSessionStore::new(
                paths.clone(),
            )),
            topology_enabled: crate::product_config::SessionPolicy::load(&paths)
                .unwrap()
                .topology_enabled,
            product_home: paths.home().to_path_buf(),
            session,
            target: HostTargetAuthority::Registered {
                provider: NativeProvider::Codex,
            },
            model_id: "offline-loopback-v1".into(),
            effort: Some(ReasoningEffort::Medium),
            effort_explicit: true,
            compaction_enabled: false,
            max_model_rounds: crate::product_agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            root_template: None,
            attached_one_shot: false,
            interactive: true,
            cwd: workspace,
            ready_catalog: parity_ready_catalog(),
            resume_publication: None,
            runtime_factory: injection,
            ready_path: ReadyPathTiming::default(),
            seed_history: None,
            source_releases: crate::product_host::SourceReleases::default(),
        }
    }
}

/// One scripted step: an intent, and the observation that settles it.
struct Step {
    intent: UserAction,
    before: Option<CrashBoundary>,
    settled: Box<dyn Fn(&HostEvent) -> bool + Send>,
}

fn step(intent: UserAction, settled: impl Fn(&HostEvent) -> bool + Send + 'static) -> Step {
    Step {
        intent,
        before: None,
        settled: Box::new(settled),
    }
}

/// Settles when the turn this intent started reaches its terminal status.
///
/// The host publishes an `idle` status at attach as well, so the predicate
/// waits for the busy edge first.
fn after_turn() -> impl Fn(&HostEvent) -> bool + Send {
    let busy = std::cell::Cell::new(false);
    move |event| match event {
        // A completed turn may supersede its busy=true observation before
        // this driver reads it; the admission receipt is not latest-state.
        HostEvent::PromptStarted(_) | HostEvent::SetBusy(true) => {
            busy.set(true);
            false
        }
        HostEvent::Status { phase, .. } => busy.get() && (phase == "idle" || phase == "error"),
        // A turn refused before admission never raises the busy edge; the
        // typed refusal note is its terminal observation.
        HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(
            plakous_host_protocol::views::transcript::NoticeRow {
                tone: plakous_host_protocol::views::transcript::NoticeTone::Error,
                ..
            },
        ))
        | HostEvent::ModelRefusal { .. } => true,
        _ => false,
    }
}

#[test]
fn turn_wait_uses_start_receipt_when_busy_edges_coalesce() {
    let (sender, receiver) =
        plakous_host_protocol::observation_channel::<crate::surface_protocol::PlakExtension>();
    sender.send(HostEvent::PromptStarted(None)).unwrap();
    sender.send(HostEvent::SetBusy(true)).unwrap();
    sender.send(HostEvent::SetBusy(false)).unwrap();
    sender
        .send(HostEvent::Status {
            phase: "idle".into(),
            queue_len: 0,
            detail: None,
        })
        .unwrap();
    let events: Vec<_> = receiver.try_iter().collect();
    assert!(!events.contains(&HostEvent::SetBusy(true)));
    let settled = after_turn();
    assert!(events.iter().any(settled));
}

#[test]
fn workflow_wait_uses_its_receipt_when_busy_edges_coalesce() {
    let (sender, receiver) =
        plakous_host_protocol::observation_channel::<crate::surface_protocol::PlakExtension>();
    sender
        .send(HostEvent::IntentStarted(Some("attach".into())))
        .unwrap();
    sender
        .send(HostEvent::Status {
            phase: "idle".into(),
            queue_len: 0,
            detail: None,
        })
        .unwrap();
    let settled = after_workflow();
    assert!(!receiver.try_iter().any(|event| settled(&event)));
    sender
        .send(HostEvent::IntentStarted(Some("parity-step:0".into())))
        .unwrap();
    sender.send(HostEvent::SetBusy(true)).unwrap();
    sender.send(HostEvent::SetBusy(false)).unwrap();
    sender
        .send(HostEvent::Status {
            phase: "idle".into(),
            queue_len: 0,
            detail: None,
        })
        .unwrap();
    assert!(receiver.try_iter().any(|event| settled(&event)));
}

#[test]
fn queue_normalization_matches_unread_supersession_across_receipts() {
    use plakous_host_protocol::observation::QueueDrainState;
    let (sender, receiver) =
        plakous_host_protocol::observation_channel::<crate::surface_protocol::PlakExtension>();
    let events = vec![
        HostEvent::SetBusy(true),
        HostEvent::QueueSnapshot {
            items: Vec::new(),
            drain: QueueDrainState::LiveAutoDrain,
        },
        HostEvent::PromptStarted(None),
        HostEvent::QueueSnapshot {
            items: Vec::new(),
            drain: QueueDrainState::RestoredPaused,
        },
        HostEvent::SetBusy(false),
    ];
    for event in &events {
        sender.send(event.clone()).unwrap();
    }
    let slow: Vec<_> = receiver.try_iter().collect();
    assert_eq!(slow.len(), 3);
    assert_eq!(
        normalize(&events, Path::new("/fixture"), Script::Effect),
        normalize(&slow, Path::new("/fixture"), Script::Effect)
    );
    assert!(slow.contains(&HostEvent::PromptStarted(None)));
    let mut changed_final_state = slow.clone();
    *changed_final_state.last_mut().unwrap() = HostEvent::SetBusy(true);
    assert_ne!(
        normalize(&slow, Path::new("/fixture"), Script::Effect),
        normalize(&changed_final_state, Path::new("/fixture"), Script::Effect)
    );
    let changed_final_queue = vec![
        HostEvent::PromptStarted(None),
        HostEvent::QueueSnapshot {
            items: Vec::new(),
            drain: QueueDrainState::LiveAutoDrain,
        },
        HostEvent::SetBusy(false),
    ];
    assert_ne!(
        normalize(&slow, Path::new("/fixture"), Script::Effect),
        normalize(&changed_final_queue, Path::new("/fixture"), Script::Effect)
    );
}

// ---------------------------------------------------------------------------
// Drivers
// ---------------------------------------------------------------------------

/// Driver 3: the host loop as the TUI drives it — typed intents in, typed
/// observations out, nothing rendered.
fn drive_loop(fixture: &Fixture, steps: Vec<Step>, jsonl_face: bool) -> Vec<HostEvent> {
    drive_config(fixture.host_config(), steps, jsonl_face)
}

fn drive_config(
    config: HostConfig<crate::surface_protocol::PlakExtension>,
    steps: Vec<Step>,
    jsonl_face: bool,
) -> Vec<HostEvent> {
    let topology_enabled = config.live_topology_enabled().unwrap();
    let tree_id =
        crate::topology_product::durable_topology_tree_id(&config.state_roots, &config.session);
    let evidence_path = config.product_home.parent().unwrap().join("evidence.jsonl");
    let (actions, events, worker) = spawn_host_worker(config).expect("host worker");
    let mut observed: Vec<HostEvent> = Vec::new();
    let mut attach_actions_sent = !jsonl_face;
    for (step_index, step) in steps.into_iter().enumerate() {
        let intent = format!("{:?}", step.intent);
        let deadline = Instant::now() + STEP_TIMEOUT;
        if let Some(boundary) = step.before {
            while !plakous_offline_crash_harness::evidence::read_evidence(&evidence_path)
                .is_ok_and(|rows| rows.iter().any(|row| row.boundary == boundary))
            {
                assert!(
                    Instant::now() < deadline,
                    "parity step {step_index} {intent}, jsonl={jsonl_face}: waiting for {boundary:?}\nobservations: {observed:#?}\nphysical evidence: {}",
                    std::fs::read_to_string(&evidence_path).unwrap_or_default()
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        actions
            .send_correlated(step.intent, Some(format!("parity-step:{step_index}")))
            .expect("intent");
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "parity step {step_index} {intent}, jsonl={jsonl_face}: timed out\nobservations: {observed:#?}\nphysical evidence: {}",
                std::fs::read_to_string(&evidence_path).unwrap_or_default()
            );
            let event = events.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!(
                    "parity step {step_index} {intent}, jsonl={jsonl_face}: {error:?}\nobservations: {observed:#?}\nphysical evidence: {}",
                    std::fs::read_to_string(&evidence_path).unwrap_or_default()
                )
            });
            // `agent_stdio::actions_on_successful_attach`: the JSONL face opens
            // the navigator as soon as the host reports attachment.
            if !attach_actions_sent && matches!(event, HostEvent::AgentAttached(true)) {
                attach_actions_sent = true;
                for action in crate::agent_stdio::actions_on_successful_attach(
                    false,
                    true,
                    topology_enabled
                        && plakous_hosted::topology_runtime::lookup(&tree_id).is_some(),
                ) {
                    actions.send(action).expect("attach intent");
                }
            }
            let done = (step.settled)(&event);
            observed.push(event);
            if done {
                break;
            }
        }
    }
    actions.send(UserAction::Quit).expect("quit");
    worker
        .join()
        .expect("host worker joined")
        .expect("host worker settled");
    observed.extend(events.try_iter());
    observed
}

/// Driver 1 invokes the production one-shot host; no model or turn loop lives here.
fn drive_oneshot(fixture: &Fixture, prompt: &str) -> Vec<Record> {
    let mut events = Vec::new();
    let result = plakous_hosted::runtime::OneShot::run(
        fixture.host_config(),
        prompt.to_owned(),
        |event, _| {
            if let Some(event) = event {
                events.push(event.clone());
            }
        },
    );
    match fixture.script {
        Script::CapacityRefusal => assert!(result.is_err(), "capacity must fail the one-shot"),
        _ => assert!(
            result.is_ok(),
            "one-shot publication/release failed: {result:?}"
        ),
    }
    let mut records = normalize(&events, fixture.path(), fixture.script);
    if result.is_err() {
        // One-shot terminal failures are rendered from the returned CLI error,
        // while the long-lived host publishes an ErrorNote for the same outcome.
        records.push(Record::synthetic("error", "error".to_owned()));
    }
    records
}

// ---------------------------------------------------------------------------
// Goldens
// ---------------------------------------------------------------------------

fn golden_path(scenario: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("parity")
        .join(format!("{scenario}.jsonl"))
}

fn assert_golden(scenario: &str, records: &[Record]) {
    let path = golden_path(scenario);
    let rendered = records
        .iter()
        .map(Record::line)
        .collect::<Vec<String>>()
        .join("\n")
        + "\n";
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::create_dir_all(path.parent().expect("parity dir")).expect("parity dir");
        std::fs::write(&path, &rendered).expect("write golden");
        return;
    }
    let stored = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing parity golden {}; regenerate with PLAK_UPDATE_GOLDENS=1",
            path.display()
        )
    });
    let expected: Vec<Record> = stored
        .lines()
        .filter(|line| !line.is_empty())
        .map(Record::parse)
        .collect();
    let render = |rows: &[Record]| {
        rows.iter()
            .map(|row| {
                format!(
                    "{} {}",
                    row.kind,
                    row.payload.chars().take(90).collect::<String>()
                )
            })
            .collect::<Vec<String>>()
            .join("\n")
    };
    assert_eq!(
        records.len(),
        expected.len(),
        "observation count changed for {scenario}\n--- observed\n{}\n--- golden\n{}",
        render(records),
        render(&expected),
    );
    for (index, (observed, want)) in records.iter().zip(expected.iter()).enumerate() {
        assert_eq!(observed, want, "{scenario} observation {index} diverged");
    }
}

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

/// Whether driver 1 can run this scenario at all.
enum OneShot {
    /// `plak run` drives this scenario with the given prompt.
    Prompt(&'static str),
    /// `plak run` cannot express the scenario; the named spec sentence says so.
    Unsupported(&'static str),
}

/// Turns one driver's raw observations into the comparable sequence: the
/// mailbox's own supersede rule applied maximally, and the status strip reduced
/// to the phase and queue length it means. Every other observation is compared
/// exactly.
fn normalize(events: &[HostEvent], root: &Path, _script: Script) -> Vec<Record> {
    let records = events
        .iter()
        // Background projector settlements have no arrival-count or ordering
        // contract. Their complete values are compared in terminal snapshots.
        .filter(|event| {
            !matches!(
                event,
                HostEvent::Row(crate::store::sessions::TranscriptRow::Tool(_))
            )
        })
        .map(|event| Record::from_event(event, root))
        // IntentStarted is an in-process correlation receipt consumed by the
        // JSONL adapter, not a user-visible observation on either surface.
        .filter(|record| record.kind != "intent_started")
        .filter(|record| !transient_workflow_status(record))
        .map(drop_status_detail)
        .map(drop_cancelled_tool_detail)
        .collect();
    coalesce(records)
}

/// Whether this is one of the host's intra-workflow progress statuses.
///
/// `Status` is a latest-state observation, so the mailbox supersedes one that
/// is still unread (`channel::latest_state_event`). A workflow that publishes
/// several progress statuses in a row — `clear`, `rebind`, `compacting` and the
/// `interrupting` ("`interrupting` means a cancel was requested
/// and has not been observed to settle") — therefore reaches a fast driver as
/// several observations and a slow one as one, and its `cumulative elapsed`
/// detail is a wall clock. Which of them a driver saw is not a contract and is
/// not compared; the terminal phases the strip settles on are.
fn transient_workflow_status(record: &Record) -> bool {
    if record.kind != "status" {
        return false;
    }
    let Some(phase) = debug_field(&record.payload, "phase") else {
        return false;
    };
    !matches!(phase.as_str(), "idle" | "running" | "error" | "interrupted")
}

/// Reduces a status strip observation to its phase and queue length.
///
/// `Status` is latest-state, so a driver holds whichever of several
/// superseding statuses it happened to drain, and the `detail` is the
/// operator-facing hint attached to that one ("turn complete", "compact
/// workflow committed", "ready · <model>"). The phase and the queue length are
/// what the strip means; the hint text is not a sequence contract, and
/// comparing it made the oracle a race detector for the mailbox rather than
/// for the drivers.
fn drop_status_detail(record: Record) -> Record {
    if record.kind != "status" {
        return record;
    }
    let phase = debug_field(&record.payload, "phase").unwrap_or_default();
    let queue_len = record
        .payload
        .split("queue_len: ")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .unwrap_or("?")
        .to_owned();
    Record::synthetic(
        "status",
        format!("Status {{ phase: {phase:?}, queue_len: {queue_len} }}"),
    )
}

/// Drops the expandable body from the terminal line of a tool cancelled in
/// flight.
///
/// The host publishes that line either from the tool's own observed terminal —
/// carrying the tool's message — or, when the turn terminalised before that
/// event arrived, from the persisted row, which holds only the arguments. Which
/// one happens is a race between the fake tool's join and the runtime's
/// terminal, not a property of the driver, and it is the same observation gap
/// `runtime::events` closes elsewhere. The line, its state, its call and its
/// position are compared; the body is not.
fn drop_cancelled_tool_detail(record: Record) -> Record {
    if record.kind != "tool"
        || debug_field(&record.payload, "state").as_deref() != Some("cancelled")
    {
        return record;
    }
    let Some(head) = record.payload.split(", subject:").next() else {
        return record;
    };
    Record::synthetic("tool", format!("{head} }}"))
}

fn run_parity(scenario: &str, script: Script, one_shot: OneShot, steps: impl Fn() -> Vec<Step>) {
    let tui_fixture = Fixture::new(script);
    let tui_events = drive_loop(&tui_fixture, steps(), false);
    let golden = normalize(&tui_events, tui_fixture.path(), script);
    assert_golden(scenario, &golden);

    let jsonl_fixture = Fixture::new(script);
    let jsonl_events = drive_loop(&jsonl_fixture, steps(), true);
    jsonl_projection_matches_production(&jsonl_events, jsonl_fixture.path());
    let jsonl_records = normalize(&jsonl_events, jsonl_fixture.path(), script);
    assert_eq!(
        jsonl_projection(&jsonl_records),
        jsonl_projection(&golden),
        "{scenario}: the JSONL driver's v1-carried observations diverged from the golden"
    );

    match one_shot {
        OneShot::Prompt(prompt) => {
            let one_shot_fixture = Fixture::new(script);
            let one_shot_records = drive_oneshot(&one_shot_fixture, prompt);
            assert_eq!(
                oneshot_projection(&one_shot_records),
                oneshot_projection(&golden),
                "{scenario}: `plak run` diverged from the host loop"
            );
            assert_eq!(
                oneshot_projection(&one_shot_records),
                oneshot_projection(&jsonl_records),
                "{scenario}: `plak run` diverged from `plak agent`"
            );
        }
        OneShot::Unsupported(_reason) => {}
    }
    assert_eq!(
        oneshot_projection(&golden),
        oneshot_projection(&jsonl_records),
        "{scenario}: the TUI and JSONL drivers disagree on the turn's core observations"
    );
}

/// The kind table in [`jsonl_carries`] is a restatement of the exhaustive match
/// in `plakous_host_protocol::jsonl::project_host_event`. This pins them
/// together for every observation the scenarios actually produce, so the
/// projection can never quietly become a looser comparison than the wire.
fn jsonl_projection_matches_production(events: &[HostEvent], root: &Path) {
    for event in events {
        let record = Record::from_event(event, root);
        let projected = project_host_event(event, JsonlCorrelation::default()).is_some();
        assert_eq!(
            jsonl_carries(&record),
            projected,
            "the oracle's v1 kind table disagrees with project_host_event for {}",
            record.kind
        );
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

fn prompt_step(text: &'static str) -> Step {
    step(UserAction::Prompt(text.into()), after_turn())
}

#[test]
fn disabled_topology_jsonl_prompt_completes_without_navigator_error() {
    let fixture = Fixture::new(Script::Effect);
    let mut config = fixture.host_config();
    config.topology_enabled = false;
    config.topology_admission = None;
    let events = drive_config(config, vec![prompt_step("run the loopback")], true);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::SetBusy(true)))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HostEvent::Status { phase, .. } if phase == "idle"))
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::Notice(
                    plakous_host_protocol::views::transcript::NoticeRow { .. }
                )
            ) | HostEvent::TopologyNavigatorUnavailable { .. }
        )),
        "{events:#?}"
    );
}

#[test]
fn parity_plain_turn_with_one_tool_call() {
    run_parity(
        "plain_turn_tool_call",
        Script::Effect,
        OneShot::Prompt("run the loopback"),
        || {
            vec![step(
                UserAction::Prompt("run the loopback".into()),
                after_turn(),
            )]
        },
    );
}

#[test]
fn parity_turn_with_a_tool_error() {
    run_parity(
        "tool_error",
        Script::FailingEffect,
        OneShot::Prompt("run the loopback"),
        || vec![prompt_step("run the loopback")],
    );
}

#[test]
fn parity_turn_blocked_by_capacity() {
    run_parity(
        "capacity_refusal",
        Script::CapacityRefusal,
        OneShot::Prompt("run the loopback"),
        || vec![prompt_step("run the loopback")],
    );
}

#[test]
fn parity_clear_then_turn() {
    run_parity(
        "clear_then_turn",
        Script::Effect,
        // Spec 02: `plak run` is one prompt on one freshly opened session and
        // exits; there is no session to clear and no second turn to observe.
        OneShot::Unsupported("`plak run` opens one session, drives one turn, exits"),
        || {
            vec![
                prompt_step("run the loopback"),
                step(UserAction::ClearSession, |event| {
                    matches!(event, HostEvent::ClearView)
                }),
                prompt_step("run it again"),
            ]
        },
    );
}

#[test]
fn parity_interrupt_mid_turn_then_second_prompt() {
    run_parity(
        "interrupt_then_prompt",
        Script::GatedEffect,
        OneShot::Unsupported("`plak run` maps Ctrl-C to one exit code, not to a second prompt"),
        || {
            vec![
                step(
                    UserAction::Prompt("run the loopback".into()),
                    tool_running(),
                ),
                interrupt_step(),
                prompt_step("run it again"),
            ]
        },
    );
}

#[test]
fn parity_escape_then_quit() {
    run_parity(
        "escape_then_quit",
        Script::GatedEffect,
        OneShot::Unsupported("`plak run` maps Ctrl-C to one exit code, not to a settled cancel"),
        || {
            vec![
                step(
                    UserAction::Prompt("run the loopback".into()),
                    tool_running(),
                ),
                interrupt_step(),
            ]
        },
    );
}

/// One tool-heavy turn whose oldest result body is replaced between rounds.
#[test]
fn parity_context_placeholders() {
    run_parity(
        "context_placeholders",
        Script::Placeholders,
        OneShot::Prompt("run the loopback"),
        || vec![prompt_step("run the loopback")],
    );
}

#[test]
fn parity_compaction() {
    run_parity(
        "compaction",
        Script::CompactionSummary,
        OneShot::Unsupported("`plak run` has no `/compact`; compaction is `plak compact <id>`"),
        || {
            vec![
                prompt_step("run the loopback"),
                prompt_step("run it again"),
                prompt_step("and once more"),
                step(UserAction::Compact, after_workflow()),
            ]
        },
    );
}

/// Settles when the host has opened the tool row the gated effect parks in.
fn tool_running() -> impl Fn(&HostEvent) -> bool + Send {
    |event: &HostEvent| matches!(event, HostEvent::ToolLine { state, .. } if state == "running")
}

/// Interrupt only after the fixture has physically invoked its gated effect.
/// The earlier tool-row announcement is not dispatch admission evidence.
fn interrupt_step() -> Step {
    Step {
        intent: UserAction::Interrupt,
        before: Some(CrashBoundary::EffectDispatch),
        settled: Box::new(after_cancel()),
    }
}

/// Settles only after the interrupted status, canonical cancelled turn, and
/// final busy-false edge. The interrupted status can precede terminal delivery.
fn after_cancel() -> impl Fn(&HostEvent) -> bool + Send {
    let interrupted = std::cell::Cell::new(false);
    let cancelled = std::cell::Cell::new(false);
    move |event: &HostEvent| match event {
        HostEvent::Status { phase, .. } if phase == "error" => {
            panic!("gated cancellation reported an unexpected error: {event:?}")
        }
        HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(
            plakous_host_protocol::views::transcript::NoticeRow {
                tone: plakous_host_protocol::views::transcript::NoticeTone::Error,
                ..
            },
        ))
        | HostEvent::ModelRefusal { .. } => {
            panic!("gated cancellation reported an unexpected failure: {event:?}")
        }
        HostEvent::Status { phase, .. } if phase == "interrupted" => {
            interrupted.set(true);
            false
        }
        HostEvent::TurnLifecycle(lifecycle) if lifecycle.phase == TurnPhase::Cancelled => {
            cancelled.set(true);
            false
        }
        HostEvent::SetBusy(false) => interrupted.get() && cancelled.get(),
        _ => false,
    }
}

fn cancelled_turn() -> HostEvent {
    HostEvent::TurnLifecycle(TurnLifecycle {
        turn_id: "current-turn".into(),
        phase: TurnPhase::Cancelled,
        model_error_kind: None,
        http_status: None,
    })
}

#[test]
fn cancel_wait_requires_terminal_then_final_busy_false() {
    let settled = after_cancel();
    assert!(!settled(&HostEvent::Status {
        phase: "idle".into(),
        queue_len: 0,
        detail: None,
    }));
    assert!(!settled(&HostEvent::Status {
        phase: "interrupted".into(),
        queue_len: 0,
        detail: None,
    }));
    assert!(!settled(&cancelled_turn()));
    assert!(settled(&HostEvent::SetBusy(false)));
}

#[test]
fn cancel_wait_ignores_busy_false_before_terminal() {
    let settled = after_cancel();
    assert!(!settled(&HostEvent::SetBusy(false)));
    assert!(!settled(&HostEvent::Status {
        phase: "interrupted".into(),
        queue_len: 0,
        detail: None,
    }));
    assert!(!settled(&HostEvent::SetBusy(false)));
    assert!(!settled(&cancelled_turn()));
    assert!(settled(&HostEvent::SetBusy(false)));
}

#[test]
fn cancel_wait_rejects_busy_false_without_interrupted_status() {
    let settled = after_cancel();
    assert!(!settled(&cancelled_turn()));
    assert!(!settled(&HostEvent::SetBusy(false)));
}

#[test]
#[should_panic(expected = "gated cancellation reported an unexpected error")]
fn cancel_wait_rejects_error_status() {
    after_cancel()(&HostEvent::Status {
        phase: "error".into(),
        queue_len: 0,
        detail: Some("unexpected fixture failure".into()),
    });
}

fn after_workflow() -> impl Fn(&HostEvent) -> bool + Send {
    let started = std::cell::Cell::new(false);
    move |event| match event {
        HostEvent::IntentStarted(Some(id)) if id.starts_with("parity-step:") => {
            started.set(true);
            false
        }
        HostEvent::Status { phase, .. } => started.get() && (phase == "idle" || phase == "error"),
        _ => false,
    }
}

/// `plak agent` must terminalise every tool it announced.
///
/// This was a divergence the oracle recorded: `project_host_event` listed
/// `ToolLine { duration_ms: Some(_), .. }` among the observations with no v1
/// line, while `runtime::events` stamps a duration on every terminal tool line
/// it can ("Carried on every line the host can stamp"), so a live
/// turn emitted `{"kind":"tool","state":"running"}` and nothing more. Fixed at
/// the root in `plakous_host_protocol::jsonl`; this is the parity assertion
/// that holds the fix.
#[test]
fn jsonl_carries_the_terminal_tool_line() {
    let fixture = Fixture::new(Script::Effect);
    let events = drive_loop(&fixture, vec![prompt_step("run the loopback")], true);
    let terminal: Vec<String> = events
        .iter()
        .filter(|event| matches!(event, HostEvent::ToolLine { state, .. } if state != "running"))
        .filter_map(|event| project_host_event(event, JsonlCorrelation::default()))
        .collect();
    assert_eq!(
        terminal.len(),
        1,
        "one tool ran; the v1 wire carried {} terminal lines",
        terminal.len()
    );
    let line = &terminal[0];
    assert!(line.contains(r#""state":"done""#), "{line}");
    assert!(line.contains(r#""duration_ms":"#), "{line}");
    assert!(
        plakous_host_protocol::envelope::required_stdout_kind(line),
        "{line}"
    );
}

/// A tool cancelled in flight terminalises once, as `cancelled`, before the
/// strip says `interrupted`.
///
/// This was a divergence the oracle recorded: the terminal line was published
/// by whichever path reached the row first — the tool's own observed terminal
/// (which painted `failed`, since a joined cancel is a failed observation like
/// any other) or the settlement sweep (which painted `cancelled`) — so across
/// runs of one script the label, the position relative to `Status{interrupted}`
/// and sometimes the line itself changed. Fixed in `runtime::turn::
/// apply_cancel_settlement` and `runtime::events`; this holds the fix, over the
/// gated effect that parks until it is cancelled.
#[test]
fn a_cancelled_tool_terminalises_once_before_the_interrupted_status() {
    for attempt in 0..5 {
        let fixture = Fixture::new(Script::GatedEffect);
        let events = drive_loop(
            &fixture,
            vec![
                step(
                    UserAction::Prompt("run the loopback".into()),
                    tool_running(),
                ),
                interrupt_step(),
            ],
            false,
        );
        let mut terminal_tool_lines = 0;
        let mut interrupted_at = None;
        let mut cancelled_at = None;
        for (index, event) in events.iter().enumerate() {
            match event {
                HostEvent::ToolLine { state, .. } if state != "running" => {
                    terminal_tool_lines += 1;
                    assert_eq!(
                        state, "cancelled",
                        "attempt {attempt}: a tool cancelled in flight must terminalise as \
                         cancelled, not `{state}`"
                    );
                    cancelled_at.get_or_insert(index);
                }
                HostEvent::Status { phase, .. } if phase == "interrupted" => {
                    interrupted_at.get_or_insert(index);
                }
                _ => {}
            }
        }
        assert_eq!(
            terminal_tool_lines, 1,
            "attempt {attempt}: exactly one terminal line for the one tool that ran: {events:#?}"
        );
        let cancelled_at =
            cancelled_at.unwrap_or_else(|| panic!("attempt {attempt}: no cancelled tool line"));
        let interrupted_at =
            interrupted_at.unwrap_or_else(|| panic!("attempt {attempt}: no interrupted status"));
        assert!(
            cancelled_at < interrupted_at,
            "attempt {attempt}: the strip said interrupted at {interrupted_at} before the tool \
             it cancelled terminalised at {cancelled_at}"
        );
    }
}

/// One turn that spawns a child, waits for it, and adopts what it sealed.
///
/// The tree is real — `plakous-recursive-subagents` over `FakeVcs` and a
/// `MemoryTreeStore`, with its own ten tool executors compiled into the
/// session's tool set — so the three drivers are compared on the observations
/// a topology turn actually produces: three tool lines, their terminal states,
/// and the assistant text that closes the turn. What stands in for a real child
/// session is [`ParityChildBinder`] at the product's `SpawnedSessionBinder`
/// seam: it binds a locator and completes the child's one turn, which is what a
/// bound child session would have done. The tree's own behaviour under that
/// seam — the mint, the seal, the wait row, the adopt receipt — is the crate's
/// to test; this pins that the three drivers observe it identically.
#[test]
fn parity_spawn_wait_adopt() {
    run_parity(
        "topology_spawn_wait_adopt",
        Script::Topology,
        OneShot::Prompt("delegate the offline parity change"),
        || vec![prompt_step("delegate the offline parity change")],
    );
}

pub(crate) fn refusal_config(
    fixture: &Fixture,
    text: &'static str,
) -> HostConfig<crate::surface_protocol::PlakExtension> {
    std::fs::create_dir_all(fixture.path().join("workspace")).unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        fixture.path().join("workspace/refusal-events.jsonl"),
    )
    .unwrap();
    refusal_config_with_controller(fixture, text, controller)
}

pub(crate) fn refusal_config_with_controller(
    fixture: &Fixture,
    text: &'static str,
    controller: BoundaryController,
) -> HostConfig<crate::surface_protocol::PlakExtension> {
    let mut config = fixture.host_config();
    let workspace = config.cwd.clone();
    let binding = plakous_offline_crash_harness::text_runtime_binding_with_finish(
        &workspace,
        controller.clone(),
        text,
        text,
        plakous::model::FinishReason::Refusal,
    )
    .unwrap();
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
        rebind_target: binding.target_binding(),
        runtime: Arc::new(move |_| {
            plakous_offline_crash_harness::text_runtime_binding_with_finish(
                &workspace,
                controller.clone(),
                text,
                text,
                plakous::model::FinishReason::Refusal,
            )
            .map_err(|error| error.to_string())
        }),
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    config
}

#[test]
fn refusal_normal_host_and_jsonl_preserve_text_and_specific_note_once() {
    for text in ["", "I cannot help with that."] {
        let fixture = Fixture::new(Script::Effect);
        let config = refusal_config(&fixture, text);
        let evidence = config.cwd.join("refusal-events.jsonl");
        let events = drive_config(config, vec![prompt_step("refusal fixture")], true);
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    HostEvent::AssistantDelta(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            text
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, HostEvent::ModelRefusal { .. }))
                .count(),
            1
        );
        assert!(!events.iter().any(
            |event| matches!(event, HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("empty stream"))
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, HostEvent::AssistantDone))
                .count(),
            usize::from(!text.is_empty())
        );
        let refusal_index = events
            .iter()
            .position(|event| matches!(event, HostEvent::ModelRefusal { .. }))
            .unwrap();
        assert!(
            events[..refusal_index]
                .iter()
                .any(|event| matches!(event, HostEvent::PresentationSnapshot(_)))
        );
        let projected = events
            .iter()
            .filter_map(|event| project_host_event(event, JsonlCorrelation::default()))
            .collect::<Vec<_>>();
        assert_eq!(
            projected
                .iter()
                .filter(|value| value.contains("model refused the request"))
                .count(),
            1
        );
        let wire: Vec<serde_json::Value> = projected
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            wire.iter()
                .filter(|value| value["kind"] == "assistant")
                .filter_map(|value| value["text"].as_str())
                .collect::<String>(),
            text
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, HostEvent::TurnUsage { .. }))
                .count(),
            1
        );
        assert_eq!(
            wire.iter()
                .filter(|value| value["kind"] == "model_refusal")
                .count(),
            1
        );
        let rows = plakous_offline_crash_harness::evidence::read_evidence(&evidence).unwrap();
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind
                    == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched)
                .count(),
            1
        );
        assert!(!rows.iter().any(|row| row.kind
            == plakous_offline_crash_harness::evidence::EvidenceKind::EffectDispatched));
    }
}
