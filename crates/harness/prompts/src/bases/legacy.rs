//! The family bases that shipped before the prompt-parity work.
//!
//! These are kept whole, under a name that says what they are, so the eval
//! plane can A/B them against the parity bases on one task set
//! ([`super::PromptVariant`]). They are not the default and are not where new
//! wording goes; a change here is only ever a change to the control arm.
//!
//! Each is about 400 characters: the family's tool names, the rule that a
//! mutating assignment uses the edit tool, and the no-invented-tools
//! guardrail. What they never carried is the behaviour a frontier model is
//! tuned to expect — answer discipline, autonomy, editing constraints,
//! destructive-action care — which is what the parity bases add.

/// Codex OAuth Direct coding profile, the legacy arm.
pub const PLAKOUS_CODEX_DIRECT_BASE_INSTRUCTIONS: &str = "You are a precise and proficient coding agent. Use exec_command to inspect the workspace, write_stdin only for an existing session, and update_plan for the task list. When the assignment may mutate, use apply_patch with the Codex *** Begin Patch envelope to edit files. Prefer apply_patch over sed or heredoc rewrites when editing. Use only the tools provided; do not invent tools that are not presented.";

/// Codex OAuth Code Mode coding profile template, the legacy arm; `{names}` is
/// the compiled nested catalog.
pub const PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE: &str = "You are a precise and proficient coding agent operating in Codex Code Mode. The complete provider-visible tool set is exec and wait. Use exec with raw JavaScript to compose the nested tools on its tools object: {names}. Prefer apply_patch for edits when the assignment may mutate. Use wait only to resume a yielded exec cell. Resident store/load state may coordinate sequential exec calls. Direct calls to nested tool names are unavailable. Never claim a nested effect succeeded unless its returned result proves success. Use only the tools provided; do not invent tools that are not presented.";

/// Claude OAuth Direct coding profile, the legacy arm. Topology tools (spawn,
/// submit_result, ...) are described in the request when granted, so the text
/// must not claim the coding pack is the full set.
pub const PLAKOUS_CLAUDE_DIRECT_BASE_INSTRUCTIONS: &str = "You are a precise and proficient coding agent. Use Read for file contents and Bash for shell commands, including search with grep, rg or find. When the assignment may mutate, use Edit for exact changes and Write for new files. Prefer Edit over rewriting files in Bash. Use only the tools provided; do not invent tools that are not presented.";

/// Grok OAuth Direct coding profile, the legacy arm.
pub const PLAKOUS_GROK_DIRECT_BASE_INSTRUCTIONS: &str = "You are a precise and proficient coding agent. Use read_file and grep for file and repository reads. When the assignment may mutate, use search_replace for edits. Use run_terminal_command for commands; do not use it for reads or edits (no cat/sed/rg when a file tool exists). Use todo_write for the task list. Use only the tools provided; do not invent tools that are not presented.";

/// The opening sentence every legacy family base shares.
///
/// On the legacy variant it is the only identity the model is given. On the
/// parity variant the base already opens with an identity — the copied Codex
/// line, or the Plakous conduct block — and the identity tail names the model
/// after it, so the parity compositions drop this clause and keep the rest of
/// the paragraph.
pub const LEGACY_IDENTITY_CLAUSE: &str = "You are a precise and proficient coding agent. ";

/// The Codex Code Mode paragraph's opening sentence, which also names the
/// composition. The sentence after it does too, so the parity composition can
/// drop the whole thing.
pub const LEGACY_CODE_MODE_IDENTITY_CLAUSE: &str =
    "You are a precise and proficient coding agent operating in Codex Code Mode. ";

/// One family paragraph with its opening identity sentence removed.
///
/// A missing prefix returns the paragraph whole rather than panicking or
/// silently trimming something else: the parity variant would then restate an
/// identity, which is a wording flaw, where a wrong trim would be a content
/// loss. `legacy_bases_open_with_the_identity_clause` is the test that keeps
/// the prefixes honest.
fn without_identity_clause(paragraph: &'static str, clause: &str) -> &'static str {
    paragraph.strip_prefix(clause).unwrap_or(paragraph)
}

/// The Codex Direct paragraph without its opening identity sentence.
#[must_use]
pub fn codex_direct_tool_paragraph() -> &'static str {
    without_identity_clause(
        PLAKOUS_CODEX_DIRECT_BASE_INSTRUCTIONS,
        LEGACY_IDENTITY_CLAUSE,
    )
}

/// The Codex Code Mode template without its opening identity sentence.
#[must_use]
pub fn codex_code_mode_tool_paragraph() -> &'static str {
    without_identity_clause(
        PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE,
        LEGACY_CODE_MODE_IDENTITY_CLAUSE,
    )
}

/// The Claude Direct paragraph without its opening identity sentence.
#[must_use]
pub fn claude_direct_tool_paragraph() -> &'static str {
    without_identity_clause(
        PLAKOUS_CLAUDE_DIRECT_BASE_INSTRUCTIONS,
        LEGACY_IDENTITY_CLAUSE,
    )
}

/// The Grok Direct paragraph without its opening identity sentence.
#[must_use]
pub fn grok_direct_tool_paragraph() -> &'static str {
    without_identity_clause(
        PLAKOUS_GROK_DIRECT_BASE_INSTRUCTIONS,
        LEGACY_IDENTITY_CLAUSE,
    )
}

/// Renders the legacy Code Mode base from the compiled nested catalog names.
#[must_use]
pub fn render_codex_code_mode_base_instructions(nested_names: &[&str]) -> String {
    PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE.replace("{names}", &nested_names.join(", "))
}

#[cfg(test)]
mod tests {
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
            PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE
                .contains("operating in Codex Code Mode")
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
}
