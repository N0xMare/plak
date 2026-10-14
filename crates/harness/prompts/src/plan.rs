//! The first-turn assembler: spec 02 §6.1 order, encoded once.
//!
//! Before this module the order lived in two places that had to agree by
//! accident. `product_target_identity_profile` appended the identity tail to a
//! family base, and `overlay_product_composition_instructions` then searched the
//! result for the literal `"Workspace root:"` to find where the tail began so it
//! could re-attach it to a PTC base. Native targets applied identity before the
//! overlay and OpenRouter applied it after, so the same bytes were reached by
//! two different routes.
//!
//! [`PromptPlan`] carries the parts instead of parsing them back out of a
//! string. One `assemble` writes the order; [`target_instructions`] is the one
//! site that picks a composition base over the family base, so no route has a
//! tail to find or re-attach.
//!
//! The earned paragraphs arrive as the [`PromptContribution`]s of one compiled
//! surface the composition root compiled. That same catalog conditions the
//! family base's editing guidance, so no tier list or separate tool list can
//! advertise an edit tool absent from Direct or nested composition.

use std::path::Path;

use plakous_tools::{CompiledSurface, PromptContribution};

use crate::identity::{PLAK_PRODUCT_WORKING_AGREEMENTS, product_identity_tail};
use crate::ptc::PtcHarness;
use crate::vocabulary::Composition;

/// One first-turn instruction plan.
///
/// Item 1 of spec 02 §6.1 — the dialect costume — is not here: a costume is a
/// provider envelope, owned by the provider crate, and never reaches tool
/// behaviour (spec 02 §6.4). Items 5 and 6 — enrolled `AGENTS.md` and the user
/// or assignment message — are separate context sources, not instruction text,
/// and are attached by the harness plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptPlan {
    /// Item 2: the family or composition base instruction.
    base: String,
    /// Item 3: workspace root, cwd, OS, the model and effort sentences, the
    /// execution sentence, the commit rule.
    identity: String,
    /// Item 4: the always-on working agreements.
    agreements: &'static str,
    /// Item 4 continued: the paragraphs this node's compiled tools earned.
    contributions: Vec<PromptContribution>,
}

impl PromptPlan {
    /// Builds the plan for one node from its base, its resolved identity, and
    /// the paragraphs its compiled surface earned, in surface order.
    pub fn new(
        base: impl Into<String>,
        workspace_root: &Path,
        cwd: &Path,
        selected_model: &str,
        selected_effort: Option<&str>,
        contributions: Vec<PromptContribution>,
    ) -> Self {
        Self {
            base: base.into(),
            identity: product_identity_tail(workspace_root, cwd, selected_model, selected_effort),
            agreements: PLAK_PRODUCT_WORKING_AGREEMENTS,
            contributions,
        }
    }

    /// Writes spec 02 §6.1 order.
    ///
    /// An empty base — a profile that carries no family instruction — yields the
    /// identity block alone, exactly as the two-step path did.
    #[must_use]
    pub fn assemble(&self) -> String {
        let mut assembled = String::new();
        if !self.base.is_empty() {
            assembled.push_str(&self.base);
            assembled.push_str("\n\n");
        }
        assembled.push_str(&self.identity);
        assembled.push_str("\n\n");
        assembled.push_str(self.agreements);
        for contribution in &self.contributions {
            assembled.push_str("\n\n");
            assembled.push_str(contribution.text());
        }
        assembled
    }

    /// The ids of the contributions this plan earned, in order.
    #[cfg(test)]
    pub fn contribution_ids(&self) -> Vec<&'static str> {
        self.contributions
            .iter()
            .map(PromptContribution::id)
            .collect()
    }
}

/// The complete first-turn instruction string for one target.
///
/// This is the single assembly site and the single base-selection site. A
/// JavaScript or Python PTC composition replaces the family base here, before
/// the identity tail is written, so there is no marker to find and no tail to
/// re-attach; the PTC base lists the surface's exposed names — the nested
/// catalog — and the paragraphs are the ones that surface earned.
#[must_use]
pub fn target_instructions(
    family_base: &str,
    composition: Composition,
    surface: &CompiledSurface,
    workspace_root: &Path,
    cwd: &Path,
    selected_model: &str,
    selected_effort: Option<&str>,
) -> String {
    let base = match composition {
        Composition::JavaScriptCell => PtcHarness::JavaScript.instructions(surface.exposed_names()),
        Composition::PythonCell => PtcHarness::Python.instructions(surface.exposed_names()),
        Composition::Direct | Composition::NativeCell => {
            catalog_bound_family_base(family_base, surface)
        }
    };
    PromptPlan::new(
        base,
        workspace_root,
        cwd,
        selected_model,
        selected_effort,
        surface.contributions().to_vec(),
    )
    .assemble()
}

const CATALOG_BOUND_EDIT_SENTENCES: &[(&[&str], &str)] = &[
    (
        &["apply_patch"],
        "When the assignment may mutate, use apply_patch with the Codex *** Begin Patch envelope to edit files. ",
    ),
    (
        &["apply_patch"],
        "Prefer apply_patch over sed or heredoc rewrites when editing. ",
    ),
    (
        &["apply_patch"],
        "Prefer apply_patch for edits when the assignment may mutate. ",
    ),
    (
        &["Edit", "Write"],
        "When the assignment may mutate, use Edit for exact changes and Write for new files. ",
    ),
    (&["Edit"], "Prefer Edit over rewriting files in Bash. "),
    (
        &["search_replace"],
        "When the assignment may mutate, use search_replace for edits. ",
    ),
];

fn catalog_bound_family_base(base: &str, surface: &CompiledSurface) -> String {
    let presents = |name: &str| {
        surface
            .exposed_names()
            .iter()
            .any(|tool| tool.as_str() == name)
    };
    let mut base = base.to_owned();
    // These are complete, pinned sentences, not a heuristic for parsing arbitrary prose.
    for &(required, sentence) in CATALOG_BOUND_EDIT_SENTENCES {
        if !required.iter().all(|name| presents(name)) {
            base = base.replace(sentence, "");
        }
    }
    if !presents("apply_patch") {
        for line in crate::bases::codex_vendor::CODEX_VENDOR_GENERAL_AND_EDITING_CONSTRAINTS
            .lines()
            .filter(|line| line.contains("apply_patch"))
        {
            base = base.replace(&format!("{line}\n"), "");
        }
    }
    base
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::identity::{
        PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS, PLAK_PRODUCT_IDENTITY_ENVIRONMENT_MARKER,
        PLAK_PRODUCT_TOPOLOGY_AGREEMENTS, product_target_identity_block,
    };
    use crate::surface_fixture::surface;
    use plakous_tools::CodingPresentation;

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
            Path::new("/repo"),
            Path::new("/repo/src"),
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
                Path::new("/repo"),
                Path::new("/repo/src"),
                "gpt-5",
                Some("high"),
                surface.contributions().to_vec(),
            );
            assert_eq!(
                plan.assemble(),
                product_target_identity_block(
                    Path::new("/repo"),
                    Path::new("/repo/src"),
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
                Path::new("/repo"),
                Path::new("/repo/src"),
                "gpt-5",
                Some("high"),
                surface.contributions().to_vec(),
            );
            let expected = format!(
                "family base\n\n{}",
                product_target_identity_block(
                    Path::new("/repo"),
                    Path::new("/repo/src"),
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
                    Path::new("/repo"),
                    Path::new("/repo/src"),
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
                Path::new("/repo"),
                Path::new("/repo/src"),
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
                Path::new("/r"),
                Path::new("/r"),
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
            hub.find(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS)
                < hub.find(PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS)
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
            Path::new("/repo"),
            Path::new("/repo/src"),
            "gpt-5",
            Some("high"),
        );
        assert!(assembled.contains("Nested tools available inside cells:"));
        assert!(assembled.contains("spawn_agent, wait_agent."));
        assert!(assembled.contains(PLAK_PRODUCT_TOPOLOGY_AGREEMENTS));
        assert!(!assembled.contains(PLAK_PRODUCT_ASSIGNMENT_AGREEMENTS));
    }
}
