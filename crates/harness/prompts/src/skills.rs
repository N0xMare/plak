//! Bounded Agent Skills discovery and the per-family skill listing.
//!
//! A skill is a directory `<root>/<name>/SKILL.md` whose front matter names it
//! and describes it. Discovery reads the project root before the user root, so
//! a project skill wins a name both define. The listing is rendered once, at
//! construction, as one instruction source after the enrolled `AGENTS.md`
//! files, in the wording each family's own client uses (spec 02 §6.1.1). The
//! model then reads a skill's files on demand.
//!
//! The project root must resolve inside the workspace and holds no links, so
//! every path it lists is one the workspace read can open. Only the user root
//! is a candidate for the read exception outside the workspace (spec 04 §2.1).

use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use plakous_agent::{ProfileError, RegisteredContextSource};
use plakous_model::{hex_lower, sha256_bytes};
use rustix::fs::{Mode, OFlags};

mod front_matter;
mod listing;

#[cfg(test)]
pub(crate) use listing::SKILLS_PROMPT_PINS;
pub use listing::SkillsPresentation;
use listing::render_skills_listing;

/// The one skill file name discovery looks for.
pub const SKILL_FILE_NAME: &str = "SKILL.md";
/// The context-source id the listing is enrolled under.
pub const SKILLS_SOURCE_ID: &str = "plak.skills.v1";

const PROJECT_SKILLS_DIRECTORY: &str = ".agents/skills";
const USER_SKILLS_DIRECTORY: &str = ".agents/skills";
const MAX_SKILL_NAME_CHARS: usize = 64;
const MAX_SKILL_DESCRIPTION_CHARS: usize = 1024;
/// Bytes of front matter discovery reads; a longer block is refused.
const MAX_FRONT_MATTER_BYTES: usize = 64 * 1024;
/// Bytes of one `SKILL.md` the `Skill` tool loads.
const MAX_SKILL_FILE_BYTES: usize = 256 * 1024;
/// Entries examined under one root, in byte order.
const MAX_ROOT_ENTRIES: usize = 256;
/// Skills one discovery lists; later candidates are counted, not read.
const MAX_LISTED_SKILLS: usize = 64;
/// Bytes of listing entries; later skills are counted in one closing line.
const MAX_SKILL_LISTING_BYTES: usize = 16 * 1024;

const CONTENT_DIGEST_DOMAIN: &[u8] = b"plak.skills.content.v1";

/// Which root a skill was found under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillScope {
    /// `<workspace root>/.agents/skills`.
    Project,
    /// `~/.agents/skills`.
    User,
}

/// One directory discovery reads skills from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillRoot {
    scope: SkillScope,
    path: PathBuf,
}

impl SkillRoot {
    /// The project root under one workspace root.
    #[must_use]
    pub fn project(workspace_root: &Path) -> Self {
        Self {
            scope: SkillScope::Project,
            path: workspace_root.join(PROJECT_SKILLS_DIRECTORY),
        }
    }

    /// The user root under one home directory.
    #[must_use]
    pub fn user(home: &Path) -> Self {
        Self {
            scope: SkillScope::User,
            path: home.join(USER_SKILLS_DIRECTORY),
        }
    }

    /// Returns the scope.
    #[must_use]
    pub const fn scope(&self) -> SkillScope {
        self.scope
    }

    /// Returns the configured (not yet canonical) path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// One listed skill.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skill {
    name: String,
    description: String,
    scope: SkillScope,
    root: PathBuf,
    file: PathBuf,
}

impl Skill {
    /// The skill's name, equal to its directory name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The one-line description: front matter whitespace collapsed to spaces.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Which root it was found under.
    #[must_use]
    pub const fn scope(&self) -> SkillScope {
        self.scope
    }

    /// The canonical root directory it was found under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<canonical root>/<name>/SKILL.md`.
    #[must_use]
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// `<canonical root>/<name>`, the skill's base directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        self.file.parent().unwrap_or(&self.root)
    }

    /// Loads this listed skill now: its file is read and validated again, and
    /// a skill that is no longer valid, or no longer this skill, is refused.
    ///
    /// # Errors
    ///
    /// Returns the diagnostic that would now keep the skill out of a listing.
    pub fn load(&self) -> Result<LoadedSkill, SkillDiagnostic> {
        let diagnose = |problem| SkillDiagnostic {
            path: self.directory().to_path_buf(),
            problem,
        };
        let text = read_skill_text(self.scope, &self.root, &self.name, MAX_SKILL_FILE_BYTES)
            .map_err(diagnose)?;
        let Some(valid) = validated(&text, &self.name).map_err(diagnose)? else {
            return Err(diagnose(SkillProblem::NotModelInvocable));
        };
        Ok(LoadedSkill {
            name: self.name.clone(),
            directory: self.directory().to_path_buf(),
            body: valid.body,
        })
    }
}

/// Why one candidate was not listed or loaded.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SkillProblem {
    /// The root exists but is not a directory, or could not be listed.
    UnreadableRoot,
    /// The project root resolves outside the workspace.
    OutsideWorkspace,
    /// The root holds more entries than are examined; the rest, in byte
    /// order, were skipped.
    TooManyEntries,
    /// The skill directory or its `SKILL.md` resolves outside the root.
    OutsideRoot,
    /// A project skill directory or `SKILL.md` is a symbolic link, which the
    /// workspace read cannot open.
    LinkedPath,
    /// `SKILL.md` is missing, not a regular file, or unreadable.
    MissingSkillFile,
    /// `SKILL.md` is larger than the `Skill` tool loads.
    TooLarge,
    /// The front matter is longer than discovery reads.
    FrontMatterTooLarge,
    /// `SKILL.md` is not UTF-8.
    InvalidUtf8,
    /// The front matter is missing or is not the supported subset.
    FrontMatter(&'static str),
    /// The name breaks the Agent Skills name rules.
    InvalidName(&'static str),
    /// The front matter name differs from the directory name.
    NameMismatch {
        /// The name the front matter states.
        name: String,
    },
    /// The description is missing or empty.
    MissingDescription,
    /// The description is longer than the Agent Skills format allows.
    DescriptionTooLong,
    /// A skill of the same name was already found in an earlier root.
    Shadowed,
    /// The listing is full: this many further entries of this root, and every
    /// later root, were not read.
    TooManySkills {
        /// Entries of this root not read.
        rest: usize,
        /// Later roots not read.
        later_roots: usize,
    },
    /// The skill now sets `disable-model-invocation: true`.
    NotModelInvocable,
}

/// One candidate discovery did not list, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDiagnostic {
    path: PathBuf,
    problem: SkillProblem,
}

impl SkillDiagnostic {
    /// The path the problem is about.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The problem.
    #[must_use]
    pub const fn problem(&self) -> &SkillProblem {
        &self.problem
    }
}

impl fmt::Display for SkillDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.problem {
            SkillProblem::UnreadableRoot => {
                write!(formatter, "skills directory {path} could not be listed")
            }
            SkillProblem::OutsideWorkspace => write!(
                formatter,
                "skills directory {path} resolves outside the workspace"
            ),
            SkillProblem::TooManyEntries => write!(
                formatter,
                "skills directory {path} holds more than {MAX_ROOT_ENTRIES} entries; the rest were skipped"
            ),
            SkillProblem::OutsideRoot => {
                write!(
                    formatter,
                    "skill {path} resolves outside its skills directory"
                )
            }
            SkillProblem::LinkedPath => write!(
                formatter,
                "skill {path} is a symbolic link, which the workspace read cannot open"
            ),
            SkillProblem::MissingSkillFile => {
                write!(formatter, "skill {path} has no readable {SKILL_FILE_NAME}")
            }
            SkillProblem::TooLarge => write!(
                formatter,
                "skill {path} {SKILL_FILE_NAME} exceeds {MAX_SKILL_FILE_BYTES} bytes"
            ),
            SkillProblem::FrontMatterTooLarge => write!(
                formatter,
                "skill {path} front matter exceeds {MAX_FRONT_MATTER_BYTES} bytes"
            ),
            SkillProblem::InvalidUtf8 => {
                write!(formatter, "skill {path} {SKILL_FILE_NAME} is not UTF-8")
            }
            SkillProblem::FrontMatter(reason) => {
                write!(formatter, "skill {path} front matter {reason}")
            }
            SkillProblem::InvalidName(reason) => write!(formatter, "skill {path} name {reason}"),
            SkillProblem::NameMismatch { name } => write!(
                formatter,
                "skill {path} names {name:?}, not its directory name"
            ),
            SkillProblem::MissingDescription => {
                write!(formatter, "skill {path} has no description")
            }
            SkillProblem::DescriptionTooLong => write!(
                formatter,
                "skill {path} description exceeds {MAX_SKILL_DESCRIPTION_CHARS} characters"
            ),
            SkillProblem::Shadowed => write!(
                formatter,
                "skill {path} is shadowed by a project skill of the same name"
            ),
            SkillProblem::TooManySkills { rest, later_roots } => write!(
                formatter,
                "skills directory {path}: {MAX_LISTED_SKILLS} skills are listed; the rest of this directory ({rest} entries) and {later_roots} later skills directories were not read"
            ),
            SkillProblem::NotModelInvocable => {
                write!(formatter, "skill {path} sets disable-model-invocation")
            }
        }
    }
}

/// One canonical root that existed as a directory at discovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredRoot {
    scope: SkillScope,
    path: PathBuf,
}

impl DiscoveredRoot {
    /// Returns the scope.
    #[must_use]
    pub const fn scope(&self) -> SkillScope {
        self.scope
    }

    /// Returns the canonical path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The complete result of one discovery.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillDiscovery {
    skills: Vec<Skill>,
    roots: Vec<DiscoveredRoot>,
    diagnostics: Vec<SkillDiagnostic>,
}

impl SkillDiscovery {
    /// Listed skills, project root first, each root in name order.
    #[must_use]
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// Canonical roots that existed as directories, in root order.
    #[must_use]
    pub fn roots(&self) -> &[DiscoveredRoot] {
        &self.roots
    }

    /// Candidates that were not listed.
    #[must_use]
    pub fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }

    /// The listing as one instruction source, or `None` when nothing is listed.
    ///
    /// # Errors
    ///
    /// Returns a profile error when the source cannot be registered.
    pub fn registered_source(
        &self,
        presentation: SkillsPresentation,
    ) -> Result<Option<RegisteredContextSource>, ProfileError> {
        let Some(content) = render_skills_listing(presentation, &self.skills) else {
            return Ok(None);
        };
        let length = (content.len() as u64).to_be_bytes();
        let digest = sha256_bytes(&[CONTENT_DIGEST_DOMAIN, &[0], &length, content.as_bytes()]);
        RegisteredContextSource::new(
            SKILLS_SOURCE_ID,
            content,
            format!("sha256:{}", hex_lower(&digest)),
            format!("plak-skills-v1:{}", presentation.label()),
        )
        .map(Some)
    }

    /// The skills the `presentation` listing actually names: a skill cut by
    /// the listing's byte bound is not listed, so it is not loadable either.
    #[must_use]
    pub fn listed_skills(&self, presentation: SkillsPresentation) -> &[Skill] {
        &self.skills[..listing::listed_count(presentation, &self.skills)]
    }

    /// Keeps only the skills a frozen Claude listing names with the same
    /// description, so a session reopened later loads exactly what it listed.
    pub fn retain_listed_in(&mut self, frozen_claude_listing: &str) {
        self.skills.retain(|skill| {
            let line = listing::claude_entry(skill);
            frozen_claude_listing
                .lines()
                .any(|listed| listed == line.trim_end())
        });
    }
}

/// Discovers skills under `roots`, in order. A missing root is not an error;
/// every other problem is a diagnostic, never a failure, so one malformed
/// skill does not stop a session. A project root is admitted only when its
/// canonical path stays inside the canonical `workspace_root`.
#[must_use]
pub fn discover_skills(workspace_root: &Path, roots: &[SkillRoot]) -> SkillDiscovery {
    let mut discovery = discover_skill_roots(workspace_root, roots);
    for index in 0..discovery.roots.len() {
        let root = discovery.roots[index].clone();
        let Some(names) = discovery.root_entries(&root.path) else {
            continue;
        };
        for (position, name) in names.iter().enumerate() {
            if discovery.skills.len() == MAX_LISTED_SKILLS {
                let problem = SkillProblem::TooManySkills {
                    rest: names.len() - position,
                    later_roots: discovery.roots.len() - index - 1,
                };
                discovery.diagnose(&root.path, problem);
                return discovery;
            }
            let candidate = root.path.join(name);
            let Ok(metadata) = candidate.symlink_metadata() else {
                continue;
            };
            if metadata.is_symlink() {
                if root.scope == SkillScope::Project {
                    discovery.diagnose(&candidate, SkillProblem::LinkedPath);
                    continue;
                }
            } else if !metadata.is_dir() {
                continue;
            }
            match read_listed(root.scope, &root.path, name) {
                Ok(None) => {}
                Ok(Some(skill)) => {
                    if discovery
                        .skills
                        .iter()
                        .any(|known| known.name == skill.name)
                    {
                        discovery.diagnose(&candidate, SkillProblem::Shadowed);
                    } else {
                        discovery.skills.push(skill);
                    }
                }
                Err(problem) => discovery.diagnose(&candidate, problem),
            }
        }
    }
    discovery
}

/// The canonical roots alone, without reading any skill: what an exact
/// reopen needs for its read exception.
#[must_use]
pub fn discover_skill_roots(workspace_root: &Path, roots: &[SkillRoot]) -> SkillDiscovery {
    let mut discovery = SkillDiscovery::default();
    let workspace = std::fs::canonicalize(workspace_root).ok();
    for root in roots {
        let canonical = match std::fs::canonicalize(root.path()) {
            Ok(path) if path.is_dir() => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(_) | Err(_) => {
                discovery.diagnose(root.path(), SkillProblem::UnreadableRoot);
                continue;
            }
        };
        if root.scope() == SkillScope::Project
            && !workspace
                .as_ref()
                .is_some_and(|workspace| canonical.starts_with(workspace))
        {
            discovery.diagnose(root.path(), SkillProblem::OutsideWorkspace);
            continue;
        }
        if discovery.roots.iter().any(|known| known.path == canonical) {
            continue;
        }
        discovery.roots.push(DiscoveredRoot {
            scope: root.scope(),
            path: canonical,
        });
    }
    discovery
}

impl SkillDiscovery {
    fn diagnose(&mut self, path: &Path, problem: SkillProblem) {
        self.diagnostics.push(SkillDiagnostic {
            path: path.to_path_buf(),
            problem,
        });
    }

    /// Every entry name under one root, dot entries skipped, sorted by byte
    /// order and then capped at [`MAX_ROOT_ENTRIES`].
    fn root_entries(&mut self, root: &Path) -> Option<Vec<String>> {
        let Ok(entries) = std::fs::read_dir(root) else {
            self.diagnose(root, SkillProblem::UnreadableRoot);
            return None;
        };
        let mut names = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else {
                self.diagnose(root, SkillProblem::UnreadableRoot);
                return None;
            };
            if let Some(name) = entry.file_name().to_str()
                && !name.starts_with('.')
            {
                names.push(name.to_owned());
            }
        }
        names.sort();
        if names.len() > MAX_ROOT_ENTRIES {
            names.truncate(MAX_ROOT_ENTRIES);
            self.diagnose(root, SkillProblem::TooManyEntries);
        }
        Some(names)
    }
}

/// A skill a `Skill` call loaded: its base directory and body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedSkill {
    name: String,
    directory: PathBuf,
    body: String,
}

impl LoadedSkill {
    /// The skill's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `<canonical root>/<name>`.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// `SKILL.md` after its front matter.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }
}

struct Validated {
    description: String,
    body: String,
}

/// The listed facts of one `SKILL.md`, `Ok(None)` when the skill opts out of
/// model invocation.
fn validated(text: &str, directory_name: &str) -> Result<Option<Validated>, SkillProblem> {
    let parsed = front_matter::parse(text)?;
    if parsed.disable_model_invocation {
        return Ok(None);
    }
    let declared = parsed
        .name
        .ok_or(SkillProblem::FrontMatter("has no name"))?;
    if let Some(reason) = front_matter::name_problem(&declared) {
        return Err(SkillProblem::InvalidName(reason));
    }
    if declared != directory_name {
        return Err(SkillProblem::NameMismatch { name: declared });
    }
    let description = parsed
        .description
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.is_empty())
        .ok_or(SkillProblem::MissingDescription)?;
    if description.chars().count() > MAX_SKILL_DESCRIPTION_CHARS {
        return Err(SkillProblem::DescriptionTooLong);
    }
    Ok(Some(Validated {
        description,
        body: parsed.body.to_owned(),
    }))
}

fn read_listed(scope: SkillScope, root: &Path, name: &str) -> Result<Option<Skill>, SkillProblem> {
    let text = read_skill_text(scope, root, name, MAX_FRONT_MATTER_BYTES)?;
    let Some(front) = front_matter::leading_block(&text) else {
        if text.len() >= MAX_FRONT_MATTER_BYTES {
            return Err(SkillProblem::FrontMatterTooLarge);
        }
        return Err(front_matter::parse(&text)
            .err()
            .unwrap_or(SkillProblem::FrontMatter("is not closed")));
    };
    Ok(validated(front, name)?.map(|valid| Skill {
        name: name.to_owned(),
        description: valid.description,
        scope,
        root: root.to_path_buf(),
        file: root.join(name).join(SKILL_FILE_NAME),
    }))
}

/// Reads up to `limit` bytes of `<root>/<name>/SKILL.md`. The canonical path
/// must stay inside `root`, and a project skill may not be reached through a
/// link. Only a regular file is opened; the open never follows a link and
/// never blocks. Over `limit`, the `Skill` tool's whole-file read refuses and
/// discovery's front-matter read keeps the prefix.
fn read_skill_text(
    scope: SkillScope,
    root: &Path,
    name: &str,
    limit: usize,
) -> Result<String, SkillProblem> {
    let path = root.join(name).join(SKILL_FILE_NAME);
    let canonical = std::fs::canonicalize(&path).map_err(|_| SkillProblem::MissingSkillFile)?;
    if !canonical.starts_with(root) {
        return Err(SkillProblem::OutsideRoot);
    }
    if scope == SkillScope::Project && canonical != path {
        return Err(SkillProblem::LinkedPath);
    }
    if !std::fs::symlink_metadata(&canonical).is_ok_and(|metadata| metadata.is_file()) {
        return Err(SkillProblem::MissingSkillFile);
    }
    let descriptor = rustix::fs::open(
        &canonical,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| SkillProblem::MissingSkillFile)?;
    let file = File::from(descriptor);
    let Ok(metadata) = file.metadata() else {
        return Err(SkillProblem::MissingSkillFile);
    };
    if !metadata.is_file() {
        return Err(SkillProblem::MissingSkillFile);
    }
    // Discovery refuses what the `Skill` tool could not load, so every listed
    // skill is loadable.
    if metadata.len() > MAX_SKILL_FILE_BYTES as u64 {
        return Err(SkillProblem::TooLarge);
    }
    let whole = limit == MAX_SKILL_FILE_BYTES;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SkillProblem::MissingSkillFile)?;
    if bytes.len() > limit {
        if whole {
            return Err(SkillProblem::TooLarge);
        }
        bytes.truncate(limit);
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        // A prefix cut inside a character keeps its valid part.
        Err(error) if !whole && error.utf8_error().error_len().is_none() => {
            let valid = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).map_err(|_| SkillProblem::InvalidUtf8)
        }
        Err(_) => Err(SkillProblem::InvalidUtf8),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, reason = "unit tests")]
mod tests;
