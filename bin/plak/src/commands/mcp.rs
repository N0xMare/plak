//! `plak mcp add|list|show|test|remove|enable|disable`: MCP server setup
//! without hand editing. Tables are written through
//! [`crate::product_config::mcp_edit`], so a command and a hand-written file
//! pass the same parser; `test` starts the server through the same spawn,
//! environment and secret path a session bind uses.

use std::io;

use plakous::mcp::McpOAuthStatus;
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
            let added = mcp_edit::add_server(paths, scope, &id, &server, replace)
                .map_err(CliFailure::new)?;
            writeln!(
                stdout,
                "added [mcp_servers.{id}] to {}",
                added.file.display()
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
            if let Some(project) = &added.overridden_by {
                out.push_str(&format!(
                    "note: {} also defines [mcp_servers.{id}]; in this project that table overrides the one just written\n",
                    project.display()
                ));
            }
            if let Some(home) = &added.overrides {
                out.push_str(&format!(
                    "note: in this project this table overrides [mcp_servers.{id}] in {}\n",
                    home.display()
                ));
            }
            match missing_secrets(paths, &server) {
                Ok(missing) => {
                    for name in missing {
                        out.push_str(&format!(
                            "secret {name} is not set; run `plak secret set {name}`\n"
                        ));
                    }
                }
                Err(error) => out.push_str(&format!(
                    "warning: stored secrets could not be listed ({error}); `plak mcp show {id}` checks them again\n"
                )),
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
                out.push_str(&format!(
                    "removed mcp:{id} from template {}\n",
                    template.label()
                ));
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
        lines.extend(server_lines(paths, config, id, server)?);
    }
    Ok(lines)
}

/// One server's block: a header line, then indented facts.
fn server_lines(
    paths: &PlakPaths,
    config: &ProductConfig,
    id: &str,
    server: &McpServerConfig,
) -> CliResult<Vec<String>> {
    let (transport, target) = transport_line(server);
    let scope = mcp_edit::server_scope(paths, config, id).map_err(CliFailure::new)?;
    let mut lines = vec![format!("{id}  {transport}  {}  {target}", scope.as_str())];
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
    if server.resources {
        lines.push("  resources: enabled".to_owned());
    }
    if let Some(login) = login_line(paths, config, id) {
        lines.push(format!("  {login}"));
    }
    Ok(lines)
}

/// An OAuth server's stored login, read without contacting the server;
/// `None` for a server without `auth = "oauth"`.
fn login_line(paths: &PlakPaths, config: &ProductConfig, id: &str) -> Option<String> {
    let rendered = config.tool_sections.render_server(id)?;
    let secrets = secrets::ProductNamedSecrets::shared(paths);
    let status = plakous_hosted::mcp_binding::mcp_login_status(id, &rendered, secrets.as_ref());
    let login = format!("run `plak mcp login {id}`");
    let text = match status {
        Ok(None) => return None,
        Err(error) => format!("unreadable ({}); {login}", shown(&error)),
        Ok(Some(McpOAuthStatus::LoggedIn {
            expires_in,
            refreshable,
            scope,
        })) => {
            let mut text = match expires_in {
                Some(left) if left.is_zero() => "logged in, access token expired".to_owned(),
                Some(left) => format!(
                    "logged in, access token expires in {}",
                    lifetime(left.as_secs())
                ),
                None => "logged in".to_owned(),
            };
            if refreshable {
                text.push_str(", refreshable");
            }
            if let Some(scope) = scope {
                text.push_str(&format!(", scope \"{}\"", shown(&scope)));
            }
            text
        }
        Ok(Some(McpOAuthStatus::NotLoggedIn)) => format!("not logged in; {login}"),
        Ok(Some(McpOAuthStatus::Refreshing)) => "being refreshed by another process".to_owned(),
        Ok(Some(McpOAuthStatus::Quarantined)) => {
            format!("quarantined: a refresh outcome is unknown; {login}")
        }
        Ok(Some(McpOAuthStatus::OtherServer)) => {
            format!("stored for another URL, issuer or client; {login}")
        }
        Ok(Some(McpOAuthStatus::WrittenByNewerBuild)) => {
            format!(
                "written by a newer build; use that build, or `plak mcp logout {id} --local` first"
            )
        }
        Ok(Some(_)) => format!("cannot be used; {login}"),
    };
    Some(format!("login: {text}"))
}

/// A token lifetime in the largest whole unit.
fn lifetime(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds} s"),
        60..7200 => format!("{} min", seconds / 60),
        _ => format!("{} h", seconds / 3600),
    }
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
    if definition.resources {
        out.push_str("resources: enabled\n");
    }
    if let Some(login) = login_line(paths, &config, id) {
        out.push_str(&format!("{login}\n"));
    }
    Ok(out)
}

/// Text a server chose (tool names, refusal notices), made safe to print.
fn shown(text: &str) -> String {
    crate::cli_output::safe_terminal_text(text, 512)
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
    .map_err(|error| CliFailure::new(format!("mcp server {id} failed: {}", shown(&error))))?;
    let mut out = format!(
        "mcp server {id}: {} tool{} a session would bind as mcp:{id}\n",
        probe.tools.len(),
        if probe.tools.len() == 1 { "" } else { "s" }
    );
    for (name, exposed) in &probe.tools {
        out.push_str(&format!("  {}  ({})\n", shown(name), shown(exposed)));
    }
    if !(probe.unlowerable.is_empty() && probe.not_allowed.is_empty()) {
        out.push_str("left out:\n");
        for line in &probe.unlowerable {
            out.push_str(&format!("  {}\n", shown(line)));
        }
        for name in &probe.not_allowed {
            out.push_str(&format!(
                "  {}: not in [mcp_servers.{id}] tools\n",
                shown(name)
            ));
        }
    }
    for name in &probe.not_listed {
        out.push_str(&format!(
            "warning: [mcp_servers.{id}] tools names {}, which the server does not list\n",
            shown(name)
        ));
    }
    if let Some((resources, templates)) = probe.resources {
        out.push_str(&format!(
            "resources: {resources} listed, {templates} resource template{}\n",
            if templates == 1 { "" } else { "s" }
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
    let empty = plakous_hosted::runtime::SurfaceMcp::default();
    let mcp = session
        .and_then(|session| session.mcp.as_ref())
        .unwrap_or(&empty);
    let mut lines = Vec::new();
    let config = match ProductConfig::load(paths) {
        Ok(config) => Some(config),
        Err(error) => {
            lines.push(format!("config.toml could not be read: {error}"));
            None
        }
    };
    if let Some(config) = &config {
        if config.tool_sections.mcp_servers.is_empty() {
            lines.push(NONE_CONFIGURED.to_owned());
        } else {
            lines.push("configured servers:".to_owned());
        }
        for (id, server) in &config.tool_sections.mcp_servers {
            match server_lines(paths, config, id, server) {
                Ok(block) => lines.extend(block.into_iter().map(|line| format!("  {line}"))),
                Err(error) => lines.push(format!("  {id}: {error}")),
            }
            lines.push(format!("    {}", session_state(mcp, config, id)));
        }
    }
    for id in mcp.servers.iter().filter(|id| {
        config
            .as_ref()
            .is_none_or(|config| !config.tool_sections.mcp_servers.contains_key(*id))
    }) {
        lines.push(format!(
            "  {id}: no longer configured; this session: bound, {}",
            tool_count(mcp, id)
        ));
    }
    if !mcp.omitted.is_empty() {
        lines.push(String::new());
        lines.push("left out when this session bound (as its transcript still shows):".to_owned());
        lines.extend(mcp.omitted.iter().map(|line| format!("  {}", shown(line))));
    }
    lines.push(String::new());
    lines.push("change it from a shell (new sessions pick it up):".to_owned());
    lines.extend(PANEL_COMMANDS.iter().map(|command| format!("  {command}")));
    lines.join("\n")
}

fn tool_count(mcp: &plakous_hosted::runtime::SurfaceMcp, id: &str) -> String {
    let tools = mcp.tools.iter().filter(|(server, _)| server == id).count();
    format!("{tools} tool{}", if tools == 1 { "" } else { "s" })
}

fn session_state(
    mcp: &plakous_hosted::runtime::SurfaceMcp,
    config: &ProductConfig,
    id: &str,
) -> String {
    if !mcp.servers.iter().any(|server| server == id) {
        return "this session: not bound".to_owned();
    }
    let count = tool_count(mcp, id);
    if mcp.definitions.get(id) != config.tool_sections.render_server(id).as_ref() {
        return format!(
            "this session: bound with an earlier definition, {count}; new sessions get the current one"
        );
    }
    format!("this session: bound, {count}")
}

/// The commands the `/mcp` panel names; a test walks each through the parser.
const PANEL_COMMANDS: &[&str] = &[
    "plak mcp add <id> -- <command> [args...]",
    "plak mcp add <id> --url <url>",
    "plak mcp test <id>",
    "plak mcp enable <id> --template <name>",
];

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
