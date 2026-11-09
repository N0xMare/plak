use std::path::Path;

use super::*;
use crate::identity::{
    PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS, PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER,
    PLAK_PRODUCT_TOPOLOGY_AGREEMENTS, product_target_identity_block,
};
use crate::surface_fixture::surface;
use plakous_tools::CodingPresentation;

#[test]
fn gpt6_assembly_uses_the_model_family_and_compiled_catalog() {
    use crate::bases::{self, PromptVariant};
    for composition in [Composition::Direct, Composition::NativeCell] {
        let family_base = match composition {
            Composition::Direct => bases::codex_direct_base_instructions_for(PromptVariant::Parity),
            _ => bases::codex_code_mode_base_instructions_template_for(PromptVariant::Parity),
        };
        let surface = surface(CodingPresentation::Codex, &[]);
        let render = |model| {
            target_instructions(
                family_base,
                composition,
                &surface,
                PromptWorkspace::new(Path::new("/repo"), Path::new("/repo"), std::env::consts::OS),
                model,
                Some("high"),
            )
        };
        let gpt6 = render("gpt-6.1-sol");
        let gpt5 = render("gpt-5.6-sol");
        assert!(gpt6.starts_with("You are Codex, an agent based on GPT-6."));
        assert!(gpt5.starts_with("You are Codex, a coding agent based on GPT-5."));
        assert_eq!(
            gpt6.contains(bases::codex::GPT6_EDIT_BULLET),
            surface
                .exposed_names()
                .iter()
                .any(|name| name.as_str() == "apply_patch")
        );
        assert!(gpt6.contains("Never sleep-poll.")); // No-wake paragraph stays intact.
        assert!(gpt6.contains(crate::identity::CODEX_WORKING_AGREEMENTS));
        assert!(!gpt6.contains("Do not narrate tool calls"));
        assert!(!gpt6.contains("You may call independent presented tools together"));
        assert_eq!(gpt6.matches("Workspace root:").count(), 1);
        assert_eq!(
            gpt6.matches("Commit or push only when the user asks.")
                .count(),
            1
        );
    }
}

#[test]
fn catalog_bound_edit_sentences_occur_verbatim_in_family_bases() {
    use crate::bases::{self, PromptVariant};

    let mut family_bases = Vec::new();
    for variant in [PromptVariant::Parity, PromptVariant::Legacy] {
        family_bases.extend([
            bases::codex_direct_base_instructions_for(variant),
            bases::codex_code_mode_base_instructions_template_for(variant),
            bases::claude_direct_base_instructions_for(variant),
            bases::grok_direct_base_instructions_for(variant),
        ]);
    }
    for &(required, sentence) in CATALOG_BOUND_EDIT_SENTENCES {
        assert!(
            family_bases.iter().any(|base| base.contains(sentence)),
            "catalog-bound sentence for {required:?} occurs in no family base: {sentence:?}"
        );
    }
}

/// The four agreement scopes a node can earn, each as the surface that
/// earns it: single-agent, leaf, root hub, nested hub.
fn surfaces() -> [CompiledSurface; 4] {
    [
        surface(CodingPresentation::Codex, &[]),
        surface(CodingPresentation::Codex, &["submit_result"]),
        surface(CodingPresentation::Codex, &["spawn_agent", "wait_agent"]),
        surface(
            CodingPresentation::Codex,
            &["spawn_agent", "wait_agent", "submit_result"],
        ),
    ]
}

/// The retired path: identity block appended to the family base, then the
/// composition base re-attached from the environment marker on.
fn marker_splice(
    family_base: &str,
    composition_base: &str,
    contributions: &[PromptContribution],
) -> String {
    let identity = product_target_identity_block(
        PromptWorkspace::new(
            Path::new("/repo"),
            Path::new("/repo/src"),
            std::env::consts::OS,
        ),
        "gpt-5",
        Some("high"),
        contributions,
    );
    let assembled = if family_base.is_empty() {
        identity
    } else {
        format!("{family_base}\n\n{identity}")
    };
    match assembled.find(PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER) {
        Some(start) => format!("{composition_base}\n\n{}", &assembled[start..]),
        None => composition_base.to_owned(),
    }
}

#[test]
fn assemble_reproduces_the_identity_block_for_every_scope() {
    for surface in surfaces() {
        let plan = PromptPlan::new(
            "",
            PromptWorkspace::new(
                Path::new("/repo"),
                Path::new("/repo/src"),
                std::env::consts::OS,
            ),
            "gpt-5",
            Some("high"),
            surface.contributions().to_vec(),
        );
        assert_eq!(
            plan.assemble(),
            product_target_identity_block(
                PromptWorkspace::new(
                    Path::new("/repo"),
                    Path::new("/repo/src"),
                    std::env::consts::OS
                ),
                "gpt-5",
                Some("high"),
                surface.contributions()
            )
        );
    }
}

#[test]
fn assemble_reproduces_the_family_base_plus_identity_for_every_scope() {
    for surface in surfaces() {
        let plan = PromptPlan::new(
            "family base",
            PromptWorkspace::new(
                Path::new("/repo"),
                Path::new("/repo/src"),
                std::env::consts::OS,
            ),
            "gpt-5",
            Some("high"),
            surface.contributions().to_vec(),
        );
        let expected = format!(
            "family base\n\n{}",
            product_target_identity_block(
                PromptWorkspace::new(
                    Path::new("/repo"),
                    Path::new("/repo/src"),
                    std::env::consts::OS
                ),
                "gpt-5",
                Some("high"),
                surface.contributions()
            )
        );
        assert_eq!(plan.assemble(), expected);
    }
}

/// The gate for deleting the splice: the composition base swap in
/// `target_instructions` and the marker search must agree byte for byte on
/// every composition and every scope.
#[test]
fn with_base_reproduces_the_marker_splice_byte_for_byte() {
    for surface in surfaces() {
        for (composition, composition_base) in [
            (
                Composition::JavaScriptCell,
                PtcHarness::JavaScript.instructions(surface.exposed_names()),
            ),
            (
                Composition::PythonCell,
                PtcHarness::Python.instructions(surface.exposed_names()),
            ),
        ] {
            let assembled = target_instructions(
                "family base",
                composition,
                &surface,
                PromptWorkspace::new(
                    Path::new("/repo"),
                    Path::new("/repo/src"),
                    std::env::consts::OS,
                ),
                "gpt-5",
                Some("high"),
            );
            assert_eq!(
                assembled,
                marker_splice("family base", &composition_base, surface.contributions()),
                "{composition:?}"
            );
            assert_eq!(assembled.matches("Workspace root:").count(), 1);
            assert!(!assembled.contains("family base"));
        }
    }
}

#[test]
fn a_direct_composition_keeps_the_family_base() {
    let surface = surface(CodingPresentation::Codex, &[]);
    for composition in [Composition::Direct, Composition::NativeCell] {
        let assembled = target_instructions(
            "family base",
            composition,
            &surface,
            PromptWorkspace::new(
                Path::new("/repo"),
                Path::new("/repo/src"),
                std::env::consts::OS,
            ),
            "gpt-5",
            Some("high"),
        );
        assert!(assembled.starts_with("family base\n\nWorkspace root: /repo"));
    }
}

/// A leaf earns no topology paragraph, and earned paragraphs keep
/// topology-before-assignment order. The names are judged
/// by `compile`, not by the plan.
#[test]
fn contributions_are_earned_by_the_compiled_tool_names() {
    let plan = |names: &[&str]| {
        PromptPlan::new(
            "",
            PromptWorkspace::new(Path::new("/r"), Path::new("/r"), std::env::consts::OS),
            "m",
            Some("high"),
            surface(CodingPresentation::Codex, names)
                .contributions()
                .to_vec(),
        )
    };
    assert_eq!(
        plan(&[]).contribution_ids(),
        ["plak.agreements.process-codex-wait"]
    );
    assert_eq!(
        plan(&["submit_result", "list_agents"]).contribution_ids(),
        [
            "plak.agreements.assignment",
            "plak.agreements.process-codex-wait"
        ]
    );
    assert_eq!(
        plan(&["spawn_agent", "wait_agent", "submit_result"]).contribution_ids(),
        [
            "plak.agreements.topology",
            "plak.agreements.child-wait",
            "plak.agreements.assignment",
            "plak.agreements.process-codex-wait",
        ]
    );
    let hub = plan(&["spawn_agent", "wait_agent", "submit_result"]).assemble();
    assert!(
        hub.find(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS) < hub.find(PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS)
    );
}

/// A PTC hub's `spawn_agent` is callable only inside a cell; the surface
/// still earns the topology paragraph and the PTC base lists the name.
#[test]
fn a_ptc_hub_earns_the_topology_paragraph_from_the_callable_surface() {
    let hub = surface(CodingPresentation::Claude, &["spawn_agent", "wait_agent"]);
    let assembled = target_instructions(
        "family base",
        Composition::JavaScriptCell,
        &hub,
        PromptWorkspace::new(
            Path::new("/repo"),
            Path::new("/repo/src"),
            std::env::consts::OS,
        ),
        "gpt-5",
        Some("high"),
    );
    assert!(assembled.contains("Nested tools available inside cells:"));
    assert!(assembled.contains("spawn_agent, wait_agent."));
    assert!(assembled.contains(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS));
    assert!(!assembled.contains(PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS));
}
