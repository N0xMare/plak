//! Agent Skills for one node: the product setting, discovery when the node's
//! tools are built, the per-family listing, the user skill roots reads may
//! reach, and Claude Code's `Skill` tool (spec 02 §6.1.1, §6.3).
//!
//! Whether a session's skills are on is frozen in its registry: it holds the
//! `Skill` registration exactly while they are on. A new session, a spawn or
//! `/new` follows the live setting; a fork, rebind or switch successor
//! inherits its source's value; an exact reopen and a compaction follow what
//! the session froze, its listing included.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous::agent::RegisteredContextSource;
use plakous::model::{
    ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
};
use plakous::prompts::instructions::InstructionSourceReader;
use plakous::prompts::skills::{
    Skill, SkillDiscovery, SkillRoot, SkillScope, SkillsPresentation, discover_skill_roots,
    discover_skills,
};
use plakous::tools::{
    RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId,
    ToolFuture, ToolInvocation, ToolOutcome, ToolRegistration, ToolRuntimeError,
    ToolRuntimeErrorKind,
};
use serde::Deserialize;
use serde_json::json;

use crate::error::{CliFailure, CliResult};

/// Whether new constructions discover skills, and the home the user root is
/// under (`~/.agents/skills`). The product resolves both; nothing here reads
/// the environment.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillsSetting {
    enabled: bool,
    home: Option<PathBuf>,
}

impl SkillsSetting {
    /// The setting `skills.enabled` binds, with the user root under `home`
    /// when one is known. The home is kept when skills are off, so a session
    /// that froze skills still reaches its user root on reopen.
    #[must_use]
    pub const fn new(enabled: bool, home: Option<PathBuf>) -> Self {
        Self { enabled, home }
    }

    /// Whether new constructions discover skills.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn roots(&self, workspace_root: &Path) -> Vec<SkillRoot> {
        let mut roots = vec![SkillRoot::project(workspace_root)];
        roots.extend(self.home.as_deref().map(SkillRoot::user));
        roots
    }
}

/// What an exact reopen read back from the session it reopens.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FrozenSkills {
    /// The session's skills are on: its sealed registry holds `Skill`.
    pub(crate) on: bool,
    /// The sealed skills listing, when the session enrolled one.
    pub(crate) listing: Option<String>,
}

/// Reads whether the session at `locator` has its skills on, and its frozen
/// listing, from one exact load.
pub(crate) async fn frozen_skills(
    sessions: &plakous::sessions::AgentSessions,
    locator: plakous::sessions::SessionLocator,
) -> Result<FrozenSkills, plakous::sessions::AgentSessionsError> {
    let Ok(name) = ToolName::new(SKILL_TOOL_NAME) else {
        return Ok(FrozenSkills::default());
    };
    let (on, sources) = sessions
        .created_tool_registration_and_sources(locator, &name)
        .await?;
    let listing = sources
        .into_iter()
        .find(|source| source.source_id() == plakous::prompts::skills::SKILLS_SOURCE_ID)
        .map(|source| source.content().to_owned());
    Ok(FrozenSkills { on, listing })
}

/// Which skills authority one node build follows.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SkillsMode<'a> {
    /// A new session, spawn or `/new`: the live setting.
    New,
    /// A fork, rebind or switch successor: the source session's frozen on/off,
    /// with its skills discovered again.
    Inherited(bool),
    /// An exact reopen or compaction: exactly what the session froze.
    Frozen(&'a FrozenSkills),
}

/// One node's skills, discovered once when its tools are built.
#[derive(Clone)]
pub(crate) struct NodeSkills {
    presentation: SkillsPresentation,
    on: bool,
    new_construction: bool,
    discovery: Arc<SkillDiscovery>,
    /// Where the skill files are read, at discovery and at every `Skill` load.
    reader: Arc<dyn InstructionSourceReader>,
}

impl std::fmt::Debug for NodeSkills {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NodeSkills")
            .field("presentation", &self.presentation)
            .field("on", &self.on)
            .field("new_construction", &self.new_construction)
            .field("discovery", &self.discovery)
            .finish_non_exhaustive()
    }
}

impl NodeSkills {
    /// Discovers the node's skills.
    ///
    /// # Errors
    /// Fails when the reader cannot answer, so an unreachable skills root is
    /// never frozen into the session as an empty listing.
    pub(crate) fn discover(
        setting: &SkillsSetting,
        mode: SkillsMode<'_>,
        workspace_root: &Path,
        presentation: SkillsPresentation,
    ) -> CliResult<Self> {
        let roots = setting.roots(workspace_root);
        let reader: Arc<dyn InstructionSourceReader> =
            crate::runner_reader::instruction_reader(None);
        let unreadable =
            |error| CliFailure::new(format!("skills could not be discovered: {error}"));
        let fresh = |on: bool| {
            Ok(Self {
                presentation,
                on,
                new_construction: true,
                discovery: Arc::new(if on {
                    discover_skills(reader.as_ref(), workspace_root, &roots).map_err(unreadable)?
                } else {
                    SkillDiscovery::default()
                }),
                reader: Arc::clone(&reader),
            })
        };
        let frozen = match mode {
            SkillsMode::New => return fresh(setting.enabled),
            SkillsMode::Inherited(on) => return fresh(on),
            SkillsMode::Frozen(frozen) => frozen,
        };
        let discovery = if frozen.on && presentation == SkillsPresentation::Claude {
            // The tool may load exactly what the frozen listing names.
            let mut discovery =
                discover_skills(reader.as_ref(), workspace_root, &roots).map_err(unreadable)?;
            discovery.retain_listed_in(frozen.listing.as_deref().unwrap_or_default());
            discovery
        } else if frozen.on || frozen.listing.is_some() {
            discover_skill_roots(reader.as_ref(), workspace_root, &roots).map_err(unreadable)?
        } else {
            SkillDiscovery::default()
        };
        Ok(Self {
            presentation,
            on: frozen.on,
            new_construction: false,
            discovery: Arc::new(discovery),
            reader,
        })
    }

    /// Whether `Skill` is presented: the node's skills are on and it speaks
    /// Claude vocabulary.
    pub(crate) fn presented(&self) -> bool {
        self.on && self.presentation == SkillsPresentation::Claude
    }

    /// Canonical user roots that existed as directories when the tools were
    /// built: the only directories outside the workspace this node's read may
    /// reach. The project root is inside the workspace and never one of them.
    pub(crate) fn read_roots(&self) -> Vec<PathBuf> {
        self.discovery
            .roots()
            .iter()
            .filter(|root| root.scope() == SkillScope::User)
            .map(|root| root.path().to_path_buf())
            .collect()
    }

    /// The listing as one instruction source after `AGENTS.md`; a reopen
    /// keeps the one it froze, so it has none to enroll.
    pub(crate) fn listing_source(&self) -> CliResult<Option<RegisteredContextSource>> {
        if !self.new_construction {
            return Ok(None);
        }
        self.discovery
            .registered_source(self.presentation)
            .map_err(|error| CliFailure::new(format!("skills listing is invalid: {error}")))
    }

    /// One notice naming every skill a new construction did not list; a
    /// reopen reports nothing.
    pub(crate) fn notice(&self) -> Option<String> {
        let diagnostics = self.discovery.diagnostics();
        (self.new_construction && !diagnostics.is_empty()).then(|| {
            let lines = diagnostics
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            format!("skills not listed: {}", lines.join("; "))
        })
    }

    /// The `Skill` registration, on every family's node while its skills are
    /// on, whether or not any skill was found: canonical tool authority then
    /// never moves with the vocabulary or the disk. Only a Claude node presents
    /// it, and it loads only the skills its listing names.
    pub(crate) fn tool_registration(&self) -> CliResult<Option<ToolRegistration>> {
        if !self.on {
            return Ok(None);
        }
        let loadable: Arc<[Skill]> = if !self.presented() {
            Arc::new([])
        } else if self.new_construction {
            self.discovery.listed_skills(self.presentation).into()
        } else {
            self.discovery.skills().into()
        };
        skill_tool_registration(loadable, Arc::clone(&self.reader)).map(Some)
    }
}

/// The `Skill` tool description, aligned with Claude Code's own and limited to
/// what Plak does: the skill's instructions arrive in the result.
pub(crate) const SKILL_TOOL_DESCRIPTION: &str = "Invoke a skill.\n\nA skill is a packaged set of instructions the user or project has set up for a particular kind of task (deploy steps, a review checklist, a repo-specific workflow). Available skills appear in the instructions' skill listing with one-line descriptions. When the task at hand is one a listed skill covers, call this tool first: the skill's instructions arrive in the result for you to follow in place of your default approach. Users may also ask for one by name; that's a request to invoke it.\n\n- `skill`: exact name from the listing, no leading slash.\n- `args`: optional arguments to pass through.\n\nOnly names from the listing are valid. If a skill's instructions are already in this turn, follow them rather than calling again. The result names the skill's base directory; Read reads the files inside it.";

pub(crate) const SKILL_TOOL_NAME: &str = "Skill";
/// Bytes of a loaded skill the result carries; a longer body is refused.
const MAX_SKILL_RESULT_BYTES: usize = 200 * 1024;
const ISSUER: &[u8] = b"plak.skills";

fn skill_tool_registration(
    skills: Arc<[Skill]>,
    reader: Arc<dyn InstructionSourceReader>,
) -> CliResult<ToolRegistration> {
    let invalid = |_| CliFailure::new("Skill tool definition is invalid");
    let definition = ToolDefinition::new(
        ToolName::new(SKILL_TOOL_NAME).map_err(invalid)?,
        "plak.skills.v1",
        SKILL_TOOL_DESCRIPTION,
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The name of a skill from the available-skills list. Do not guess names."
                },
                "args": {
                    "type": "string",
                    "description": "Optional arguments for the skill"
                }
            },
            "required": ["skill"],
            "additionalProperties": false
        }))
        .map_err(|_| CliFailure::new("Skill tool schema is invalid"))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Parallel,
        ToolEffect::ReadOnly,
    )
    .map_err(|_| CliFailure::new("Skill tool definition is invalid"))?;
    Ok(ToolRegistration::new(
        RegisteredToolId::new("plak.skills.skill.v1")
            .map_err(|_| CliFailure::new("Skill tool id is invalid"))?,
        ToolExecutorId::new("plak.skills.skill.executor.v1")
            .map_err(|_| CliFailure::new("Skill executor id is invalid"))?,
        ToolExecutorBindingIdentity::new(1, &[ISSUER, SKILL_TOOL_NAME.as_bytes()])
            .map_err(|_| CliFailure::new("Skill binding identity is invalid"))?,
        definition,
        Arc::new(SkillTool { skills, reader }),
    )
    .nested_eligible())
}

struct SkillTool {
    skills: Arc<[Skill]>,
    reader: Arc<dyn InstructionSourceReader>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillArgs {
    skill: String,
    #[serde(default)]
    args: Option<String>,
}

impl ToolExecutor for SkillTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            context.check_cancelled()?;
            let args: SkillArgs = serde_json::from_value(
                invocation.arguments().value().as_ref().clone(),
            )
            .map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "Skill arguments could not be decoded",
                )
            })?;
            let name = args.skill.trim();
            let name = name.strip_prefix('/').unwrap_or(name);
            let subject: String = name.chars().take(100).collect();
            context.progress().report(&subject, None, None)?;
            // The reader is synchronous: its answers are read on a blocking
            // worker, never on the turn's async worker (spec 01 section 8.5).
            let (skills, reader, name) = (
                Arc::clone(&self.skills),
                Arc::clone(&self.reader),
                name.to_owned(),
            );
            let loaded = tokio::task::spawn_blocking(move || {
                skill_result(&skills, reader.as_ref(), &name, args.args.as_deref())
            })
            .await
            .unwrap_or_else(|_| Err("the skill could not be read: its reader failed".to_owned()));
            match loaded {
                Ok(text) => ToolOutcome::success_text(text),
                Err(message) => ToolOutcome::failure_text(message),
            }
        })
    }
}

/// The result text Claude Code's `Skill` produces: the launch line, the base
/// directory, the skill body, and any arguments. Only a listed skill loads,
/// and only while it is still valid.
fn skill_result(
    skills: &[Skill],
    reader: &dyn InstructionSourceReader,
    name: &str,
    args: Option<&str>,
) -> Result<String, String> {
    let listed = skills
        .iter()
        .find(|skill| skill.name() == name)
        .ok_or_else(|| format!("Unknown skill: {name}"))?;
    let loaded = listed
        .load(reader)
        .map_err(|diagnostic| format!("Unknown skill: {name} ({diagnostic})"))?;
    let mut text = format!(
        "Launching skill: {}\n\nBase directory for this skill: {}\n\n{}",
        loaded.name(),
        loaded.directory().display(),
        loaded.body().trim_end()
    );
    if let Some(args) = args.map(str::trim).filter(|args| !args.is_empty()) {
        text.push_str("\n\nARGUMENTS: ");
        text.push_str(args);
    }
    if text.len() > MAX_SKILL_RESULT_BYTES {
        return Err(format!(
            "Skill {name} is larger than {MAX_SKILL_RESULT_BYTES} bytes; read its SKILL.md with Read in pages instead."
        ));
    }
    Ok(text)
}

/// The skills notices already shown for one root session, so its spawns and
/// forks, which discover the same roots, repeat none of them.
#[derive(Debug, Default)]
pub(crate) struct ShownSkillNotices {
    root_session: String,
    shown: std::collections::HashSet<String>,
}

impl ShownSkillNotices {
    /// The `notices` not yet shown for `root_session`, in order; a different
    /// root session starts afresh.
    pub(crate) fn fresh(&mut self, root_session: &str, notices: Vec<String>) -> Vec<String> {
        if self.root_session != root_session {
            root_session.clone_into(&mut self.root_session);
            self.shown.clear();
        }
        notices
            .into_iter()
            .filter(|notice| self.shown.insert(notice.clone()))
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
