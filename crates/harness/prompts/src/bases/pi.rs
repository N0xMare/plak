//! The pi pack's base instruction, in pi 1.1.0's structure.
//!
//! pi assembles its system prompt from a one-sentence preamble, a `<tools>`
//! list of one-line snippets for the tools it presents, and `<rules>` drawn
//! from each presented tool's guidelines plus two closing rules. This is that
//! structure over the tools Plak's pi presentation compiled: the snippets and
//! guidelines are pi's, adapted only where Plak's tool differs (the shell is
//! `/bin/sh`; `wait` and `stop` are Plak's). pi's documentation section is
//! dropped. Project instructions, the working directory and the earned
//! paragraphs follow in Plak's own order (spec 02 §6.1).
//!
//! The pi text is pi's (MIT); see the README License section.

use plakous_model::ToolName;

/// pi's preamble when `read`, `bash`, `edit` and `write` are all presented.
pub const PI_PREAMBLE: &str = "You are an expert coding assistant. You help users by reading files, executing commands, editing code, and writing new files.";

/// The preamble for one surface: the capability clause names only tools that
/// are presented. `PI_PREAMBLE` is that sentence when all four are.
fn pi_preamble_for(presented: &[ToolName]) -> String {
    let presents = |name: &str| presented.iter().any(|tool| tool.as_str() == name);
    let mut capabilities = Vec::new();
    if presents("read") {
        capabilities.push("reading files");
    }
    if presents("bash") {
        capabilities.push("executing commands");
    }
    if presents("edit") {
        capabilities.push("editing code");
    }
    if presents("write") {
        capabilities.push("writing new files");
    }
    let Some(last) = capabilities.pop() else {
        return "You are an expert coding assistant.".to_owned();
    };
    let help = match capabilities.as_slice() {
        [] => format!("You help users by {last}."),
        head => format!("You help users by {}, and {last}.", head.join(", ")),
    };
    format!("You are an expert coding assistant. {help}")
}

/// One presented tool's snippet and the rules it contributes, in pi's order.
const PI_TOOLS: &[(&str, &str, &[&str])] = &[
    (
        "read",
        "Read file contents",
        &["Use read to examine files instead of cat or sed."],
    ),
    ("bash", "Execute shell commands (ls, rg, find, etc.)", &[]),
    (
        "edit",
        "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
        &[
            "Use edit for precise changes (edits[].oldText must match exactly)",
            "When changing multiple separate locations in one file, use one edit call with multiple entries in edits[] instead of multiple edit calls",
            "Each edits[].oldText is matched against the original file, not after earlier edits are applied. Do not emit overlapping or nested edits. Merge nearby changes into one edit.",
            "Keep edits[].oldText as small as possible while still being unique in the file. Do not pad with large unchanged regions.",
        ],
    ),
    (
        "write",
        "Create or overwrite files",
        &["Use write only for new files or complete rewrites."],
    ),
    (
        "wait",
        "Wait for background work when this turn needs its result",
        &[],
    ),
    ("stop", "Stop a background process", &[]),
];

/// pi's first rule when `bash` is presented without its search tools, which
/// this pack never presents.
const PI_BASH_RULE: &str = "Use bash for file operations like ls, rg, find";

/// pi's line after the tool list: other tools (topology, extensions) may join.
const PI_OTHER_TOOLS: &str = "In addition to the tools above, you may have access to other custom tools depending on the project.";

/// The rules pi closes every prompt with.
const PI_CLOSING_RULES: &[&str] = &[
    "Be concise in your responses",
    "Show file paths clearly when working with files",
];

/// The pi base for one compiled surface: the preamble, `<tools>` for the pi
/// tools it presents and `<rules>` from those tools' guidelines.
#[must_use]
pub fn pi_base_instructions(presented: &[ToolName]) -> String {
    let preamble = pi_preamble_for(presented);
    let presents = |name: &str| presented.iter().any(|tool| tool.as_str() == name);
    let tools = PI_TOOLS
        .iter()
        .filter(|(name, _, _)| presents(name))
        .collect::<Vec<_>>();
    let snippets = tools
        .iter()
        .map(|(name, snippet, _)| format!("- {name}: {snippet}"))
        .collect::<Vec<_>>()
        .join("\n");
    let lead = presents("bash").then_some(&PI_BASH_RULE);
    let rules = lead
        .into_iter()
        .chain(tools.iter().flat_map(|(_, _, rules)| rules.iter()))
        .chain(PI_CLOSING_RULES)
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{preamble}\n\n<tools>\n{snippets}\n\n{PI_OTHER_TOOLS}\n</tools>\n\n<rules>\n{rules}\n</rules>"
    )
}

/// Every model-visible string this module owns, one per tool row.
#[cfg(test)]
fn pinned_text() -> String {
    let mut text = format!("{PI_PREAMBLE}{PI_BASH_RULE}{PI_OTHER_TOOLS}");
    for (name, snippet, rules) in PI_TOOLS {
        text.push_str(name);
        text.push_str(snippet);
        for rule in *rules {
            text.push_str(rule);
        }
    }
    for rule in PI_CLOSING_RULES {
        text.push_str(rule);
    }
    text
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "unit tests may assert with expect")]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<ToolName> {
        names
            .iter()
            .map(|name| ToolName::new(*name).expect("tool name"))
            .collect()
    }

    #[test]
    fn the_pi_base_lists_presented_tools_and_their_rules_in_pi_order() {
        let base = pi_base_instructions(&names(&[
            "bash",
            "edit",
            "read",
            "stop",
            "wait",
            "write",
            "spawn_agent",
        ]));
        assert!(base.starts_with(PI_PREAMBLE));
        assert_eq!(
            pi_preamble_for(&names(&["read", "bash", "edit", "write", "wait", "stop"])),
            PI_PREAMBLE
        );
        assert!(base.contains(
            "<tools>\n- read: Read file contents\n- bash: Execute shell commands (ls, rg, find, etc.)\n- edit: "
        ));
        assert!(
            base.contains("- stop: Stop a background process\n\nIn addition to the tools above")
        );
        assert!(base.contains(
            "<rules>\n- Use bash for file operations like ls, rg, find\n- Use read to examine files instead of cat or sed.\n"
        ));
        assert!(base.ends_with(
            "- Be concise in your responses\n- Show file paths clearly when working with files\n</rules>"
        ));
        assert!(!base.contains("spawn_agent"));
        assert!(!base.contains("pi documentation"));
        assert!(!base.contains("inside pi"));
    }

    /// An occupancy fence that drops `edit` and `write` drops their snippets
    /// and their rules too.
    #[test]
    fn a_fenced_surface_names_only_its_tools() {
        let base = pi_base_instructions(&names(&["bash", "read", "stop", "wait"]));
        assert!(!base.contains("- edit:"));
        assert!(!base.contains("- write:"));
        assert!(!base.contains("edits[]"));
        assert!(!base.contains("editing code"));
        assert!(!base.contains("writing new files"));
        assert!(base.contains("reading files, and executing commands"));
        assert!(base.contains("- Use bash for file operations like ls, rg, find"));
    }

    #[test]
    fn the_pi_text_is_pinned() {
        assert_eq!(
            plakous_tools::PromptDigest::of(&pinned_text()),
            plakous_tools::PromptDigest::from_hex(
                "2ce838a74efe26f3920e3e7bc22792562da779c7ad4e8fdfaaec8c2da09ea8ec"
            )
        );
    }
}
