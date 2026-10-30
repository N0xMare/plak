//! Codex OAuth recommended profile base instructions.
//!
//! The parity base is the kept Codex sections, copied at
//! `vendor/codex-prompt` and joined by [`super::codex_vendor`], followed by
//! the Plakous paragraph that names the tools this composition actually
//! presents. The GPT-6 family uses the separately pinned 0.163.0 template
//! with the recorded edits in its provenance. Source snapshots are unchanged;
//! the final assembler omits
//! the tool-specific editing bullet when `apply_patch` is absent. The
//! environment block that corrects the copied
//! text's sandbox and approval assumptions is written after the base by
//! [`crate::plan::PromptPlan`].
//!
//! Every entry here takes the variant explicitly. Nothing in this crate reads
//! the environment to decide what a model is told: the product reads
//! [`super::PromptVariant::from_env`] once and passes the answer down.

use std::sync::LazyLock;

use super::codex_vendor::CODEX_VENDOR_BASE;
use super::legacy;
use super::variant::PromptVariant;

/// GPT-6 family instructions from Codex `rust-v0.163.0`, commit
/// `83086188ae0614bd882818ff1aa23f3194962012`, `gpt-6.1-sol` instructions_template.
/// Source sha256: `e1bdd4f8f0df4b20f4a0ffc8a861ce819df45325d8cecdfb92e80379cf8d142e`.
/// The approved edits and omissions are recorded in the vendor provenance.
pub(crate) const GPT6_NATIVE: &str = include_str!("../../vendor/codex-prompt/gpt6/native-ptc.md");
pub(crate) const GPT6_DIRECT: &str = include_str!("../../vendor/codex-prompt/gpt6/direct.md");

pub(crate) const GPT6_EDIT_BULLET: &str = "- Edit files with apply_patch. Do not use it for generated changes (lockfiles, formatter output) or for a search-and-replace across many files; script those.\n";

/// Recognizes only this pack's bases, not arbitrary operator instructions.
pub(crate) fn is_codex_base(base: &str) -> bool {
    base.starts_with(CODEX_VENDOR_BASE.trim_end())
        || base == legacy::PLAKOUS_CODEX_DIRECT_BASE_INSTRUCTIONS
        || base.starts_with(
            "You are a precise and proficient coding agent operating in Codex Code Mode.",
        )
}

pub(crate) fn model_base(
    base: &str,
    model: &str,
    composition: crate::vocabulary::Composition,
    names: &[plakous_model::ToolName],
) -> String {
    if model.starts_with("gpt-6") && base.starts_with(CODEX_VENDOR_BASE.trim_end()) {
        match composition {
            crate::vocabulary::Composition::Direct => return GPT6_DIRECT.trim_end().to_owned(),
            crate::vocabulary::Composition::NativeCell => {
                return GPT6_NATIVE.trim_end().replace(
                    "{names}",
                    &names
                        .iter()
                        .map(plakous_model::ToolName::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            _ => {}
        }
    }
    base.to_owned()
}

/// The vendored sections, then the Plakous Direct tool paragraph.
///
/// The tool paragraph's own opening sentence is dropped here. On the legacy
/// variant it was the only identity the model got; on the parity variant the
/// copied identity line already opened the base and the identity tail names
/// the model after it, so keeping it would state an identity three times.
/// What the paragraph is kept for — which tool does what, and the
/// no-invented-tools guardrail — is untouched.
static CODEX_PARITY_DIRECT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{}\n\n{}",
        CODEX_VENDOR_BASE.trim_end(),
        legacy::codex_direct_tool_paragraph()
    )
});

/// The vendored sections, then the Plakous Code Mode paragraph; `{names}` is
/// still the compiled nested catalog. Its opening sentence is dropped for the
/// same reason, and the sentence that follows it names the composition.
static CODEX_PARITY_CODE_MODE_TEMPLATE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{}\n\n{}",
        CODEX_VENDOR_BASE.trim_end(),
        legacy::codex_code_mode_tool_paragraph()
    )
});

/// Codex OAuth Direct coding profile base, for one variant.
#[must_use]
pub fn codex_direct_base_instructions_for(variant: PromptVariant) -> &'static str {
    match variant {
        PromptVariant::Parity => CODEX_PARITY_DIRECT.as_str(),
        PromptVariant::Legacy => legacy::PLAKOUS_CODEX_DIRECT_BASE_INSTRUCTIONS,
    }
}

/// Codex OAuth Code Mode base template, for one variant.
#[must_use]
pub fn codex_code_mode_base_instructions_template_for(variant: PromptVariant) -> &'static str {
    match variant {
        PromptVariant::Parity => CODEX_PARITY_CODE_MODE_TEMPLATE.as_str(),
        PromptVariant::Legacy => legacy::PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE,
    }
}

/// Renders Code Mode base instructions from the compiled nested catalog names,
/// for one variant.
#[must_use]
pub fn render_codex_code_mode_base_instructions_for(
    variant: PromptVariant,
    nested_names: &[&str],
) -> String {
    codex_code_mode_base_instructions_template_for(variant)
        .replace("{names}", &nested_names.join(", "))
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests;
