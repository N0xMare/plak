//! Claude OAuth recommended profile base instructions.
//!
//! Anthropic does not publish the Claude Code base prompt, so none of it is
//! copied. The parity base is the Plakous-authored conduct block
//! ([`super::plak_authored`]) followed by the Plakous paragraph that names the
//! tools this family presents. Topology tools (spawn, submit_result, ...) are
//! described in the request when granted, so the text must not claim the
//! coding pack is the full set.

use std::sync::LazyLock;

use super::legacy;
use super::plak_authored::PLAK_AUTHORED_CONDUCT;
use super::variant::PromptVariant;

static CLAUDE_PARITY_DIRECT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{}\n\n{}",
        PLAK_AUTHORED_CONDUCT,
        legacy::claude_direct_tool_paragraph()
    )
});

/// Claude OAuth Direct coding profile base, for one variant.
#[must_use]
pub fn claude_direct_base_instructions_for(variant: PromptVariant) -> &'static str {
    match variant {
        PromptVariant::Parity => CLAUDE_PARITY_DIRECT.as_str(),
        PromptVariant::Legacy => legacy::PLAKOUS_CLAUDE_DIRECT_BASE_INSTRUCTIONS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parity_base_is_plakous_conduct_plus_the_tool_paragraph() {
        let parity = claude_direct_base_instructions_for(PromptVariant::Parity);
        assert!(parity.starts_with("# How you communicate"));
        assert!(parity.ends_with(legacy::claude_direct_tool_paragraph()));
        // The conduct block opens the base and the identity tail names the
        // model; the tool paragraph no longer opens with a third identity.
        assert!(!parity.contains("You are Codex"));
        assert!(!parity.contains("You are a precise and proficient coding agent"));
        assert!(parity.contains("## Destructive actions"));
        assert!(parity.contains("search with grep, rg or find"));
        assert!(!parity.contains("Grep"));
        assert!(!parity.contains("full set"));
        assert!(!parity.contains("You are Claude Code"));
    }

    /// Nothing of the Codex copy leaks into a family whose prompt is not open
    /// source.
    #[test]
    fn the_parity_base_copies_nothing_from_the_codex_vendor_snapshot() {
        let parity = claude_direct_base_instructions_for(PromptVariant::Parity);
        for vendored in [
            "You are Codex",
            "# Working with the user",
            "## Autonomy and Persistence",
            "GitHub-flavored Markdown",
        ] {
            assert!(!parity.contains(vendored), "{vendored}");
        }
    }

    #[test]
    fn the_legacy_variant_is_the_short_base() {
        assert_eq!(
            claude_direct_base_instructions_for(PromptVariant::Legacy),
            legacy::PLAKOUS_CLAUDE_DIRECT_BASE_INSTRUCTIONS
        );
    }
}
