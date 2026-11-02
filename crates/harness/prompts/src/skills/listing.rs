//! The skill listing in each family's own wording.
//!
//! - Codex: the host-filesystem `<skills_instructions>` catalog Codex CLI
//!   sends, with its roots table and how-to, vendored under Apache-2.0
//!   (`vendor/codex-prompt/skills`, see its `PROVENANCE.md`). Codex reads a
//!   skill through its shell, as Codex CLI does.
//! - Claude: Claude Code's listing sentence for its `Skill` tool.
//! - Grok: grok-build's listing with each skill's absolute path, read with
//!   `read_file`.
//! - pi (OpenRouter): pi's `<skills>` section and `<available_skills>` XML.
//! - Neutral (the typed Python SDK): pi's wording for a reader reached through
//!   another tool.

use std::fmt::Write as _;
use std::path::PathBuf;

use super::{MAX_SKILL_LISTING_BYTES, Skill};

pub(super) const CODEX_INTRO: &str =
    include_str!("../../vendor/codex-prompt/skills/intro-host-aliases.txt");
const CODEX_HOW_TO: &str =
    include_str!("../../vendor/codex-prompt/skills/how-to-use-host-aliases.txt");
const CLAUDE_HEADER: &str = "The following skills are available for use with the Skill tool:";
const GROK_HEADER: &str = "The following skills are available for use:";
const GROK_READ: &str = "Read a skill's SKILL.md with read_file at its absolute path before following it; read_file also reads the other files in a listed skill's directory.";
const PI_PREAMBLE: &str =
    "The following skills provide specialized instructions for specific tasks.";
const PI_READ: &str =
    "Use the read tool to load a skill's file when the task matches its description.";
const NEUTRAL_READ: &str = "Load a skill's file when the task matches its description.";
const PI_RELATIVE: &str = "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.";

/// Which family's listing a node gets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillsPresentation {
    /// Codex CLI's `<skills_instructions>` catalog.
    Codex,
    /// Claude Code's `Skill` tool listing.
    Claude,
    /// grok-build's listing with absolute paths.
    Grok,
    /// pi's `<skills>` section.
    Pi,
    /// pi's wording for a reader reached through another tool.
    Neutral,
}

impl SkillsPresentation {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Grok => "grok",
            Self::Pi => "pi",
            Self::Neutral => "neutral",
        }
    }
}

/// Renders the listing for `skills`, or `None` when there are none. Entries
/// keep discovery order; once their bytes would pass
/// [`MAX_SKILL_LISTING_BYTES`], the rest are counted in one closing line.
#[must_use]
pub(super) fn render_skills_listing(
    presentation: SkillsPresentation,
    skills: &[Skill],
) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let roots = listed_roots(skills);
    let entries = fitting_entries(presentation, skills, &roots);
    let omitted = skills.len() - entries.len();
    let omitted_line = (omitted > 0).then(|| {
        let noun = if omitted == 1 { "skill" } else { "skills" };
        format!("{omitted} additional {noun} omitted from this bounded skills list.")
    });
    let listed = &skills[..entries.len()];
    Some(match presentation {
        SkillsPresentation::Codex => {
            let mut text =
                format!("<skills_instructions>\n## Skills\n{CODEX_INTRO}\n### Skill roots\n");
            for (index, root) in roots.iter().enumerate() {
                if listed.iter().any(|skill| skill.root() == root) {
                    let _ = writeln!(text, "- `r{index}` = `{}`", root.display());
                }
            }
            text.push_str("### Available skills\n");
            push_lines(&mut text, &entries, omitted_line.as_deref(), "- ");
            let _ = write!(
                text,
                "### How to use skills\n{CODEX_HOW_TO}\n</skills_instructions>"
            );
            text
        }
        SkillsPresentation::Claude => {
            let mut text = format!("{CLAUDE_HEADER}\n\n");
            push_lines(&mut text, &entries, omitted_line.as_deref(), "- ");
            text.truncate(text.trim_end().len());
            text
        }
        SkillsPresentation::Grok => {
            let mut text = format!("{GROK_HEADER}\n\n");
            push_lines(&mut text, &entries, omitted_line.as_deref(), "- ");
            let _ = write!(text, "\n{GROK_READ}");
            text
        }
        SkillsPresentation::Pi | SkillsPresentation::Neutral => {
            let read = if presentation == SkillsPresentation::Pi {
                PI_READ
            } else {
                NEUTRAL_READ
            };
            let mut text =
                format!("<skills>\n{PI_PREAMBLE}\n{read}\n{PI_RELATIVE}\n\n<available_skills>\n");
            for entry in &entries {
                text.push_str(entry);
            }
            text.push_str("</available_skills>\n");
            if let Some(line) = omitted_line {
                let _ = writeln!(text, "{line}");
            }
            text.push_str("</skills>");
            text
        }
    })
}

/// How many of `skills`, from the first, the listing names within its bound.
pub(super) fn listed_count(presentation: SkillsPresentation, skills: &[Skill]) -> usize {
    fitting_entries(presentation, skills, &listed_roots(skills)).len()
}

/// The rendered entries that fit in [`MAX_SKILL_LISTING_BYTES`], in order.
fn fitting_entries(
    presentation: SkillsPresentation,
    skills: &[Skill],
    roots: &[PathBuf],
) -> Vec<String> {
    let mut entries = Vec::new();
    let mut used = 0_usize;
    for skill in skills {
        let entry = entry(presentation, skill, roots);
        if used + entry.len() > MAX_SKILL_LISTING_BYTES {
            break;
        }
        used += entry.len();
        entries.push(entry);
    }
    entries
}

/// Distinct canonical roots of `skills`, in discovery order.
fn listed_roots(skills: &[Skill]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for skill in skills {
        if !roots.iter().any(|root| root == skill.root()) {
            roots.push(skill.root().to_path_buf());
        }
    }
    roots
}

/// One Claude listing line, with its terminator.
pub(super) fn claude_entry(skill: &Skill) -> String {
    entry(SkillsPresentation::Claude, skill, &[])
}

fn entry(presentation: SkillsPresentation, skill: &Skill, roots: &[PathBuf]) -> String {
    let name = skill.name();
    let description = skill.description();
    match presentation {
        SkillsPresentation::Codex => {
            let index = roots
                .iter()
                .position(|root| root == skill.root())
                .unwrap_or_default();
            format!("- {name}: {description} (file: r{index}/{name}/SKILL.md)\n")
        }
        SkillsPresentation::Claude => format!("- {name}: {description}\n"),
        SkillsPresentation::Grok => format!(
            "- {name}: {description}\n  Absolute path: {}\n",
            skill.file().display()
        ),
        SkillsPresentation::Pi | SkillsPresentation::Neutral => format!(
            "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>\n",
            escape_xml(name),
            escape_xml(description),
            escape_xml(&skill.file().display().to_string())
        ),
    }
}

fn push_lines(text: &mut String, entries: &[String], omitted: Option<&str>, prefix: &str) {
    for entry in entries {
        text.push_str(entry);
    }
    if let Some(line) = omitted {
        let _ = writeln!(text, "{prefix}{line}");
    }
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
pub(crate) const SKILLS_PROMPT_PINS: &[plakous_tools::PromptPin] = &[
    plakous_tools::PromptPin::new(
        "plak.skills.codex.intro.v1",
        CODEX_INTRO,
        "76f14bd0018d03a0b25728d96451d5956cf95652cc9b6c8fde0d93f55a32c89d",
    ),
    plakous_tools::PromptPin::new(
        "plak.skills.codex.how-to.v1",
        CODEX_HOW_TO,
        "5ce6d8ff23ccc49a791c92995741f0e21daf82e62779485b3008f7f78ff4f29b",
    ),
    plakous_tools::PromptPin::new(
        "plak.skills.claude.header.v1",
        CLAUDE_HEADER,
        "fb3f6694ba004cbcefd492af973e6b9b2a296a8221b162d7a9e55318cacf616c",
    ),
    plakous_tools::PromptPin::new(
        "plak.skills.grok.header.v1",
        GROK_HEADER,
        "f4ee42f91e417f6cd2a81efd7e83b51493e70a9b214341a2f9f60bbcd24cff0a",
    ),
    plakous_tools::PromptPin::new(
        "plak.skills.grok.read.v1",
        GROK_READ,
        "5f66e4e0982315f34468faa88a52b6562538b5cd5a60406d2621a4c65a82a23e",
    ),
    plakous_tools::PromptPin::new(
        "plak.skills.pi.preamble.v1",
        PI_PREAMBLE,
        "a89485117ba39a4f7719d7116b3c86ad425581801c1b6ee3ca642ede81d0b05f",
    ),
    plakous_tools::PromptPin::new(
        "plak.skills.pi.read.v1",
        PI_READ,
        "f3585c9a01b6bcacb2f19306cb3899d9a2a5644c16392b8d0e0fdf6a5ff0737f",
    ),
    plakous_tools::PromptPin::new(
        "plak.skills.neutral.read.v1",
        NEUTRAL_READ,
        "ba7b3aa5872f9726c962afef959bd5fa3167a785c473cff16b02e955b8f92c70",
    ),
    plakous_tools::PromptPin::new(
        "plak.skills.pi.relative.v1",
        PI_RELATIVE,
        "7d359b5b2928fef716a205dae29cea374fe5a913cd24974799b13a398616f974",
    ),
];
