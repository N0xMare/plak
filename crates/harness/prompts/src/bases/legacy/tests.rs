use super::*;

/// The control arm is the text that shipped, to the byte.
#[test]
fn the_legacy_bases_are_unchanged() {
    assert_eq!(
        [
            PLAKOUS_CODEX_DIRECT_BASE_INSTRUCTIONS,
            PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE,
        ]
        .concat()
        .len(),
        998
    );
    assert_eq!(PLAKOUS_CLAUDE_DIRECT_BASE_INSTRUCTIONS.len(), 341);
    assert_eq!(PLAKOUS_GROK_DIRECT_BASE_INSTRUCTIONS.len(), 382);
    for text in [
        PLAKOUS_CODEX_DIRECT_BASE_INSTRUCTIONS,
        PLAKOUS_CLAUDE_DIRECT_BASE_INSTRUCTIONS,
        PLAKOUS_GROK_DIRECT_BASE_INSTRUCTIONS,
    ] {
        assert!(text.starts_with("You are a precise and proficient coding agent."));
        assert!(text.contains("When the assignment may mutate"));
        assert!(text.contains("do not invent tools that are not presented"));
        assert!(!text.contains('&'));
    }
    assert!(!PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE.contains('&'));
    assert!(
        PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE.contains("operating in Codex Code Mode")
    );
    assert!(
        PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE
            .contains("Direct calls to nested tool names are unavailable")
    );
    assert!(PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE.contains("{names}"));
    // Claude's pack has no search tool: search goes through the shell.
    assert!(PLAKOUS_CLAUDE_DIRECT_BASE_INSTRUCTIONS.contains("search with grep, rg or find"));
    for removed in ["Grep", "Glob", "TodoWrite", "BashOutput"] {
        assert!(!PLAKOUS_CLAUDE_DIRECT_BASE_INSTRUCTIONS.contains(removed));
    }
    assert!(!PLAKOUS_CLAUDE_DIRECT_BASE_INSTRUCTIONS.contains("full set"));
    assert!(PLAKOUS_GROK_DIRECT_BASE_INSTRUCTIONS.contains("no cat/sed/rg"));
    assert!(!PLAKOUS_GROK_DIRECT_BASE_INSTRUCTIONS.contains("run_terminal_cmd"));
}

/// Every prefix the parity compositions strip is really there, and what
/// is left still carries the tool naming and the guardrail.
#[test]
fn legacy_bases_open_with_the_identity_clause() {
    for (whole, stripped) in [
        (
            PLAKOUS_CODEX_DIRECT_BASE_INSTRUCTIONS,
            codex_direct_tool_paragraph(),
        ),
        (
            PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE,
            codex_code_mode_tool_paragraph(),
        ),
        (
            PLAKOUS_CLAUDE_DIRECT_BASE_INSTRUCTIONS,
            claude_direct_tool_paragraph(),
        ),
        (
            PLAKOUS_GROK_DIRECT_BASE_INSTRUCTIONS,
            grok_direct_tool_paragraph(),
        ),
    ] {
        assert!(
            stripped.len() < whole.len(),
            "prefix did not match: {whole}"
        );
        assert!(whole.ends_with(stripped));
        assert!(!stripped.starts_with("You are"));
        assert!(stripped.contains("do not invent tools that are not presented"));
    }
    assert_eq!(
        codex_direct_tool_paragraph().len(),
        PLAKOUS_CODEX_DIRECT_BASE_INSTRUCTIONS.len() - LEGACY_IDENTITY_CLAUSE.len()
    );
    assert!(
        codex_code_mode_tool_paragraph()
            .starts_with("The complete provider-visible tool set is exec and wait.")
    );
}

#[test]
fn the_legacy_code_mode_render_lists_the_catalog() {
    let rendered = render_codex_code_mode_base_instructions(&[
        "exec_command",
        "write_stdin",
        "apply_patch",
        "update_plan",
    ]);
    assert!(rendered.contains(
        "nested tools on its tools object: exec_command, write_stdin, apply_patch, update_plan."
    ));
    assert!(!rendered.contains("read_file"));
    assert!(!rendered.contains("{names}"));
}
