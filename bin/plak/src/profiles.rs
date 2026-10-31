//! Named agent profiles.
//!
//! A profile selects the root template and the working set together. The
//! built-ins ship inside the binary as `{id}-root`, `{id}-worker`,
//! `{id}-reviewer`, and `{id}-researcher`, so a user's own `worker.toml` is
//! left alone. A file in `~/.plak/profiles/<name>.toml` names templates the
//! user already has. The built-in ids in [`builtins`] are reserved.
//!
//! Nothing here writes. [`prepare`] answers the config a profile would
//! produce; the caller saves it once the choice has landed.

use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::product_config::{AgentTemplate, ProductConfig, parse_template};
use crate::surface_protocol::{ProfileRole, ProfileRow, ProfilesView};
use plakous_hosted::agent_tree_protocol::TemplateName;

pub(crate) struct BuiltinFile {
    pub(crate) role: &'static str,
    pub(crate) body: &'static str,
}

pub(crate) struct Builtin {
    pub(crate) id: &'static str,
    /// Short names `/agents` accepts, unless a user profile owns that stem.
    pub(crate) aliases: &'static [&'static str],
    pub(crate) files: &'static [BuiltinFile],
}

/// The roles every built-in ships, the working set in this order.
const ROLES: [&str; 3] = ["worker", "reviewer", "researcher"];

// Root and reviewer on Claude, worker on Codex, researcher on Grok.
const MULTI: Builtin = Builtin {
    id: "multi-default",
    aliases: &["multi"],
    files: &[
        BuiltinFile {
            role: "root",
            body: include_str!("../presets/multi/agents/root-agent.toml"),
        },
        BuiltinFile {
            role: "worker",
            body: include_str!("../presets/multi/agents/worker.toml"),
        },
        BuiltinFile {
            role: "reviewer",
            body: include_str!("../presets/multi/agents/reviewer.toml"),
        },
        BuiltinFile {
            role: "researcher",
            body: include_str!("../presets/multi/agents/researcher.toml"),
        },
    ],
};

fn builtins() -> impl Iterator<Item = &'static Builtin> {
    crate::capabilities::presets().chain(std::iter::once(&MULTI))
}

/// A profile ready to apply: the config it produces and its root binding.
pub(crate) struct Prepared {
    /// The profile's id (an alias resolves to it).
    pub id: String,
    /// The loaded config pointed at the profile, not yet saved.
    pub config: ProductConfig,
    /// The root template.
    pub root: AgentTemplate,
}

/// `~/.plak/profiles/<name>.toml`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserFile {
    root: String,
    working_set: Vec<String>,
}

/// One profile's templates, resolved against the loaded config.
struct Resolved {
    id: String,
    root: (String, AgentTemplate),
    cards: Vec<(String, AgentTemplate)>,
}

/// Every selectable profile, the one the config already points at, and why
/// any user file was left out. Built-ins are always listed.
#[must_use]
pub(crate) fn list(config: &ProductConfig, home: &Path) -> ProfilesView {
    let (users, mut diagnostics) = user_profiles(config, home);
    let mut rows = Vec::new();
    for builtin in builtins() {
        match resolve_builtin(config, builtin) {
            Ok(resolved) => rows.push(row(
                config,
                &resolved,
                builtin
                    .aliases
                    .iter()
                    .filter(|alias| !users.iter().any(|user| user.id == **alias))
                    .map(|alias| (*alias).to_owned())
                    .collect(),
            )),
            Err(error) => diagnostics.push(format!("{}: {error}", builtin.id)),
        }
    }
    rows.extend(users.iter().map(|user| row(config, user, Vec::new())));
    ProfilesView {
        current: current(config, &rows, &users),
        rows,
        diagnostics,
    }
}

/// The config `name` produces. The built-in templates are added in memory
/// only where the name is still free, so a template the user already edited
/// under that name is kept. Nothing is written; saving the config later
/// publishes the new template files and the pointers together.
///
/// # Errors
/// An unknown or unreadable profile, a missing template, or a config the
/// product would refuse.
pub(crate) fn prepare(config: &ProductConfig, home: &Path, name: &str) -> Result<Prepared, String> {
    let resolved = resolve(config, home, name)?;
    let mut prepared = config.clone();
    for (template_name, template) in std::iter::once(&resolved.root).chain(&resolved.cards) {
        prepared
            .templates
            .entry(template_name.clone())
            .or_insert_with(|| template.clone());
    }
    let cards: Vec<&str> = resolved
        .cards
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    prepared.root_composition = None;
    prepared.set_key("root.agent", &resolved.root.0)?;
    prepared.set_key("roster.working_set", &cards.join(","))?;
    Ok(Prepared {
        id: resolved.id,
        config: prepared,
        root: resolved.root.1,
    })
}

/// The profile one session runs: the root template and working set its tree
/// froze at genesis. Profile choice is per session; the home's `[root] agent`
/// and `[roster] working_set` are only the default for brand-new sessions.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionSelection {
    /// The root template, when the session runs its tuple.
    pub(crate) root: Option<String>,
    /// The templates the root may spawn.
    pub(crate) working_set: Vec<String>,
}

impl SessionSelection {
    /// The selection a frozen tree epoch records.
    pub(crate) fn from_snapshot(
        snapshot: &plakous_hosted::orchestration::OrchestrationSnapshot,
    ) -> Self {
        Self {
            root: snapshot.root_template.clone(),
            working_set: snapshot
                .working_set
                .iter()
                .map(|name| name.as_str().to_owned())
                .collect(),
        }
    }
}

/// The profile `record`'s own tree froze, or `None` for a session without a
/// readable tree (no topology, or not yet opened).
pub(crate) fn session_selection(
    paths: &crate::home::paths::PlakPaths,
    record: &crate::store::sessions::SessionRecord,
) -> Option<SessionSelection> {
    let (_, observed) = crate::topology_product::inspect_session_tree(paths, record).ok()?;
    match observed {
        plakous_recursive_subagents_store::TreeInspection::Tree(tree) => {
            Some(SessionSelection::from_snapshot(&tree.rows().snapshot))
        }
        _ => None,
    }
}

/// The home configuration with `selection` as its root and working set,
/// resolved against the current template files (and, for a built-in
/// profile's `{id}-{role}` names the home never saved, the built-in bodies).
/// The result is in memory only.
///
/// # Errors
/// A named template that no longer loads, or a working set the product would
/// refuse (a missing description, too many members).
pub(crate) fn selection_config(
    config: &ProductConfig,
    selection: &SessionSelection,
) -> Result<ProductConfig, String> {
    let mut selected = config.clone();
    for name in selection.root.iter().chain(&selection.working_set) {
        if selected.templates.contains_key(name) {
            continue;
        }
        let template = builtin_template(name)
            .ok_or_else(|| format!("template `{name}` no longer exists"))??;
        selected.templates.insert(name.clone(), template);
    }
    selected.root_composition = None;
    selected.root_agent.clone_from(&selection.root);
    selected.working_set =
        plakous_hosted::agent_tree_protocol::WorkingSet::parse(selection.working_set.clone())?;
    selected.validate()?;
    Ok(selected)
}

/// A built-in profile's own body for `{id}-{role}`.
fn builtin_template(name: &str) -> Option<Result<AgentTemplate, String>> {
    builtins().find_map(|builtin| {
        let role = name.strip_prefix(builtin.id)?.strip_prefix('-')?;
        let file = builtin.files.iter().find(|file| file.role == role)?;
        Some(parse_template(file.body, name))
    })
}

/// The profile `name` selects: an exact id first, then a built-in alias no
/// user profile owns. The Agents list and `/agents <name>` both use this.
fn resolve(config: &ProductConfig, home: &Path, name: &str) -> Result<Resolved, String> {
    let name = name.trim();
    let (users, _) = user_profiles(config, home);
    if let Some(user) = users.into_iter().find(|user| user.id == name) {
        return Ok(user);
    }
    if let Some(builtin) =
        builtins().find(|builtin| builtin.id == name || builtin.aliases.contains(&name))
    {
        return resolve_builtin(config, builtin);
    }
    let path = home.join("profiles").join(format!("{name}.toml"));
    if path.is_file() {
        return Err(user_profile(config, name, &path)
            .err()
            .unwrap_or_else(|| format!("profile `{name}` could not be read")));
    }
    Err(format!("unknown profile `{name}`"))
}

fn resolve_builtin(config: &ProductConfig, builtin: &Builtin) -> Result<Resolved, String> {
    let mut root = None;
    let mut cards = Vec::new();
    for file in builtin.files {
        let name = format!("{}-{}", builtin.id, file.role);
        // A template the user already has under this name is the one used.
        let template = match config.templates.get(&name) {
            Some(template) => template.clone(),
            None => parse_template(file.body, &name)?,
        };
        if file.role == "root" {
            root = Some((name, template));
        } else {
            cards.push((name, template));
        }
    }
    Ok(Resolved {
        id: builtin.id.to_owned(),
        root: root.ok_or_else(|| "built-in profile has no root".to_owned())?,
        cards,
    })
}

/// User profiles in name order, plus a line for each file left out.
fn user_profiles(config: &ProductConfig, home: &Path) -> (Vec<Resolved>, Vec<String>) {
    let dir = home.join("profiles");
    let mut diagnostics = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (Vec::new(), diagnostics);
        }
        Err(error) => {
            diagnostics.push(format!("could not read {}: {error}", dir.display()));
            return (Vec::new(), diagnostics);
        }
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        files.push((stem.to_owned(), path));
    }
    files.sort();
    let mut profiles = Vec::new();
    for (stem, path) in files {
        if builtins().any(|builtin| builtin.id == stem) {
            diagnostics.push(format!(
                "profiles/{stem}.toml is ignored: `{stem}` is the built-in profile's name"
            ));
            continue;
        }
        match user_profile(config, &stem, &path) {
            Ok(profile) => profiles.push(profile),
            Err(error) => diagnostics.push(format!("profiles/{stem}.toml is ignored: {error}")),
        }
    }
    (profiles, diagnostics)
}

fn user_profile(config: &ProductConfig, id: &str, path: &Path) -> Result<Resolved, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("could not be read: {error}"))?;
    let file: UserFile = toml::from_str(&text).map_err(|error| error.message().to_owned())?;
    if file.root.is_empty() || file.working_set.is_empty() {
        return Err("it needs `root` and a non-empty `working_set`".to_owned());
    }
    let template = |name: &str| {
        config
            .templates
            .get(name)
            .cloned()
            .map(|template| (name.to_owned(), template))
            .ok_or_else(|| format!("it names `{name}`, and agents/{name}.toml does not exist"))
    };
    Ok(Resolved {
        id: id.to_owned(),
        root: template(&file.root)?,
        cards: file
            .working_set
            .iter()
            .map(|name| template(name))
            .collect::<Result<_, _>>()?,
    })
}

/// The profile whose root and working set the config already points at.
fn current(config: &ProductConfig, rows: &[ProfileRow], users: &[Resolved]) -> Option<String> {
    let root = config.root_agent.as_deref()?;
    let cards: Vec<&str> = config
        .working_set
        .names()
        .iter()
        .map(TemplateName::as_str)
        .collect();
    let builtin = builtins().find(|builtin| {
        root == format!("{}-root", builtin.id)
            && cards
                == ROLES
                    .map(|role| format!("{}-{role}", builtin.id))
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
    });
    if let Some(builtin) = builtin {
        return rows
            .iter()
            .any(|row| row.id == builtin.id)
            .then(|| builtin.id.to_owned());
    }
    users
        .iter()
        .find(|user| {
            user.root.0 == root
                && user
                    .cards
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .eq(cards.iter().copied())
        })
        .map(|user| user.id.clone())
}

/// The working set's cards, each followed by the templates it spawns,
/// indented under it.
fn row(config: &ProductConfig, resolved: &Resolved, aliases: Vec<String>) -> ProfileRow {
    let template = |name: &str| {
        resolved
            .cards
            .iter()
            .find(|(card, _)| card == name)
            .map(|(_, template)| template)
            .or_else(|| config.templates.get(name))
    };
    let working_set: Vec<TemplateName> = resolved
        .cards
        .iter()
        .filter_map(|(name, _)| TemplateName::parse(name).ok())
        .collect();
    let cards = crate::roster_input::nested_spawns(&working_set, |name| {
        template(name.as_str())
            .map(|template| template.spawns.clone())
            .unwrap_or_default()
    })
    .into_iter()
    .filter_map(|(depth, name)| {
        let template = template(name.as_str())?;
        // A built-in card reads as its role; a user card as its file.
        let label = name
            .as_str()
            .strip_prefix(&format!("{}-", resolved.id))
            .unwrap_or(name.as_str());
        Some(role(label, template, depth))
    })
    .collect();
    ProfileRow {
        id: resolved.id.clone(),
        differences: Vec::new(),
        aliases,
        root: role("root", &resolved.root.1, 0),
        cards,
    }
}

fn role(label: &str, template: &AgentTemplate, depth: usize) -> ProfileRole {
    ProfileRole {
        label: label.to_owned(),
        account: template.account.clone(),
        model: template.model.clone(),
        effort: template
            .effort
            .clone()
            .unwrap_or_else(|| "default".to_owned()),
        fast: template.is_fast(),
        depth,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
