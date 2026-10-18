//! `plak mcp add|list|show|test|remove|enable|disable`: MCP server setup
//! without hand editing. Tables are written through
//! [`crate::product_config::mcp_edit`], so a command and a hand-written file
//! pass the same parser; `test` starts the server through the same spawn,
//! environment and secret path a session bind uses.

use std::io;

use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};

use crate::home::paths::PlakPaths;
use crate::home::secrets;
use crate::product_config::mcp_edit::{self, TemplateMcpEdit, TemplateReach};
use crate::product_config::{ConfigWriteScope, McpServerConfig, McpTransport, ProductConfig};

/// One parsed `plak mcp` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum McpCommand {
    Add {
        id: String,
        scope: ConfigWriteScope,
        server: McpServerConfig,
        replace: bool,
    },
    List,
    Show {
        id: String,
    },
    Test {
        id: String,
    },
    Remove {
        id: String,
        scope: Option<ConfigWriteScope>,
        force: bool,
    },
    Enable {
        id: String,
        template: String,
        tools: Vec<String>,
    },
    Disable {
        id: String,
        template: String,
    },
}

const NEW_SESSIONS: &str = "new sessions pick this up; existing sessions keep the tools they froze";

pub(crate) fn run(
    paths: &PlakPaths,
    command: McpCommand,
    stdout: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    let mut out = String::new();
    match command {
        McpCommand::Add {
            id,
            scope,
            server,
            replace,
        } => {
            let file = mcp_edit::add_server(paths, scope, &id, &server, replace)
                .map_err(CliFailure::new)?;
            out.push_str(&format!("added [mcp_servers.{id}] to {}\n", file.display()));
            for name in missing_secrets(paths, &server)? {
                out.push_str(&format!(
                    "secret {name} is not set; run `plak secret set {name}`\n"
                ));
            }
            out.push_str(&format!(
                "next: `plak mcp test {id}`, then `plak mcp enable {id} --template <name>`\n"
            ));
        }
        McpCommand::List => out = list(paths)?,
        McpCommand::Show { id } => out = show(paths, &id)?,
        McpCommand::Test { id } => out = test(paths, &id)?,
        McpCommand::Remove { id, scope, force } => {
            let removed =
                mcp_edit::remove_server(paths, scope, &id, force).map_err(CliFailure::new)?;
            for template in &removed.disabled {
                out.push_str(&format!("removed mcp:{id} from template {template}\n"));
            }
            out.push_str(&format!(
                "removed [mcp_servers.{id}] from {}\n",
                removed.file.display()
            ));
        }
        McpCommand::Enable {
            id,
            template,
            tools,
        } => {
            let edit = if tools.is_empty() {
                TemplateMcpEdit::EnableAll
            } else {
                TemplateMcpEdit::EnableTools(tools)
            };
            let (file, list) = mcp_edit::edit_template_tools(paths, &template, &id, &edit)
                .map_err(CliFailure::new)?;
            out.push_str(&template_edited(&file, &list));
        }
        McpCommand::Disable { id, template } => {
            let (file, list) =
                mcp_edit::edit_template_tools(paths, &template, &id, &TemplateMcpEdit::Disable)
                    .map_err(CliFailure::new)?;
            out.push_str(&template_edited(&file, &list));
        }
    }
    stdout
        .write_all(out.as_bytes())
        .map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

fn template_edited(file: &std::path::Path, list: &[String]) -> String {
    let tools = if list.is_empty() {
        "[]".to_owned()
    } else {
        list.join(", ")
    };
    format!("{}: tools = {tools}\n{NEW_SESSIONS}\n", file.display())
}

fn load(paths: &PlakPaths) -> CliResult<ProductConfig> {
    ProductConfig::load(paths).map_err(CliFailure::new)
}

fn server<'a>(config: &'a ProductConfig, id: &str) -> CliResult<&'a McpServerConfig> {
    config.tool_sections.mcp_servers.get(id).ok_or_else(|| {
        CliFailure::new(format!(
            "no MCP server {id}; `plak mcp list` shows the configured servers"
        ))
    })
}

fn missing_secrets(paths: &PlakPaths, server: &McpServerConfig) -> CliResult<Vec<String>> {
    let stored = secrets::list_named_secrets(paths).map_err(CliFailure::new)?;
    Ok(mcp_edit::secret_names(server)
        .into_iter()
        .filter(|name| !stored.contains(name))
        .collect())
}

fn transport_line(server: &McpServerConfig) -> (&'static str, String) {
    match &server.transport {
        McpTransport::Stdio { command, args, .. } => {
            let mut argv = vec![command.clone()];
            argv.extend(args.iter().cloned());
            ("stdio", argv.join(" "))
        }
        McpTransport::Http { url, .. } => ("http", url.clone()),
    }
}

fn reach_text(reaching: &[TemplateReach]) -> String {
    reaching
        .iter()
        .map(|reach| match &reach.tools {
            None => format!("{} (all)", reach.template),
            Some(tools) => format!("{} ({})", reach.template, tools.join(", ")),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The configured servers: one block each with transport, scope, the
/// templates that enable it and the secrets it still needs.
pub(crate) fn overview_lines(paths: &PlakPaths, config: &ProductConfig) -> CliResult<Vec<String>> {
    let mut lines = Vec::new();
    for (id, server) in &config.tool_sections.mcp_servers {
        let (transport, target) = transport_line(server);
        let scope = mcp_edit::server_scope(paths, config, id).map_err(CliFailure::new)?;
        lines.push(format!("{id}  {transport}  {}  {target}", scope.as_str()));
        let reaching = mcp_edit::templates_reaching(config, id);
        lines.push(if reaching.is_empty() {
            "  templates: none".to_owned()
        } else {
            format!("  templates: {}", reach_text(&reaching))
        });
        let missing = missing_secrets(paths, server)?;
        if !missing.is_empty() {
            lines.push(format!(
                "  missing secrets: {} (plak secret set <name>)",
                missing.join(", ")
            ));
        }
    }
    Ok(lines)
}

const NONE_CONFIGURED: &str = "no MCP servers configured; add one with `plak mcp add <id> -- <command> [args...]` or `plak mcp add <id> --url <url>`";

fn list(paths: &PlakPaths) -> CliResult<String> {
    let config = load(paths)?;
    let lines = overview_lines(paths, &config)?;
    if lines.is_empty() {
        return Ok(format!("{NONE_CONFIGURED}\n"));
    }
    Ok(lines.iter().map(|line| format!("{line}\n")).collect())
}

fn show(paths: &PlakPaths, id: &str) -> CliResult<String> {
    let config = load(paths)?;
    let definition = server(&config, id)?;
    let scope = mcp_edit::server_scope(paths, &config, id).map_err(CliFailure::new)?;
    let file = mcp_edit::config_file_for(paths, scope).map_err(CliFailure::new)?;
    let mut out = config.tool_sections.render_server(id).unwrap_or_default();
    out.push_str(&format!(
        "\nscope: {} ({})\n",
        scope.as_str(),
        file.display()
    ));
    let reaching = mcp_edit::templates_reaching(&config, id);
    out.push_str(&format!(
        "templates: {}\n",
        if reaching.is_empty() {
            "none".to_owned()
        } else {
            reach_text(&reaching)
        }
    ));
    let missing = missing_secrets(paths, definition)?;
    for name in mcp_edit::secret_names(definition) {
        let state = if missing.contains(&name) {
            format!("not set; run `plak secret set {name}`")
        } else {
            "set".to_owned()
        };
        out.push_str(&format!("secret {name}: {state}\n"));
    }
    Ok(out)
}

fn test(paths: &PlakPaths, id: &str) -> CliResult<String> {
    let config = load(paths)?;
    server(&config, id)?;
    let rendered = config.tool_sections.render_server(id).unwrap_or_default();
    let workspace =
        std::env::current_dir().map_err(|_| CliFailure::new("working directory is unavailable"))?;
    let probe = plakous_hosted::mcp_binding::probe_mcp_server(
        id,
        &rendered,
        &workspace,
        secrets::ProductNamedSecrets::shared(paths),
    )
    .map_err(|error| CliFailure::new(format!("mcp server {id} failed: {error}")))?;
    let mut out = format!(
        "mcp server {id}: {} tool{} a session would bind as mcp:{id}\n",
        probe.tools.len(),
        if probe.tools.len() == 1 { "" } else { "s" }
    );
    for (name, exposed) in &probe.tools {
        out.push_str(&format!("  {name}  ({exposed})\n"));
    }
    if !(probe.unlowerable.is_empty() && probe.not_allowed.is_empty()) {
        out.push_str("left out:\n");
        for line in &probe.unlowerable {
            out.push_str(&format!("  {line}\n"));
        }
        for name in &probe.not_allowed {
            out.push_str(&format!("  {name}: not in [mcp_servers.{id}] tools\n"));
        }
    }
    for name in &probe.not_listed {
        out.push_str(&format!(
            "warning: [mcp_servers.{id}] tools names {name}, which the server does not list\n"
        ));
    }
    Ok(out)
}

/// The TUI's read-only `/mcp` view: the configured servers, which of them
/// the current session bound and with how many tools, what its bind left
/// out, and the CLI commands that change any of it.
pub(crate) fn session_overview(
    paths: &PlakPaths,
    session: Option<&plakous_hosted::runtime::SurfaceSession>,
) -> String {
    let mut lines = Vec::new();
    let empty = plakous_hosted::runtime::SurfaceMcp::default();
    let mcp = session.map_or(&empty, |session| &session.mcp);
    let bound = |id: &str| mcp.tools.iter().filter(|(server, _)| server == id).count();
    match ProductConfig::load(paths) {
        Err(error) => lines.push(format!("config.toml could not be read: {error}")),
        Ok(config) => match overview_lines(paths, &config) {
            Err(error) => lines.push(error.to_string()),
            Ok(overview) if overview.is_empty() => lines.push(NONE_CONFIGURED.to_owned()),
            Ok(overview) => {
                lines.push("configured servers:".to_owned());
                let mut ids = config.tool_sections.mcp_servers.keys();
                for line in overview {
                    let starts_server = !line.starts_with(' ');
                    lines.push(format!("  {line}"));
                    if starts_server && let Some(id) = ids.next() {
                        lines.push(format!("    {}", session_state(mcp, id, bound(id))));
                    }
                }
            }
        },
    }
    let configured = config_ids(paths);
    for id in mcp.servers.iter().filter(|id| !configured.contains(*id)) {
        lines.push(format!(
            "  {id}: bound by this session, no longer configured; {}",
            session_state(mcp, id, bound(id))
        ));
    }
    if !mcp.omitted.is_empty() {
        lines.push(String::new());
        lines.push("left out when this session bound:".to_owned());
        lines.extend(mcp.omitted.iter().map(|line| format!("  {line}")));
    }
    lines.push(String::new());
    lines.push("change it from a shell (new sessions pick it up):".to_owned());
    lines.extend(PANEL_COMMANDS.iter().map(|command| format!("  {command}")));
    lines.join("\n")
}

fn config_ids(paths: &PlakPaths) -> Vec<String> {
    ProductConfig::load(paths)
        .map(|config| config.tool_sections.mcp_servers.into_keys().collect())
        .unwrap_or_default()
}

/// The commands the `/mcp` panel names; a test walks each through the parser.
const PANEL_COMMANDS: &[&str] = &[
    "plak mcp add <id> -- <command> [args...]",
    "plak mcp add <id> --url <url>",
    "plak mcp test <id>",
    "plak mcp enable <id> --template <name>",
];

fn session_state(mcp: &plakous_hosted::runtime::SurfaceMcp, id: &str, tools: usize) -> String {
    if !mcp.servers.iter().any(|server| server == id) {
        return "this session: not bound".to_owned();
    }
    format!(
        "this session: bound, {tools} tool{}",
        if tools == 1 { "" } else { "s" }
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_panel_command_is_a_plak_command() {
        for command in super::PANEL_COMMANDS {
            let concrete = command
                .replace("<id>", "docs")
                .replace("<command> [args...]", "srv --stdio")
                .replace("<url>", "https://x.test/mcp")
                .replace("<name>", "worker");
            let words = concrete
                .split_whitespace()
                .skip(1)
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>();
            assert!(
                matches!(
                    crate::cli::parse(words),
                    Ok(crate::command::Command::Mcp(_))
                ),
                "{command} does not parse as a plak mcp command"
            );
        }
    }
}
