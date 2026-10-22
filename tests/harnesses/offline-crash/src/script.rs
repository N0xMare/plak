//! A declarative script for the offline loopback model.
//!
//! The Rust `*_runtime_binding` constructors in `crate::fixture` each pin one
//! closed scenario. A spawned process cannot select one of those by calling a
//! function, so this module carries the same dials as data: one JSON document a
//! real `plak` process reads before it attaches, naming the assistant steps the
//! offline model replays and the faults it injects.
//!
//! The document is a fixture format, not a product contract. It exists so the
//! repository gate can drive complete lifecycles of the shipped binary — the
//! CLI face, the agent JSONL face and the terminal application — against a
//! model that never leaves the process.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Exact version this crate reads. A script that names another is refused.
pub const SCRIPT_VERSION: u16 = 1;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "the document shape's own unit tests")]
mod gate_tests {
    #[test]
    fn registry_and_file_gates_are_scripted_without_a_timing_window() {
        for gate in [
            serde_json::json!({"registry": "held-response"}),
            serde_json::json!({"file": "release-response"}),
        ] {
            let plan = serde_json::json!({
                "version": super::SCRIPT_VERSION,
                "steps": [{"stream": [{"gate": gate}], "output": {"text": "released"}}],
            });
            let parsed: super::ScriptPlan = serde_json::from_value(plan.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(parsed).unwrap()["steps"][0]["stream"],
                plan["steps"][0]["stream"]
            );
        }
    }
}

/// One declarative offline model script.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent fixture dial, not a state machine"
)]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptPlan {
    /// Must equal [`SCRIPT_VERSION`].
    pub version: u16,
    /// The assistant steps, in round order. Round `n` reads `steps[n - 1]`.
    #[serde(default)]
    pub steps: Vec<ScriptStep>,
    /// Rounds past the end of `steps` replay the last step when true, and
    /// otherwise commit the fixture's ordinary final text.
    #[serde(default)]
    pub repeat_last_step: bool,
    /// The admitted projected-input ceiling of the scripted descriptor.
    #[serde(default = "default_maximum_projected_input")]
    pub maximum_projected_input: u64,
    /// No capacity contract, to qualify unknown-window carry.
    #[serde(default)]
    pub omit_context_contract: bool,
    /// Arm the between-rounds compaction fence.
    #[serde(default)]
    pub scheduled_compaction: bool,
    /// Project the whole request rather than its input identity, so a large
    /// committed tool result really does move the projected count.
    #[serde(default)]
    pub content_sensitive_projection: bool,
    /// Body size of the loopback tool's success text, in bytes.
    #[serde(default)]
    pub tool_result_bytes: Option<usize>,
    /// Make the loopback tool report a bounded execution failure.
    #[serde(default)]
    pub tool_fails: bool,
    /// How the scripted model answers an independently admitted summary
    /// (fold) request.
    #[serde(default)]
    pub fold: FoldDisposition,
    /// The tool policy the scripted binding is built with.
    #[serde(default)]
    pub tool_policy: ScriptToolPolicy,
    /// Make a `stall_ms` deaf to the call's cancellation token.
    ///
    /// **This changes the state a cut lands on, which is the whole point.** A
    /// stall that observes cancellation resolves the in-flight model request
    /// before the process dies, so a reopen finds nothing outstanding and has
    /// nothing to abandon. A stall that ignores it leaves the request
    /// unobserved, which is the condition every "process lost mid-request"
    /// case depends on.
    ///
    /// Default false, because a real provider call is cancellable and a fixture
    /// that is not misreports the product.
    /// Signal tests that need an unobserved request set this and say why.
    #[serde(default)]
    pub stall_ignores_cancellation: bool,
    /// Shrink the observation budget to this many bytes.
    ///
    /// The budget is otherwise derived from the prepared request's own
    /// headroom, which on any realistic route is hundreds of kilobytes, so no
    /// tool output a test can plausibly produce ever reaches it. This only ever
    /// shrinks the derived budget and never raises it, so a script cannot let a
    /// turn observe more than policy allows.
    #[serde(default)]
    pub observation_budget_cap_bytes: Option<usize>,
    /// Attach a real Code Mode profile instead of exposing tools directly.
    ///
    /// The model is then offered the `exec` and `wait` markers over the Codex
    /// minimal nested catalog, and a step's `cell` output carries the
    /// JavaScript program. Without this a `composition` label on the session
    /// record changes nothing: the binding still exposes the bare tools and the
    /// turn takes the ordinary direct path.
    #[serde(default)]
    pub code_mode: bool,
    /// Declare image input support on the scripted model, so an image prompt
    /// is admitted; without it the runtime refuses one naming the route.
    #[serde(default)]
    pub image_input: bool,
    /// Select the product JavaScript markers for the real sidecar fixture.
    #[serde(default)]
    pub js_ptc: bool,
    /// Register the standard coding packs over a real POSIX shell instead of
    /// the single loopback effect.
    ///
    /// This is what lets a script drive `process_session_start`,
    /// `process_session_write`, `process_session_poll` and
    /// `process_session_terminate` against real child processes, so a lifecycle
    /// test reaches the process backend's spawn and teardown.
    #[serde(default)]
    pub process_tools: bool,
    /// Seal the final provider response with managed family replay artifacts.
    #[serde(default)]
    pub seal_family_artifacts: bool,
    /// Prepend a completed provider-hosted call/result to each final text response.
    /// This is provider evidence, never a local tool dispatch.
    #[serde(default)]
    pub hosted_tool_call_final: bool,
    /// Expose the agent-tree topology tools over a real tree with a child
    /// binder, instead of the loopback effect.
    ///
    /// Without this there is no child route at all from a spawned process: an
    /// injected runtime binding replaces the whole tool registry, so the
    /// topology tools do not come along with it and `spawn_agent` is simply not
    /// there. With it, a step's `tool` output can name a topology tool and the
    /// turn reaches a real spawned child.
    ///
    /// Mutually exclusive with [`Self::code_mode`] and [`Self::process_tools`]:
    /// each names a different registry, and a script that asks for two is a
    /// script whose author did not decide.
    #[serde(default)]
    pub tree: bool,
    /// Publish the scripted tree through the product `subagents.sqlite3`
    /// instead of the in-memory store.
    ///
    /// `plak session show` reads that file. A child that exists only in the
    /// process hub is invisible after exit, so a recovery assertion against
    /// show would pass on an empty tree. Requires [`Self::tree`].
    #[serde(default)]
    pub durable_tree: bool,
    /// At the first bound child locator publish and thereafter, the store
    /// returns the classified `disk_full` category and does not write.
    ///
    /// The string is the one the sqlite store emits for `SQLITE_FULL`
    /// (`could not durably commit agent tree: disk_full`): no backend text
    /// and no path. Requires [`Self::durable_tree`]. The committed child row
    /// lands before the fault, independently of projector save counts.
    #[serde(default)]
    pub disk_full_at_child_locator: bool,
    /// How long a held child sleeps after its locator is published, before
    /// it submits a result.
    ///
    /// The sleep is not on the tree mutex. A quit during it finds the child
    /// still running. Requires [`Self::tree`].
    #[serde(default)]
    pub child_hold_ms: u64,
    /// Children are held only once this many have already been bound.
    ///
    /// `0` holds the first child. Ignored when [`Self::child_hold_ms`] is 0.
    #[serde(default)]
    pub child_hold_after: u32,
    /// Bind a real child agent with a promoted shell process, for run-exit tests.
    #[serde(default)]
    pub child_process: bool,
    /// Complete the real process child's turn before its live handle is bound.
    #[serde(default)]
    pub child_process_quiescent: bool,
    /// The first child whose zero-based bind ordinal reaches this value
    /// returns recovery-required instead of running.
    ///
    /// Earlier children complete. Requires [`Self::tree`].
    #[serde(default)]
    pub fail_child_after_successes: Option<u32>,
    /// Replaces the account-derived response namespace for scripted call ids.
    ///
    /// Call ids are `(namespace << 64) + ordinal`, and the ordinal is wire
    /// size, item count and round. Wire size is the JSON length of a digest,
    /// so two sessions with different prompts can still share an ordinal.
    /// Distinct namespaces cannot: only the low 48 bits are used, and an
    /// ordinal this binding can admit stays below `2^63`.
    #[serde(default)]
    pub identity_namespace: Option<u128>,
}

const fn default_maximum_projected_input() -> u64 {
    10_000_000
}

/// How the scripted model answers a semantic-summary request.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FoldDisposition {
    /// Return the fixture's bounded summary text.
    #[default]
    Accept,
    /// Refuse preparation, which is what drops the compaction ladder onto its
    /// offline placeholder rung.
    Refuse,
    /// Dispatch, then fail definitely with a rate-limit refusal, which is a
    /// paid fold that changed nothing.
    Fail,
    /// Dispatch, then stall for this many milliseconds (observing
    /// cancellation) before answering, so an Esc, a deadline or a cut can land
    /// inside a paid fold.
    Stall(u64),
}

/// How the scripted binding's tool policy answers a mutating call.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptToolPolicy {
    /// Every registered tool is allowed without a decision (`AlwaysAllowPolicy`).
    #[default]
    Allow,
    /// Every mutating call pauses in an effect-decision frame until the
    /// control plane answers, so a process can be cut inside that frame.
    DecisionRequired,
}

/// One scripted assistant round.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptStep {
    /// What the terminal response carries.
    #[serde(default)]
    pub output: ScriptOutput,
    /// Text streamed as deltas before the terminal response.
    #[serde(default)]
    pub prefix: Option<String>,
    /// The stop reason reported with the terminal response.
    #[serde(default)]
    pub finish: ScriptFinish,
    /// Usage reported on the terminal response.
    #[serde(default)]
    pub usage: Option<ScriptUsage>,
    /// A provider fault injected instead of, or during, this round.
    #[serde(default)]
    pub fault: Option<ScriptFault>,
    /// How long the provider operation takes before it answers.
    ///
    /// This and a streamed [`ScriptStreamEvent::Stall`] are the dials that
    /// make a scripted turn long enough for a signal, a cancel or a quit to
    /// land inside it. Keep it short: a gate that waits is a gate that is
    /// skipped.
    #[serde(default)]
    pub stall_ms: u64,
    /// Hold dispatch before returning a stream, until explicitly released.
    #[serde(default)]
    pub gate: Option<crate::gate::ScriptGate>,
    /// Events streamed, in order, before the terminal response (after
    /// `prefix`).
    #[serde(default)]
    pub stream: Vec<ScriptStreamEvent>,
}

/// One scripted event streamed ahead of a round's terminal response.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ScriptStreamEvent {
    /// Hold this stream until the test releases the rendezvous.
    Gate(crate::gate::ScriptGate),
    /// One visible reasoning block carrying this text: started, streamed and
    /// completed.
    Reasoning(String),
    /// Assistant text deltas.
    Text(String),
    /// A retry notice that discards everything this request streamed so far;
    /// what follows is the replay.
    RetryDiscarded,
    /// Hold the stream this many milliseconds, observing cancellation unless
    /// the plan's stalls ignore it.
    Stall(u64),
}

/// The content block one scripted round commits.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ScriptOutput {
    /// The fixture's ordinary final text.
    #[default]
    FinalText,
    /// Exact assistant text.
    Text(String),
    /// One requested tool call.
    Tool(ScriptToolCall),
    /// Several requested tool calls in one round, in order.
    Tools(Vec<ScriptToolCall>),
    /// One Code Mode cell: a call to the `exec` marker.
    ///
    /// The exec marker's input contract is `ToolInputContract::Utf8`, so the
    /// program is the call's arguments **verbatim** rather than a JSON object.
    /// Emitting it as JSON would be rejected by the contract.
    Cell(String),
    /// One Code Mode `wait`, whose contract is an ordinary structured object.
    Wait(ScriptWait),
    /// One omitted reasoning block and no text.
    ReasoningOnly,
    /// No output items at all.
    Empty,
    /// Assistant text naming each inline image the request's newest user
    /// item carries (`image/png 1234 bytes`), so a test sees what reached the
    /// model.
    ImageReport,
    /// Reports the text actually present in the next model request.
    HistoryReport,
}

/// One scripted tool call.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptToolCall {
    /// The exposed tool name, which must be registered in the binding.
    pub name: String,
    /// The call arguments, serialized to the provider's JSON string.
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// One scripted `wait` on a Code Mode cell.
///
/// `cell_id` is required by the marker's schema. Besides a real cell it admits
/// `none`, `null` and `all`, matched case-insensitively after trimming, which
/// lower onto the running cell or return the typed empty success when nothing
/// is running (spec 9.4).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptWait {
    /// The cell to wait on, or a lowered spelling.
    pub cell_id: String,
    /// How long to wait before yielding output.
    #[serde(default)]
    pub yield_time_ms: Option<u64>,
    /// Stop the running cell rather than waiting for more output.
    #[serde(default)]
    pub terminate: bool,
}

/// The stop reason a scripted round reports.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptFinish {
    /// Ordinary completion.
    #[default]
    Stop,
    /// The round requested tools.
    ToolCalls,
    /// The output bound was exhausted.
    Length,
    /// The model refused.
    Refusal,
}

/// Usage figures reported on a scripted terminal response.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptUsage {
    /// Prompt tokens. `null` reports this request's own projected size.
    #[serde(default)]
    pub input: Option<u64>,
    /// Completion tokens.
    #[serde(default)]
    pub output: u64,
    /// Reasoning tokens, which are part of `output`.
    #[serde(default)]
    pub reasoning: u64,
    /// Cached prompt tokens.
    #[serde(default)]
    pub cached_input: u64,
    /// Cache-write tokens.
    #[serde(default)]
    pub cache_write: u64,
}

/// A provider fault injected into one scripted round.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptFault {
    /// Optional canonical operation phase for typed host recovery tests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<plakous::model::ModelErrorPhase>,
    /// Optional canonical retry disposition; absent scripts keep Unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryability: Option<plakous::model::ModelRetryability>,
    /// Where in the operation the fault lands.
    pub at: ScriptFaultPhase,
    /// The error class the provider reports.
    pub kind: ScriptErrorKind,
    /// The bounded operator-visible message.
    #[serde(default)]
    pub message: Option<String>,
    /// For [`ScriptFaultPhase::InBand`], how many text deltas precede it.
    #[serde(default)]
    pub after_events: u32,
    /// For [`ScriptFaultPhase::Stalled`], how long the operation stalls first.
    #[serde(default)]
    pub stall_ms: u64,
}

/// Where a scripted fault lands in the provider operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptFaultPhase {
    /// The operation fails before it yields a stream, which is the shape of a
    /// refused request head (an HTTP status the client never streams).
    PreStream,
    /// The stream yields events and then an error, which is the shape of an
    /// in-band provider error frame.
    InBand,
    /// The operation stalls, then reports the error, which is the shape a
    /// client deadline turns into.
    Stalled,
}

/// The provider error class a scripted fault reports.
///
/// This is a closed projection of `plakous::model::ModelErrorKind`; the
/// binding maps it back. Keeping it closed means a script cannot name a class
/// the fixture has never been seen to produce.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptErrorKind {
    /// The route is unavailable (a 5xx head).
    Unavailable,
    /// The transport failed mid-stream.
    Transport,
    /// The operation exceeded its deadline.
    Timeout,
    /// The route refused for rate reasons.
    RateLimited,
    /// The stream ended before a complete response.
    IncompleteResponse,
    /// The operation's outcome is unknown.
    OutcomeUnknown,
    /// The protocol operation failed.
    Protocol,
    /// The provider refused the input as over the model's context window.
    ContextOverflow,
}

/// A refusal to read or admit a script document.
#[derive(Debug)]
pub struct ScriptError(String);

impl ScriptError {
    /// The bounded operator-visible reason.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ScriptError {}

impl ScriptPlan {
    /// Reads one script document from a file.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, is not the exact
    /// document shape, or names another [`SCRIPT_VERSION`].
    pub fn read(path: &Path) -> Result<Self, ScriptError> {
        let bytes = std::fs::read(path)
            .map_err(|error| ScriptError(format!("offline script {}: {error}", path.display())))?;
        let plan: Self = serde_json::from_slice(&bytes)
            .map_err(|error| ScriptError(format!("offline script {}: {error}", path.display())))?;
        plan.validate()?;
        Ok(plan)
    }

    /// Admits a script document.
    ///
    /// # Errors
    ///
    /// Returns an error when the document names another [`SCRIPT_VERSION`] or
    /// carries a step the fixture cannot replay.
    pub fn validate(&self) -> Result<(), ScriptError> {
        if self.version != SCRIPT_VERSION {
            return Err(ScriptError(format!(
                "offline script version {} is not {SCRIPT_VERSION}",
                self.version
            )));
        }
        if self.js_ptc && !self.code_mode {
            return Err(ScriptError(
                "offline script js_ptc requires code_mode".to_owned(),
            ));
        }
        if self.child_process_quiescent && !self.child_process {
            return Err(ScriptError(
                "offline script child_process_quiescent requires child_process".to_owned(),
            ));
        }
        // Each of these names a different tool registry, and the binding can
        // only build one. Refusing here says so; letting it through would
        // silently pick whichever branch the builder tests first, and the
        // script's author would debug a registry they did not ask for.
        let registries = [
            ("code_mode", self.code_mode),
            ("process_tools", self.process_tools),
            ("tree", self.tree),
        ]
        .into_iter()
        .filter_map(|(name, asked)| asked.then_some(name))
        .collect::<Vec<_>>();
        if registries.len() > 1 {
            return Err(ScriptError(format!(
                "offline script asks for more than one tool registry: {}",
                registries.join(", ")
            )));
        }
        if self.seal_family_artifacts && (self.code_mode || self.tree) {
            return Err(ScriptError(
                "offline script seal_family_artifacts requires a loopback or process registry"
                    .to_owned(),
            ));
        }
        // These dials change the tree the product persists. On a loopback or
        // Code Mode script they would be ignored, and a green run would be
        // about a tree that was never there.
        let tree_dials = [
            ("durable_tree", self.durable_tree),
            (
                "disk_full_at_child_locator",
                self.disk_full_at_child_locator,
            ),
            ("child_hold_ms", self.child_hold_ms != 0),
            ("child_hold_after", self.child_hold_after != 0),
            ("child_process", self.child_process),
            ("child_process_quiescent", self.child_process_quiescent),
            (
                "fail_child_after_successes",
                self.fail_child_after_successes.is_some(),
            ),
        ]
        .into_iter()
        .filter_map(|(name, asked)| asked.then_some(name))
        .collect::<Vec<_>>();
        if !tree_dials.is_empty() && !self.tree {
            return Err(ScriptError(format!(
                "offline script tree dials require tree: {}",
                tree_dials.join(", ")
            )));
        }
        if self.disk_full_at_child_locator && !self.durable_tree {
            return Err(ScriptError(
                "offline script disk_full_at_child_locator requires durable_tree".to_owned(),
            ));
        }
        if self
            .identity_namespace
            .is_some_and(|namespace| namespace > 0x0000_ffff_ffff_ffff)
        {
            return Err(ScriptError(
                "offline script identity_namespace does not fit in 48 bits".to_owned(),
            ));
        }
        for (index, step) in self.steps.iter().enumerate() {
            let calls = match &step.output {
                ScriptOutput::Tool(call) => std::slice::from_ref(call),
                ScriptOutput::Tools(calls) if calls.is_empty() => {
                    return Err(ScriptError(format!(
                        "offline script step {index} names no tools"
                    )));
                }
                ScriptOutput::Tools(calls) => calls.as_slice(),
                _ => &[],
            };
            if calls.iter().any(|call| call.name.trim().is_empty()) {
                return Err(ScriptError(format!(
                    "offline script step {index} names an empty tool"
                )));
            }
        }
        Ok(())
    }

    /// The step a one-based round replays, if any.
    #[must_use]
    pub fn step(&self, round: u64) -> Option<&ScriptStep> {
        let index = usize::try_from(round.saturating_sub(1)).unwrap_or(usize::MAX);
        self.steps.get(index).or_else(|| {
            if self.repeat_last_step {
                self.steps.last()
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "the document shape's own unit tests"
)]
mod tests {
    #[test]
    fn hosted_tool_call_final_dial_is_admitted() {
        let document = serde_json::json!({"version": 1, "hosted_tool_call_final": true});
        let plan: ScriptPlan = serde_json::from_value(document).expect("hosted final dial");
        plan.validate().expect("valid hosted final");
        assert_eq!(
            serde_json::to_value(plan).unwrap()["hosted_tool_call_final"],
            true
        );
    }

    use super::*;

    #[test]
    fn a_script_reads_its_closed_document_shape() {
        let plan: ScriptPlan = serde_json::from_str(
            r#"{"version":1,"steps":[
                {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls"},
                {"output":{"text":"done"},"usage":{"output":7}}
            ]}"#,
        )
        .expect("closed document");
        plan.validate().expect("admitted");
        assert_eq!(plan.maximum_projected_input, 10_000_000);
        assert!(matches!(
            plan.step(1).map(|step| &step.output),
            Some(ScriptOutput::Tool(call)) if call.name == "loopback_effect"
        ));
        assert_eq!(
            plan.step(2).and_then(|step| step.usage).map(|u| u.output),
            Some(7)
        );
        assert!(plan.step(3).is_none());
    }

    #[test]
    fn a_round_may_request_several_tools_but_not_none_or_an_empty_name() {
        let plan: ScriptPlan = serde_json::from_str(
            r#"{"version":1,"steps":[
                {"output":{"tools":[{"name":"edit"},{"name":"write"}]},"finish":"tool_calls"}
            ]}"#,
        )
        .expect("closed document");
        plan.validate().expect("admitted");
        assert!(matches!(
            plan.step(1).map(|step| &step.output),
            Some(ScriptOutput::Tools(calls))
                if calls.iter().map(|call| call.name.as_str()).eq(["edit", "write"])
        ));
        for refused in [
            r#"{"version":1,"steps":[{"output":{"tools":[]}}]}"#,
            r#"{"version":1,"steps":[{"output":{"tools":[{"name":"edit"},{"name":" "}]}}]}"#,
        ] {
            let plan: ScriptPlan = serde_json::from_str(refused).expect("document");
            assert!(plan.validate().is_err(), "{refused}");
        }
    }

    #[test]
    fn a_foreign_version_is_refused() {
        let plan: ScriptPlan =
            serde_json::from_str(r#"{"version":9,"steps":[]}"#).expect("document");
        assert!(plan.validate().is_err());
    }

    #[test]
    fn an_unknown_field_is_refused() {
        assert!(serde_json::from_str::<ScriptPlan>(r#"{"version":1,"nope":true}"#).is_err());
    }

    #[test]
    fn sealed_family_artifacts_require_a_supported_registry() {
        let admitted: ScriptPlan =
            serde_json::from_str(r#"{"version":1,"seal_family_artifacts":true,"steps":[]}"#)
                .expect("document");
        admitted.validate().expect("artifact loopback");
        assert!(admitted.seal_family_artifacts);
        for registry in ["tree", "code_mode"] {
            let mut document = serde_json::to_value(&admitted).expect("document");
            document[registry] = serde_json::json!(true);
            let refused: ScriptPlan = serde_json::from_value(document).expect("document");
            assert!(refused.validate().is_err());
        }
        let mut process = admitted;
        process.process_tools = true;
        process.validate().expect("artifact process registry");
    }

    #[test]
    fn an_identity_namespace_must_fit_the_high_half_of_a_call_id() {
        let admitted: ScriptPlan =
            serde_json::from_str(r#"{"version":1,"identity_namespace":2,"steps":[]}"#)
                .expect("document");
        admitted.validate().expect("48-bit namespace");
        let refused: ScriptPlan = serde_json::from_str(
            r#"{"version":1,"identity_namespace":281474976710656,"steps":[]}"#,
        )
        .expect("document");
        assert!(refused.validate().is_err());
    }

    #[test]
    fn tree_durability_dials_require_a_tree_and_durable_locator_publishes() {
        let bare: ScriptPlan =
            serde_json::from_str(r#"{"version":1,"child_hold_ms":1,"steps":[]}"#)
                .expect("document");
        assert!(bare.validate().unwrap_err().message().contains("tree"));
        let memory: ScriptPlan = serde_json::from_str(
            r#"{"version":1,"tree":true,"disk_full_at_child_locator":true,"steps":[]}"#,
        )
        .expect("document");
        assert!(
            memory
                .validate()
                .unwrap_err()
                .message()
                .contains("durable_tree")
        );
        let admitted: ScriptPlan = serde_json::from_str(
            r#"{"version":1,"tree":true,"durable_tree":true,"disk_full_at_child_locator":true,"child_hold_ms":1000,"child_hold_after":1,"fail_child_after_successes":2,"steps":[]}"#,
        )
        .expect("document");
        admitted.validate().expect("admitted");
        assert!(admitted.disk_full_at_child_locator);
        assert_eq!(admitted.fail_child_after_successes, Some(2));
    }

    #[test]
    fn a_repeating_script_replays_its_last_step() {
        let plan: ScriptPlan = serde_json::from_str(
            r#"{"version":1,"repeat_last_step":true,"steps":[{"output":{"text":"a"}}]}"#,
        )
        .expect("document");
        assert!(matches!(
            plan.step(4).map(|step| &step.output),
            Some(ScriptOutput::Text(text)) if text == "a"
        ));
    }
}
