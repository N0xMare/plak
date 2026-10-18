//! `plak mcp`: the `[mcp_servers.<id>]` tables of one config file and the
//! `tools` array of one template file, edited in place so every other line,
//! comment and key order survives. Each edit is validated with the parsers a
//! load uses against both layers it would leave: the home alone (what
//! `load_user` reads) and the home with the trusted project over it. Only the
//! home's templates and the current trusted project's templates are checked;
//! another project's templates are not visible from here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use plakous_hosted::tool_plan::SelectedTool;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

use super::tool_sections::{DEFAULT_MCP_CALL_SECONDS, DEFAULT_MCP_STARTUP_SECONDS};
use super::{
    AgentTemplate, ConfigValueOrigin, ConfigWriteScope, McpServerConfig, McpTransport,
    ProductConfig, parse_template,
};
use crate::home::paths::PlakPaths;

/// What a template's `tools` gains or loses for one server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TemplateMcpEdit {
    /// Add `mcp:<id>` (every allowed tool), replacing any single-tool entries.
    EnableAll,
    /// Add `mcp:<id>/<tool>` for each tool.
    EnableTools(Vec<String>),
    /// Remove `mcp:<id>` and every `mcp:<id>/<tool>`.
    Disable,
}

/// One template whose `tools` reaches a server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateReach {
    /// Template name.
    pub template: String,
    /// `None` for `mcp:<id>`, else the single tools listed.
    pub tools: Option<Vec<String>>,
}

/// The directory a template file lives in.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Layer {
    /// `~/.plak/agents`.
    Home,
    /// The trusted project's `.plak/agents`.
    Project,
}

/// One template file.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TemplateFile {
    pub layer: Layer,
    pub name: String,
}

impl TemplateFile {
    fn path(&self, paths: &PlakPaths) -> Result<PathBuf, String> {
        let directory = match self.layer {
            Layer::Home => paths.user_agents_dir(),
            Layer::Project => paths.project_agents_dir().ok_or(NO_PROJECT)?,
        };
        Ok(directory.join(format!("{}.toml", self.name)))
    }

    /// `name`, or `name (project)` for a project file.
    #[must_use]
    pub fn label(&self) -> String {
        match self.layer {
            Layer::Home => self.name.clone(),
            Layer::Project => format!("{} (project)", self.name),
        }
    }
}

const NO_PROJECT: &str = "no project .plak was discovered from the current directory; create .plak/config.toml (it may be empty) and run `plak config trust`";

/// The config file `scope` writes.
pub fn config_file_for(paths: &PlakPaths, scope: ConfigWriteScope) -> Result<PathBuf, String> {
    match scope {
        ConfigWriteScope::User => Ok(paths.config_file()),
        ConfigWriteScope::Project => {
            let config = ProductConfig::load(paths)?;
            match (&config.project, paths.project_config_file()) {
                (Some(project), Some(file)) if project.trusted => Ok(file),
                (Some(project), Some(_)) => Err(format!(
                    "the project .plak in {} is not trusted; run `plak config trust` before writing it",
                    project.root.display()
                )),
                _ => Err(NO_PROJECT.to_owned()),
            }
        }
    }
}

/// The server tables one config file carries, or none when it is absent.
fn file_servers(path: &Path) -> Result<BTreeMap<String, McpServerConfig>, String> {
    Ok(ProductConfig::load_config_file(path)?
        .tool_sections
        .mcp_servers)
}

/// Which file decides each effective server: the trusted project's when it
/// carries the id, the home's otherwise.
pub fn server_scope(
    paths: &PlakPaths,
    config: &ProductConfig,
    id: &str,
) -> Result<ConfigWriteScope, String> {
    if config
        .project
        .as_ref()
        .is_some_and(|project| project.trusted)
        && let Some(file) = paths.project_config_file()
        && file_servers(&file)?.contains_key(id)
    {
        return Ok(ConfigWriteScope::Project);
    }
    Ok(ConfigWriteScope::User)
}

/// Every template whose `tools` reaches `id`, in name order.
#[must_use]
pub fn templates_reaching(config: &ProductConfig, id: &str) -> Vec<TemplateReach> {
    let mut out = Vec::new();
    for (name, template) in &config.templates {
        let mut all = false;
        let mut tools = Vec::new();
        for entry in template.tools.entries() {
            match entry {
                SelectedTool::McpServer(server) if server.as_str() == id => all = true,
                SelectedTool::McpTool { server, tool } if server.as_str() == id => {
                    tools.push(tool.clone());
                }
                _ => {}
            }
        }
        if all || !tools.is_empty() {
            out.push(TemplateReach {
                template: name.clone(),
                tools: (!all).then_some(tools),
            });
        }
    }
    out
}

/// Every secret name a server definition reads at start.
#[must_use]
pub fn secret_names(server: &McpServerConfig) -> Vec<String> {
    let mut names = match &server.transport {
        McpTransport::Stdio { secret_env, .. } => secret_env.values().cloned().collect(),
        McpTransport::Http {
            bearer_secret,
            secret_headers,
            ..
        } => bearer_secret
            .iter()
            .chain(secret_headers.values())
            .cloned()
            .collect::<Vec<_>>(),
    };
    names.sort();
    names.dedup();
    names
}

/// Both configuration layers as they are on disk.
struct Layers {
    /// The home alone, as `load_user` reads it.
    home: ProductConfig,
    /// The home with the trusted project over it, as `load` reads it.
    merged: ProductConfig,
    /// The trusted project's config file and its servers.
    project: Option<(PathBuf, BTreeMap<String, McpServerConfig>)>,
}

/// What an edit would change, checked before any file is written.
#[derive(Default)]
struct Pending {
    home_servers: Option<BTreeMap<String, McpServerConfig>>,
    project_servers: Option<BTreeMap<String, McpServerConfig>>,
    templates: Vec<(TemplateFile, AgentTemplate)>,
}

impl Layers {
    fn read(paths: &PlakPaths) -> Result<Self, String> {
        let merged = ProductConfig::load(paths)?;
        let home = ProductConfig::load_user(paths)?;
        let project = match paths.project_config_file() {
            Some(file)
                if merged
                    .project
                    .as_ref()
                    .is_some_and(|project| project.trusted) =>
            {
                let servers = file_servers(&file)?;
                Some((file, servers))
            }
            _ => None,
        };
        Ok(Self {
            home,
            merged,
            project,
        })
    }

    fn home_servers(&self) -> &BTreeMap<String, McpServerConfig> {
        &self.home.tool_sections.mcp_servers
    }

    fn project_servers(&self) -> Option<&BTreeMap<String, McpServerConfig>> {
        self.project.as_ref().map(|(_, servers)| servers)
    }

    fn layer_of(&self, name: &str) -> Layer {
        match self.merged.template_origin(name) {
            ConfigValueOrigin::Project => Layer::Project,
            ConfigValueOrigin::User | ConfigValueOrigin::Default => Layer::Home,
        }
    }

    /// The two would-be configurations: the home alone, and the home with
    /// the project over it.
    fn apply(&self, pending: &Pending) -> (ProductConfig, ProductConfig) {
        let home_servers = pending
            .home_servers
            .as_ref()
            .unwrap_or_else(|| self.home_servers());
        let mut home = self.home.clone();
        home.tool_sections.mcp_servers = home_servers.clone();
        let mut merged = self.merged.clone();
        let mut servers = home_servers.clone();
        if let Some(project) = pending
            .project_servers
            .as_ref()
            .or_else(|| self.project_servers())
        {
            servers.extend(project.clone());
        }
        merged.tool_sections.mcp_servers = servers;
        for (file, template) in &pending.templates {
            if file.layer == Layer::Home {
                home.templates.insert(file.name.clone(), template.clone());
            }
            if file.layer == Layer::Project || self.layer_of(&file.name) == Layer::Home {
                merged.templates.insert(file.name.clone(), template.clone());
            }
        }
        (home, merged)
    }

    /// Both layers must load after `pending`.
    fn check(&self, paths: &PlakPaths, pending: &Pending) -> Result<(), String> {
        let (home, merged) = self.apply(pending);
        home.validate().map_err(|error| {
            format!(
                "the home configuration ({}) would not load: {error}",
                paths.home().display()
            )
        })?;
        merged.validate().map_err(|error| match &self.project {
            Some((file, _)) => format!(
                "with the project configuration ({}) over the home it would not load: {error}",
                file.display()
            ),
            None => error,
        })
    }

    /// The template files that list `id` in a way `pending` leaves invalid,
    /// in either layer.
    fn broken_by(&self, pending: &Pending, id: &str) -> BTreeSet<TemplateFile> {
        let (home, merged) = self.apply(pending);
        let mut out = BTreeSet::new();
        for (name, template) in &home.templates {
            if breaks(template, &home, id) {
                out.insert(TemplateFile {
                    layer: Layer::Home,
                    name: name.clone(),
                });
            }
        }
        for (name, template) in &merged.templates {
            if breaks(template, &merged, id) {
                out.insert(TemplateFile {
                    layer: self.layer_of(name),
                    name: name.clone(),
                });
            }
        }
        out
    }
}

/// Whether `template` lists `id` and `config` no longer serves that entry.
fn breaks(template: &AgentTemplate, config: &ProductConfig, id: &str) -> bool {
    let server = config.tool_sections.mcp_servers.get(id);
    template.tools.entries().iter().any(|entry| match entry {
        SelectedTool::McpServer(listed) => listed.as_str() == id && server.is_none(),
        SelectedTool::McpTool {
            server: listed,
            tool,
        } => {
            listed.as_str() == id
                && server.is_none_or(|server| {
                    server
                        .tools
                        .as_ref()
                        .is_some_and(|allowed| !allowed.contains(tool))
                })
        }
        SelectedTool::Optional(_) => false,
    })
}

/// What adding a server wrote, and which other file defines the same id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Added {
    /// The config file written.
    pub file: PathBuf,
    /// The trusted project's file that overrides this home table here.
    pub overridden_by: Option<PathBuf>,
    /// The home file whose table this project table overrides here.
    pub overrides: Option<PathBuf>,
}

/// Writes `[mcp_servers.<id>]` into `scope`'s config file. An existing table
/// of that id is refused unless `replace`, which keeps its place and
/// surrounding comments.
pub fn add_server(
    paths: &PlakPaths,
    scope: ConfigWriteScope,
    id: &str,
    server: &McpServerConfig,
    replace: bool,
) -> Result<Added, String> {
    let layers = Layers::read(paths)?;
    let path = config_file_for(paths, scope)?;
    let existing = read_optional(&path)?;
    let mut document = parse_document(existing.as_deref(), &path)?;
    let servers = servers_table(&mut document, &path)?;
    let mut table = server_table(server);
    match servers.get(id).and_then(Item::as_table) {
        Some(_) if !replace => {
            return Err(format!(
                "[mcp_servers.{id}] already exists in {}; pass --replace to overwrite it",
                path.display()
            ));
        }
        Some(old) => {
            *table.decor_mut() = old.decor().clone();
            table.set_position(old.position());
        }
        None if existing.as_deref().is_some_and(|text| !text.is_empty()) => {
            table.decor_mut().set_prefix("\n");
        }
        None => {}
    }
    servers.insert(id, Item::Table(table));
    let text = document.to_string();
    let written = ProductConfig::parse(&text)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .tool_sections
        .mcp_servers;
    let pending = match scope {
        ConfigWriteScope::User => Pending {
            home_servers: Some(written),
            ..Pending::default()
        },
        ConfigWriteScope::Project => Pending {
            project_servers: Some(written),
            ..Pending::default()
        },
    };
    layers.check(paths, &pending)?;
    if scope == ConfigWriteScope::User {
        paths.ensure_layout()?;
    }
    publish(&path, &text)?;
    let project_has = layers
        .project
        .as_ref()
        .filter(|(_, servers)| servers.contains_key(id))
        .map(|(file, _)| file.clone());
    Ok(match scope {
        ConfigWriteScope::User => Added {
            file: path,
            overridden_by: project_has,
            overrides: None,
        },
        ConfigWriteScope::Project => Added {
            file: path,
            overridden_by: None,
            overrides: layers
                .home_servers()
                .contains_key(id)
                .then(|| paths.config_file()),
        },
    })
}

/// What removing a server did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Removed {
    /// The config file the table left.
    pub file: PathBuf,
    /// Templates whose `tools` lost their entries for the server.
    pub disabled: Vec<TemplateFile>,
}

/// Removes `[mcp_servers.<id>]` from the file that defines it (or from
/// `scope`'s). Template files that would be left listing what neither layer
/// serves refuse the removal unless `force`, which removes their entries for
/// the server; every file is checked together before any is written, and the
/// templates are written before the config.
pub fn remove_server(
    paths: &PlakPaths,
    scope: Option<ConfigWriteScope>,
    id: &str,
    force: bool,
) -> Result<Removed, String> {
    let layers = Layers::read(paths)?;
    let user_has = layers.home_servers().contains_key(id);
    let project_has = layers
        .project_servers()
        .is_some_and(|servers| servers.contains_key(id));
    let scope = match (scope, user_has, project_has) {
        (Some(scope), _, _) => scope,
        (None, true, true) => {
            return Err(format!(
                "[mcp_servers.{id}] is defined in both the home and the project config; pass --scope user or --scope project"
            ));
        }
        (None, _, true) => ConfigWriteScope::Project,
        (None, _, false) => ConfigWriteScope::User,
    };
    let path = config_file_for(paths, scope)?;
    let present = match scope {
        ConfigWriteScope::User => user_has,
        ConfigWriteScope::Project => project_has,
    };
    if !present {
        return Err(format!(
            "{} has no [mcp_servers.{id}]; `plak mcp list` shows where each server is defined",
            path.display()
        ));
    }
    let text = read_optional(&path)?.unwrap_or_default();
    let mut document = parse_document(Some(&text), &path)?;
    remove_table(&mut document, id, &path)?;
    let text = document.to_string();
    let remaining = ProductConfig::parse(&text)
        .map_err(|error| format!("{}: {error}", path.display()))?
        .tool_sections
        .mcp_servers;
    let mut pending = match scope {
        ConfigWriteScope::User => Pending {
            home_servers: Some(remaining),
            ..Pending::default()
        },
        ConfigWriteScope::Project => Pending {
            project_servers: Some(remaining),
            ..Pending::default()
        },
    };
    let broken = layers.broken_by(&pending, id);
    if !broken.is_empty() && !force {
        let names = broken
            .iter()
            .map(TemplateFile::label)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "templates would be left listing mcp:{id}: {names}; run `plak mcp disable {id} --template <name>` for each, or pass --force to remove those entries too"
        ));
    }
    let mut writes = Vec::new();
    for file in &broken {
        let template_path = file.path(paths)?;
        let (edited, template) =
            edited_template(&template_path, &file.name, id, &TemplateMcpEdit::Disable)?;
        pending.templates.push((file.clone(), template));
        writes.push((template_path, edited));
    }
    layers.check(paths, &pending)?;
    for (template_path, edited) in &writes {
        publish(template_path, edited)?;
    }
    publish(&path, &text)?;
    Ok(Removed {
        file: path,
        disabled: broken.into_iter().collect(),
    })
}

/// Rewrites the `tools` array of the template file `name` resolves to, and
/// nothing else in it. Returns the file and the resulting list.
pub fn edit_template_tools(
    paths: &PlakPaths,
    name: &str,
    id: &str,
    edit: &TemplateMcpEdit,
) -> Result<(PathBuf, Vec<String>), String> {
    let layers = Layers::read(paths)?;
    if !layers.merged.templates.contains_key(name) {
        return Err(format!(
            "no template {name}; templates are files under ~/.plak/agents or .plak/agents"
        ));
    }
    let file = TemplateFile {
        layer: layers.layer_of(name),
        name: name.to_owned(),
    };
    if file.layer == Layer::Home
        && *edit != TemplateMcpEdit::Disable
        && !layers.home_servers().contains_key(id)
        && let Some((project_file, servers)) = &layers.project
        && servers.contains_key(id)
    {
        return Err(format!(
            "mcp:{id} is defined only in {}; a home template cannot list it; add it with `plak mcp add {id} --scope user ...` or enable it on a project template",
            project_file.display()
        ));
    }
    let path = file.path(paths)?;
    let (text, template) = edited_template(&path, name, id, edit)?;
    let list = template
        .tools
        .entries()
        .iter()
        .map(ToString::to_string)
        .collect();
    layers.check(
        paths,
        &Pending {
            templates: vec![(file, template)],
            ..Pending::default()
        },
    )?;
    publish(&path, &text)?;
    Ok((path, list))
}

/// The template file at `path` with its `tools` edited, and what it parses to.
fn edited_template(
    path: &Path,
    name: &str,
    id: &str,
    edit: &TemplateMcpEdit,
) -> Result<(String, AgentTemplate), String> {
    let text = fs::read_to_string(path)
        .map_err(|_| format!("could not read template {}", path.display()))?;
    let mut document = parse_document(Some(&text), path)?;
    let root = document.as_table_mut();
    if !root.contains_key("tools") {
        root.insert("tools", Item::Value(Value::Array(Array::new())));
    }
    let tools = root
        .get_mut("tools")
        .and_then(Item::as_array_mut)
        .ok_or_else(|| format!("{}: tools must be an array of strings", path.display()))?;
    let whole = format!("mcp:{id}");
    let prefix = format!("mcp:{id}/");
    let listed =
        |tools: &Array, entry: &str| tools.iter().any(|value| value.as_str() == Some(entry));
    match edit {
        TemplateMcpEdit::EnableAll => {
            if listed(tools, &whole) {
                return Err(format!("template {name} already lists {whole}"));
            }
            remove_entries(tools, |entry| entry.starts_with(&prefix));
            push_entry(tools, &whole);
        }
        TemplateMcpEdit::EnableTools(names) => {
            if listed(tools, &whole) {
                return Err(format!(
                    "template {name} already lists {whole}, which includes every allowed tool; run `plak mcp disable {id} --template {name}` first to list single tools"
                ));
            }
            let mut added = false;
            for tool in names {
                let entry = format!("{prefix}{tool}");
                if !listed(tools, &entry) {
                    push_entry(tools, &entry);
                    added = true;
                }
            }
            if !added {
                return Err(format!(
                    "template {name} already lists those tools of mcp:{id}"
                ));
            }
        }
        TemplateMcpEdit::Disable => {
            if remove_entries(tools, |entry| entry == whole || entry.starts_with(&prefix)) == 0 {
                return Err(format!("template {name} does not list mcp:{id}"));
            }
        }
    }
    let text = document.to_string();
    let template =
        parse_template(&text, name).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok((text, template))
}

/// Removes every string entry `doomed` matches. toml_edit keeps the comment
/// after an element's comma as the *next* element's prefix, so the first
/// removed element of a run hands its prefix (the comment of the line before
/// it) to the next survivor, or to the array's trailing text when it was last.
fn remove_entries(tools: &mut Array, doomed: impl Fn(&str) -> bool) -> usize {
    let mut carried: Option<String> = None;
    let mut removed = 0;
    let mut index = 0;
    while index < tools.len() {
        let Some(value) = tools.get(index) else {
            break;
        };
        if value.as_str().is_some_and(&doomed) {
            let prefix = decor_prefix(value);
            carried.get_or_insert(prefix);
            tools.remove(index);
            removed += 1;
            continue;
        }
        if let Some(prefix) = carried.take()
            && let Some(value) = tools.get_mut(index)
        {
            value.decor_mut().set_prefix(prefix);
        }
        index += 1;
    }
    if let Some(prefix) = carried {
        let trailing = tools.trailing().as_str().unwrap_or_default().to_owned();
        let head = prefix.rfind('\n').map_or("", |end| &prefix[..=end]);
        let tail = trailing
            .rfind('\n')
            .map_or(trailing.as_str(), |end| &trailing[end + 1..]);
        if !head.is_empty() {
            tools.set_trailing(format!("{head}{tail}"));
        }
    }
    removed
}

/// Appends `entry`; in a one-element-per-line array the new element takes
/// its own line, indented as the last one, after the last line's comment.
fn push_entry(tools: &mut Array, entry: &str) {
    let indent = tools
        .iter()
        .last()
        .map(decor_prefix)
        .and_then(|prefix| prefix.rfind('\n').map(|end| prefix[end + 1..].to_owned()));
    tools.push(entry);
    let Some(indent) = indent else {
        return;
    };
    let trailing = tools.trailing().as_str().unwrap_or_default().to_owned();
    let (head, tail) = match trailing.rfind('\n') {
        Some(end) => (trailing[..=end].to_owned(), trailing[end + 1..].to_owned()),
        None => ("\n".to_owned(), trailing),
    };
    if let Some(value) = tools.get_mut(tools.len() - 1) {
        value.decor_mut().set_prefix(format!("{head}{indent}"));
    }
    tools.set_trailing(format!("\n{tail}"));
}

fn decor_prefix(value: &Value) -> String {
    value
        .decor()
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Removes `[mcp_servers.<id>]`, handing its header's prefix (the blank
/// lines and comments above it) to the table that follows it in the file,
/// or to the end of the document.
fn remove_table(document: &mut DocumentMut, id: &str, path: &Path) -> Result<(), String> {
    let servers = servers_table(document, path)?;
    let Some(Item::Table(removed)) = servers.remove(id) else {
        return Err(format!(
            "{}: [mcp_servers.{id}] is not a table",
            path.display()
        ));
    };
    if servers.is_empty() {
        document.as_table_mut().remove("mcp_servers");
    }
    let prefix = removed
        .decor()
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .unwrap_or_default()
        .to_owned();
    if prefix.trim().is_empty() {
        return Ok(());
    }
    let after = removed.position().unwrap_or(isize::MAX);
    if let Some(next) = next_table(document.as_table_mut(), after) {
        let own = next
            .decor()
            .prefix()
            .and_then(|own| own.as_str())
            .unwrap_or_default()
            .trim_start_matches('\n')
            .to_owned();
        next.decor_mut().set_prefix(format!("{prefix}{own}"));
    } else {
        let trailing = document.trailing().as_str().unwrap_or_default().to_owned();
        document.set_trailing(format!("{prefix}{trailing}"));
    }
    Ok(())
}

/// The table whose header comes first after document position `after`.
fn next_table(table: &mut Table, after: isize) -> Option<&mut Table> {
    let mut best: Option<isize> = None;
    collect_positions(table, &mut |position| {
        if position > after && best.is_none_or(|best| position < best) {
            best = Some(position);
        }
    });
    find_position(table, best?)
}

fn collect_positions(table: &Table, visit: &mut dyn FnMut(isize)) {
    for (_, item) in table {
        if let Item::Table(child) = item {
            if let Some(position) = child.position() {
                visit(position);
            }
            collect_positions(child, visit);
        }
    }
}

fn find_position(table: &mut Table, position: isize) -> Option<&mut Table> {
    for (_, item) in table.iter_mut() {
        if let Item::Table(child) = item {
            if child.position() == Some(position) {
                return Some(child);
            }
            if let Some(found) = find_position(child, position) {
                return Some(found);
            }
        }
    }
    None
}

/// The table `add` writes: the keys given, and no default spelled out.
fn server_table(server: &McpServerConfig) -> Table {
    let mut table = Table::new();
    match &server.transport {
        McpTransport::Stdio {
            command,
            args,
            env,
            secret_env,
        } => {
            table.insert("transport", toml_edit::value("stdio"));
            table.insert("command", toml_edit::value(command.as_str()));
            if !args.is_empty() {
                table.insert("args", toml_edit::value(string_array(args)));
            }
            if !env.is_empty() {
                table.insert("env", toml_edit::value(inline_table(env)));
            }
            if !secret_env.is_empty() {
                table.insert("secret_env", toml_edit::value(inline_table(secret_env)));
            }
        }
        McpTransport::Http {
            url,
            bearer_secret,
            headers,
            secret_headers,
        } => {
            table.insert("transport", toml_edit::value("http"));
            table.insert("url", toml_edit::value(url.as_str()));
            if let Some(secret) = bearer_secret {
                table.insert("bearer_secret", toml_edit::value(secret.as_str()));
            }
            if !headers.is_empty() {
                table.insert("headers", toml_edit::value(inline_table(headers)));
            }
            if !secret_headers.is_empty() {
                table.insert(
                    "secret_headers",
                    toml_edit::value(inline_table(secret_headers)),
                );
            }
        }
    }
    if let Some(tools) = &server.tools {
        table.insert("tools", toml_edit::value(string_array(tools)));
    }
    if server.startup_seconds != DEFAULT_MCP_STARTUP_SECONDS {
        table.insert(
            "startup_seconds",
            toml_edit::value(i64::from(server.startup_seconds)),
        );
    }
    if server.call_seconds != DEFAULT_MCP_CALL_SECONDS {
        table.insert(
            "call_seconds",
            toml_edit::value(i64::from(server.call_seconds)),
        );
    }
    table
}

fn string_array(values: &[String]) -> Array {
    values.iter().map(String::as_str).collect()
}

fn inline_table(values: &BTreeMap<String, String>) -> InlineTable {
    let mut table = InlineTable::new();
    for (key, value) in values {
        table.insert(key, Value::from(value.as_str()));
    }
    table
}

/// The `[mcp_servers]` parent, created implicit so only `[mcp_servers.<id>]`
/// headers print.
fn servers_table<'a>(document: &'a mut DocumentMut, path: &Path) -> Result<&'a mut Table, String> {
    let root = document.as_table_mut();
    if !root.contains_key("mcp_servers") {
        let mut table = Table::new();
        table.set_implicit(true);
        root.insert("mcp_servers", Item::Table(table));
    }
    root.get_mut("mcp_servers")
        .and_then(Item::as_table_mut)
        .ok_or_else(|| {
            format!(
                "{}: mcp_servers is not a table of [mcp_servers.<id>] sections",
                path.display()
            )
        })
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("could not read {}: {error}", path.display())),
    }
}

fn parse_document(text: Option<&str>, path: &Path) -> Result<DocumentMut, String> {
    text.unwrap_or_default()
        .parse()
        .map_err(|error| format!("could not read {} as TOML: {error}", path.display()))
}

/// Writes `text` over `path` through a synced temporary file and a rename.
/// A symlinked config is written at its target, and an existing file keeps
/// its permissions.
fn publish(path: &Path, text: &str) -> Result<(), String> {
    let target = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(path)
            .map_err(|error| format!("could not resolve {}: {error}", path.display()))?,
        _ => path.to_path_buf(),
    };
    let permissions = fs::metadata(&target)
        .ok()
        .map(|metadata| metadata.permissions());
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
    let temporary = parent.join(format!(".publication.{}.tmp", uuid::Uuid::new_v4()));
    let written = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &target)
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(format!("could not write {}: {error}", target.display()));
    }
    OpenOptions::new()
        .read(true)
        .open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            format!(
                "{} was written, but its directory could not be synced: {error}",
                target.display()
            )
        })
}

#[cfg(test)]
mod tests;
