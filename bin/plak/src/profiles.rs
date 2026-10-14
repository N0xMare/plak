//! Named agent profiles.
//!
//! A profile selects the root template and the working set together. The
//! three provider homes ship inside the binary as `{id}-root`,
//! `{id}-worker`, `{id}-reviewer`, and `{id}-scout`, so a user's own
//! `worker.toml` is left alone. A file in `~/.plak/profiles/<name>.toml`
//! names templates the user already has. The names `codex-default`,
//! `claude-default`, and `grok-default` are reserved for the built-ins.
//!
//! Nothing here writes. [`prepare`] answers the config a profile would
//! produce; the caller saves it once the choice has landed.

use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::product_config::{AgentTemplate, ProductConfig, parse_template};
use crate::surface_protocol::{ProfileRole, ProfileRow, ProfilesView};
use plakous_hosted::agent_tree_protocol::TemplateName;

struct BuiltinFile {
    role: &'static str,
    body: &'static str,
}

struct Builtin {
    id: &'static str,
    /// Short names `/agents` accepts, unless a user profile owns that stem.
    aliases: &'static [&'static str],
    files: &'static [BuiltinFile],
}

const CODEX: Builtin = Builtin {
    id: "codex-default",
    aliases: &["codex", "chatgpt"],
    files: &[
        BuiltinFile {
            role: "root",
            body: include_str!("../../../configs/providers/codex/agents/root-agent.toml"),
        },
        BuiltinFile {
            role: "worker",
            body: include_str!("../../../configs/providers/codex/agents/worker.toml"),
        },
        BuiltinFile {
            role: "reviewer",
            body: include_str!("../../../configs/providers/codex/agents/reviewer.toml"),
        },
        BuiltinFile {
            role: "scout",
            body: include_str!("../../../configs/providers/codex/agents/scout.toml"),
        },
    ],
};

const CLAUDE: Builtin = Builtin {
    id: "claude-default",
    aliases: &["claude"],
    files: &[
        BuiltinFile {
            role: "root",
            body: include_str!("../../../configs/providers/claude/agents/root-agent.toml"),
        },
        BuiltinFile {
            role: "worker",
            body: include_str!("../../../configs/providers/claude/agents/worker.toml"),
        },
        BuiltinFile {
            role: "reviewer",
            body: include_str!("../../../configs/providers/claude/agents/reviewer.toml"),
        },
        BuiltinFile {
            role: "scout",
            body: include_str!("../../../configs/providers/claude/agents/scout.toml"),
        },
    ],
};

const GROK: Builtin = Builtin {
    id: "grok-default",
    aliases: &["grok", "supergrok"],
    files: &[
        BuiltinFile {
            role: "root",
            body: include_str!("../../../configs/providers/grok/agents/root-agent.toml"),
        },
        BuiltinFile {
            role: "worker",
            body: include_str!("../../../configs/providers/grok/agents/worker.toml"),
        },
        BuiltinFile {
            role: "reviewer",
            body: include_str!("../../../configs/providers/grok/agents/reviewer.toml"),
        },
        BuiltinFile {
            role: "scout",
            body: include_str!("../../../configs/providers/grok/agents/scout.toml"),
        },
    ],
};

const BUILTINS: &[Builtin] = &[CODEX, CLAUDE, GROK];

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
    for builtin in BUILTINS {
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
    prepared.set_key("root.agent", &resolved.root.0)?;
    prepared.set_key("roster.working_set", &cards.join(","))?;
    Ok(Prepared {
        id: resolved.id,
        config: prepared,
        root: resolved.root.1,
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
    if let Some(builtin) = BUILTINS
        .iter()
        .find(|builtin| builtin.id == name || builtin.aliases.contains(&name))
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
        if BUILTINS.iter().any(|builtin| builtin.id == stem) {
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
    let builtin = BUILTINS.iter().find(|builtin| {
        root == format!("{}-root", builtin.id)
            && cards
                == ["worker", "reviewer", "scout"]
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
mod tests {
    use super::{list, prepare};
    use crate::home::paths::PlakPaths;
    use crate::product_config::ProductConfig;
    use std::fs;

    fn home() -> (tempfile::TempDir, PlakPaths) {
        let (root, paths) = PlakPaths::isolated_test_home();
        fs::create_dir_all(paths.home().join("profiles")).unwrap();
        fs::create_dir_all(paths.user_agents_dir()).unwrap();
        (root, paths)
    }

    fn ids(config: &ProductConfig, paths: &PlakPaths) -> Vec<String> {
        list(config, paths.home())
            .rows
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    #[test]
    fn builtins_are_listed_without_a_profiles_directory() {
        let (_root, paths) = PlakPaths::isolated_test_home();
        let view = list(&ProductConfig::default_product(), paths.home());
        assert_eq!(
            view.rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["codex-default", "claude-default", "grok-default"]
        );
        assert!(view.diagnostics.is_empty());
        assert!(
            view.rows[2]
                .cards
                .iter()
                .any(|card| card.label == "scout" && card.model == "grok-4.7-build-fast")
        );
    }

    #[test]
    fn a_reserved_or_broken_user_file_is_reported_and_hides_nothing() {
        let (_root, paths) = home();
        let dir = paths.home().join("profiles");
        fs::write(dir.join("codex-default.toml"), "root = \"x\"\n").unwrap();
        fs::write(dir.join("broken.toml"), "root = \"x\"\nworking_set = [1]\n").unwrap();
        fs::write(
            dir.join("missing.toml"),
            "root = \"nope\"\nworking_set = [\"nope\"]\n",
        )
        .unwrap();
        fs::write(
            dir.join("extra.toml"),
            "root = \"x\"\nworking_set = [\"x\"]\nmodel = \"y\"\n",
        )
        .unwrap();
        let config = ProductConfig::load(&paths).unwrap();
        let view = list(&config, paths.home());
        assert_eq!(view.rows.len(), 3, "built-ins stay listed");
        assert_eq!(view.diagnostics.len(), 4, "{:?}", view.diagnostics);
        assert!(
            view.diagnostics
                .iter()
                .any(|line| line.contains("codex-default.toml") && line.contains("built-in"))
        );
        assert!(
            view.diagnostics
                .iter()
                .any(|line| line.contains("missing.toml") && line.contains("agents/nope.toml"))
        );
        assert!(prepare(&config, paths.home(), "missing").is_err());
    }

    #[test]
    fn a_user_profile_owns_an_alias_stem_in_the_list_and_in_prepare() {
        let (_root, paths) = home();
        fs::write(
            paths.user_agents_dir().join("mine.toml"),
            "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\ndescription = \"d\"\n",
        )
        .unwrap();
        fs::write(
            paths.home().join("profiles/codex.toml"),
            "root = \"mine\"\nworking_set = [\"mine\"]\n",
        )
        .unwrap();
        let config = ProductConfig::load(&paths).unwrap();
        let view = list(&config, paths.home());
        let builtin = view
            .rows
            .iter()
            .find(|row| row.id == "codex-default")
            .unwrap();
        assert_eq!(builtin.aliases, ["chatgpt"], "the user file owns `codex`");
        assert_eq!(
            ids(&config, &paths).last().map(String::as_str),
            Some("codex")
        );
        assert_eq!(prepare(&config, paths.home(), "codex").unwrap().id, "codex");
        assert_eq!(
            prepare(&config, paths.home(), "chatgpt").unwrap().id,
            "codex-default"
        );
    }

    /// A working-set card's own `spawns` are listed under it, indented one
    /// level per hop, each template once.
    #[test]
    fn a_user_profile_lists_nested_children_under_their_parent() {
        let (_root, paths) = home();
        for (name, extra) in [
            (
                "lead",
                "spawns = [\"helper\", \"lead\"]\nmay_adopt = true\n",
            ),
            ("helper", "spawns = [\"leaf\"]\n"),
            ("leaf", ""),
            ("solo", ""),
        ] {
            fs::write(
                paths.user_agents_dir().join(format!("{name}.toml")),
                format!(
                    "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\ndescription = \"{name}\"\n{extra}"
                ),
            )
            .unwrap();
        }
        fs::write(
            paths.home().join("profiles/nest.toml"),
            "root = \"solo\"\nworking_set = [\"lead\", \"solo\"]\n",
        )
        .unwrap();
        let config = ProductConfig::load(&paths).unwrap();
        let view = list(&config, paths.home());
        let row = view.rows.iter().find(|row| row.id == "nest").unwrap();
        assert_eq!(
            row.cards
                .iter()
                .map(|card| (card.label.as_str(), card.depth))
                .collect::<Vec<_>>(),
            [("lead", 0), ("helper", 1), ("leaf", 2), ("solo", 0)]
        );
    }

    /// A working-set card that names another working-set member shows that
    /// name once under the parent, in config order, and does not walk it.
    #[test]
    fn a_working_set_sibling_is_a_reference_under_its_parent() {
        let (_root, paths) = home();
        for (name, extra) in [
            ("zeta", "spawns = [\"alpha\", \"zeta\"]\n"),
            ("alpha", "spawns = [\"note\"]\n"),
            ("note", ""),
            ("rootish", ""),
        ] {
            fs::write(
                paths.user_agents_dir().join(format!("{name}.toml")),
                format!(
                    "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\ndescription = \"{name}\"\n{extra}"
                ),
            )
            .unwrap();
        }
        fs::write(
            paths.home().join("profiles/nest.toml"),
            "root = \"rootish\"\nworking_set = [\"zeta\", \"alpha\"]\n",
        )
        .unwrap();
        let config = ProductConfig::load(&paths).unwrap();
        let view = list(&config, paths.home());
        let row = view.rows.iter().find(|row| row.id == "nest").unwrap();
        assert_eq!(
            row.cards
                .iter()
                .map(|card| (card.label.as_str(), card.depth))
                .collect::<Vec<_>>(),
            [("zeta", 0), ("alpha", 1), ("alpha", 0), ("note", 1)]
        );
    }

    #[test]
    fn prepare_writes_nothing_and_keeps_a_template_the_user_edited() {
        let (_root, paths) = home();
        let edited = "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\neffort = \"low\"\ndescription = \"my worker\"\n";
        fs::write(
            paths.user_agents_dir().join("codex-default-worker.toml"),
            edited,
        )
        .unwrap();
        let config = ProductConfig::load(&paths).unwrap();
        let prepared = prepare(&config, paths.home(), "codex").unwrap();
        assert_eq!(prepared.id, "codex-default");
        assert_eq!(
            prepared.config.root_agent.as_deref(),
            Some("codex-default-root")
        );
        assert!(
            !paths
                .user_agents_dir()
                .join("codex-default-root.toml")
                .exists()
        );
        assert_eq!(
            prepared
                .config
                .templates
                .get("codex-default-worker")
                .map(|t| t.model.as_str()),
            Some("gpt-5.6-luna")
        );
        prepared.config.save(&paths).unwrap();
        assert!(
            paths
                .user_agents_dir()
                .join("codex-default-root.toml")
                .is_file()
        );
        assert_eq!(
            fs::read_to_string(paths.user_agents_dir().join("codex-default-worker.toml")).unwrap(),
            edited
        );
        let reloaded = ProductConfig::load(&paths).unwrap();
        assert_eq!(
            list(&reloaded, paths.home()).current.as_deref(),
            Some("codex-default")
        );
    }
}
