//! Assembled model-visible lifecycle goldens (tool results, compaction, child mail, mid-session).
//!
//! Included from `agent.rs` under `#[cfg(test)]`. The inner `#[cfg(test)]`
//! module keeps `tools/checks/check_workspace.py` from treating golden helpers as
//! production CLI code. No production behaviour.

#[cfg(test)]
#[allow(
    clippy::too_many_lines,
    reason = "offline golden renderer walks tool-result, compaction, child-mail, and mid-session surfaces"
)]
mod suite {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use plakous::agent::{HarnessProfileSpec, ReasoningEffort};
    use plakous::model::{
        Actor, AttemptId, CallId, CanonicalHistory, CompactionBlock, ContentBlock,
        ContextHistoryPlan, ConversationItem, ConversationView, EntryId, HistorySegment,
        HistorySegmentKind, NamespaceId, NativeOptions, Provenance, RunId,
        SEMANTIC_COMPACTION_INSTRUCTION, SemanticCompactionCut, TextBlock, ToolCallerLineage,
        ToolCorrelation, ToolName, ToolResultPart, TurnId, WorkingProjection,
    };
    use plakous::subscriptions::NativeProvider;
    use plakous::tools::{
        CodexMinimalLocalCodeModeV1, CodexNativeLocalCodeModeV1, CodingPresentation,
        CompiledSurface, ExecutionEnvironment, ExecutionEnvironmentConfig,
        ExecutionEnvironmentScope, ProductJavaScriptCompositionV1, PythonPtcPresentationV1,
        PythonPtcV1, ToolInvocation, ToolInvocationRequest, ToolOutcome, ToolProjectionMap,
    };
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::super::rendered_prompts::suite::{
        Occupancy, ProviderKind, RouteSpec, SYNTHETIC_CWD, SYNTHETIC_ROOT, compile_projection,
        extra_names, golden_card, golden_dir, golden_epoch, instruction_fixture, native_profile,
        occupancy_tools, pin_macos, print_pins_requested, registry, render_digest,
        synthetic_target,
    };
    use super::super::{assignment_start_prompt, selected_harness_profile, submit_harness_prompt};

    /// The Codex Direct surface every lifecycle profile here is assembled
    /// against: no topology extensions, so no earned paragraph.
    fn lifecycle_surface() -> CompiledSurface {
        let extras = ProductToolExtensions::empty();
        product_surface(
            plakous::tools::JobConfig::default(),
            CodingPresentation::Codex,
            registry(
                ProductCompositionSelection::Direct,
                false,
                &extras,
                "golden-lifecycle-direct",
            ),
            synthetic_target(b"golden-lifecycle-direct"),
            &extras,
        )
        .expect("lifecycle surface")
    }
    use crate::agent_tree::text::errors::{
        topology_mail_request_invalid_message, topology_output_schema_mismatch_message,
        topology_submit_request_invalid_message, topology_wait_request_invalid_message,
    };
    use crate::agent_tree::{AwaitAgents, ProductAgentTree};
    use crate::agent_tree_protocol::{
        AttemptScopedRequest, ListAgentsRequest, MAX_MESSAGE_BYTES, MAX_OUTPUT_JSON_BYTES,
        MAX_WAIT_TARGETS, MessagePurpose, SendAgentMessageRequest, SpawnAgentRequest,
        SubmitResultRequest, TopologyErrorCode, WaitAgentRequest, WriterMode,
    };
    use crate::binding::CompositionSelection as ProductCompositionSelection;
    use crate::child_prompts::ChildAssignmentStartKind;
    use crate::tools::{ProductToolExtensions, product_surface};
    use plakous::prompts::identity::{
        PLAK_PRODUCT_ENVIRONMENT_TEMPLATE, PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER,
        PLAK_PRODUCT_MODEL_IDENTITY_TEMPLATE,
    };

    /// `crates/harness/tool-process/src/text.rs` `PLAKOUS_TOOL_OUTPUT_OMITTED`, rendered for 1234 bytes.
    const PLAKOUS_TOOL_OUTPUT_OMITTED: &str = "\n[... 1234 bytes of output omitted ...]\n";
    /// `crates/harness/tool-files/src/text.rs` `PLAKOUS_TOOL_PATCH_HUNK_CONFLICT`.
    const PLAKOUS_TOOL_PATCH_HUNK_CONFLICT: &str =
        "patch conflict: hunk context did not match; re-read the file and retry with current text";
    /// `crates/harness/tool-files/src/text.rs` `PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND`.
    const PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND: &str =
        "old_string was not found in the file; re-read it and copy the exact text";
    /// `crates/core/agent/src/runtime/prompts.rs` `PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED`.
    const PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED: &str = "Tool output was withheld for context capacity. The harness compacts automatically when enabled; retry with a smaller read or a narrower query afterwards.";
    /// `crates/core/agent/src/runtime/prompts.rs` `PLAKOUS_RUNTIME_OBSERVATION_OMITTED`.
    const PLAKOUS_RUNTIME_OBSERVATION_OMITTED: &str =
        "bytes omitted from the middle of this output to fit the context budget";
    /// `crates/core/agent/src/runtime/compaction/mod.rs` `NATIVE_COMPACTION_TRIGGER_V1`.
    const NATIVE_COMPACTION_TRIGGER_V1: &str = "native_responses_trigger_v1";
    const NATIVE_COMPACTION_NAMESPACE: &str = "plakous.compaction";

    const CHILD_SCHEMA: &str =
        r#"{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}"#;
    const CHILD_TURN_TOKEN: u64 = 1;
    const READ_SUCCESS: &str =
        r#"{"content":"hello\n","offset":1,"lines":1,"eof":true,"next_offset":null}"#;
    const SEARCH_HITS: &str = "matches=2 files_considered=1 files_scanned=1 bytes_scanned=24 skipped_binary=0 skipped_oversize=0 incomplete=false\nsrc/lib.rs:1:4:fn hello() {}\nsrc/lib.rs:2:4:fn world() {}";
    const PATCH_SUCCESS: &str = "changed_files=1";
    const PROCESS_OUTPUT: &str = "hello\n";

    fn lifecycle_dir() -> PathBuf {
        golden_dir().join("lifecycle")
    }

    fn pin_workspace_path(text: &str, actual_root: &Path) -> String {
        text.replace(&actual_root.display().to_string(), SYNTHETIC_ROOT)
    }

    fn fence(lang: &str, body: &str) -> String {
        format!("```{lang}\n{body}\n```\n\n")
    }

    fn surface(title: &str, note: &str, lang: &str, body: &str) -> String {
        let mut out = format!("### {title}\n\n");
        if !note.is_empty() {
            out.push_str(note);
            out.push_str("\n\n");
        }
        out.push_str(&fence(lang, body));
        out
    }

    fn ids(n: u128) -> (RunId, TurnId, AttemptId, CallId) {
        (
            RunId::from_str(&Uuid::from_u128(n).to_string()).expect("run"),
            TurnId::from_str(&Uuid::from_u128(n + 1).to_string()).expect("turn"),
            AttemptId::from_str(&Uuid::from_u128(n + 2).to_string()).expect("attempt"),
            CallId::from_str(&Uuid::from_u128(n + 3).to_string()).expect("call"),
        )
    }

    fn request(tool: &str, arguments: &str, n: u128) -> ToolInvocationRequest {
        let (run, turn, attempt, call) = ids(n);
        ToolInvocationRequest::new(
            run,
            turn,
            attempt,
            call,
            ToolName::new(tool).expect("tool name"),
            ToolCorrelation::new(None, None, ToolCallerLineage::default()).expect("correlation"),
            arguments,
        )
    }

    fn model_text(outcome: &ToolOutcome, invocation: &ToolInvocation) -> String {
        let result = outcome
            .to_model_result(invocation)
            .expect("to_model_result");
        result
            .parts()
            .iter()
            .filter_map(|part| match part {
                ToolResultPart::Text(text) => Some(text.text()),
                _ => None,
            })
            .collect()
    }

    /// Raising label derived from the raised body itself (`raising()` is
    /// crate-private in plakous-tools), so the golden's prose cannot drift from
    /// what `raise` actually produced.
    fn raising_note(body: &str, extra: &str) -> String {
        let wrapped = serde_json::from_str::<Value>(body)
            .is_ok_and(|value| value.get("result").is_some_and(Value::is_string));
        let mut note = if wrapped {
            "Raising: `TextResultObjectV1` → `{\"result\": …}`.".to_owned()
        } else {
            "Raising: `RawUtf8V1` (raw text, no envelope).".to_owned()
        };
        if !extra.is_empty() {
            note.push(' ');
            note.push_str(extra);
        }
        note
    }

    fn raise_text(
        map: &ToolProjectionMap,
        tool: &str,
        arguments: &str,
        outcome: ToolOutcome,
        n: u128,
    ) -> String {
        // `ToolProjectionReceiptV2` is pub in `plakous-tools` but not on the
        // plakous facade; keep it unnamed here.
        let prepared = map
            .prepare(request(tool, arguments, n))
            .unwrap_or_else(|error| {
                panic!("prepare {tool} (catalog {}): {error}", catalog_names(map))
            });
        let (invocation, receipt) = prepared.into_parts();
        let receipt = receipt.unwrap_or_else(|| panic!("receipt for {tool}"));
        let raised = map.raise(&receipt, outcome).expect("raise");
        model_text(&raised, &invocation)
    }

    fn canonical_process(output: &str) -> String {
        json!({
            "status": "exited",
            "exit_code": 0,
            "session_id": null,
            "cursor": 0,
            "output": output,
        })
        .to_string()
    }

    fn empty_extras() -> ProductToolExtensions {
        ProductToolExtensions::empty()
    }

    fn catalog_names(map: &ToolProjectionMap) -> String {
        map.entries()
            .iter()
            .map(|entry| entry.exposed_name().as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn lifecycle_route(
        provider: ProviderKind,
        composition: ProductCompositionSelection,
        model: &'static str,
        code_mode: bool,
    ) -> RouteSpec {
        RouteSpec {
            file_stem: "lifecycle".to_owned(),
            title: "lifecycle".to_owned(),
            composition,
            provider_wire: String::new(),
            provider,
            occupancy: Occupancy::SingleAgent,
            model,
            code_mode,
        }
    }

    fn direct_map(provider: ProviderKind, seed: &[u8]) -> ToolProjectionMap {
        compile_projection(
            &lifecycle_route(
                provider,
                ProductCompositionSelection::Direct,
                "test-model",
                false,
            ),
            &empty_extras(),
            synthetic_target(seed),
        )
        .unwrap_or_else(|error| panic!("direct projection: {error}"))
    }

    fn code_mode_nested() -> ToolProjectionMap {
        let extras = empty_extras();
        let extra = extra_names(&extras);
        let registry = registry(
            ProductCompositionSelection::FollowCatalog,
            true,
            &extras,
            "golden-lifecycle-code-mode",
        );
        CodexMinimalLocalCodeModeV1::compile_with_extensions(
            plakous::tools::JobConfig::default(),
            registry,
            synthetic_target(b"golden-lifecycle-code-mode"),
            &extra,
        )
        .expect("code mode")
        .nested_projection()
        .clone()
    }

    /// Seven-tool nested catalog (`read_file` / `search_repository` / `inspect_image`
    /// plus the Codex OAuth four). Product NativePtc goldens use the four-tool
    /// minimal set; this is the full `CodexNativeLocalCodeModeV1` nested map.
    fn full_code_nested() -> ToolProjectionMap {
        let extras = empty_extras();
        let registry = registry(
            ProductCompositionSelection::FollowCatalog,
            true,
            &extras,
            "golden-lifecycle-code-mode-full",
        );
        CodexNativeLocalCodeModeV1::compile(
            plakous::tools::JobConfig::default(),
            registry,
            synthetic_target(b"golden-lifecycle-code-mode-full"),
        )
        .expect("full code mode")
        .nested_projection()
        .clone()
    }

    fn js_ptc_nested(kind: CodingPresentation, seed: &[u8]) -> ToolProjectionMap {
        let extras = empty_extras();
        let registry = registry(
            ProductCompositionSelection::JsPtc,
            false,
            &extras,
            "golden-lifecycle-js-ptc",
        );
        let names = product_surface(
            plakous::tools::JobConfig::default(),
            kind,
            registry.clone(),
            synthetic_target(seed),
            &extras,
        )
        .expect("js surface")
        .exposed_names()
        .to_vec();
        ProductJavaScriptCompositionV1::compile_for_kind(
            plakous::tools::JobConfig::default(),
            registry,
            synthetic_target(seed),
            kind,
            names,
        )
        .expect("js ptc")
        .nested_projection()
        .clone()
    }

    fn js_ptc_nested_with(
        kind: CodingPresentation,
        extras: &ProductToolExtensions,
        seed: &[u8],
    ) -> ToolProjectionMap {
        let registry = registry(
            ProductCompositionSelection::JsPtc,
            false,
            extras,
            "golden-lifecycle-js-ptc",
        );
        let names = product_surface(
            plakous::tools::JobConfig::default(),
            kind,
            registry.clone(),
            synthetic_target(seed),
            extras,
        )
        .expect("js surface")
        .exposed_names()
        .to_vec();
        ProductJavaScriptCompositionV1::compile_for_kind(
            plakous::tools::JobConfig::default(),
            registry,
            synthetic_target(seed),
            kind,
            names,
        )
        .expect("js ptc")
        .nested_projection()
        .clone()
    }

    fn py_ptc_nested() -> ToolProjectionMap {
        let extras = empty_extras();
        let registry = registry(
            ProductCompositionSelection::PyPtc,
            false,
            &extras,
            "golden-lifecycle-py-ptc",
        );
        let names = product_surface(
            plakous::tools::JobConfig::default(),
            CodingPresentation::Grok,
            registry.clone(),
            synthetic_target(b"golden-lifecycle-py-ptc"),
            &extras,
        )
        .expect("py surface")
        .exposed_names()
        .to_vec();
        PythonPtcV1::compile_for_kind(
            plakous::tools::JobConfig::default(),
            registry,
            synthetic_target(b"golden-lifecycle-py-ptc"),
            PythonPtcPresentationV1::StructuredFunctions,
            CodingPresentation::Grok,
            names,
        )
        .expect("py ptc")
        .nested_projection()
        .clone()
    }

    fn workdir_failure(map: &ToolProjectionMap, tool: &str, arguments: &str) -> String {
        let tmp = tempfile::tempdir().expect("workdir fixture");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).expect("cwd");
        let (run, _, _, _) = ids(0x20);
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(run),
            root,
            "src",
            ExecutionEnvironmentConfig::default(),
        )
        .expect("environment");
        match map.prepare_in_environment(request(tool, arguments, 0x21), &environment) {
            Ok(_) => "prepare succeeded; absolute workdir was not rejected".to_owned(),
            // `normalized_tool_failure` in `agent/src/runtime/body.rs` prefixes
            // the prepare error; that is the model-visible tool-result text.
            Err(error) => format!("tool invocation failed: {error}"),
        }
    }

    fn render_tool_results() -> String {
        let mut md = String::new();
        md.push_str("# Lifecycle: tool results\n\n");
        md.push_str("## Header\n\n");
        md.push_str("| Field | Value |\n| --- | --- |\n");
        md.push_str("| what | Exact model-visible tool result text after `ToolProjectionMap::raise` then `ToolOutcome::to_model_result` |\n");
        md.push_str("| why | First-turn goldens show schemas; this file shows what comes back |\n");
        md.push_str("| production fns | `plakous_tools::exposure::ToolProjectionMap::raise`, `plakous_tools::outcome::ToolOutcome::to_model_result`, `coding.rs` result shapes, `prepare_in_environment` for absolute `workdir` |\n");
        md.push_str("| faithful vs synthetic | Raised envelopes are faithful. Canonical `ToolOutcome` bodies are synthetic copies of `coding.rs` formats (`read_file` stamp, process `into_outcome` JSON, search hit list, `changed_files=N`). Error bodies cite `crates/harness/tool-files/src/text.rs` and `crates/harness/tool-process/src/text.rs`. Observation-budget *runtime* replacement texts cite `crates/core/agent/src/runtime/prompts.rs` (private `clip_tool_result` is not called). |\n\n");

        md.push_str("Known/deferred: Code Mode `exec` bytes are upstream-pinned; nested `inspect_image` in PTC cells.\n\n");

        let codex = direct_map(ProviderKind::Codex, b"lifecycle-codex");
        let grok = direct_map(ProviderKind::Grok, b"lifecycle-grok");
        let claude = direct_map(ProviderKind::Claude, b"lifecycle-claude");
        let pi = direct_map(
            ProviderKind::OpenRouter {
                wire: crate::binding::ResolvedProtocol::Chat,
            },
            b"lifecycle-chat",
        );
        let code_nested = code_mode_nested();
        let full_code_nested = full_code_nested();
        let js_nested = js_ptc_nested(CodingPresentation::Grok, b"lifecycle-js-ptc");
        let py_nested = py_ptc_nested();

        md.push_str("## Nested catalogs\n\n");
        md.push_str(&format!(
            "- Product NativePtc (`CodexMinimalLocalCodeModeV1`): `{}`\n",
            catalog_names(&code_nested)
        ));
        md.push_str(&format!(
            "- Full Code Mode nested (`CodexNativeLocalCodeModeV1`): `{}`\n",
            catalog_names(&full_code_nested)
        ));
        md.push_str(&format!(
            "- JS PTC nested (SuperGrok OAuth Direct names): `{}`\n",
            catalog_names(&js_nested)
        ));
        md.push_str(&format!(
            "- Python PTC nested (SuperGrok OAuth Direct names): `{}`\n\n",
            catalog_names(&py_nested)
        ));
        md.push_str("Product NativePtc goldens use the four-tool minimal nested set: no `read_file` / `search_repository`. Those appear in the seven-tool `CodexNativeLocalCodeModeV1` nested map and in js-ptc/py-ptc cells. Every nested lineage raises the raw string (`RawUtf8V1`): js-ptc/py-ptc via `nested_cell_raising` (`coding_presentation.rs`), Code Mode nested via `capability_entry` (`codex_code_mode.rs`); Direct keeps `TextResultObjectV1`.\n\n");

        md.push_str("## Successful `read_file` (short)\n\n");
        md.push_str("Canonical `WorkspaceFileReadResult` JSON (no workspace generation):\n\n");
        md.push_str(&fence("json", READ_SUCCESS));
        md.push_str("Codex OAuth Direct and product-minimal Code Mode nested do not expose `read_file`.\n\n");
        md.push_str(&surface(
            "Grok Direct `read_file`",
            "Raising: `TextResultObjectV1` → `{\"result\": …}`.",
            "json",
            &raise_text(
                &grok,
                "read_file",
                r#"{"target_file":"src/lib.rs"}"#,
                ToolOutcome::success_text(READ_SUCCESS).expect("read"),
                0x30,
            ),
        ));
        md.push_str(&surface(
            "Claude Direct `Read`",
            "Raising: `TextResultObjectV1`.",
            "json",
            &raise_text(
                &claude,
                "Read",
                r#"{"file_path":"src/lib.rs"}"#,
                ToolOutcome::success_text(READ_SUCCESS).expect("read"),
                0x31,
            ),
        ));
        md.push_str(&surface(
            "OpenRouter pi Direct `read`",
            "Raising: `TextResultObjectV1`; pi's plain lines.",
            "json",
            &raise_text(
                &pi,
                "read",
                r#"{"path":"src/lib.rs"}"#,
                ToolOutcome::success_text(READ_SUCCESS).expect("read"),
                0x32,
            ),
        ));
        let body = raise_text(
            &full_code_nested,
            "read_file",
            r#"{"path":"src/lib.rs"}"#,
            ToolOutcome::success_text(READ_SUCCESS).expect("read"),
            0x33,
        );
        md.push_str(&surface(
            "Code Mode nested `read_file` (seven-tool `CodexNativeLocalCodeModeV1`)",
            &raising_note(&body, "Product NativePtc nested catalog omits `read_file`."),
            "text",
            &body,
        ));
        md.push_str(&surface(
            "JS PTC nested `read_file`",
            "Raising: `RawUtf8V1`.",
            "text",
            &raise_text(
                &js_nested,
                "read_file",
                r#"{"target_file":"src/lib.rs"}"#,
                ToolOutcome::success_text(READ_SUCCESS).expect("read"),
                0x34,
            ),
        ));
        md.push_str(&surface(
            "Python PTC nested `read_file`",
            "Raising: `RawUtf8V1`. Python cells return a JSON-encoded UTF-8 string; parse it and use `content`.",
            "text",
            &raise_text(
                &py_nested,
                "read_file",
                r#"{"target_file":"src/lib.rs"}"#,
                ToolOutcome::success_text(READ_SUCCESS).expect("read"),
                0x35,
            ),
        ));

        let process_canonical = canonical_process(PROCESS_OUTPUT);
        md.push_str("## Successful process (`exec_command` / `run_terminal_command` / `Bash`)\n\n");
        md.push_str("Canonical `ProcessExecutionResult::into_outcome` JSON before raise:\n\n");
        md.push_str(&fence("json", &process_canonical));
        md.push_str(&surface(
            "Codex Direct `exec_command`",
            "Process raising → `RaisedProcessResultWireV2` `{exit_code, output}` (no `session_id` when joined).",
            "json",
            &raise_text(
                &codex,
                "exec_command",
                r#"{"cmd":"true"}"#,
                ToolOutcome::success_text(&process_canonical).expect("process"),
                0x40,
            ),
        ));
        md.push_str(&surface(
            "Grok Direct `run_terminal_command`",
            "Same process raising as Codex.",
            "json",
            &raise_text(
                &grok,
                "run_terminal_command",
                r#"{"command":"true","description":"verify"}"#,
                ToolOutcome::success_text(&process_canonical).expect("process"),
                0x41,
            ),
        ));
        md.push_str(&surface(
            "Claude Direct `Bash`",
            "Same process raising.",
            "json",
            &raise_text(
                &claude,
                "Bash",
                r#"{"command":"true"}"#,
                ToolOutcome::success_text(&process_canonical).expect("process"),
                0x42,
            ),
        ));
        md.push_str(&surface(
            "OpenRouter pi Direct `bash`",
            "Same process raising.",
            "json",
            &raise_text(
                &pi,
                "bash",
                r#"{"command":"true"}"#,
                ToolOutcome::success_text(&process_canonical).expect("process"),
                0x43,
            ),
        ));
        md.push_str(&surface(
            "JS PTC nested `run_terminal_command`",
            "Nested text raising is `RawUtf8V1`, but process tools keep process raising, so nested cells still see `{exit_code, output}`, not the canonical `status/cursor` JSON.",
            "json",
            &raise_text(
                &js_nested,
                "run_terminal_command",
                r#"{"command":"true","description":"verify"}"#,
                ToolOutcome::success_text(&process_canonical).expect("process"),
                0x44,
            ),
        ));
        md.push_str(&surface(
            "Product NativePtc nested `exec_command`",
            "The four-tool nested catalog includes `exec_command` (the outer cell is `exec`/`wait`). Same process raising as Direct. Deferred: outer Code Mode `exec` bytes are upstream-pinned.",
            "json",
            &raise_text(
                &code_nested,
                "exec_command",
                r#"{"cmd":"true"}"#,
                ToolOutcome::success_text(&process_canonical).expect("process"),
                0x45,
            ),
        ));

        let promoted = canonical_process("hello\nfull output: /state/jobs/s/b-p3.log");
        md.push_str("## Promotion result names `full output:`\n\n");
        md.push_str(
            "A promoted command's canonical `output` ends with the `full output:` line (`push_output_file` in `crates/harness/tool-process/src/lib.rs`). Process raising keeps that line inside `output`.\n\n",
        );
        md.push_str(&surface(
            "Codex Direct `exec_command` promotion",
            "The last line of `output` is the issued path.",
            "json",
            &raise_text(
                &codex,
                "exec_command",
                r#"{"cmd":"true"}"#,
                ToolOutcome::success_text(&promoted).expect("promoted"),
                0x46,
            ),
        ));
        md.push_str(&surface(
            "Grok Direct `run_terminal_command` promotion",
            "Same process raising. Grok reads the path with `read_file`.",
            "json",
            &raise_text(
                &grok,
                "run_terminal_command",
                r#"{"command":"true","description":"verify"}"#,
                ToolOutcome::success_text(&promoted).expect("promoted"),
                0x47,
            ),
        ));
        md.push_str(&surface(
            "Claude Direct `Bash` promotion",
            "Same process raising. Claude reads the path with `Read`.",
            "json",
            &raise_text(
                &claude,
                "Bash",
                r#"{"command":"true"}"#,
                ToolOutcome::success_text(&promoted).expect("promoted"),
                0x48,
            ),
        ));
        md.push_str(&surface(
            "OpenRouter pi Direct `bash` promotion",
            "Same process raising. Pi reads the path with `read`.",
            "json",
            &raise_text(
                &pi,
                "bash",
                r#"{"command":"true"}"#,
                ToolOutcome::success_text(&promoted).expect("promoted"),
                0x49,
            ),
        ));

        let (_hub_tmp, _hub_home, hub_root, _hub_cwd) = instruction_fixture();
        let hubs = occupancy_tools(&hub_root);
        let grok_direct_hub = compile_projection(
            &lifecycle_route(
                ProviderKind::Grok,
                ProductCompositionSelection::Direct,
                "test-model",
                false,
            ),
            &hubs.grok_root,
            synthetic_target(b"lifecycle-grok-hub"),
        )
        .expect("grok hub projection");
        let pi_direct_hub = compile_projection(
            &lifecycle_route(
                ProviderKind::OpenRouter {
                    wire: crate::binding::ResolvedProtocol::Chat,
                },
                ProductCompositionSelection::Direct,
                "test-model",
                false,
            ),
            &hubs.pi_root,
            synthetic_target(b"lifecycle-pi-hub"),
        )
        .expect("pi hub projection");
        let grok_cell = js_ptc_nested_with(
            CodingPresentation::Grok,
            &hubs.grok_nested_parent,
            b"lifecycle-grok-cell",
        );
        let pi_cell = js_ptc_nested_with(
            CodingPresentation::Pi,
            &hubs.pi_nested_parent,
            b"lifecycle-pi-cell",
        );
        let receipt = json!({
            "mail": [],
            "woke_by": "timeout",
            "agents": [{"agent_id": "c1", "status": "running"}],
            "timed_out": true,
            "processes": [],
            "jobs": []
        });
        let raw_receipt = receipt.to_string();
        let direct_receipt = json!({"result": raw_receipt}).to_string();
        md.push_str("## Pi and Grok await receipts\n\n");
        md.push_str(
            "Synthetic `AwaitOutput`. Direct raising is `ModelContentIdentityV1`: the executor already wrapped the receipt as `{\"result\": \"<receipt json>\"}` (`present_task_receipt`), and raise leaves that text. A js-ptc cell returns the raw receipt (`js_ptc_grok_hub_child_await_returns_the_raw_receipt`).\n\n",
        );
        md.push_str(&surface(
            "Grok Direct hub `get_command_or_subagent_output`",
            "Envelope `{\"result\": \"<receipt json>\"}`.",
            "json",
            &raise_text(
                &grok_direct_hub,
                "get_command_or_subagent_output",
                r#"{"task_ids":["c1"],"timeout_ms":300000}"#,
                ToolOutcome::success_text(&direct_receipt).expect("receipt"),
                0x4a,
            ),
        ));
        md.push_str(&surface(
            "Grok js-ptc cell `get_command_or_subagent_output`",
            "Raw receipt, no topology envelope.",
            "json",
            &raise_text(
                &grok_cell,
                "get_command_or_subagent_output",
                r#"{"task_ids":["c1"],"timeout_ms":300000}"#,
                ToolOutcome::success_text(&raw_receipt).expect("receipt"),
                0x4b,
            ),
        ));
        md.push_str(&surface(
            "OpenRouter pi Direct hub `wait`",
            "Envelope `{\"result\": \"<receipt json>\"}`.",
            "json",
            &raise_text(
                &pi_direct_hub,
                "wait",
                r#"{"ids":["c1"],"timeout_ms":300000}"#,
                ToolOutcome::success_text(&direct_receipt).expect("receipt"),
                0x4c,
            ),
        ));
        md.push_str(&surface(
            "OpenRouter pi js-ptc cell `wait`",
            "Raw receipt, no topology envelope.",
            "json",
            &raise_text(
                &pi_cell,
                "wait",
                r#"{"ids":["c1"],"timeout_ms":300000}"#,
                ToolOutcome::success_text(&raw_receipt).expect("receipt"),
                0x4d,
            ),
        ));

        md.push_str("## `search_repository` hit list\n\n");
        md.push_str("Canonical `coding.rs` bounded hit list:\n\n");
        md.push_str(&fence("text", SEARCH_HITS));
        md.push_str(
            "Codex OAuth Direct and product-minimal Code Mode nested do not expose search. Claude OAuth Direct exposes no search tool: it searches through `Bash`. Seven-tool Code Mode nested and js-ptc/py-ptc expose `search_repository` or the vertical's Direct search names.\n\n",
        );
        md.push_str(&surface(
            "Grok Direct `grep`",
            "From `PLAKOUS_TOOL_SEARCH_*` formatting in `coding.rs`; raising `TextResultObjectV1`.",
            "json",
            &raise_text(
                &grok,
                "grep",
                r#"{"pattern":"hello"}"#,
                ToolOutcome::success_text(SEARCH_HITS).expect("search"),
                0x50,
            ),
        ));
        let body = raise_text(
            &full_code_nested,
            "search_repository",
            r#"{"path":"src","query":"hello"}"#,
            ToolOutcome::success_text(SEARCH_HITS).expect("search"),
            0x53,
        );
        md.push_str(&surface(
            "Code Mode nested `search_repository` (seven-tool)",
            &raising_note(&body, ""),
            "text",
            &body,
        ));
        md.push_str(&surface(
            "JS PTC nested `grep`",
            "Raising `RawUtf8V1`.",
            "text",
            &raise_text(
                &js_nested,
                "grep",
                r#"{"pattern":"hello"}"#,
                ToolOutcome::success_text(SEARCH_HITS).expect("search"),
                0x54,
            ),
        ));

        md.push_str("## `apply_patch` success\n\n");
        md.push_str("Canonical success from `coding.rs`: `changed_files=N`.\n\n");
        md.push_str(&surface(
            "Codex Direct `apply_patch`",
            "Raising `TextResultObjectV1`.",
            "json",
            &raise_text(
                &codex,
                "apply_patch",
                r#"{"patch":"*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch"}"#,
                ToolOutcome::success_text(PATCH_SUCCESS).expect("patch"),
                0x60,
            ),
        ));
        md.push_str(&surface(
            "Grok Direct `search_replace`",
            "Grok presents apply as `search_replace`. Same canonical success text.",
            "json",
            &raise_text(
                &grok,
                "search_replace",
                r#"{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
                ToolOutcome::success_text(PATCH_SUCCESS).expect("patch"),
                0x61,
            ),
        ));
        md.push_str(&surface(
            "Claude Direct `Edit`",
            "Raising `TextResultObjectV1`.",
            "json",
            &raise_text(
                &claude,
                "Edit",
                r#"{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
                ToolOutcome::success_text(PATCH_SUCCESS).expect("patch"),
                0x62,
            ),
        ));
        md.push_str(&surface(
            "OpenRouter pi Direct `edit`",
            "Raising `TextResultObjectV1`.",
            "json",
            &raise_text(
                &pi,
                "edit",
                r#"{"path":"src/lib.rs","edits":[{"oldText":"a","newText":"b"}]}"#,
                ToolOutcome::success_text(PATCH_SUCCESS).expect("patch"),
                0x63,
            ),
        ));
        let body = raise_text(
            &code_nested,
            "apply_patch",
            r#"{"patch":"x"}"#,
            ToolOutcome::success_text(PATCH_SUCCESS).expect("patch"),
            0x64,
        );
        md.push_str(&surface(
            "Code Mode nested `apply_patch` (product NativePtc four-tool)",
            &raising_note(&body, ""),
            "text",
            &body,
        ));
        md.push_str(&surface(
            "JS PTC nested `search_replace`",
            "Raising `RawUtf8V1`.",
            "text",
            &raise_text(
                &js_nested,
                "search_replace",
                r#"{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
                ToolOutcome::success_text(PATCH_SUCCESS).expect("patch"),
                0x65,
            ),
        ));

        md.push_str("## Patch-conflict failure (recovery text)\n\n");
        md.push_str("Failures skip `TextResultObjectV1` wrapping (`raise_text_result_object` returns the outcome as-is when not `Success`).\n\n");
        md.push_str(&surface(
            "Codex Direct `apply_patch` hunk conflict",
            "From `PLAKOUS_TOOL_PATCH_HUNK_CONFLICT`.",
            "text",
            &raise_text(
                &codex,
                "apply_patch",
                r#"{"patch":"x"}"#,
                ToolOutcome::failure_text(PLAKOUS_TOOL_PATCH_HUNK_CONFLICT).expect("fail"),
                0x70,
            ),
        ));
        md.push_str(&surface(
            "Grok Direct `search_replace` not found",
            "From `PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND` (Grok search_replace path, not unified-diff hunk conflict).",
            "text",
            &raise_text(
                &grok,
                "search_replace",
                r#"{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
                ToolOutcome::failure_text(PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND).expect("fail"),
                0x71,
            ),
        ));
        md.push_str(&surface(
            "Code Mode nested `apply_patch` hunk conflict",
            "From `PLAKOUS_TOOL_PATCH_HUNK_CONFLICT`. Failures skip the `{\"result\": …}` wrap.",
            "text",
            &raise_text(
                &code_nested,
                "apply_patch",
                r#"{"patch":"x"}"#,
                ToolOutcome::failure_text(PLAKOUS_TOOL_PATCH_HUNK_CONFLICT).expect("fail"),
                0x72,
            ),
        ));
        md.push_str(&surface(
            "JS PTC nested `search_replace` not found",
            "From `PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND`. Failures are already raw text.",
            "text",
            &raise_text(
                &js_nested,
                "search_replace",
                r#"{"file_path":"src/lib.rs","old_string":"a","new_string":"b"}"#,
                ToolOutcome::failure_text(PLAKOUS_TOOL_SEARCH_REPLACE_NOT_FOUND).expect("fail"),
                0x73,
            ),
        ));

        let truncated_canonical =
            canonical_process(&format!("head{PLAKOUS_TOOL_OUTPUT_OMITTED}tail"));
        md.push_str("## Observation budget\n\n");
        md.push_str("### Tool-level omission (`PLAKOUS_TOOL_OUTPUT_OMITTED`)\n\n");
        md.push_str("A process page over its byte cap keeps its head and tail with the omitted byte count between them, then raise runs.\n\n");
        md.push_str(&surface(
            "Codex Direct `exec_command` truncated",
            "Process raising keeps the marker inside `output`.",
            "json",
            &raise_text(
                &codex,
                "exec_command",
                r#"{"cmd":"true"}"#,
                ToolOutcome::success_text(&truncated_canonical).expect("trunc"),
                0x80,
            ),
        ));
        md.push_str(&surface(
            "OpenRouter pi Direct `read` line-limited page",
            "Pi's read keeps the exact lines and ends a cut page with where to continue; Direct wraps the result string.",
            "json",
            &raise_text(
                &pi,
                "read",
                r#"{"path":"src/lib.rs"}"#,
                ToolOutcome::success_text(
                    r#"{"content":"xxxxx\n","offset":1,"lines":1,"eof":false,"next_offset":2}"#,
                )
                .expect("paged read"),
                0x81,
            ),
        ));
        md.push_str("### Runtime turn observation budget (`clip_tool_result` in `agent/src/runtime/body.rs`)\n\n");
        md.push_str("Applied after raise, on the model-visible `ToolResultBlock`. Remaining budget `0` on unstructured text yields `PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED`. Overflow of text or a process `output` keeps about 30% head and 70% tail on line boundaries, joined by one `[N PLAKOUS_RUNTIME_OBSERVATION_OMITTED]` line; a paged read stays head-first.\n\n");
        md.push_str(&surface(
            "Exhausted replacement",
            "From `PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED` (`crates/core/agent/src/runtime/prompts.rs`). Private `clip_tool_result` returns this const verbatim when remaining is 0 and the body is not structured `{\"output\":…}` / neutral read JSON.",
            "text",
            PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED,
        ));
        md.push_str(&surface(
            "Omitted-middle marker",
            "From `PLAKOUS_RUNTIME_OBSERVATION_OMITTED`, rendered for a 120000-byte omission. Distinct from the tool-level `\\n[observation truncated]`.",
            "text",
            &format!("[120000 {PLAKOUS_RUNTIME_OBSERVATION_OMITTED}]"),
        ));

        md.push_str("## Argument-validation failure (absolute `workdir`)\n\n");
        md.push_str("Driven through `ToolProjectionMap::prepare_in_environment`. The prepare error is **not** from `crates/harness/tool-files/src/text.rs`; it is `projection_workdir_error` in `exposure.rs`. The runtime then prefixes it via private `normalized_tool_failure` (`tool invocation failed: …`). Grok `run_terminal_command`, Claude `Bash` and pi `bash` do not accept `workdir` (hardcoded `.`).\n\n");
        md.push_str(&surface(
            "Codex Direct `exec_command` absolute workdir",
            "Faithful prepare failure.",
            "text",
            &workdir_failure(
                &codex,
                "exec_command",
                r#"{"cmd":"true","workdir":"/tmp/outside"}"#,
            ),
        ));

        md.push_str("## Nested raw-string vs Direct `{\"result\": …}` envelope\n\n");
        md.push_str("Same canonical read success, Direct vs nested raise:\n\n");
        md.push_str(&surface(
            "Direct envelope (Grok)",
            "Faithful `TextResultObjectV1`.",
            "json",
            &raise_text(
                &grok,
                "read_file",
                r#"{"target_file":"src/lib.rs"}"#,
                ToolOutcome::success_text(READ_SUCCESS).expect("read"),
                0x90,
            ),
        ));
        let body = raise_text(
            &full_code_nested,
            "read_file",
            r#"{"path":"src/lib.rs"}"#,
            ToolOutcome::success_text(READ_SUCCESS).expect("read"),
            0x91,
        );
        md.push_str(&surface(
            "Code Mode nested (seven-tool) raw string",
            &raising_note(&body, "Nested lineage raises raw text (G1-2)."),
            "text",
            &body,
        ));
        md.push_str(&surface(
            "JS PTC nested raw string",
            "Faithful `RawUtf8V1` via `nested_cell_raising`.",
            "text",
            &raise_text(
                &js_nested,
                "read_file",
                r#"{"target_file":"src/lib.rs"}"#,
                ToolOutcome::success_text(READ_SUCCESS).expect("read"),
                0x92,
            ),
        ));
        md.push_str(&surface(
            "Python PTC nested raw string",
            "Faithful `RawUtf8V1`.",
            "text",
            &raise_text(
                &py_nested,
                "read_file",
                r#"{"target_file":"src/lib.rs"}"#,
                ToolOutcome::success_text(READ_SUCCESS).expect("read"),
                0x93,
            ),
        ));
        md.push_str("Process tools are the exception: nested process raising still emits `{exit_code, output}` (see JS PTC `run_terminal_command` above), not the canonical `status/cursor` object and not a `{\"result\": …}` wrapper.\n");
        md
    }

    fn entry(n: u128, actor: Actor, text: &str, provenance: Provenance) -> ConversationItem {
        ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(n)).expect("entry"),
            actor,
            vec![ContentBlock::Text(
                TextBlock::new(text, None).expect("text"),
            )],
            provenance,
        )
        .expect("item")
    }

    fn render_segment(index: usize, segment: &HistorySegment) -> String {
        let mut body = format!("segment[{index}] kind={:?}\n", segment.kind());
        for item in segment.items() {
            let texts: Vec<&str> = item
                .blocks()
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text()),
                    ContentBlock::Compaction(compaction) => Some(compaction.summary()),
                    _ => None,
                })
                .collect();
            body.push_str(&format!("  {:?}: {}\n", item.actor(), texts.join(" | ")));
        }
        body
    }

    fn render_view(view: &ConversationView) -> String {
        let mut body = String::new();
        body.push_str("history:\n");
        for (index, segment) in view.history().segments().iter().enumerate() {
            body.push_str(&render_segment(index, segment));
        }
        body.push_str("working: ");
        if view.working().segments().is_empty() {
            body.push_str("(empty)\n");
        } else {
            body.push('\n');
            for (index, segment) in view.working().segments().iter().enumerate() {
                body.push_str(&render_segment(index, segment));
            }
        }
        body
    }

    fn render_compaction() -> String {
        let mut md = String::new();
        md.push_str("# Lifecycle: compaction\n\n");
        md.push_str("## Header\n\n");
        md.push_str("| Field | Value |\n| --- | --- |\n");
        md.push_str("| what | Compaction request as the model sees it, and how the summary is re-injected on the next coding turn |\n");
        md.push_str("| why | Product semantic compact vs Codex native trigger |\n");
        md.push_str("| production fns | `plakous_model::prompts::PLAKOUS_SEMANTIC_COMPACTION_INSTRUCTION` / `SEMANTIC_COMPACTION_INSTRUCTION`; `SemanticCompactionCut::seal` / `summary_input` / `build_successor`; `runtime/compaction/request.rs` `harness_compaction_prefix` (ToolChoice::None, same system prefix, trailing user instruction); `NATIVE_COMPACTION_TRIGGER_V1` in `NativeOptions` |\n");
        md.push_str("| faithful vs synthetic | Instruction const is faithful. History is a synthetic conversation sealed through the real cut. Native trigger string is the production const value. Wire `compaction_trigger` item lives in `target/prompt-renders/rendered/wire/**`. |\n\n");

        let segments = vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![entry(
                    1,
                    Actor::User,
                    "old intent: fix the parser",
                    Provenance::UserInput,
                )],
            )
            .expect("old intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![entry(
                    2,
                    Actor::Assistant,
                    "old answer: added error paths in src/parser.rs",
                    Provenance::ModelOutput,
                )],
            )
            .expect("old answer"),
            HistorySegment::new(
                HistorySegmentKind::Steering,
                vec![entry(
                    3,
                    Actor::User,
                    "also keep tests green",
                    Provenance::UserInput,
                )],
            )
            .expect("steering"),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![entry(
                    4,
                    Actor::User,
                    "latest intent: ship the fix",
                    Provenance::UserInput,
                )],
            )
            .expect("latest intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![entry(
                    5,
                    Actor::Assistant,
                    "latest answer: waiting on review",
                    Provenance::ModelOutput,
                )],
            )
            .expect("latest answer"),
        ];
        let history = CanonicalHistory::new(segments).expect("history");
        let source = ConversationView::from_shared(history, WorkingProjection::default());
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        let cut = SemanticCompactionCut::seal(&source, &plan).expect("cut");

        md.push_str("## Semantic (product) compaction request\n\n");
        md.push_str("Same system/prefix as the live turn (cache affinity). The compaction instruction is only the trailing user item. `ToolChoice::None`. Coding-runtime compact keeps reasoning enabled.\n\n");
        md.push_str(&surface(
            "Trailing user instruction",
            "Faithful `PLAKOUS_SEMANTIC_COMPACTION_INSTRUCTION` (`crates/core/model/src/prompts.rs`).",
            "text",
            SEMANTIC_COMPACTION_INSTRUCTION,
        ));
        md.push_str(&surface(
            "Source history (synthetic conversation)",
            "Faithful segment kinds and item text; synthetic content.",
            "text",
            &render_view(&source),
        ));
        md.push_str(&surface(
            "`cut.summary_input()` — what is fed to the summarizer",
            "Faithful `SemanticCompactionCut::summary_input` (selected prefix; working empty). Retained tail is omitted from this request.",
            "text",
            &render_view(cut.summary_input()),
        ));
        md.push_str(&format!(
            "Selected segments: {}. Retained segments: {}.\n\n",
            cut.selected_segments().len(),
            cut.retained_segment_count()
        ));

        let summary_item = ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(99)).expect("summary id"),
            Actor::Assistant,
            vec![ContentBlock::Compaction(
                CompactionBlock::new(
                    "Summary of earlier work: Parser error paths were added in src/parser.rs. Tests must stay green. Review is outstanding.",
                )
                .expect("summary block"),
            )],
            Provenance::ModelOutput,
        )
        .expect("summary item");
        let summary = HistorySegment::new(HistorySegmentKind::Compaction, vec![summary_item])
            .expect("summary segment");
        let successor = cut.build_successor(&summary).expect("successor");
        let next_turn = ConversationView::from_shared(successor, WorkingProjection::default());
        md.push_str(&surface(
            "Next coding turn after semantic compact",
            "Faithful `build_successor`: position-zero compaction segment, then retained handles. This is what the next coding request history contains.",
            "text",
            &render_view(&next_turn),
        ));

        md.push_str("## Codex native trigger (`NATIVE_COMPACTION_TRIGGER_V1`)\n\n");
        md.push_str("Native compact does **not** append the semantic instruction as a trailing user item. `native_compaction_dispatch_prefix` copies the live prefix and sets `NativeOptions` namespace `plakous.compaction` to `native_responses_trigger_v1`. The Responses dialect then emits a `{ \"type\": \"compaction_trigger\" }` input item (wire goldens).\n\n");
        let mut entries = BTreeMap::new();
        entries.insert(
            NamespaceId::new(NATIVE_COMPACTION_NAMESPACE).expect("namespace"),
            Value::String(NATIVE_COMPACTION_TRIGGER_V1.to_owned()),
        );
        let native = NativeOptions::new(entries);
        let native_json = json!({
            NATIVE_COMPACTION_NAMESPACE: native.entries().get(
                &NamespaceId::new(NATIVE_COMPACTION_NAMESPACE).expect("namespace")
            )
        });
        md.push_str(&surface(
            "NativeOptions enrolled for native compact",
            "Faithful const `NATIVE_COMPACTION_TRIGGER_V1` from `crates/core/agent/src/runtime/compaction/mod.rs`.",
            "json",
            &serde_json::to_string_pretty(&native_json).expect("native json"),
        ));
        md.push_str("Native successor keeps newest user/developer/system messages under a 64k token budget, inserts one compaction item, then keeps the current round's tool batch with everything after it verbatim when that successor fits and shrinks (otherwise it ends at the compaction item). That fold is not sealed through `SemanticCompactionCut::build_successor` (`request.version >= 3` uses `native_compaction_successor`).\n");
        md
    }

    fn topology_failure(code: TopologyErrorCode, message: String) -> String {
        json!({ "code": code, "message": message }).to_string()
    }

    /// `list_agents` occupancy (`dirty`/`branch`/`ahead`/`behind`) is live VCS
    /// of the checkout. Pin it so the golden is independent of git/jj state.
    fn pin_list_agents(text: &str, root: &Path) -> String {
        let pinned = pin_workspace_path(text, root);
        let mut parsed: Value = serde_json::from_str(&pinned).expect("list json");
        if let Some(agents) = parsed.get_mut("agents").and_then(Value::as_array_mut) {
            for agent in agents {
                let Some(obj) = agent.as_object_mut() else {
                    continue;
                };
                obj.insert("dirty".to_owned(), Value::Bool(false));
                obj.remove("branch");
                obj.remove("ahead");
                obj.remove("behind");
            }
        }
        pin_elapsed(&mut parsed);
        parsed.to_string()
    }

    /// `usage.elapsed_ms` is a wall-clock measurement in milliseconds;
    /// pin it so the golden does not drift by the millisecond
    /// the test took.
    fn pin_elapsed(parsed: &mut Value) {
        if let Some(agents) = parsed.get_mut("agents").and_then(Value::as_array_mut) {
            for agent in agents {
                if let Some(elapsed) = agent
                    .get_mut("usage")
                    .and_then(|usage| usage.get_mut("elapsed_ms"))
                {
                    *elapsed = Value::from(0);
                }
            }
        }
    }

    fn pin_wait_rows(text: &str, root: &Path) -> String {
        let pinned = pin_workspace_path(text, root);
        let mut parsed: Value = serde_json::from_str(&pinned).expect("wait json");
        pin_elapsed(&mut parsed);
        parsed.to_string()
    }

    fn render_child_mail(root: &Path) -> String {
        let mut md = String::new();
        md.push_str("# Lifecycle: child mail\n\n");
        md.push_str("## Header\n\n");
        md.push_str("| Field | Value |\n| --- | --- |\n");
        md.push_str("| what | Child Start/reminder/force prompts, parent-visible topology tool JSON, topology error samples |\n");
        md.push_str("| why | First-turn route Markdown embeds genesis Start in the child wire user turn; this file shows the rest of the child lifecycle |\n");
        md.push_str("| production fns | `assignment_start_prompt` / `submit_harness_prompt` (`crates/hosting/hosted/src/agent/children.rs` filling `crates/harness/prompts/src/child.rs`); `ProductAgentTree::{list_agents,timeout_wait,send_agent_message,interrupt_agent,close_agent,record_turn_completed,record_host_materialization,submit_result}`; topology tools wrap payloads with `serde_json::json!(output).to_string()`; errors from `prompts/errors.rs` |\n");
        md.push_str("| faithful vs synthetic | Prompt fills and tree JSON are faithful. Workspace `path` is pinned to `/work/repo`. `list_agents` occupancy (`dirty`/`branch`/`ahead`/`behind`) is pinned (live VCS would drift). Mail ids are tree-assigned `m1`. |\n\n");

        md.push_str("## Start prompt\n\n");
        md.push_str("Genesis Start (without mail) is in the first user turn of child route wire JSON, e.g. `target/prompt-renders/rendered/wire/codex.native-ptc.child.request.json` and the matching `.md` Request fence. Not duplicated here. Production fill is `assignment_start_prompt` over the composition-specific child Start templates in `prompts/child.rs`.\n\n");

        let reminder = submit_harness_prompt(1, CHILD_TURN_TOKEN, CHILD_SCHEMA);
        let force = submit_harness_prompt(2, CHILD_TURN_TOKEN, CHILD_SCHEMA);
        md.push_str(&surface(
            "Reminder nudge (nudge=1)",
            "Faithful `submit_harness_prompt` → `PLAK_CHILD_SUBMIT_NUDGE_REMINDER_TEMPLATE`.",
            "text",
            &reminder,
        ));
        md.push_str(&surface(
            "Force nudge (nudge≥2)",
            "Faithful `submit_harness_prompt` → `PLAK_CHILD_SUBMIT_NUDGE_FORCE_TEMPLATE`.",
            "text",
            &force,
        ));
        let start_with_mail = assignment_start_prompt(
            ChildAssignmentStartKind::ToolsPtc,
            "reviewer",
            crate::agent_tree_protocol::Tier::ReadOnly,
            "reviewer",
            "Review src/parser.rs for missing error paths and return a summary.",
            CHILD_SCHEMA,
            CHILD_TURN_TOKEN,
            Some("please also check error-path tests"),
            Some(SYNTHETIC_ROOT),
        );
        md.push_str(&surface(
            "Start prompt with mail body (for comparison; genesis without mail is in child route wire user turn)",
            "Faithful `assignment_start_prompt` plus `PLAK_CHILD_ASSIGNMENT_MESSAGE_PREFIX`.",
            "text",
            &start_with_mail,
        ));

        // the route goldens only exercise a read_only child, so
        // the other two tier lines are shown here.
        for (tier, label) in [
            (
                crate::agent_tree_protocol::Tier::Execute,
                "Start prompt, tier execute (own snapshot copy)",
            ),
            (
                crate::agent_tree_protocol::Tier::Mutate,
                "Start prompt, tier mutate (own fork)",
            ),
        ] {
            let start = assignment_start_prompt(
                ChildAssignmentStartKind::Direct,
                "implementer",
                tier,
                "implementer",
                "Add the missing error paths in src/parser.rs.",
                CHILD_SCHEMA,
                CHILD_TURN_TOKEN,
                None,
                Some("/work/repo/.plak/workspaces/a2"),
            );
            md.push_str(&surface(
                label,
                "Faithful `assignment_start_prompt`; the closing line is `child_tier_line` from `prompts/child.rs`.",
                "text",
                &start,
            ));
        }

        let mut tree = ProductAgentTree::bootstrap_root(
            "golden-tree",
            "root",
            "root",
            root.to_path_buf(),
            "gpt-5.6-sol",
            WriterMode::SingleLease,
        )
        .expect("tree");
        tree.install_snapshot(golden_epoch(
            ["implementer", "reviewer"]
                .into_iter()
                .map(|name| {
                    let mut card = golden_card(name, "gpt-5.6-sol", false);
                    card.description = format!("{name} for the lifecycle golden");
                    card
                })
                .collect(),
        ));

        md.push_str("## `wait_agent` row JSON\n\n");
        let spawned = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named(
                    "reviewer",
                    crate::agent_tree_protocol::Tier::ReadOnly,
                    "reviewer",
                    "review the diff",
                ),
                None,
            )
            .expect("spawn");
        let child = spawned.agent_id.clone();
        let wait_running = tree
            .timeout_wait_rows_for_test("root", &AwaitAgents::Named(vec![child.clone()]))
            .expect("timeout wait");
        md.push_str(&surface(
            "Timeout (child still running)",
            "Faithful `timeout_wait` → topology tool `json!(output)`. `timed_out: true`.",
            "json",
            &pin_wait_rows(&json!(wait_running).to_string(), root),
        ));

        let missed = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named(
                    "reviewer",
                    crate::agent_tree_protocol::Tier::ReadOnly,
                    "reviewer",
                    "review the diff",
                ),
                None,
            )
            .expect("spawn missed");
        tree.bind_attempt_turn(&missed.agent_id, "golden-missed-turn")
            .expect("admitted missed turn");
        tree.release_attempt_turn(&missed.agent_id)
            .expect("missed turn ended");
        tree.record_turn_completed(&missed.agent_id)
            .expect("completed without submission");
        let wait_no_submit = tree
            .timeout_wait_rows_for_test("root", &AwaitAgents::Named(vec![missed.agent_id.clone()]))
            .expect("wait no submit");
        md.push_str(&surface(
            "`completed_without_submission`",
            "Faithful `record_turn_completed` with no `submit_result`.",
            "json",
            &pin_wait_rows(&json!(wait_no_submit).to_string(), root),
        ));

        let recovered = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named(
                    "reviewer",
                    crate::agent_tree_protocol::Tier::ReadOnly,
                    "reviewer",
                    "review the diff",
                ),
                None,
            )
            .expect("spawn recovery");
        tree.record_host_materialization(&recovered.agent_id)
            .and_then(|()| tree.fold_due(&recovered.agent_id))
            .expect("recovery required");
        let wait_recovery = tree
            .timeout_wait_rows_for_test(
                "root",
                &AwaitAgents::Named(vec![recovered.agent_id.clone()]),
            )
            .expect("wait recovery");
        md.push_str(&surface(
            "`recovery_required`",
            "Faithful `record_host_materialization`.",
            "json",
            &pin_wait_rows(&json!(wait_recovery).to_string(), root),
        ));

        tree.submit_result(
            &child,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .expect("submit");
        tree.bind_attempt_turn(&child, "golden-submitted-turn")
            .expect("admitted submitted turn");
        tree.release_attempt_turn(&child)
            .expect("submitted turn ended");
        tree.record_turn_completed(&child).expect("completed");
        let wait_done = tree
            .timeout_wait_rows_for_test("root", &AwaitAgents::Named(vec![child.clone()]))
            .expect("wait done");
        md.push_str(&surface(
            "Completed with `output_json`",
            "Faithful submit + `record_turn_completed`.",
            "json",
            &pin_wait_rows(&json!(wait_done).to_string(), root),
        ));

        md.push_str("## `list_agents`\n\n");
        let mut listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_read: None,
                    include_self: Some(true),
                    include_completed: Some(true),
                },
            )
            .expect("list");
        // Keep the lifecycle's admitted-turn fixture offset; fold intent
        // revisions remain part of the faithful observation.
        listed.revision -= 6;
        md.push_str(&surface(
            "Hub `list_agents` (include_self + include_completed)",
            "Faithful `ProductAgentTree::list_agents`. Paths pinned to `/work/repo`; occupancy fields pinned.",
            "json",
            &pin_list_agents(&json!(listed).to_string(), root),
        ));

        md.push_str("## `send_agent_message` acks\n\n");
        let terminal_mail = tree
            .send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: child.clone(),
                    message: "follow-up on the review".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .expect_err("terminal assignment rejects mail");
        md.push_str(&surface(
            "Failure JSON when the child assignment is already terminal",
            "Faithful `send_agent_message` error. Topology tools wrap `{\"code\",\"message\"}`. Message is hardcoded in `agent_tree.rs`, not `prompts/errors.rs`.",
            "json",
            &json!({ "code": terminal_mail.code, "message": terminal_mail.message }).to_string(),
        ));

        let live = tree
            .spawn_agent(
                "root",
                &SpawnAgentRequest::test_named(
                    "reviewer",
                    crate::agent_tree_protocol::Tier::ReadOnly,
                    "reviewer",
                    "review the diff",
                ),
                None,
            )
            .expect("live child");
        let started = tree
            .send_agent_message(
                "root",
                &SendAgentMessageRequest {
                    agent_id: live.agent_id.clone(),
                    message: "start brief already applied; this is extra mail".into(),
                    purpose: None,
                    priority: None,
                    in_reply_to: None,
                    output_schema_json: None,
                },
                None,
            )
            .expect("start or queue");
        md.push_str(&surface(
            "Ack on a child whose turn is not yet bound",
            "Faithful. `disposition` is `started` when the child has no bound turn, else `queued` / `steered`.",
            "json",
            &json!(started).to_string(),
        ));

        md.push_str("## `interrupt_agent` / `close_agent`\n\n");
        let interrupt = tree
            .interrupt_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: live.agent_id.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .expect("interrupt");
        md.push_str(&surface(
            "`interrupt_agent` result",
            "Faithful. `interrupt_agent` only flips `Running` → `Interrupted` when `active_turn` is set (`agent_tree.rs`). A child whose turn never bound stays `running`.",
            "json",
            &json!({ "agent_id": live.agent_id, "status": interrupt }).to_string(),
        ));
        let closed = tree
            .close_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: live.agent_id.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .expect("close");
        let close_payload = json!({
            "agents": closed
                .iter()
                .map(|(agent_id, status)| json!({ "agent_id": agent_id, "status": status }))
                .collect::<Vec<_>>()
        });
        md.push_str(&surface(
            "`close_agent` result",
            "Faithful topology tool payload.",
            "json",
            &close_payload.to_string(),
        ));

        md.push_str("## Topology error texts (`prompts/errors.rs`)\n\n");
        md.push_str("Each sample is the message from the production helper, then the failure JSON the topology tool returns (`{\"code\",\"message\"}`).\n\n");

        let wait_empty = topology_wait_request_invalid_message(
            TopologyErrorCode::SchemaMismatch,
            &WaitAgentRequest {
                agent_ids: Some(vec![]),
                timeout_ms: None,
            },
        );
        md.push_str(&surface(
            "wait: empty `agent_ids`",
            "From `topology_wait_request_invalid_message`.",
            "json",
            &topology_failure(TopologyErrorCode::SchemaMismatch, wait_empty),
        ));
        let wait_cap = topology_wait_request_invalid_message(
            TopologyErrorCode::SchemaMismatch,
            &WaitAgentRequest {
                agent_ids: Some((0..=MAX_WAIT_TARGETS).map(|i| format!("c{i}")).collect()),
                timeout_ms: None,
            },
        );
        md.push_str(&surface(
            "wait: too many ids",
            "From `topology_wait_request_invalid_message`.",
            "json",
            &topology_failure(TopologyErrorCode::SchemaMismatch, wait_cap),
        ));
        let wait_dup = topology_wait_request_invalid_message(
            TopologyErrorCode::SchemaMismatch,
            &WaitAgentRequest {
                agent_ids: Some(vec!["c1".into(), "c1".into()]),
                timeout_ms: None,
            },
        );
        md.push_str(&surface(
            "wait: duplicate ids",
            "From `topology_wait_request_invalid_message`.",
            "json",
            &topology_failure(TopologyErrorCode::SchemaMismatch, wait_dup),
        ));
        let wait_timeout = topology_wait_request_invalid_message(
            TopologyErrorCode::BoundExceeded,
            &WaitAgentRequest {
                agent_ids: Some(vec!["c1".into()]),
                timeout_ms: Some(0),
            },
        );
        md.push_str(&surface(
            "wait: `timeout_ms` < 1",
            "From `topology_wait_request_invalid_message`.",
            "json",
            &topology_failure(TopologyErrorCode::BoundExceeded, wait_timeout),
        ));

        let mail_self = topology_mail_request_invalid_message(
            TopologyErrorCode::SelfSend,
            &SendAgentMessageRequest {
                agent_id: "root".into(),
                message: "hi".into(),
                purpose: None,
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            "root",
        );
        md.push_str(&surface(
            "mail: self-send",
            "From `topology_mail_request_invalid_message`.",
            "json",
            &topology_failure(TopologyErrorCode::SelfSend, mail_self),
        ));
        let mail_bound = topology_mail_request_invalid_message(
            TopologyErrorCode::BoundExceeded,
            &SendAgentMessageRequest {
                agent_id: "c1".into(),
                message: String::new(),
                purpose: None,
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            "root",
        );
        md.push_str(&surface(
            "mail: empty / oversized message",
            &format!("From `topology_mail_request_invalid_message` (limit {MAX_MESSAGE_BYTES})."),
            "json",
            &topology_failure(TopologyErrorCode::BoundExceeded, mail_bound),
        ));
        let mail_reply = topology_mail_request_invalid_message(
            TopologyErrorCode::ReplyInvalid,
            &SendAgentMessageRequest {
                agent_id: "c1".into(),
                message: "hi".into(),
                purpose: Some(MessagePurpose::Reply),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            },
            "root",
        );
        md.push_str(&surface(
            "mail: `purpose=reply` without `in_reply_to`",
            "From `topology_mail_request_invalid_message`.",
            "json",
            &topology_failure(TopologyErrorCode::ReplyInvalid, mail_reply),
        ));
        let mail_in_reply = topology_mail_request_invalid_message(
            TopologyErrorCode::ReplyInvalid,
            &SendAgentMessageRequest {
                agent_id: "c1".into(),
                message: "hi".into(),
                purpose: None,
                in_reply_to: Some("m1".into()),
                priority: None,
                output_schema_json: None,
            },
            "root",
        );
        md.push_str(&surface(
            "mail: `in_reply_to` without `purpose=reply`",
            "From `topology_mail_request_invalid_message`.",
            "json",
            &topology_failure(TopologyErrorCode::ReplyInvalid, mail_in_reply),
        ));

        let submit_root = topology_submit_request_invalid_message(
            TopologyErrorCode::SubmitRootForbidden,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: "{}".into(),
            },
        );
        md.push_str(&surface(
            "submit: root forbidden",
            "From `topology_submit_request_invalid_message`.",
            "json",
            &topology_failure(TopologyErrorCode::SubmitRootForbidden, submit_root),
        ));
        let submit_token = topology_submit_request_invalid_message(
            TopologyErrorCode::SchemaMismatch,
            &SubmitResultRequest {
                turn_token: 0,
                output_json: "{}".into(),
            },
        );
        md.push_str(&surface(
            "submit: `turn_token` < 1",
            "From `topology_submit_request_invalid_message`.",
            "json",
            &topology_failure(TopologyErrorCode::SchemaMismatch, submit_token),
        ));
        let submit_bound = topology_submit_request_invalid_message(
            TopologyErrorCode::BoundExceeded,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: "x".repeat(MAX_OUTPUT_JSON_BYTES + 1),
            },
        );
        md.push_str(&surface(
            "submit: oversized `output_json`",
            &format!(
                "From `topology_submit_request_invalid_message` (limit {MAX_OUTPUT_JSON_BYTES})."
            ),
            "json",
            &topology_failure(TopologyErrorCode::BoundExceeded, submit_bound),
        ));
        let output_mismatch =
            topology_output_schema_mismatch_message("required property 'ok' is missing");
        md.push_str(&surface(
            "submit: frozen schema mismatch",
            "From `topology_output_schema_mismatch_message`.",
            "json",
            &topology_failure(TopologyErrorCode::SchemaMismatch, output_mismatch),
        ));
        md
    }

    fn identity_tail(profile: &HarnessProfileSpec) -> String {
        let base = pin_macos(profile.base_instructions());
        match base.find(PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER) {
            Some(start) => base[start..].to_owned(),
            None => base,
        }
    }

    fn render_mid_session() -> String {
        let (_tmp, home, root, cwd) = instruction_fixture();
        let extras = empty_extras();
        let mut md = String::new();
        md.push_str("# Lifecycle: mid-session changes\n\n");
        md.push_str("## Header\n\n");
        md.push_str("| Field | Value |\n| --- | --- |\n");
        md.push_str("| what | What changes in the next model request when a session is bound to another effort or another model of the same provider (a profile Fork now or a new session from another template), after `/clear`, and on reopen/fork under a different model |\n");
        md.push_str("| why | Identity and effort are request-prefix, not first-turn-only |\n");
        md.push_str("| production fns | `selected_harness_profile`, `product_target_identity_profile`, and identity templates in `prompts/identity.rs` |\n");
        md.push_str("| faithful vs synthetic | Identity/effort text is faithful (synthetic workspace `/work/repo`, OS `macos`). History emptiness after `/clear` is described from production behaviour (new session), not a live store. |\n\n");

        let medium = selected_harness_profile(
            NativeProvider::Codex,
            &plakous::model::ModelId::new("gpt-5.6-luna").expect("model"),
            ReasoningEffort::Medium,
            false,
            false,
            &[],
            false,
            ProductCompositionSelection::Direct,
            &lifecycle_surface(),
            Path::new(SYNTHETIC_ROOT),
            Path::new(SYNTHETIC_CWD),
        )
        .expect("effort profile")
        .expect("new session profile");
        let high = selected_harness_profile(
            NativeProvider::Codex,
            &plakous::model::ModelId::new("gpt-5.6-luna").expect("model"),
            ReasoningEffort::High,
            false,
            false,
            &[],
            false,
            ProductCompositionSelection::Direct,
            &lifecycle_surface(),
            Path::new(SYNTHETIC_ROOT),
            Path::new(SYNTHETIC_CWD),
        )
        .expect("effort profile")
        .expect("new session profile");

        md.push_str("## Another effort (Fork now or a new session)\n\n");
        md.push_str("`selected_harness_profile` applies `with_preferred_reasoning_effort`. Instructions and tools are unchanged; the next request's reasoning policy carries the new preferred effort. Codex `recommended()` defaults to High; the before row is an explicit Medium override.\n\n");
        md.push_str(&format!(
            "Before (effort `medium`): `preferred_reasoning_effort` = {:?}\n\n",
            medium.preferred_reasoning_effort()
        ));
        md.push_str(&format!(
            "After binding effort `high`: `preferred_reasoning_effort` = {:?}\n\n",
            high.preferred_reasoning_effort()
        ));
        md.push_str(&surface(
            "Identity tail is unchanged by effort",
            "Faithful product identity block.",
            "text",
            &identity_tail(&medium),
        ));

        md.push_str("## Another model of the same provider (Fork now or a new session)\n\n");
        let (next_model, _) = native_profile(
            NativeProvider::Codex,
            "gpt-5.6-sol",
            false,
            &extras,
            ProductCompositionSelection::Direct,
            &lifecycle_surface(),
            &home,
            &root,
            &cwd,
        );
        md.push_str("The next request keeps the same Codex family prompt and tools (unless Code Mode eligibility changes). The identity tail's model sentence changes.\n\n");
        md.push_str(&surface(
            "Before (`gpt-5.6-luna`)",
            "Faithful `product_target_identity_profile`.",
            "text",
            &identity_tail(&medium),
        ));
        md.push_str(&surface(
            "After binding `gpt-5.6-sol` (same provider)",
            "Faithful. `PLAK_PRODUCT_MODEL_IDENTITY_TEMPLATE` with the new model id.",
            "text",
            &identity_tail(&next_model),
        ));

        md.push_str("## `/clear`\n\n");
        md.push_str("Production `/clear` starts a new session: empty canonical history and empty working projection. The harness profile / identity tail is the same construction as a new session (still includes the model-history sentence). Tools and composition are unchanged.\n\n");
        md.push_str(&surface(
            "Next request history after `/clear`",
            "Synthetic description of the production empty projection.",
            "text",
            "history: (empty)\nworking: (empty)\n",
        ));
        md.push_str(&surface(
            "Identity tail after `/clear` (new session)",
            "Faithful; same as genesis.",
            "text",
            &identity_tail(&medium),
        ));

        md.push_str("## Reopen / fork under a different model\n\n");
        md.push_str("`selected_harness_profile(..., opening_durable_session=true)` returns `None`: exact resume reconstructs the durable harness plan from the checkpoint and does not issue a new profile. A **fork** to another model builds a new session profile via `product_target_identity_profile`.\n\n");
        md.push_str("The model-history sentence is **not** reopen-only. `product_target_identity_profile` appends it on every product session, including genesis, so earlier model names in retained history cannot override the current target.\n\n");
        md.push_str("Fragments:\n\n");
        md.push_str(&format!(
            "- `PLAK_PRODUCT_ENVIRONMENT_TEMPLATE`: `{PLAK_PRODUCT_ENVIRONMENT_TEMPLATE}`\n"
        ));
        md.push_str(&format!(
            "- `PLAK_PRODUCT_MODEL_IDENTITY_TEMPLATE`: `{PLAK_PRODUCT_MODEL_IDENTITY_TEMPLATE}`\n"
        ));
        md.push_str(&format!(
            "- `PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER`: `{PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER}`\n\n"
        ));
        md.push_str(&surface(
            "Fork/reopen target identity for `gpt-5.6-sol` (history may still name `gpt-5.6-luna`)",
            "Faithful `product_target_identity_profile`. History items are conversation content; this marker is the model-visible disclaimer.",
            "text",
            &identity_tail(&next_model),
        ));
        md
    }

    /// sha256 of each rendered lifecycle surface. Correctness is pinned here;
    /// the rendered Markdown itself is a local artifact only.
    ///
    /// Re-pin with:
    /// `PLAK_PRINT_PROMPT_PINS=1 cargo test -p plakous-hosted --lib rendered_lifecycle -- --nocapture`
    const LIFECYCLE_RENDER_PINS: &[(&str, &str)] = &[
        (
            "tool-results",
            "ab71dc81f35663c8f9f590b5a353c81650919c343568a1c87a752a297b1b75e0",
        ),
        (
            "compaction",
            "77aa65d827fddfbde6e2ea302bb0338ae3905846f1eb5eec74211e5b35be806d",
        ),
        (
            "child-mail",
            "9ac1c4f31f3c61fbbb1756c4362a545c05d99358bf60a00651eec05c36dee6fd",
        ),
        (
            "mid-session",
            "232b5342ae0e5b3bff5be05c29d1f5f8f7893a64719fc0ae7346e6719908d944",
        ),
    ];

    fn pin_or_write(
        stem: &str,
        actual: &str,
        dir: &Path,
        update: bool,
        failures: &mut Vec<String>,
        rows: &mut Vec<String>,
    ) {
        let digest = render_digest(actual);
        match LIFECYCLE_RENDER_PINS
            .iter()
            .find(|(name, _)| *name == stem)
            .map(|(_, pin)| *pin)
        {
            Some(pinned) if pinned == digest => {}
            Some(pinned) => failures.push(format!(
                "{stem}: rendered lifecycle surface changed (pinned {pinned}, rendered {digest})"
            )),
            None => failures.push(format!("{stem}: lifecycle surface has no pinned digest")),
        }
        rows.push(format!("        (\"{stem}\", \"{digest}\"),"));
        if update {
            std::fs::create_dir_all(dir).expect("create lifecycle artifact dir");
            std::fs::write(dir.join(format!("{stem}.md")), actual)
                .expect("write lifecycle artifact");
        }
    }

    #[test]
    fn rendered_lifecycle_goldens_match() {
        let (_tmp, _home, root, _cwd) = instruction_fixture();
        let dir = lifecycle_dir();
        let update = std::env::var("PLAK_UPDATE_GOLDENS").is_ok_and(|value| value == "1");
        let mut failures = Vec::new();
        let mut rows = Vec::new();
        for (stem, rendered) in [
            ("tool-results", render_tool_results()),
            ("compaction", render_compaction()),
            ("child-mail", render_child_mail(&root)),
            ("mid-session", render_mid_session()),
        ] {
            pin_or_write(stem, &rendered, &dir, update, &mut failures, &mut rows);
        }
        if print_pins_requested() {
            println!("LIFECYCLE_RENDER_PINS:\n{}", rows.join("\n"));
        }
        assert!(
            failures.is_empty(),
            "lifecycle render drift (inspect with `just prompts-diff`; re-pin with PLAK_PRINT_PROMPT_PINS=1 cargo test -p plakous-hosted --lib rendered_lifecycle):\n\n{}",
            failures.join("\n\n")
        );
    }
}
