//! SuperGrok / Grok OAuth recommended profile base instructions.
//!
//! xAI does not publish the Grok coding prompt, so none of it is copied. The
//! parity base is the Plakous-authored conduct block
//! ([`super::plak_authored`]) followed by the Plakous paragraph that names the
//! tools this family presents.

use std::sync::LazyLock;

use super::legacy;
use super::plak_authored::PLAK_AUTHORED_CONDUCT;
use super::variant::PromptVariant;

static GROK_PARITY_DIRECT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{}\n\n{}",
        PLAK_AUTHORED_CONDUCT,
        legacy::grok_direct_tool_paragraph()
    )
});

/// Grok OAuth Direct coding profile base, for one variant.
#[must_use]
pub fn grok_direct_base_instructions_for(variant: PromptVariant) -> &'static str {
    match variant {
        PromptVariant::Parity => GROK_PARITY_DIRECT.as_str(),
        PromptVariant::Legacy => legacy::PLAKOUS_GROK_DIRECT_BASE_INSTRUCTIONS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parity_base_is_plakous_conduct_plus_the_tool_paragraph() {
        let parity = grok_direct_base_instructions_for(PromptVariant::Parity);
        assert!(parity.starts_with("# How you communicate"));
        assert!(parity.ends_with(legacy::grok_direct_tool_paragraph()));
        // The conduct block opens the base and the identity tail names the
        // model; the tool paragraph no longer opens with a third identity.
        assert!(!parity.contains("You are Codex"));
        assert!(!parity.contains("You are a precise and proficient coding agent"));
        assert!(parity.contains("## Editing constraints"));
        assert!(parity.contains("no cat/sed/rg"));
        assert!(!parity.contains("run_terminal_cmd"));
    }

    /// Claude and Grok carry the same Plakous conduct block and differ only in
    /// the tool paragraph, so a footprint comparison across the two is a
    /// comparison of tool surfaces.
    #[test]
    fn claude_and_grok_share_one_conduct_block() {
        let grok = grok_direct_base_instructions_for(PromptVariant::Parity);
        let claude =
            super::super::claude::claude_direct_base_instructions_for(PromptVariant::Parity);
        assert!(grok.starts_with(PLAK_AUTHORED_CONDUCT));
        assert!(claude.starts_with(PLAK_AUTHORED_CONDUCT));
        assert_ne!(grok, claude);
    }

    #[test]
    fn the_legacy_variant_is_the_short_base() {
        assert_eq!(
            grok_direct_base_instructions_for(PromptVariant::Legacy),
            legacy::PLAKOUS_GROK_DIRECT_BASE_INSTRUCTIONS
        );
    }
}
