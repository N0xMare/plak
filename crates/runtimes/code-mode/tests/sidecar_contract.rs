//! Focused parse/DTO and exact protocol-helper contracts.
#![allow(clippy::expect_used, reason = "focused sidecar boundary tests")]

use plakous_code_mode::*;
use serde_json::json;
use std::collections::BTreeMap;

fn one_tool() -> ToolDefinition {
    ToolDefinition::try_new(
        "exec_command",
        ToolName::plain("exec_command").expect("valid name"),
        "Run a bounded command",
        CodeModeToolKind::Function,
        Some(json!({"type":"object","properties":{},"additionalProperties":false})),
        None,
    )
    .expect("valid tool")
}

fn seven_tools() -> Vec<ToolDefinition> {
    V1_NESTED_TOOL_NAMES
        .iter()
        .map(|name| {
            ToolDefinition::try_new(
                *name,
                ToolName::plain(*name).expect("valid canonical name"),
                format!("Frozen V1 {name} tool"),
                CodeModeToolKind::Function,
                Some(json!({"type":"object","properties":{},"additionalProperties":true})),
                None,
            )
            .expect("valid frozen tool")
        })
        .collect()
}

#[test]
fn constructors_reject_unbounded_and_conflicting_dtos() {
    assert!(CellId::try_new("").is_err());
    assert!(CellId::try_new("x".repeat(MAX_ID_BYTES + 1)).is_err());
    assert!(ExecutionLimits::try_new(0, 0).is_ok());
    assert!(ExecutionLimits::try_new(MAX_YIELD_TIME_MS, MAX_OUTPUT_TOKENS).is_ok());
    assert!(ExecutionLimits::try_new(MAX_YIELD_TIME_MS + 1, 1).is_err());
    assert!(ExecutionLimits::try_new(1, MAX_OUTPUT_TOKENS + 1).is_err());
    assert!(WaitRequest::try_new(CellId::try_new("cell").expect("cell"), 0).is_ok());
    assert!(ExecuteRequest::try_new("outer", seven_tools(), "text('ok')", 1, 1).is_ok());
    assert!(
        ExecuteRequest::try_new(
            "outer",
            seven_tools(),
            "x".repeat(MAX_SOURCE_BYTES + 1),
            1,
            1
        )
        .is_err()
    );
    assert!(ExecuteRequest::try_new("outer", vec![one_tool()], "text('x')", 1, 1).is_err());
    assert!(
        ExecuteRequest::try_new_bounded_catalog("outer", vec![one_tool()], "text('x')", 1, 1)
            .is_ok()
    );
    assert!(
        ExecuteRequest::try_new_bounded_catalog("outer", Vec::new(), "text('x')", 1, 1).is_err()
    );
    assert!(
        ExecuteRequest::try_new_bounded_catalog(
            "outer",
            vec![one_tool(), one_tool()],
            "text('x')",
            1,
            1
        )
        .is_err()
    );
    let mut reordered = seven_tools();
    reordered.swap(0, 1);
    assert!(ExecuteRequest::try_new("outer", reordered, "text('x')", 1, 1).is_err());
}

#[test]
fn exact_exec_parser_and_description_use_official_protocol() {
    let parsed = parse_exec_source(
        "// @exec: {\"yield_time_ms\": 7, \"max_output_tokens\": 11}\ntext('ok')",
    )
    .expect("valid source");
    assert_eq!(parsed.code, "text('ok')");
    assert_eq!(parsed.yield_time_ms, Some(7));
    assert!(parse_exec_source("// @exec: {\"unknown\": 1}\ntext('x')").is_err());
    let tool = one_tool();
    let description = build_exec_description(&ExecDescription {
        enabled_tools: &[tool],
        deferred_tools: &[],
        namespace_descriptions: &BTreeMap::new(),
        default_exec_yield_time_ms: 7,
        code_mode_only: true,
        image_detail_visible: false,
    });
    assert!(description.contains("exec_command"));
    assert!(description.contains("Defaults to 7 ms."));
}
