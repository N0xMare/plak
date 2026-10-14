//! Byte-identical pins on the model-visible Code Mode grammar.
//!
//! The rendered `exec` and `wait` descriptions and the `exec` input grammar are
//! frozen upstream wording. These goldens are what proves the reviewed vendor
//! snapshot in `vendor/codex-code-mode-protocol` renders exactly what the
//! former `git` dependency rendered. A diff here is a change to what the model
//! reads and needs its own review, never a golden refresh.

use plakous_code_mode::{
    CodeModeToolKind, ExecDescription, ToolDefinition, ToolName, ToolNamespaceDescription,
    build_exec_description, build_wait_description, parse_exec_source,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;

#[test]
fn pinned_descriptions_and_grammar_render_byte_identically()
-> Result<(), Box<dyn std::error::Error>> {
    let enabled = vec![
        ToolDefinition::try_new(
            "exec_command",
            ToolName::namespaced("functions", "exec_command")?,
            "Runs a shell command.",
            CodeModeToolKind::Function,
            Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "array", "items": {"type": "string"}},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["command"]
            })),
            None,
        )?,
        ToolDefinition::try_new(
            "mcp__ologs__get_profile",
            ToolName::namespaced("mcp__ologs", "get_profile")?,
            "Reads one profile.",
            CodeModeToolKind::Freeform,
            None,
            Some(serde_json::json!({"type": "string"})),
        )?,
    ];
    let deferred = vec![ToolDefinition::try_new(
        "apply_patch",
        ToolName::plain("apply_patch")?,
        "Applies a patch.",
        CodeModeToolKind::Freeform,
        None,
        None,
    )?];
    let mut namespaces = BTreeMap::new();
    namespaces.insert(
        "mcp__ologs".to_string(),
        ToolNamespaceDescription {
            name: "mcp__ologs".into(),
            description: "Ologs MCP server.".into(),
        },
    );
    for (label, visible, only) in [
        ("exec_visible_mixed", true, false),
        ("exec_hidden_code_mode_only", false, true),
    ] {
        let rendered = build_exec_description(&ExecDescription {
            enabled_tools: &enabled,
            deferred_tools: &deferred,
            namespace_descriptions: &namespaces,
            default_exec_yield_time_ms: 10_000,
            code_mode_only: only,
            image_detail_visible: visible,
        });
        assert_eq!(rendered, golden(label), "{label} description moved");
    }
    assert_eq!(
        build_wait_description(),
        golden("wait"),
        "wait description moved"
    );
    let mut parsed = String::new();
    for input in [
        "// @exec: {\"yield_time_ms\": 2500, \"max_output_tokens\": 4096}\nawait tools.exec_command({command:[\"ls\"]});\n",
        "const x = 1;\n",
        "// @exec: {}\n1 + 1\n",
        "// @exec: {\"yield_time_ms\": 99999999999999999999}\n1\n",
        "// @exec: not json\n1\n",
        "",
    ] {
        writeln!(parsed, "{input:?}\n=> {:?}\n", parse_exec_source(input))?;
    }
    assert_eq!(parsed, golden("parse_exec_source"), "exec grammar moved");
    Ok(())
}

/// Returns one checked-in golden by name.
fn golden(label: &str) -> &'static str {
    match label {
        "exec_visible_mixed" => include_str!("goldens/exec_visible_mixed.txt"),
        "exec_hidden_code_mode_only" => include_str!("goldens/exec_hidden_code_mode_only.txt"),
        "wait" => include_str!("goldens/wait.txt"),
        "parse_exec_source" => include_str!("goldens/parse_exec_source.txt"),
        other => panic!("no golden named {other}"),
    }
}
