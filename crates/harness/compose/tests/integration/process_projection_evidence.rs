//! Repository-only evidence for portable and exact projected process tools.

use std::str::FromStr;
use std::sync::Arc;

use plakous_compose::CodingPresentationExt as _;
use plakous_compose::standard_coding_packs_in_memory;
use plakous_model::{
    AttemptId, CallId, Digest, DigestDomain, EffectiveProviderConfigurationId, IntegrationClass,
    RunId, ToolCorrelation, ToolName, ToolResultPart, TurnId,
};
use plakous_tool_process::{ProcessSessionTools, ProcessToolProjectionKind};
use plakous_tools::{
    CanonicalProcessOperationV2, FrozenToolRegistry, PROCESS_SESSION_CAPABILITY_NAME,
    PROCESS_SESSION_POLL_NAME, PROCESS_SESSION_TERMINATE_NAME, PROCESS_SESSION_WRITE_NAME,
    ProviderModelProjectionIdentityV2, ToolInvocationRequest, ToolOutcome, ToolProjectionMapV2,
    ToolRegistryBuilder, ToolRuntimeErrorKind, register_packs,
};
use serde_json::{Value, json};

type TestError = Box<dyn std::error::Error>;

fn registry() -> Result<FrozenToolRegistry, TestError> {
    let process = Arc::new(ProcessSessionTools::posix_sh()?);
    let mut builder = ToolRegistryBuilder::default();
    for registration in process.registrations()? {
        builder.register(registration)?;
    }
    Ok(builder.freeze())
}

fn configuration(seed: &'static [u8]) -> Result<EffectiveProviderConfigurationId, TestError> {
    Ok(serde_json::from_value(serde_json::to_value(
        Digest::sha256(DigestDomain::EffectiveProviderConfiguration, 5, &[seed])?,
    )?)?)
}

fn target(
    model: &str,
    configuration: EffectiveProviderConfigurationId,
) -> Result<ProviderModelProjectionIdentityV2, TestError> {
    Ok(ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "test.responses",
        "test.route",
        "test.profile.r1",
        "responses-family",
        model,
        model,
        configuration,
        4,
    )?)
}

fn request(name: &str, arguments: &str) -> Result<ToolInvocationRequest, TestError> {
    Ok(ToolInvocationRequest::new(
        RunId::from_str("00000000-0000-0000-0000-000000000101")?,
        TurnId::from_str("00000000-0000-0000-0000-000000000102")?,
        AttemptId::from_str("00000000-0000-0000-0000-000000000103")?,
        CallId::from_str("00000000-0000-0000-0000-000000000104")?,
        ToolName::new(name)?,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::default())?,
        arguments,
    ))
}

fn maps() -> Result<(FrozenToolRegistry, ToolProjectionMapV2, ToolProjectionMapV2), TestError> {
    let registry = registry()?;
    let configuration = configuration(b"process-projection-evidence")?;
    let codex = ProcessToolProjectionKind::CodexUnifiedExec.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target("codex-exact", configuration)?,
    )?;
    let grok = ProcessToolProjectionKind::GrokTerminal.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target("grok-exact", configuration)?,
    )?;
    Ok((registry, codex, grok))
}

#[test]
fn grok_native_process_arguments_map_ids_snapshots_and_clamps() -> Result<(), TestError> {
    let (_, _, grok) = maps()?;
    for (hint, expected) in [(0, 250), (999_999, 600_000)] {
        let start = grok.prepare(request(
            "run_terminal_command",
            &json!({
                "command":"true", "description":"verify", "block_until_ms":hint
            })
            .to_string(),
        )?)?;
        assert_eq!(
            start.invocation().arguments().value()["yield_time_ms"].as_f64(),
            Some(f64::from(expected))
        );
    }
    let default = grok.prepare(request(
        "run_terminal_command",
        r#"{"command":"true","description":"verify"}"#,
    )?)?;
    assert_eq!(
        default.invocation().arguments().value()["yield_time_ms"].as_f64(),
        Some(30_000.0)
    );
    for (hint, expected) in [(0, 0), (3_600_001, 3_600_000)] {
        let await_call = grok.prepare(request(
            "get_command_or_subagent_output",
            &json!({"task_ids":["p1","2"],"timeout_ms":hint}).to_string(),
        )?)?;
        assert_eq!(
            await_call.invocation().arguments().value()["session_ids"]
                .as_array()
                .ok_or("session ids")?
                .iter()
                .map(Value::as_f64)
                .collect::<Vec<_>>(),
            [Some(1.0), Some(2.0)]
        );
        assert_eq!(
            await_call.invocation().arguments().value()["yield_time_ms"].as_f64(),
            Some(f64::from(expected))
        );
    }
    let snapshot = grok.prepare(request(
        "get_command_or_subagent_output",
        r#"{"task_ids":["p1"]}"#,
    )?)?;
    assert_eq!(
        snapshot.invocation().arguments().value()["session_ids"],
        json!([1])
    );
    assert_eq!(
        snapshot.invocation().arguments().value()["yield_time_ms"].as_f64(),
        Some(0.0)
    );
    for arguments in ["{}", r#"{"task_ids":[]}"#] {
        assert!(
            grok.prepare(request("get_command_or_subagent_output", arguments)?)
                .is_err()
        );
    }
    for id in ["1", "p1"] {
        let stop = grok.prepare(request(
            "kill_command_or_subagent",
            &json!({"task_id":id}).to_string(),
        )?)?;
        assert_eq!(
            stop.invocation().arguments().value()["session_id"].as_f64(),
            Some(1.0)
        );
    }
    for id in ["agent-1", "p0", "-1", "4294967296"] {
        assert!(
            grok.prepare(request(
                "kill_command_or_subagent",
                &json!({"task_id":id}).to_string()
            )?)
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn claude_native_stop_keeps_process_only_semantics_and_no_output_tool() -> Result<(), TestError> {
    let registry = registry()?;
    let map = ProcessToolProjectionKind::ClaudeOAuthBash.projection_map(
        plakous_tools::JobConfig::default(),
        registry,
        target("claude-exact", configuration(b"claude-process-vocabulary")?)?,
    )?;
    // The native five-tool pack has no output tool. Neither the
    // retired transitional `BashOutput` nor native `TaskOutput` lowers.
    for (name, args) in [
        ("BashOutput", r#"{"bash_id":"p1"}"#),
        ("TaskOutput", r#"{"task_id":"1"}"#),
    ] {
        assert!(map.prepare(request(name, args)?).is_err(), "{name}");
    }
    let stop = map.prepare(request("TaskStop", r#"{"task_id":"p1"}"#)?)?;
    assert_eq!(
        stop.invocation().arguments().value()["session_id"].as_f64(),
        Some(1.0)
    );
    assert!(
        map.prepare(request("TaskStop", r#"{"shell_id":"1"}"#)?)
            .is_err()
    );
    assert!(
        map.prepare(request("TaskStop", r#"{"task_id":"agent-1"}"#)?)
            .is_err()
    );
    assert!(
        map.prepare(request("KillShell", r#"{"shell_id":"1"}"#)?)
            .is_err()
    );
    Ok(())
}

#[test]
fn claude_promotion_reuses_native_timeout_and_message_arrival_text() -> Result<(), TestError> {
    let registry = registry()?;
    let map = ProcessToolProjectionKind::ClaudeOAuthBash.projection_map(
        plakous_tools::JobConfig::default(),
        registry,
        target("claude-exact", configuration(b"claude-promotion-wording")?)?,
    )?;
    let call = map.prepare(request("Bash", r#"{"command":"true"}"#)?)?;
    for (preempted, manual, expected) in [
        (
            false,
            false,
            "Command did not complete within its 30s timeout and was moved to the background (ID: 1).",
        ),
        (
            true,
            false,
            "Command was moved to the background (ID: 1) so that a message that arrived while it was running can reach you; it was not interrupted.",
        ),
        (
            false,
            true,
            "Command was manually backgrounded by user with ID: 1.",
        ),
    ] {
        let outcome = ToolOutcome::success_text(json!({"status":"running","exit_code":null,"session_id":1,"cursor":0,"output":"progress","foreground_hint_ms":30000,"preempted":preempted,"backgrounded_by_user":manual}).to_string())?;
        let raised = map.raise(call.receipt().ok_or("projection receipt")?, outcome)?;
        let value: Value = serde_json::from_str(&raised.joined_text())?;
        assert_eq!(value["output"], format!("progress\n{expected}"));
        assert!(!raised.joined_text().contains("Output is being written to"));
    }
    Ok(())
}

fn projected_schema_bytes(map: &ToolProjectionMapV2) -> usize {
    map.entries()
        .iter()
        .map(|entry| {
            entry
                .input_schema()
                .map_or(0, |schema| schema.document().canonical_bytes().len())
                + entry.output_schema().document().canonical_bytes().len()
        })
        .sum()
}

fn text(outcome: &ToolOutcome) -> Result<&str, TestError> {
    match outcome.parts() {
        [ToolResultPart::Text(text)] => Ok(text.text()),
        _ => Err("expected one text result".into()),
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "leak reversal and footprint goldens stay in one table"
)]
fn footprint_is_exact_and_request_view_has_no_process_synonyms() -> Result<(), TestError> {
    let (registry, codex, grok) = maps()?;
    let portable_wire_bytes = serde_json::to_vec(registry.definitions())?.len();
    let codex_view = codex.compile_request_view_v2()?;
    let grok_view = grok.compile_request_view_v2()?;
    let evidence = (
        registry.len(),
        portable_wire_bytes,
        codex_view.definitions().len(),
        projected_schema_bytes(&codex),
        serde_json::to_vec(&codex_view)?.len(),
        grok_view.definitions().len(),
        projected_schema_bytes(&grok),
        serde_json::to_vec(&grok_view)?.len(),
    );
    let (
        portable_count,
        portable_bytes,
        codex_count,
        codex_schema_bytes,
        codex_wire_bytes,
        grok_count,
        grok_schema_bytes,
        grok_wire_bytes,
    ) = evidence;
    assert_eq!((portable_count, codex_count, grok_count), (4, 2, 3));
    assert!(portable_bytes > 0);
    assert!(codex_schema_bytes > 0 && grok_schema_bytes > 0);
    assert!(codex_wire_bytes > 0 && grok_wire_bytes > 0);
    assert!(codex_wire_bytes >= codex_schema_bytes);
    assert!(grok_wire_bytes >= grok_schema_bytes);

    let canonical = [
        PROCESS_SESSION_CAPABILITY_NAME,
        PROCESS_SESSION_WRITE_NAME,
        PROCESS_SESSION_POLL_NAME,
        PROCESS_SESSION_TERMINATE_NAME,
    ];
    for view in [&codex_view, &grok_view] {
        let exposed = view
            .definitions()
            .iter()
            .map(|definition| definition.exposed_name().as_str())
            .collect::<Vec<_>>();
        assert!(canonical.iter().all(|name| !exposed.contains(name)));
        for definition in view.definitions() {
            assert!(
                canonical
                    .iter()
                    .all(|name| *name != definition.exposed_name().as_str())
            );
        }
    }
    assert_eq!(
        codex_view
            .definitions()
            .iter()
            .map(|definition| definition.exposed_name().as_str())
            .collect::<Vec<_>>(),
        ["exec_command", "write_stdin"]
    );
    assert_eq!(
        grok_view.definitions()[0].exposed_name().as_str(),
        "run_terminal_command"
    );
    for name in [PROCESS_SESSION_CAPABILITY_NAME, PROCESS_SESSION_WRITE_NAME] {
        assert_eq!(
            codex
                .prepare(request(name, "{}")?)
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::UnknownTool)
        );
    }
    assert_eq!(
        grok.prepare(request(PROCESS_SESSION_CAPABILITY_NAME, "{}")?)
            .err()
            .map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::UnknownTool)
    );
    for (name, arguments) in [
        (
            PROCESS_SESSION_POLL_NAME,
            r#"{"session_id":1,"cursor":0,"yield_time_ms":0,"max_output_bytes":1,"max_output_records":1}"#,
        ),
        (PROCESS_SESSION_TERMINATE_NAME, r#"{"session_id":1}"#),
        (
            PROCESS_SESSION_WRITE_NAME,
            r#"{"session_id":1,"input":"","disposition":"keep_open"}"#,
        ),
        (PROCESS_SESSION_CAPABILITY_NAME, "{}"),
    ] {
        assert_eq!(
            codex
                .prepare(request(name, arguments)?)
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::UnknownTool),
            "{name}"
        );
        assert_eq!(
            grok.prepare(request(name, arguments)?)
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::UnknownTool),
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn projected_lowering_matches_portable_canonical_dispatch_and_raising() -> Result<(), TestError> {
    let (registry, codex, grok) = maps()?;
    for arguments in [
        r#"{"cmd":"find . -maxdepth 2 -type f"}"#,
        r#"{"cmd":"find . -maxdepth 2 -type f","max_output_tokens":10000}"#,
    ] {
        let bounded = codex.prepare(request("exec_command", arguments)?)?;
        assert_eq!(
            bounded.invocation().arguments().value()["max_output_bytes"].as_f64(),
            Some(40000.0)
        );
    }
    assert_eq!(
        codex
            .prepare(request(
                "exec_command",
                r#"{"cmd":"true","max_output_tokens":10001}"#,
            )?)?
            .invocation()
            .arguments()
            .value()["max_output_bytes"]
            .as_f64(),
        Some(40_000.0)
    );
    let cases = [
        (
            &codex,
            "exec_command",
            r#"{"cmd":"printf exact","login":false}"#,
            CanonicalProcessOperationV2::Start,
            PROCESS_SESSION_CAPABILITY_NAME,
        ),
        (
            &codex,
            "exec_command",
            r#"{"cmd":"true","session_id":7}"#,
            CanonicalProcessOperationV2::Start,
            PROCESS_SESSION_CAPABILITY_NAME,
        ),
        (
            &codex,
            "exec_command",
            r#"{"cmd":"","session_id":7}"#,
            CanonicalProcessOperationV2::Poll,
            PROCESS_SESSION_POLL_NAME,
        ),
        (
            &codex,
            "write_stdin",
            r#"{"session_id":7,"chars":"exact"}"#,
            CanonicalProcessOperationV2::Write,
            PROCESS_SESSION_WRITE_NAME,
        ),
        (
            &grok,
            "run_terminal_command",
            r#"{"command":"printf exact","description":"evidence"}"#,
            CanonicalProcessOperationV2::Start,
            PROCESS_SESSION_CAPABILITY_NAME,
        ),
    ];
    for (map, exposed_name, arguments, operation, canonical_name) in cases {
        let projected = map.prepare(request(exposed_name, arguments)?)?;
        let receipt = projected.receipt().ok_or("missing projection receipt")?;
        assert_eq!(receipt.canonical_operation(), operation);
        assert_eq!(
            projected.invocation().identity().name().as_str(),
            canonical_name
        );
        assert_eq!(
            projected
                .invocation()
                .exposure()
                .ok_or("missing exposure")?
                .exposed_name(),
            receipt.exposed_name()
        );

        let canonical_arguments =
            std::str::from_utf8(projected.invocation().arguments().canonical())?;
        let portable = registry.prepare(request(canonical_name, canonical_arguments)?)?;
        assert_eq!(portable.identity(), projected.invocation().identity());
        assert_eq!(
            portable.arguments().digest(),
            projected.invocation().arguments().digest()
        );
        assert_eq!(
            portable.arguments().canonical(),
            projected.invocation().arguments().canonical()
        );

        let canonical_result = ToolOutcome::success_text(
            r#"{"status":"exited","exit_code":0,"session_id":null,"cursor":1,"output":"exact"}"#,
        )?;
        let raised = map.raise(receipt, canonical_result)?;
        assert_eq!(
            serde_json::from_str::<Value>(text(&raised)?)?,
            serde_json::json!({"exit_code":0,"output":"exact"})
        );
    }
    Ok(())
}

#[test]
fn exec_command_omitting_cmd_with_session_id_lowers_to_poll() -> Result<(), TestError> {
    let (_, codex, _) = maps()?;
    let projected = codex.prepare(request("exec_command", r#"{"session_id":7}"#)?)?;
    let receipt = projected.receipt().ok_or("missing projection receipt")?;
    assert_eq!(
        receipt.canonical_operation(),
        CanonicalProcessOperationV2::Poll
    );
    assert_eq!(
        projected.invocation().identity().name().as_str(),
        PROCESS_SESSION_POLL_NAME
    );
    Ok(())
}

#[test]
fn malformed_calls_and_exact_target_drift_fail_closed() -> Result<(), TestError> {
    let (registry, codex, grok) = maps()?;
    let malformed = [
        (&codex, "exec_command", r#"{"cmd":"true","cmd":"false"}"#),
        (&codex, "exec_command", r#"{"cmd":"true"} trailing"#),
        (&codex, "exec_command", r#"{"tty":false}"#),
        (&codex, "exec_command", r#"{"cmd":"true","unknown":1}"#),
        (&codex, "write_stdin", r#"{"session_id":0}"#),
        (
            &grok,
            "run_terminal_cmd",
            r#"{"command":"true","description":"legacy"}"#,
        ),
        (&grok, "run_terminal_command", r#"{"command":"true"}"#),
        (
            &grok,
            "run_terminal_command",
            r#"{"command":"true","description":"evidence","background":true}"#,
        ),
    ];
    let expected_kinds = [
        ToolRuntimeErrorKind::InvalidArguments,
        ToolRuntimeErrorKind::InvalidArguments,
        ToolRuntimeErrorKind::InvalidArguments,
        ToolRuntimeErrorKind::InvalidArguments,
        ToolRuntimeErrorKind::InvalidArguments,
        ToolRuntimeErrorKind::UnknownTool,
        ToolRuntimeErrorKind::InvalidArguments,
        ToolRuntimeErrorKind::InvalidArguments,
        ToolRuntimeErrorKind::InvalidArguments,
    ];
    for ((map, name, arguments), expected) in malformed.into_iter().zip(expected_kinds) {
        assert_eq!(
            map.prepare(request(name, arguments)?)
                .err()
                .map(|error| error.kind()),
            Some(expected),
            "{name}: {arguments}"
        );
    }

    let projected = codex.prepare(request("exec_command", r#"{"cmd":"true"}"#)?)?;
    let receipt = projected.receipt().ok_or("missing receipt")?;
    let drifted = ProcessToolProjectionKind::CodexUnifiedExec.projection_map(
        plakous_tools::JobConfig::default(),
        registry,
        target("codex-neighbor", configuration(b"drifted-configuration")?)?,
    )?;
    assert_ne!(codex.digest(), drifted.digest());
    assert_eq!(
        drifted
            .raise(
                receipt,
                ToolOutcome::success_text(
                    r#"{"status":"exited","exit_code":0,"session_id":null,"cursor":1,"output":"ok"}"#,
                )?,
            )
            .err()
            .map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Execution)
    );
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "single table-driven audit keeps every presentation identity axis visible together"
)]
fn every_presentation_target_identity_axis_rotates_the_map() -> Result<(), TestError> {
    let registry = registry()?;
    let base_configuration = configuration(b"identity-base")?;
    let make =
        |integration, protocol, route, profile, family, requested, effective, config, revision| {
            ProviderModelProjectionIdentityV2::new(
                integration,
                protocol,
                route,
                profile,
                family,
                requested,
                effective,
                config,
                revision,
            )
        };
    let base = ProcessToolProjectionKind::CodexUnifiedExec.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        make(
            IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            base_configuration,
            4,
        )?,
    )?;
    let cases = [
        (
            "integration",
            IntegrationClass::Custom,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            base_configuration,
            4,
        ),
        (
            "protocol",
            IntegrationClass::Standard,
            "test.messages",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            base_configuration,
            4,
        ),
        (
            "route",
            IntegrationClass::Standard,
            "test.responses",
            "test.other-route",
            "test.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            base_configuration,
            4,
        ),
        (
            "profile",
            IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r2",
            "responses-family",
            "exact-model",
            "exact-model",
            base_configuration,
            4,
        ),
        (
            "family",
            IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "other-family",
            "exact-model",
            "exact-model",
            base_configuration,
            4,
        ),
        (
            "model",
            IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "other-model",
            "other-model",
            base_configuration,
            4,
        ),
        (
            "configuration",
            IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            configuration(b"identity-drift")?,
            4,
        ),
        (
            "revision",
            IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            base_configuration,
            5,
        ),
    ];
    for (
        axis,
        integration,
        protocol,
        route,
        profile,
        family,
        requested,
        effective,
        config,
        revision,
    ) in cases
    {
        let drifted = ProcessToolProjectionKind::CodexUnifiedExec.projection_map(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            make(
                integration,
                protocol,
                route,
                profile,
                family,
                requested,
                effective,
                config,
                revision,
            )?,
        )?;
        assert_ne!(
            base.digest(),
            drifted.digest(),
            "axis did not rotate: {axis}"
        );
    }
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "Codex, Grok, and Claude menus stay in one catalog proof"
)]
fn coding_presentation_catalogs_are_exact_menus() -> Result<(), TestError> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let registry = builder.freeze();
    let configuration = configuration(b"coding-presentation-catalog")?;
    let codex = plakous_tools::CodingPresentation::Codex.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target("gpt-5.6-sol", configuration)?,
    )?;
    let grok = plakous_tools::CodingPresentation::Grok.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target("grok-4.6", configuration)?,
    )?;
    let claude = plakous_tools::CodingPresentation::Claude.projection_map(
        plakous_tools::JobConfig::default(),
        registry,
        target("claude-sonnet-5", configuration)?,
    )?;
    let codex_view = codex.compile_request_view_v2()?;
    let grok_view = grok.compile_request_view_v2()?;
    let claude_view = claude.compile_request_view_v2()?;
    assert_eq!(
        codex_view
            .definitions()
            .iter()
            .map(|definition| definition.exposed_name().as_str())
            .collect::<Vec<_>>(),
        ["apply_patch", "exec_command", "update_plan", "write_stdin"]
    );
    assert_eq!(
        grok_view
            .definitions()
            .iter()
            .map(|definition| definition.exposed_name().as_str())
            .collect::<Vec<_>>(),
        [
            "get_command_or_subagent_output",
            "grep",
            "kill_command_or_subagent",
            "list_dir",
            "read_file",
            "run_terminal_command",
            "search_replace",
            "todo_write",
            "write"
        ]
    );
    let grok_grep = grok_view
        .definitions()
        .iter()
        .find(|definition| definition.exposed_name().as_str() == "grep")
        .ok_or("missing grok grep")?;
    let grok_grep_schema = grok_grep
        .input_schema()
        .ok_or("grok grep is not structured")?
        .document()
        .value();
    assert!(grok_grep_schema["properties"].get("glob").is_some());
    assert!(grok_grep_schema["properties"].get("-i").is_some());
    assert!(grok_grep_schema["properties"].get("head_limit").is_some());
    assert!(
        grok_grep_schema["properties"]
            .get("case_sensitive")
            .is_none()
    );
    let grok_todo = grok_view
        .definitions()
        .iter()
        .find(|definition| definition.exposed_name().as_str() == "todo_write")
        .ok_or("missing grok todo_write")?;
    assert!(
        grok_todo
            .input_schema()
            .ok_or("grok todo_write is not structured")?
            .document()
            .value()["properties"]
            .get("merge")
            .is_some()
    );
    let grok_grep_call = grok.prepare(request(
        "grep",
        r#"{"pattern":"exact","glob":"*.rs","-i":true,"head_limit":3}"#,
    )?)?;
    let grok_grep_args = grok_grep_call.invocation().arguments().value();
    assert_eq!(grok_grep_args["query"], "exact");
    assert_eq!(grok_grep_args["filename_glob"], "*.rs");
    assert_eq!(grok_grep_args["case_sensitive"], false);
    assert_eq!(grok_grep_args["head_limit"].as_f64(), Some(3.0));
    assert_eq!(grok_grep_args["use_regex"], true);
    assert_eq!(
        claude_view
            .definitions()
            .iter()
            .map(|definition| definition.exposed_name().as_str())
            .collect::<Vec<_>>(),
        ["Bash", "Edit", "Read", "TaskStop", "Write"]
    );
    for (label, view) in [
        ("codex", &codex_view),
        ("grok", &grok_view),
        ("claude", &claude_view),
    ] {
        let schema_bytes = view
            .definitions()
            .iter()
            .fold(0_usize, |total, definition| {
                total
                    .saturating_add(definition.description().len())
                    .saturating_add(
                        definition
                            .input_schema()
                            .map_or(0, |schema| schema.document().canonical_bytes().len()),
                    )
            });
        assert!(
            schema_bytes < 64 * 1024,
            "{label} coding catalog schemas are {schema_bytes} bytes; a 1 MiB subscription input cap is not the tool menu"
        );
    }
    for name in [
        PROCESS_SESSION_CAPABILITY_NAME,
        PROCESS_SESSION_WRITE_NAME,
        PROCESS_SESSION_POLL_NAME,
        PROCESS_SESSION_TERMINATE_NAME,
        "search_repository",
        "inspect_image",
        "read_file",
    ] {
        assert_eq!(
            codex
                .prepare(request(name, "{}")?)
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::UnknownTool),
            "{name}"
        );
    }
    for name in [
        PROCESS_SESSION_CAPABILITY_NAME,
        PROCESS_SESSION_WRITE_NAME,
        PROCESS_SESSION_POLL_NAME,
        PROCESS_SESSION_TERMINATE_NAME,
        "apply_patch",
        "search_repository",
        "inspect_image",
        "update_plan",
    ] {
        assert_eq!(
            grok.prepare(request(name, "{}")?)
                .err()
                .map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::UnknownTool),
            "{name}"
        );
    }
    for name in [
        PROCESS_SESSION_CAPABILITY_NAME,
        PROCESS_SESSION_WRITE_NAME,
        PROCESS_SESSION_POLL_NAME,
        PROCESS_SESSION_TERMINATE_NAME,
        "Agent",
        "read_file",
        "bash",
        "apply_patch",
    ] {
        let error = claude
            .prepare(request(name, "{}")?)
            .err()
            .ok_or("hidden Claude name must reject")?;
        assert_eq!(error.kind(), ToolRuntimeErrorKind::UnknownTool, "{name}");
        let message = error.message().as_str();
        assert!(
            message.contains("presented tools:") && message.contains("Bash"),
            "{name}: {message}"
        );
    }
    let grok_alias = grok
        .prepare(request(
            "run_terminal_cmd",
            r#"{"command":"true","description":"alias"}"#,
        )?)
        .err()
        .ok_or("Grok legacy name must reject")?;
    assert_eq!(grok_alias.kind(), ToolRuntimeErrorKind::UnknownTool);
    let grok_cmd = grok.prepare(request(
        "run_terminal_command",
        r#"{"command":"true","description":"page"}"#,
    )?)?;
    assert!(
        grok_cmd
            .invocation()
            .arguments()
            .value()
            .get("join_before_return")
            .is_none()
    );
    assert_eq!(
        grok_cmd.invocation().arguments().value()["max_output_bytes"].as_f64(),
        Some(40_000.0)
    );
    let grep = grok.prepare(request("grep", r#"{"pattern":"exact"}"#)?)?;
    assert_eq!(
        grep.invocation().identity().name().as_str(),
        "search_repository"
    );
    let replace = grok.prepare(request(
        "search_replace",
        r#"{"file_path":"a.txt","old_string":"old","new_string":"new"}"#,
    )?)?;
    assert_eq!(
        replace.invocation().identity().name().as_str(),
        "apply_patch"
    );
    let todos = grok.prepare(request(
        "todo_write",
        r#"{"merge":true,"todos":[{"id":"a","content":"step","status":"pending"}]}"#,
    )?)?;
    assert_eq!(todos.invocation().identity().name().as_str(), "merge_plan");
    assert_eq!(
        todos.invocation().arguments().value()["merge"],
        Value::Bool(true)
    );
    let read = claude.prepare(request(
        "Read",
        r#"{"file_path":"a.txt","offset":1,"limit":20}"#,
    )?)?;
    assert_eq!(read.invocation().identity().name().as_str(), "read_file");
    // Plak reads text only: the native PDF `pages` parameter is not presented.
    assert!(
        claude
            .prepare(request("Read", r#"{"file_path":"a.pdf","pages":"1-2"}"#)?)
            .is_err()
    );
    let edit = claude.prepare(request(
        "Edit",
        r#"{"file_path":"a.txt","old_string":"old","new_string":"new"}"#,
    )?)?;
    assert_eq!(edit.invocation().identity().name().as_str(), "apply_patch");
    let write = claude.prepare(request(
        "Write",
        r#"{"file_path":"a.txt","content":"new file"}"#,
    )?)?;
    assert_eq!(write.invocation().identity().name().as_str(), "apply_patch");
    assert_eq!(write.invocation().arguments().value()["old_string"], "");
    assert_eq!(
        write.invocation().arguments().value()["new_string"],
        "new file"
    );
    // Search goes through the shell and the plan is not presented: the
    // retired lowerings no longer prepare.
    for (name, args) in [
        ("Grep", r#"{"pattern":"exact"}"#),
        ("Glob", r#"{"pattern":"**/*.rs"}"#),
        (
            "TodoWrite",
            r#"{"todos":[{"content":"build","status":"completed","activeForm":"Building"}]}"#,
        ),
    ] {
        assert!(claude.prepare(request(name, args)?).is_err(), "{name}");
    }
    let bash = claude.prepare(request("Bash", r#"{"command":"true"}"#)?)?;
    assert_eq!(
        bash.invocation().identity().name().as_str(),
        PROCESS_SESSION_CAPABILITY_NAME
    );
    assert!(
        bash.invocation()
            .arguments()
            .value()
            .get("join_before_return")
            .is_none()
    );
    assert_eq!(
        bash.invocation().arguments().value()["max_output_bytes"].as_f64(),
        Some(30_000.0)
    );
    let bash_timeout =
        claude.prepare(request("Bash", r#"{"command":"true","timeout":600000}"#)?)?;
    assert_eq!(
        bash_timeout.invocation().arguments().value()["yield_time_ms"].as_f64(),
        Some(600_000.0)
    );
    let oversized = claude.prepare(request("Bash", r#"{"command":"true","timeout":600001}"#)?)?;
    assert_eq!(
        oversized.invocation().arguments().value()["yield_time_ms"].as_f64(),
        Some(600_000.0),
        "Claude Bash clamps explicit observation hints after projected schema admission"
    );
    let with_description = claude.prepare(request(
        "Bash",
        r#"{"command":"true","description":"No-op shell check"}"#,
    )?)?;
    assert_eq!(
        with_description.invocation().identity().name().as_str(),
        PROCESS_SESSION_CAPABILITY_NAME
    );
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one linear walk over the whole pi menu"
)]
fn the_pi_catalog_is_complete_exact_and_distinct_from_oauth() -> Result<(), TestError> {
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let registry = builder.freeze();
    let configuration = configuration(b"structured-chat-catalog")?;
    let exact_target = target("generic-chat-model", configuration)?;
    let kind = plakous_tools::CodingPresentation::Pi;
    let structured = kind.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        exact_target.clone(),
    )?;
    let view = structured.compile_request_view_v2()?;

    let core = kind
        .compile_surface(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            exact_target.clone(),
            &[],
        )?
        .exposed_names()
        .to_vec();
    assert_eq!(
        core.iter().map(ToolName::as_str).collect::<Vec<_>>(),
        ["bash", "edit", "read", "stop", "wait", "write"]
    );
    assert_eq!(
        view.definitions()
            .iter()
            .map(plakous_model::ProjectedToolDefinitionV2::exposed_name)
            .collect::<Vec<_>>(),
        core.iter().collect::<Vec<_>>()
    );
    assert!(view.definitions().iter().all(|definition| {
        matches!(
            definition.presentation(),
            plakous_model::ProjectedToolPresentationV2::StructuredFunction { .. }
        )
    }));
    // Every pi name is a menu name over a canonical capability, never a
    // canonical name of its own.
    for name in &core {
        assert!(registry.definition(name).is_none(), "{name:?}");
        let prepared = structured.prepare(request(name.as_str(), "{}")?);
        assert_ne!(
            prepared.err().map(|error| error.kind()),
            Some(ToolRuntimeErrorKind::UnknownTool)
        );
    }

    // Recompiling the same vocabulary over the same target reproduces the
    // exact map; another vocabulary and a neighbouring target do not.
    let recompiled = kind.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        exact_target.clone(),
    )?;
    assert_eq!(recompiled.digest(), structured.digest());
    let drifted_target = target("generic-chat-neighbor", configuration)?;
    assert_ne!(
        kind.projection_map(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            drifted_target
        )?
        .digest(),
        structured.digest()
    );

    for oauth in [
        plakous_tools::CodingPresentation::Codex,
        plakous_tools::CodingPresentation::Grok,
        plakous_tools::CodingPresentation::Claude,
    ] {
        let map = oauth.projection_map(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            exact_target.clone(),
        )?;
        assert_ne!(structured.digest(), map.digest());
        assert_ne!(
            core,
            oauth
                .compile_surface(
                    plakous_tools::JobConfig::default(),
                    registry.clone(),
                    exact_target.clone(),
                    &[]
                )?
                .exposed_names()
                .to_vec()
        );
    }
    let pi_bash = structured
        .entries()
        .iter()
        .find(|entry| entry.exposed_name().as_str() == "bash")
        .ok_or("pi bash is absent")?;
    let claude = plakous_tools::CodingPresentation::Claude.projection_map(
        plakous_tools::JobConfig::default(),
        registry,
        exact_target,
    )?;
    let claude_bash = claude
        .entries()
        .iter()
        .find(|entry| entry.exposed_name().as_str() == "Bash")
        .ok_or("Claude Bash is absent")?;
    assert_ne!(pi_bash.lowering_identity(), claude_bash.lowering_identity());
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn grok_grep_head_limit_executes_after_semantic_json() -> Result<(), TestError> {
    use std::fs;

    use plakous_model::{CancellationToken, Clock, FixedClock, Timestamp, ToolEffect};
    use plakous_tools::{
        EffectAuthorizationGrantSource, ExecutionEnvironment, ExecutionEnvironmentConfig,
        ExecutionEnvironmentScope, ProgressSink, ToolPolicyRevision,
    };

    let workspace = tempfile::tempdir()?;
    fs::write(
        workspace.path().join("hit.rs"),
        "exact one\nexact two\nexact three\n",
    )?;
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let registry = builder.freeze();
    let grok = plakous_tools::CodingPresentation::Grok.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target("grok-4.6", configuration(b"grok-grep-head-limit")?)?,
    )?;
    let prepared = grok.prepare(request(
        "grep",
        r#"{"pattern":"exact","glob":"*.rs","-i":true,"head_limit":2}"#,
    )?)?;
    assert_eq!(
        prepared.invocation().arguments().value()["head_limit"].as_f64(),
        Some(2.0)
    );
    let run_id = prepared.invocation().run_id();
    let turn_id = prepared.invocation().turn_id();
    let attempt_id = prepared.invocation().attempt_id();
    let call_id = prepared.invocation().call_id();
    let (invocation, _) = prepared.into_parts();
    let effect = registry
        .definition(invocation.identity().name())
        .ok_or("search_repository missing")?
        .effect();
    assert_eq!(effect, ToolEffect::ReadOnly);
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let revision = ToolPolicyRevision::new("grok-grep-head-limit", 1)?;
    let binding = plakous_tools::internal::authorization_binding(
        &invocation,
        environment.descriptor().clone(),
        revision,
    )?;
    let grant = plakous_tools::internal::authorization_grant(
        binding,
        EffectAuthorizationGrantSource::Policy,
    );
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(71, 0)?));
    let (progress, _receiver) = ProgressSink::channel(8, call_id, Arc::clone(&clock))?;
    let context = plakous_tools::internal::tool_context(
        run_id,
        turn_id,
        attempt_id,
        CancellationToken::new(),
        None,
        clock,
        environment,
        grant,
        progress,
    );
    let executor = plakous_tools::internal::executor(&registry, &invocation)?;
    let outcome = executor.execute(invocation, context).await?;
    let body = text(&outcome)?;
    // Grok `grep` lowers with `use_regex=true`; the regex path emits match rows
    // only (`path:line:text`), never the literal-path `matches=` header.
    let matches = body
        .lines()
        .filter(|line| line.starts_with("hit.rs:"))
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        2,
        "Grok grep head_limit must survive semantic JSON: {body}"
    );
    assert!(
        body.lines()
            .next()
            .is_some_and(|line| line.contains("incomplete=true")
                && line.contains("match bound: stopped after 2 matches")
                && line.contains("narrow")),
        "{body}"
    );
    Ok(())
}

#[test]
fn exec_command_yield_is_floored_and_capped_like_reference_unified_exec() -> Result<(), TestError> {
    use plakous_tools::{UNIFIED_EXEC_MAX_YIELD_MS, UNIFIED_EXEC_MIN_YIELD_MS};

    let (_, codex, _) = maps()?;
    let cases = [
        (
            r#"{"cmd":"true","yield_time_ms":0}"#,
            UNIFIED_EXEC_MIN_YIELD_MS,
        ),
        (
            r#"{"cmd":"true","yield_time_ms":1}"#,
            UNIFIED_EXEC_MIN_YIELD_MS,
        ),
        (r#"{"cmd":"true"}"#, 30_000),
        (r#"{"cmd":"true","yield_time_ms":30000}"#, 30_000),
        (
            r#"{"cmd":"true","yield_time_ms":900000}"#,
            UNIFIED_EXEC_MAX_YIELD_MS,
        ),
        (
            r#"{"cmd":"","session_id":1,"yield_time_ms":0}"#,
            UNIFIED_EXEC_MIN_YIELD_MS,
        ),
        (r#"{"cmd":"","session_id":1}"#, 10_000),
    ];
    for (arguments, expected) in cases {
        let projected = codex.prepare(request("exec_command", arguments)?)?;
        let lowered = projected.invocation().arguments().value()["yield_time_ms"]
            .as_u64()
            .ok_or("yield_time_ms must lower to an integer")?;
        assert_eq!(lowered, expected, "{arguments}");
    }
    Ok(())
}

#[test]
fn all_process_start_vocabularies_treat_zero_as_an_observation_hint() -> Result<(), TestError> {
    let (registry, codex, grok) = maps()?;
    let claude = ProcessToolProjectionKind::ClaudeOAuthBash.projection_map(
        plakous_tools::JobConfig::default(),
        registry,
        target("claude-exact", configuration(b"zero-hint")?)?,
    )?;
    for (map, name, arguments) in [
        (
            &codex,
            "exec_command",
            r#"{"cmd":"true","yield_time_ms":0}"#,
        ),
        (
            &grok,
            "run_terminal_command",
            r#"{"command":"true","description":"verify","block_until_ms":0}"#,
        ),
        (&claude, "Bash", r#"{"command":"true","timeout":0}"#),
    ] {
        let projected = map.prepare(request(name, arguments)?)?;
        assert_eq!(
            projected.invocation().arguments().value()["yield_time_ms"].as_u64(),
            Some(250),
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn write_stdin_poll_and_input_use_distinct_observation_windows() -> Result<(), TestError> {
    let (_, codex, _) = maps()?;
    for (chars, hint, expected) in [
        ("", 0, 5000),
        ("", 600_000, 300_000),
        ("input", 0, 250),
        ("input", 600_000, 30_000),
    ] {
        let arguments = json!({"session_id": 1, "chars": chars, "yield_time_ms": hint}).to_string();
        let projected = codex.prepare(request("write_stdin", &arguments)?)?;
        assert_eq!(
            projected.invocation().arguments().value()["yield_time_ms"].as_u64(),
            Some(expected)
        );
    }
    Ok(())
}

/// Live-drive regression: a model that sends `yield_time_ms: 0` (with a
/// hallucinated `session_id`) must still receive the joined output of a short
/// command, not an empty running observation it never polls.
#[cfg(unix)]
#[tokio::test]
async fn exec_command_with_zero_yield_returns_the_joined_output() -> Result<(), TestError> {
    use plakous_model::{CancellationToken, Clock, FixedClock, Timestamp};
    use plakous_tools::{
        EffectAuthorizationGrantSource, ExecutionEnvironment, ExecutionEnvironmentConfig,
        ExecutionEnvironmentScope, ProgressSink, ToolPolicyRevision,
    };

    let workspace = tempfile::tempdir()?;
    let (registry, codex, _) = maps()?;
    let prepared = codex.prepare(request(
        "exec_command",
        r#"{"cmd":"printf 'drive output'","workdir":".","shell":"/bin/sh","login":false,"tty":false,"yield_time_ms":0,"max_output_tokens":10000,"session_id":1}"#,
    )?)?;
    let receipt = prepared
        .receipt()
        .ok_or("missing projection receipt")?
        .clone();
    let run_id = prepared.invocation().run_id();
    let turn_id = prepared.invocation().turn_id();
    let attempt_id = prepared.invocation().attempt_id();
    let call_id = prepared.invocation().call_id();
    let (invocation, _) = prepared.into_parts();
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let sessions = environment.joined_process_sessions()?;
    let revision = ToolPolicyRevision::new("exec-zero-yield", 1)?;
    let binding = plakous_tools::internal::authorization_binding(
        &invocation,
        environment.descriptor().clone(),
        revision,
    )?;
    let grant = plakous_tools::internal::authorization_grant(
        binding,
        EffectAuthorizationGrantSource::Policy,
    );
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(72, 0)?));
    let (progress, _receiver) = ProgressSink::channel(8, call_id, Arc::clone(&clock))?;
    let context = plakous_tools::internal::tool_context(
        run_id,
        turn_id,
        attempt_id,
        CancellationToken::new(),
        None,
        clock,
        environment,
        grant,
        progress,
    );
    let executor = plakous_tools::internal::executor(&registry, &invocation)?;
    let outcome = executor.execute(invocation, context).await?;
    let canonical: Value = serde_json::from_str(text(&outcome)?)?;
    assert_eq!(canonical["status"], "exited", "{canonical}");
    let raised = codex.raise(&receipt, outcome)?;
    assert_eq!(
        serde_json::from_str::<Value>(text(&raised)?)?,
        serde_json::json!({"exit_code":0,"output":"drive output"})
    );
    assert!(sessions.is_drained());
    Ok(())
}

/// An absolute `workdir` is refused before any process starts (seen on
/// nemotron js-ptc). It must surface as a correctable tool failure, never as
/// `OutcomeUnknown`, which would force the whole session into recovery.
#[tokio::test]
async fn exec_command_with_absolute_workdir_is_a_no_effect_failure() -> Result<(), TestError> {
    use plakous_model::{CancellationToken, Clock, FixedClock, Timestamp};
    use plakous_tools::{
        EffectAuthorizationGrantSource, ExecutionEnvironment, ExecutionEnvironmentConfig,
        ExecutionEnvironmentScope, ProgressSink, ToolOutcomeKind, ToolPolicyRevision,
    };

    let workspace = tempfile::tempdir()?;
    let (registry, codex, _) = maps()?;
    let prepared = codex.prepare(request(
        "exec_command",
        r#"{"cmd":"printf 'never runs'","workdir":"/private/tmp/elsewhere","yield_time_ms":0}"#,
    )?)?;
    let run_id = prepared.invocation().run_id();
    let turn_id = prepared.invocation().turn_id();
    let attempt_id = prepared.invocation().attempt_id();
    let call_id = prepared.invocation().call_id();
    let (invocation, _) = prepared.into_parts();
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let sessions = environment.joined_process_sessions()?;
    let revision = ToolPolicyRevision::new("exec-abs-workdir", 1)?;
    let binding = plakous_tools::internal::authorization_binding(
        &invocation,
        environment.descriptor().clone(),
        revision,
    )?;
    let grant = plakous_tools::internal::authorization_grant(
        binding,
        EffectAuthorizationGrantSource::Policy,
    );
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(72, 0)?));
    let (progress, _receiver) = ProgressSink::channel(8, call_id, Arc::clone(&clock))?;
    let context = plakous_tools::internal::tool_context(
        run_id,
        turn_id,
        attempt_id,
        CancellationToken::new(),
        None,
        clock,
        environment,
        grant,
        progress,
    );
    let executor = plakous_tools::internal::executor(&registry, &invocation)?;
    let outcome = executor.execute(invocation, context).await?;
    assert_eq!(outcome.kind(), ToolOutcomeKind::Failure);
    assert!(
        text(&outcome)?.contains("path is outside the workspace"),
        "{outcome:?}"
    );
    assert!(sessions.is_drained());
    Ok(())
}

/// One Grok or pi process call driven the way a leaf runs it: prepared by the
/// pack projection, executed by the process capability and raised by the
/// pack. With `hub`, the same call runs through the pack-neutral entry points
/// a node granted agent tools uses for a process-only call
/// ([`ProcessSessionTools::await_jobs`], [`ProcessSessionTools::await_first_job`],
/// [`ProcessSessionTools::stop_job`]), whose text the claiming capability
/// shows the model as is.
async fn drive_process(
    environment: &Arc<plakous_tools::ExecutionEnvironment>,
    registry: &FrozenToolRegistry,
    grok: &ToolProjectionMapV2,
    name: &str,
    arguments: &Value,
    hub: bool,
) -> Result<ToolOutcome, TestError> {
    use plakous_model::{CancellationToken, Clock, FixedClock, Timestamp};
    use plakous_tools::{EffectAuthorizationGrantSource, ProgressSink, ToolPolicyRevision};

    let prepared = grok.prepare(request(name, &arguments.to_string())?)?;
    let receipt = prepared.receipt().cloned();
    let run_id = prepared.invocation().run_id();
    let turn_id = prepared.invocation().turn_id();
    let attempt_id = prepared.invocation().attempt_id();
    let call_id = prepared.invocation().call_id();
    let (invocation, _) = prepared.into_parts();
    let binding = plakous_tools::internal::authorization_binding(
        &invocation,
        environment.descriptor().clone(),
        ToolPolicyRevision::new("grok-hub-leaf", 1)?,
    )?;
    let grant = plakous_tools::internal::authorization_grant(
        binding,
        EffectAuthorizationGrantSource::Policy,
    );
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(72, 0)?));
    let (progress, _receiver) = ProgressSink::channel(8, call_id, Arc::clone(&clock))?;
    let context = plakous_tools::internal::tool_context(
        run_id,
        turn_id,
        attempt_id,
        CancellationToken::new(),
        None,
        clock,
        Arc::clone(environment),
        grant,
        progress,
    );
    let number = |id: &Value| -> Result<u64, TestError> {
        let id = id.as_str().ok_or("task id")?;
        Ok(id.strip_prefix('p').unwrap_or(id).parse()?)
    };
    if hub && name == "get_command_or_subagent_output" {
        let numbers = arguments["task_ids"]
            .as_array()
            .ok_or("task_ids")?
            .iter()
            .map(number)
            .collect::<Result<Vec<_>, _>>()?;
        let timeout_ms = arguments["timeout_ms"].as_u64().unwrap_or(0);
        return Ok(ProcessSessionTools::await_jobs(&context, &numbers, timeout_ms).await?);
    }
    if hub && name == "kill_command_or_subagent" {
        return Ok(ProcessSessionTools::stop_job(&context, number(&arguments["task_id"])?).await?);
    }
    if hub && name == "wait" {
        let numbers = arguments
            .get("ids")
            .and_then(Value::as_array)
            .map_or_else(|| Ok(Vec::new()), |ids| ids.iter().map(number).collect())?;
        let timeout_ms = arguments["timeout_ms"].as_u64().unwrap_or(300_000);
        return Ok(ProcessSessionTools::await_first_job(&context, &numbers, timeout_ms).await?);
    }
    if hub && name == "stop" {
        return Ok(ProcessSessionTools::stop_job(&context, number(&arguments["id"])?).await?);
    }
    let executor = plakous_tools::internal::executor(registry, &invocation)?;
    let outcome = executor.execute(invocation, context).await?;
    Ok(grok.raise(&receipt.ok_or("missing projection receipt")?, outcome)?)
}

/// A Grok output or kill call naming only process ids shows the model the
/// same bytes on a node granted agent tools as on a leaf: per-task rows,
/// `not_found` tombstones and the stop result alike.
#[cfg(unix)]
#[tokio::test]
async fn grok_process_only_calls_show_the_same_bytes_on_hubs_and_leaves() -> Result<(), TestError> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let (registry, _, grok) = maps()?;
    let mut shown = Vec::new();
    for hub in [false, true] {
        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000101",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let mut texts = Vec::new();
        for (name, arguments) in [
            (
                "run_terminal_command",
                json!({"command": "sleep 0.5; printf done", "description": "job", "block_until_ms": 0}),
            ),
            (
                "run_terminal_command",
                json!({"command": "sleep 30", "description": "server", "block_until_ms": 0}),
            ),
            (
                "get_command_or_subagent_output",
                json!({"task_ids": ["p1", "9"], "timeout_ms": 10_000}),
            ),
            (
                "get_command_or_subagent_output",
                json!({"task_ids": ["p2"]}),
            ),
            ("kill_command_or_subagent", json!({"task_id": "p2"})),
        ] {
            let outcome =
                drive_process(&environment, &registry, &grok, name, &arguments, hub).await?;
            texts.push((name, outcome.kind(), text(&outcome)?.to_owned()));
        }
        shown.push(texts);
    }
    let [leaf, hub] = shown.as_slice() else {
        return Err("two runs".into());
    };
    let output: Value = serde_json::from_str(&leaf[2].2)?;
    assert_eq!(output["tasks"][0]["output"], "done", "{output}");
    assert_eq!(output["tasks"][1]["status"], "not_found", "{output}");
    for (leaf, hub) in leaf.iter().zip(hub).skip(2) {
        assert_eq!(leaf, hub);
    }
    Ok(())
}

/// A pi `wait` or `stop` naming only process ids, and a bare pi `wait` with no
/// child pending, shows the model the same bytes on a node granted agent
/// tools as on a leaf: first-terminal rows, `not_found` tombstones, the bare
/// await over every unreported job and the stop result alike.
#[cfg(unix)]
#[tokio::test]
async fn pi_process_only_calls_show_the_same_bytes_on_hubs_and_leaves() -> Result<(), TestError> {
    use plakous_tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    };

    let registry = registry()?;
    let pi = ProcessToolProjectionKind::PiBash.projection_map(
        plakous_tools::JobConfig::default(),
        registry.clone(),
        target("pi-exact", configuration(b"process-projection-evidence")?)?,
    )?;
    let mut shown = Vec::new();
    for hub in [false, true] {
        let workspace = tempfile::tempdir()?;
        let environment = Arc::new(ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(RunId::from_str(
                "00000000-0000-0000-0000-000000000101",
            )?),
            workspace.path(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )?);
        let mut texts = Vec::new();
        for (name, arguments) in [
            (
                "bash",
                json!({"command": "sleep 0.5; printf done", "timeout": 0}),
            ),
            ("bash", json!({"command": "sleep 30", "timeout": 0})),
            ("wait", json!({"ids": ["p1", "9"], "timeout_ms": 10_000})),
            ("wait", json!({"ids": ["p2"], "timeout_ms": 0})),
            ("stop", json!({"id": "p2"})),
            ("wait", json!({"timeout_ms": 10_000})),
        ] {
            let outcome = drive_process(&environment, &registry, &pi, name, &arguments, hub)
                .await
                .map_err(|error| format!("{name} {arguments} hub={hub}: {error:?}"))?;
            texts.push((name, outcome.kind(), text(&outcome)?.to_owned()));
        }
        shown.push(texts);
    }
    let [leaf, hub] = shown.as_slice() else {
        return Err("two runs".into());
    };
    let output: Value = serde_json::from_str(&leaf[2].2)?;
    assert_eq!(output["tasks"][0]["output"], "done", "{output}");
    assert_eq!(output["tasks"][1]["status"], "not_found", "{output}");
    for (leaf, hub) in leaf.iter().zip(hub).skip(2) {
        assert_eq!(leaf, hub);
    }
    Ok(())
}
