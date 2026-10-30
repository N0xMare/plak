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
