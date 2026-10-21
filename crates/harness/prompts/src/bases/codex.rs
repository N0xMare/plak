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
mod tests {
    use super::*;

    #[test]
    fn gpt6_source_and_approved_bases_are_pinned() {
        use plakous_tools::{PromptPin, drifted_pins};
        let pins = [
            PromptPin::new(
                "codex.gpt6.source",
                include_str!("../../vendor/codex-prompt/gpt6/instructions-template.md"),
                "3449952e84d7b22b05d440ffcb36c45f546dd91c20949b6360eedbbea712fa0a",
            ),
            PromptPin::new(
                "codex.gpt6.native",
                GPT6_NATIVE,
                "fa31d1141b7a3e47bf2fe9f538e355eaf91cf5445b9505a3e1471bc7daf6e03c",
            ),
            PromptPin::new(
                "codex.gpt6.direct",
                GPT6_DIRECT,
                "736a3caf6a63c19b830f02c9cc31f166015844cb3bc7b56d7c3b2c4ed2d61bbd",
            ),
        ];
        assert_eq!(drifted_pins(&pins), []);
        assert!(GPT6_NATIVE.contains(GPT6_EDIT_BULLET));
        assert!(GPT6_DIRECT.contains(GPT6_EDIT_BULLET));
    }

    #[test]
    fn model_family_selects_only_gpt6_parity_bases() {
        use crate::vocabulary::Composition;
        let names = [plakous_model::ToolName::new("exec_command").expect("name")];
        for composition in [Composition::Direct, Composition::NativeCell] {
            let base = match composition {
                Composition::Direct => codex_direct_base_instructions_for(PromptVariant::Parity),
                _ => codex_code_mode_base_instructions_template_for(PromptVariant::Parity),
            };
            for model in ["gpt-6", "gpt-6.1-sol", "gpt-6-astra", "gpt-6-luna"] {
                let rendered = model_base(base, model, composition, &names);
                assert!(rendered.starts_with("You are Codex, an agent based on GPT-6."));
                assert!(rendered.contains("## Intermediate commentary"));
                assert!(!rendered.contains("JSON.parse"));
                assert!(!rendered.contains("{names}"));
                assert_eq!(
                    rendered.contains("functions.exec"),
                    composition == Composition::NativeCell
                );
            }
            for model in ["gpt-5.6-sol", "gpt-5.6-luna", "gpt-5", "other"] {
                assert_eq!(model_base(base, model, composition, &names), base);
            }
            assert_eq!(
                model_base("operator base", "gpt-6.1-sol", composition, &names),
                "operator base"
            );
            let legacy = codex_direct_base_instructions_for(PromptVariant::Legacy);
            assert_eq!(
                model_base(legacy, "gpt-6.1-sol", composition, &names),
                legacy
            );
        }
    }

    /// The parity base is the vendored copy plus the Plakous paragraph, with
    /// nothing between them and nothing removed from either.
    #[test]
    fn the_parity_bases_are_the_vendored_copy_plus_the_plakous_paragraph() {
        let direct = codex_direct_base_instructions_for(PromptVariant::Parity);
        assert!(direct.starts_with("You are Codex, a coding agent based on GPT-5."));
        assert!(direct.contains(CODEX_VENDOR_BASE.trim_end()));
        assert!(direct.ends_with(legacy::codex_direct_tool_paragraph()));
        assert!(direct.contains("Use exec_command to inspect the workspace"));
        // One identity claim, from the vendored copy; the tail names the
        // model, and the tool paragraph no longer opens with a third. ("You
        // are producing plain text" in the copied formatting rules is not an
        // identity claim, so the assertion names the claim, not the words.)
        assert_eq!(direct.matches("You are Codex").count(), 1);
        assert!(!direct.contains("You are a precise and proficient coding agent"));
        assert!(direct.contains("**NEVER** use destructive commands"));
        assert!(direct.contains("do not invent tools that are not presented"));
        assert_eq!(
            direct.len(),
            CODEX_VENDOR_BASE.trim_end().len() + 2 + legacy::codex_direct_tool_paragraph().len()
        );
        assert_eq!(direct.len(), 7022);

        let template = codex_code_mode_base_instructions_template_for(PromptVariant::Parity);
        assert!(template.contains(CODEX_VENDOR_BASE.trim_end()));
        assert!(template.contains("{names}"));
        assert!(template.contains(legacy::codex_code_mode_tool_paragraph()));
        assert!(template.ends_with(legacy::codex_code_mode_tool_paragraph()));
        assert!(template.contains("The complete provider-visible tool set is exec and wait."));
        assert!(!template.contains("JSON.parse"));
        assert_eq!(template.matches("You are Codex").count(), 1);
        assert!(!template.contains("You are a precise and proficient coding agent"));
    }

    /// The legacy variant is the text that shipped, unchanged.
    #[test]
    fn the_legacy_variant_is_the_short_base() {
        assert_eq!(
            codex_direct_base_instructions_for(PromptVariant::Legacy),
            legacy::PLAKOUS_CODEX_DIRECT_BASE_INSTRUCTIONS
        );
        assert_eq!(
            codex_code_mode_base_instructions_template_for(PromptVariant::Legacy),
            legacy::PLAKOUS_CODEX_CODE_MODE_BASE_INSTRUCTIONS_TEMPLATE
        );
        assert!(
            !codex_direct_base_instructions_for(PromptVariant::Legacy)
                .contains("Working with the user")
        );
    }

    /// The nested catalog is rendered into whichever variant is selected.
    #[test]
    fn both_variants_render_the_nested_catalog() {
        let nested = ["exec_command", "write_stdin", "apply_patch", "update_plan"];
        for variant in [PromptVariant::Parity, PromptVariant::Legacy] {
            let rendered = render_codex_code_mode_base_instructions_for(variant, &nested);
            assert!(rendered.contains(
                "nested tools on its tools object: exec_command, write_stdin, apply_patch, update_plan."
            ));
            assert!(!rendered.contains("{names}"));
            assert!(!rendered.contains("read_file"));
        }
        let parity = render_codex_code_mode_base_instructions_for(PromptVariant::Parity, &nested);
        let legacy_render =
            render_codex_code_mode_base_instructions_for(PromptVariant::Legacy, &nested);
        assert!(!parity.contains("JSON.parse(await tools.exec_command"));
        assert!(!legacy_render.contains("JSON.parse(await tools.exec_command"));
        assert!(parity.len() > legacy_render.len());
        // The parity render preserves the legacy tool paragraph after dropping
        // its opening identity sentence, without an observation parsing example.
        assert!(
            parity.contains(
                legacy_render
                    .split_once(". ")
                    .expect("the legacy paragraph opens with a sentence")
                    .1
            )
        );
    }
}
