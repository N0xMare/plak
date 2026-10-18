//! `plak mcp`: the `[mcp_servers.<id>]` tables of one config file and the
//! `tools` array of one template file, edited in place so every other line,
//! comment and key order survives. Each edit is validated with the parsers a
//! load uses, against the effective configuration it would produce, before
//! anything is published.

use std::fs;
use std::path::{Path, PathBuf};

use plakous_hosted::tool_plan::SelectedTool;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value};

use super::tool_sections::{DEFAULT_MCP_CALL_SECONDS, DEFAULT_MCP_STARTUP_SECONDS};
use super::{
    ConfigValueOrigin, ConfigWriteScope, McpServerConfig, McpTransport, ProductConfig, atomic_file,
    parse_template,
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

/// The config file `scope` writes.
pub fn config_file_for(paths: &PlakPaths, scope: ConfigWriteScope) -> Result<PathBuf, String> {
    match scope {
        ConfigWriteScope::User => Ok(paths.config_file()),
        ConfigWriteScope::Project => {
            let config = ProductConfig::load(paths)?;
            match (&config.project, paths.project_config_file()) {
                (Some(project), Some(file)) if project.trusted => Ok(file),
                (Some(_), Some(_)) => Err(
                    "the project .plak is not trusted; run `plak config trust` before writing it"
                        .to_owned(),
                ),
                _ => Err(
                    "no project .plak was discovered from the current directory; create .plak/ first"
                        .to_owned(),
                ),
            }
        }
    }
}

/// The server tables one config file carries, or none when it is absent.
fn file_servers(
    path: &Path,
) -> Result<std::collections::BTreeMap<String, McpServerConfig>, String> {
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

/// Writes `[mcp_servers.<id>]` into `scope`'s config file. An existing table
/// of that id is refused unless `replace`. Returns the file written.
pub fn add_server(
    paths: &PlakPaths,
    scope: ConfigWriteScope,
    id: &str,
    server: &McpServerConfig,
    replace: bool,
) -> Result<PathBuf, String> {
    let mut effective = ProductConfig::load(paths)?;
    let path = config_file_for(paths, scope)?;
    let existing = read_optional(&path)?;
    let mut document = parse_document(existing.as_deref(), &path)?;
    let servers = servers_table(&mut document, &path)?;
    if servers.contains_key(id) && !replace {
        return Err(format!(
            "[mcp_servers.{id}] already exists in {}; pass --replace to overwrite it",
            path.display()
        ));
    }
    let mut table = server_table(server);
    if !servers.contains_key(id) && existing.as_deref().is_some_and(|text| !text.is_empty()) {
        table.decor_mut().set_prefix("\n");
    }
    servers.insert(id, Item::Table(table));
    let text = document.to_string();
    let parsed =
        ProductConfig::parse(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let written = parsed
        .tool_sections
        .mcp_servers
        .get(id)
        .cloned()
        .ok_or_else(|| format!("{}: [mcp_servers.{id}] did not read back", path.display()))?;
    if scope == ConfigWriteScope::Project
        || server_scope(paths, &effective, id)? == ConfigWriteScope::User
    {
        effective
            .tool_sections
            .mcp_servers
            .insert(id.to_owned(), written);
    }
    effective.validate()?;
    if scope == ConfigWriteScope::User {
        paths.ensure_layout()?;
    }
    publish(&path, &text)?;
    Ok(path)
}

/// What removing a server did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Removed {
    /// The config file the table left.
    pub file: PathBuf,
    /// Templates whose `tools` lost their entries for the server.
    pub disabled: Vec<String>,
}

/// Removes `[mcp_servers.<id>]` from the file that defines it (or from
/// `scope`'s). Templates that would be left naming a missing server refuse
/// the removal unless `force`, which first removes those entries from them.
pub fn remove_server(
    paths: &PlakPaths,
    scope: Option<ConfigWriteScope>,
    id: &str,
    force: bool,
) -> Result<Removed, String> {
    let config = ProductConfig::load(paths)?;
    let user_has = file_servers(&paths.config_file())?.contains_key(id);
    let project_has = config
        .project
        .as_ref()
        .is_some_and(|project| project.trusted)
        && match paths.project_config_file() {
            Some(file) => file_servers(&file)?.contains_key(id),
            None => false,
        };
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
    let survives = match scope {
        ConfigWriteScope::User => project_has,
        ConfigWriteScope::Project => user_has,
    };
    let reaching = if survives {
        Vec::new()
    } else {
        templates_reaching(&config, id)
    };
    if !reaching.is_empty() && !force {
        let names = reaching
            .iter()
            .map(|reach| reach.template.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "templates still list mcp:{id}: {names}; run `plak mcp disable {id} --template <name>` for each, or pass --force to remove those entries too"
        ));
    }
    let text = read_optional(&path)?.unwrap_or_default();
    let mut document = parse_document(Some(&text), &path)?;
    let servers = servers_table(&mut document, &path)?;
    servers.remove(id);
    if servers.is_empty() {
        document.as_table_mut().remove("mcp_servers");
    }
    let text = document.to_string();
    ProductConfig::parse(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut disabled = Vec::new();
    for reach in &reaching {
        edit_template_tools(paths, &reach.template, id, &TemplateMcpEdit::Disable)?;
        disabled.push(reach.template.clone());
    }
    publish(&path, &text)?;
    Ok(Removed {
        file: path,
        disabled,
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
    let mut config = ProductConfig::load(paths)?;
    if !config.templates.contains_key(name) {
        return Err(format!(
            "no template {name}; templates are files under ~/.plak/agents or .plak/agents"
        ));
    }
    let directory = match config.template_origin(name) {
        ConfigValueOrigin::Project => paths
            .project_agents_dir()
            .ok_or_else(|| format!("template {name} has no project directory"))?,
        ConfigValueOrigin::User | ConfigValueOrigin::Default => paths.user_agents_dir(),
    };
    let path = directory.join(format!("{name}.toml"));
    let text = fs::read_to_string(&path)
        .map_err(|_| format!("could not read template {}", path.display()))?;
    let mut document = parse_document(Some(&text), &path)?;
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
            tools.retain(|value| {
                !value
                    .as_str()
                    .is_some_and(|entry| entry.starts_with(&prefix))
            });
            tools.push(whole.as_str());
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
                    tools.push(entry.as_str());
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
            let before = tools.len();
            tools.retain(|value| {
                !value
                    .as_str()
                    .is_some_and(|entry| entry == whole || entry.starts_with(&prefix))
            });
            if tools.len() == before {
                return Err(format!("template {name} does not list mcp:{id}"));
            }
        }
    }
    let text = document.to_string();
    let template =
        parse_template(&text, name).map_err(|error| format!("{}: {error}", path.display()))?;
    let list = template
        .tools
        .entries()
        .iter()
        .map(ToString::to_string)
        .collect();
    config.templates.insert(name.to_owned(), template);
    config.validate()?;
    publish(&path, &text)?;
    Ok((path, list))
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

fn inline_table(values: &std::collections::BTreeMap<String, String>) -> InlineTable {
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

fn publish(path: &Path, text: &str) -> Result<(), String> {
    atomic_file(path, text.as_bytes(), false, &mut |_, _| Ok(()))
        .map_err(super::ConfigSaveError::into_message)
}

#[cfg(test)]
#[path = "mcp_edit_tests.rs"]
mod tests;
