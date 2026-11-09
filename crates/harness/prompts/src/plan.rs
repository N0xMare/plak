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

use plakous_tools::{CompiledSurface, PromptContribution};

use crate::identity::{PLAK_PRODUCT_WORKING_AGREEMENTS, PromptWorkspace, product_identity_tail};
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
        workspace: PromptWorkspace<'_>,
        selected_model: &str,
        selected_effort: Option<&str>,
        contributions: Vec<PromptContribution>,
    ) -> Self {
        Self {
            base: base.into(),
            identity: product_identity_tail(workspace, selected_model, selected_effort),
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
    workspace: PromptWorkspace<'_>,
    selected_model: &str,
    selected_effort: Option<&str>,
) -> String {
    let codex = crate::bases::codex::is_codex_base(family_base);
    let base = match composition {
        Composition::JavaScriptCell => PtcHarness::JavaScript.instructions(surface.exposed_names()),
        Composition::PythonCell => PtcHarness::Python.instructions(surface.exposed_names()),
        Composition::PythonSdk => crate::bases::python_sdk::BASE.to_owned(),
        Composition::Direct | Composition::NativeCell => {
            let base = crate::bases::codex::model_base(
                family_base,
                selected_model,
                composition,
                surface.exposed_names(),
            );
            catalog_bound_family_base(&base, surface)
        }
    };
    let mut plan = PromptPlan::new(
        base,
        workspace,
        selected_model,
        selected_effort,
        surface.contributions().to_vec(),
    );
    if codex {
        plan.agreements = crate::identity::CODEX_WORKING_AGREEMENTS;
    }
    plan.assemble()
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
        base = base.replace(crate::bases::codex::GPT6_EDIT_BULLET, "");
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
mod tests;
