//! The reviewed Codex base-instruction sections, read from the vendor snapshot.
//!
//! `vendor/codex-prompt` holds contiguous line ranges of the OpenAI Codex
//! base instructions at the revision `vendor/codex-code-mode-protocol` already
//! pins, copied with no edit inside a range. Its `PROVENANCE.md` records the
//! upstream paths, the ranges, and both digest columns.
//!
//! This module is the only place those bytes enter the crate. It joins the
//! sections in file order with one blank line between them and pins the
//! result, so a re-vendor that moves a byte fails here before it reaches a
//! rendered prompt.
//!
//! Nothing here is edited to match Plakous. Where a copied paragraph assumes a
//! sandbox or an approval flow that Plakous does not have, the environment
//! block written after the base states the truth; see
//! [`crate::identity::PLAK_PRODUCT_EXECUTION_POLICY`].

#[cfg(test)]
use plakous_tools::PromptPin;

/// `vendor/codex-prompt/prompt/01-identity.md`, upstream template line 1.
pub const CODEX_VENDOR_IDENTITY: &str =
    include_str!("../../../../../vendor/codex-prompt/prompt/01-identity.md");

/// `vendor/codex-prompt/prompt/02-technical-communication.md`, upstream
/// `gpt_5_2_prompt.md` lines 13-15: the Personality heading and the one
/// paragraph under it that is about technical communication.
pub const CODEX_VENDOR_TECHNICAL_COMMUNICATION: &str =
    include_str!("../../../../../vendor/codex-prompt/prompt/02-technical-communication.md");

/// `vendor/codex-prompt/prompt/03-working-with-the-user.md`, upstream template
/// lines 5-36: the whole "Working with the user" section, which at this
/// revision carries no channel mechanics and no visualizations.
pub const CODEX_VENDOR_WORKING_WITH_THE_USER: &str =
    include_str!("../../../../../vendor/codex-prompt/prompt/03-working-with-the-user.md");

/// `vendor/codex-prompt/prompt/04-autonomy-and-persistence.md`, upstream
/// `gpt_5_2_prompt.md` lines 29-32.
pub const CODEX_VENDOR_AUTONOMY_AND_PERSISTENCE: &str =
    include_str!("../../../../../vendor/codex-prompt/prompt/04-autonomy-and-persistence.md");

/// `vendor/codex-prompt/prompt/05-general-and-editing-constraints.md`,
/// upstream template lines 38-55: search preference, the file editing
/// constraints, the dirty-worktree rules and the destructive-command rule.
pub const CODEX_VENDOR_GENERAL_AND_EDITING_CONSTRAINTS: &str =
    include_str!("../../../../../vendor/codex-prompt/prompt/05-general-and-editing-constraints.md");

/// Every kept section, in file order, one blank line between them.
///
/// Each vendored file ends with the newline that ended its last upstream line,
/// so a single `"\n"` joiner produces the blank line.
pub const CODEX_VENDOR_BASE: &str = concat!(
    include_str!("../../../../../vendor/codex-prompt/prompt/01-identity.md"),
    "\n",
    include_str!("../../../../../vendor/codex-prompt/prompt/02-technical-communication.md"),
    "\n",
    include_str!("../../../../../vendor/codex-prompt/prompt/03-working-with-the-user.md"),
    "\n",
    include_str!("../../../../../vendor/codex-prompt/prompt/04-autonomy-and-persistence.md"),
    "\n",
    include_str!("../../../../../vendor/codex-prompt/prompt/05-general-and-editing-constraints.md"),
);

/// Every vendored section and the joined base.
///
/// A pin here is a re-vendor gate, not a wording review: the digests below
/// must move together with the `PROVENANCE.md` table.
#[cfg(test)]
pub static CODEX_VENDOR_PROMPT_PINS: &[PromptPin] = &[
    PromptPin::new(
        "codex_vendor.identity",
        CODEX_VENDOR_IDENTITY,
        "dace21773c84b24b60d0bab21a0701172b562a18dca90ac3763f2ad9ab353a96",
    ),
    PromptPin::new(
        "codex_vendor.technical_communication",
        CODEX_VENDOR_TECHNICAL_COMMUNICATION,
        "9dcf5738f52df327ec7d1d1377c80532cc72eaf10570696f437cde2344128b57",
    ),
    PromptPin::new(
        "codex_vendor.working_with_the_user",
        CODEX_VENDOR_WORKING_WITH_THE_USER,
        "46cf6abc662ca0e1f1fb4a95387183da138c30b83a18a47a234e94081c075d79",
    ),
    PromptPin::new(
        "codex_vendor.autonomy_and_persistence",
        CODEX_VENDOR_AUTONOMY_AND_PERSISTENCE,
        "d7c439823a73e9f83485b5072327ac049f0b966d341acb639ad2a31926113957",
    ),
    PromptPin::new(
        "codex_vendor.general_and_editing_constraints",
        CODEX_VENDOR_GENERAL_AND_EDITING_CONSTRAINTS,
        "df88cd4009776d9b0760d6da8be49aa201bc78c3c3882e167944293d1fdd02ce",
    ),
    PromptPin::new(
        "codex_vendor.base",
        CODEX_VENDOR_BASE,
        "0266db298cd639c4ac5745f47b676c7097cea4b1ad27254924ead279d45e56e3",
    ),
];

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests {
    use super::*;

    /// The join is the five files in order with one blank line between them.
    #[test]
    fn the_joined_base_is_the_sections_in_file_order() {
        let sections = [
            CODEX_VENDOR_IDENTITY,
            CODEX_VENDOR_TECHNICAL_COMMUNICATION,
            CODEX_VENDOR_WORKING_WITH_THE_USER,
            CODEX_VENDOR_AUTONOMY_AND_PERSISTENCE,
            CODEX_VENDOR_GENERAL_AND_EDITING_CONSTRAINTS,
        ];
        assert_eq!(CODEX_VENDOR_BASE, sections.join("\n"));
        let mut offset = 0;
        for section in sections {
            let found = CODEX_VENDOR_BASE[offset..]
                .find(section)
                .expect("section appears in order");
            offset += found + section.len();
        }
    }

    /// The sections that carry the behaviour the parity work is for.
    #[test]
    fn the_vendored_sections_are_the_ones_the_provenance_names() {
        assert!(CODEX_VENDOR_IDENTITY.starts_with("You are Codex, a coding agent based on GPT-5."));
        assert!(CODEX_VENDOR_TECHNICAL_COMMUNICATION.starts_with("## Personality"));
        assert!(CODEX_VENDOR_WORKING_WITH_THE_USER.starts_with("# Working with the user"));
        assert!(CODEX_VENDOR_WORKING_WITH_THE_USER.contains("## Final answer formatting rules"));
        assert!(CODEX_VENDOR_WORKING_WITH_THE_USER.contains("## Presenting your work"));
        assert!(CODEX_VENDOR_AUTONOMY_AND_PERSISTENCE.starts_with("## Autonomy and Persistence"));
        assert!(CODEX_VENDOR_GENERAL_AND_EDITING_CONSTRAINTS.contains("## Editing constraints"));
        assert!(
            CODEX_VENDOR_GENERAL_AND_EDITING_CONSTRAINTS
                .contains("**NEVER** use destructive commands")
        );
    }

    /// What the provenance says was dropped must not be in the copy.
    #[test]
    fn the_dropped_sections_are_absent() {
        for dropped in [
            "# AGENTS.md spec",
            "## AGENTS.md spec",
            "## Plan tool",
            "## Special user requests",
            "## Frontend tasks",
            "# Tool Guidelines",
            "## Shell commands",
            "## Responsiveness",
            "## Validating your work",
            "Using skills",
            "Visualizations",
            "commentary channel",
            "{{ personality }}",
        ] {
            assert!(!CODEX_VENDOR_BASE.contains(dropped), "{dropped} was copied");
        }
    }

    /// The copy is never edited; the environment block carries the correction.
    #[test]
    fn the_copy_is_not_edited_to_mention_plak() {
        for plak_word in [
            "plak",
            "Plakous",
            "bubblewrap",
            "exec_command",
            "submit_result",
        ] {
            assert!(!CODEX_VENDOR_BASE.contains(plak_word), "{plak_word}");
        }
    }

    /// The renderer approximates tokens as bytes / 4; this is the instruction
    /// cost the budget test in `plakous-hosted` spends on vendored text.
    #[test]
    fn the_vendored_base_stays_within_its_share_of_the_budget() {
        assert_eq!(CODEX_VENDOR_BASE.len(), 6664);
        assert!(CODEX_VENDOR_BASE.len() / 4 < 2_000);
    }
}
