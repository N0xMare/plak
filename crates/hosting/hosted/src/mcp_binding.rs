//! Binding the MCP servers a node's template names: definitions from the
//! frozen canonical `[mcp_servers.<id>]` tables, secret values through the
//! product's named-secret port, and one process-wide server pool.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use plakous::mcp::{
    HeaderName, HeaderValue, McpError, McpErrorKind, McpHttpEndpoint, McpHttpSecret,
    McpHttpSecretResolver, McpOAuthSecretResolver, McpOAuthSettings, McpOAuthStatus, McpPool,
    McpSecretResolver, McpServerSpec, McpToolPack, McpToolRecord, McpToolSelection,
    SensitiveBytes, ServiceSecretValue, exposed_tool_name,
};
use plakous::model::ToolName;
use plakous::tools::{
    EnvironmentVariableName, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentScope, McpServerName, ToolPack, ToolRegistration, WorkspacePolicy,
};
use serde::Deserialize;

use crate::tool_plan::{SelectedTool, TemplateToolSelection};

/// The MCP name of the resources tool a `resources = true` table adds.
const READ_RESOURCE: &str = "read_resource";

/// The product's named secrets (`plak secret set <name>`).
///
/// Hosting asks for a value by name at each server spawn and never reads the
/// secret store itself.
pub trait NamedSecrets: Send + Sync + 'static {
    /// The named secret's value, or `None` when it is not stored.
    ///
    /// # Errors
    ///
    /// Returns a one-line reason when the store refuses the read.
    fn read(&self, name: &str) -> Result<Option<NamedSecretValue>, String>;

    /// Where the OAuth login of MCP server `server` is stored: a private
    /// credential record in a directory of its own. `None`, the default, is
    /// a product that stores no MCP logins, where `auth = "oauth"` refuses.
    fn mcp_login_file(&self, server: &str) -> Option<std::path::PathBuf> {
        let _ = server;
        None
    }
}

/// One named secret's value on its way to a server: consumed whole into a
/// spawn's environment or a request header, never printed.
pub struct NamedSecretValue(String);

impl NamedSecretValue {
    /// Takes ownership of a value read from the store.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }

    fn into_environment(self) -> Result<ServiceSecretValue, String> {
        ServiceSecretValue::new(self.0)
            .map_err(|_| "it cannot be passed to a server process".to_owned())
    }

    fn into_header(self) -> SensitiveBytes {
        SensitiveBytes::new(self.0.into_bytes())
    }
}

impl std::fmt::Debug for NamedSecretValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NamedSecretValue(<redacted>)")
    }
}

/// A product with no named-secret store: every secret is unset.
pub(crate) struct NoNamedSecrets;

impl NamedSecrets for NoNamedSecrets {
    fn read(&self, _name: &str) -> Result<Option<NamedSecretValue>, String> {
        Ok(None)
    }
}

/// The node's MCP tools, frozen at bind.
pub(crate) struct BoundMcpTools {
    /// `(server, exposed name)` per selected tool.
    pub(crate) names: Vec<(McpServerName, ToolName)>,
    /// One serial-mutating registration per selected tool.
    pub(crate) registrations: Vec<ToolRegistration>,
    /// One line per tool a wildcard selection omitted.
    pub(crate) notices: Vec<String>,
    /// The frozen records, for a session record to keep.
    pub(crate) records: Vec<McpToolRecord>,
}

/// The process's MCP pool and the executor its servers run on. Sessions bind
/// and call from executors that may end before a server should; the pool's
/// own outlives them all.
fn pool() -> Result<(&'static McpPool, &'static tokio::runtime::Runtime), String> {
    static POOL: OnceLock<Result<(McpPool, tokio::runtime::Runtime), String>> = OnceLock::new();
    let pool = POOL.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("plak-mcp")
            .enable_all()
            .build()
            .map_err(|_| "mcp server executor could not be started".to_owned())?;
        Ok((McpPool::new().running_on(runtime.handle().clone()), runtime))
    });
    pool.as_ref()
        .map(|(pool, runtime)| (pool, runtime))
        .map_err(Clone::clone)
}

/// Starts or leases every server `selection` names in `workspace_root` and
/// freezes the selected tools, blocking the calling thread (bounded by each
/// server's startup budget) while the pool's executor does the work.
///
/// `tool_sections` holds the canonical tables the node's template reaches,
/// keyed `mcp_servers.<id>`. With `frozen` records the tools are rebuilt
/// from them after the lease, without a new listing; a tool whose live
/// definition drifted fails its first call, not the bind.
pub(crate) fn bind_mcp_tools(
    selection: &TemplateToolSelection,
    tool_sections: &BTreeMap<String, String>,
    workspace_root: &std::path::Path,
    secrets: Arc<dyn NamedSecrets>,
    frozen: Option<&[McpToolRecord]>,
) -> Result<BoundMcpTools, String> {
    if selection.mcp_servers().next().is_none() {
        return Ok(BoundMcpTools {
            names: Vec::new(),
            registrations: Vec::new(),
            notices: Vec::new(),
            records: Vec::new(),
        });
    }
    let (pool, runtime) = pool()?;
    let (selection, tool_sections, workspace_root, frozen) = (
        selection.clone(),
        tool_sections.clone(),
        workspace_root.to_path_buf(),
        frozen.map(<[McpToolRecord]>::to_vec),
    );
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    runtime.spawn(async move {
        let bound = bind_on_pool(
            pool,
            &selection,
            &tool_sections,
            &workspace_root,
            secrets,
            frozen.as_deref(),
        )
        .await;
        let _ = sender.send(bound);
    });
    receiver
        .recv()
        .map_err(|_| "mcp tool binding did not finish".to_owned())?
}

async fn bind_on_pool(
    pool: &McpPool,
    selection: &TemplateToolSelection,
    tool_sections: &BTreeMap<String, String>,
    workspace_root: &std::path::Path,
    secrets: Arc<dyn NamedSecrets>,
    frozen: Option<&[McpToolRecord]>,
) -> Result<BoundMcpTools, String> {
    let mut bound = BoundMcpTools {
        names: Vec::new(),
        registrations: Vec::new(),
        notices: Vec::new(),
        records: Vec::new(),
    };
    let servers = selection.mcp_servers().cloned().collect::<Vec<_>>();
    let environment = Arc::new(service_environment(workspace_root)?);
    for server in servers {
        let rendered = tool_sections
            .get(&format!("mcp_servers.{server}"))
            .ok_or_else(|| {
                format!(
                    "mcp server {server} is not configured; remedy: add [mcp_servers.{server}] \
                     to config.toml or remove mcp:{server} from the template's tools"
                )
            })?;
        let spec = server_spec(&server, rendered, &secrets)?;
        let remedy =
            format!("remedy: fix the server or remove mcp:{server} from the template's tools");
        // A refused login already names the more specific login remedy.
        let refused = |error: McpError, remedy: &str| {
            if error.kind() == McpErrorKind::Authorization {
                error.to_string()
            } else {
                format!("{error}; {remedy}")
            }
        };
        let lease = pool
            .lease(spec, Arc::clone(&environment))
            .await
            .map_err(|error| refused(error, &remedy))?;
        let (records, pack) = if let Some(frozen) = frozen {
            let records = frozen
                .iter()
                .filter(|record| record.server() == &server)
                .cloned()
                .collect::<Vec<_>>();
            let pack = McpToolPack::from_records(lease, records.clone())
                .map_err(|error| refused(error, &remedy))?;
            (records, pack)
        } else {
            let listing = match lease.freeze(&server_selection(selection, &server)).await {
                Ok(listing) => listing,
                Err(error) => {
                    let remedy = if lease.spec().resources()
                        && !lease.advertises_resources().await.unwrap_or(true)
                    {
                        format!(
                            "remedy: remove resources = true from [mcp_servers.{server}] or fix the server"
                        )
                    } else {
                        remedy
                    };
                    return Err(refused(error, &remedy));
                }
            };
            for notice in listing.notices() {
                tracing::warn!(%server, notice = %notice, "mcp tool omitted at bind");
            }
            bound.notices.extend(listing.notices().iter().cloned());
            (
                listing.records().to_vec(),
                McpToolPack::new(lease, &listing),
            )
        };
        bound.names.extend(
            pack.exposed_names()
                .map_err(|error| refused(error, &remedy))?
                .into_iter()
                .map(|name| (server.clone(), name)),
        );
        bound.records.extend(records);
        bound.registrations.extend(
            pack.registrations()
                .map_err(|error| {
                    format!("mcp server {server} tools could not be registered: {error}")
                })?
                .into_iter()
                .map(ToolRegistration::nested_eligible),
        );
    }
    Ok(bound)
}

/// What one server offers a template that lists it as `mcp:<id>`, as
/// [`probe_mcp_server`] found it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpProbe {
    /// `(mcp name, exposed name)` per tool a session would bind.
    pub tools: Vec<(String, String)>,
    /// One line per allowed tool left out because its input schema cannot be
    /// represented.
    pub unlowerable: Vec<String>,
    /// Listed tools the server's `tools` allowlist leaves out.
    pub not_allowed: Vec<String>,
    /// Allowlist names the server does not list.
    pub not_listed: Vec<String>,
    /// With `resources = true`: how many resources and resource templates
    /// the server lists.
    pub resources: Option<(usize, usize)>,
}

/// Starts or connects to one server exactly as a session bind does (the
/// definition from its canonical `[mcp_servers.<id>]` table, secrets through
/// `secrets`, the stdio environment of `workspace_root`) on a pool of its own,
/// lists its tools, and stops it. Nothing is bound and no model is called.
///
/// # Errors
///
/// Returns the one-line cause a session bind would report: an invalid table,
/// a missing secret, or a start, handshake or listing failure.
pub fn probe_mcp_server(
    server: &str,
    rendered: &str,
    workspace_root: &std::path::Path,
    secrets: Arc<dyn NamedSecrets>,
) -> Result<McpProbe, String> {
    let server = McpServerName::new(server.to_owned())
        .map_err(|_| format!("{server:?} is not a valid MCP server id"))?;
    let spec = server_spec(&server, rendered, &secrets)?;
    let allowlist = crate::frozen_tables::parse::<RenderedTables>(rendered)
        .and_then(|mut tables| tables.mcp_servers.remove(server.as_str()))
        .and_then(|definition| definition.tools);
    let environment = Arc::new(service_environment(workspace_root)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("plak-mcp-probe")
        .enable_all()
        .build()
        .map_err(|_| "mcp server executor could not be started".to_owned())?;
    let pool = McpPool::new().running_on(runtime.handle().clone());
    runtime.block_on(async {
        let probed = async {
            let lease = pool
                .lease(spec, environment)
                .await
                .map_err(|error| error.to_string())?;
            let listing = lease
                .freeze(&McpToolSelection::All)
                .await
                .map_err(|error| error.to_string())?;
            let listed = lease
                .listed_tool_names()
                .await
                .map_err(|error| error.to_string())?;
            let resources = if lease.spec().resources() {
                let resources = lease
                    .list_resources()
                    .await
                    .map_err(|error| error.to_string())?;
                let templates = lease
                    .list_resource_templates()
                    .await
                    .map_err(|error| error.to_string())?;
                Some((resources.len(), templates.len()))
            } else {
                None
            };
            Ok::<_, String>((listing, listed, resources))
        }
        .await;
        pool.shutdown().await;
        let (listing, listed, resources) = probed?;
        let mut tools = listing
            .records()
            .iter()
            .map(|record| {
                (
                    record.mcp_name().to_owned(),
                    record.exposed_name().as_str().to_owned(),
                )
            })
            .collect::<Vec<_>>();
        if resources.is_some() {
            let exposed =
                exposed_tool_name(&server, READ_RESOURCE).map_err(|error| error.to_string())?;
            tools.push((READ_RESOURCE.to_owned(), exposed.as_str().to_owned()));
        }
        let allows = |name: &str| {
            allowlist
                .as_ref()
                .is_none_or(|names| names.iter().any(|allowed| allowed == name))
        };
        Ok(McpProbe {
            tools,
            unlowerable: listing.notices().to_vec(),
            not_allowed: listed
                .iter()
                .filter(|name| !allows(name))
                .cloned()
                .collect(),
            not_listed: allowlist
                .iter()
                .flatten()
                .filter(|name| !listed.contains(name))
                .cloned()
                .collect(),
            resources,
        })
    })
}

/// The stored OAuth login of one server, read without contacting it; `None`
/// for a table without `auth = "oauth"`.
///
/// # Errors
///
/// Returns a one-line reason for an invalid table, a product without MCP
/// logins, or an unreadable store.
pub fn mcp_login_status(
    server: &str,
    rendered: &str,
    secrets: &dyn NamedSecrets,
) -> Result<Option<McpOAuthStatus>, String> {
    let server = McpServerName::new(server.to_owned())
        .map_err(|_| format!("{server:?} is not a valid MCP server id"))?;
    let definition = rendered_server(&server, rendered)?;
    let Some(settings) = oauth_settings(&server, &definition, secrets, None)? else {
        return Ok(None);
    };
    let url = definition
        .url
        .as_deref()
        .ok_or_else(|| invalid_definition(&server))?;
    let endpoint = McpHttpEndpoint::new(url)
        .and_then(|endpoint| endpoint.with_oauth(settings))
        .map_err(|error| format!("mcp server {server}: {error}"))?;
    McpServerSpec::http(server.clone(), endpoint)
        .oauth_status()
        .map_err(|error| format!("mcp server {server}: {error}"))
}

/// The definition `plak mcp login` and `logout` run against: one table with
/// `auth = "oauth"`, whose login need not exist yet.
///
/// # Errors
///
/// Returns a one-line reason for an invalid table, a table without OAuth, a
/// product without MCP logins, or a missing named secret.
pub fn mcp_oauth_spec(
    server: &str,
    rendered: &str,
    secrets: Arc<dyn NamedSecrets>,
) -> Result<McpServerSpec, String> {
    let server = McpServerName::new(server.to_owned())
        .map_err(|_| format!("{server:?} is not a valid MCP server id"))?;
    let definition = rendered_server(&server, rendered)?;
    if definition.auth.as_deref() != Some("oauth") {
        return Err(format!(
            "mcp server {server} does not use oauth; add auth = \"oauth\" to [mcp_servers.{server}]"
        ));
    }
    http_spec(&server, &definition, &secrets)
        .map_err(|error| error.unwrap_or_else(|| invalid_definition(&server)))
}

fn server_selection(selection: &TemplateToolSelection, server: &McpServerName) -> McpToolSelection {
    let mut named = Vec::new();
    for entry in selection.entries() {
        match entry {
            SelectedTool::McpServer(listed) if listed == server => return McpToolSelection::All,
            SelectedTool::McpTool {
                server: listed,
                tool,
            } if listed == server => {
                named.push(tool.clone());
            }
            _ => {}
        }
    }
    McpToolSelection::Named(named)
}

/// The environment a new pool entry for this workspace spawns in. A service
/// outlives the bind that started it, so its scope is a run of its own.
fn service_environment(workspace_root: &std::path::Path) -> Result<ExecutionEnvironment, String> {
    let unavailable = "mcp server environment could not be prepared for this workspace";
    let workspace =
        WorkspacePolicy::host(workspace_root, ".").map_err(|_| unavailable.to_owned())?;
    let run = plakous::model::RunId::try_from(uuid::Uuid::new_v4())
        .map_err(|_| unavailable.to_owned())?;
    let options = crate::process_environment::options(workspace_root)
        .map_err(|error| error.message().to_owned())?;
    ExecutionEnvironment::host_with_policy_and_process_options(
        ExecutionEnvironmentScope::Run(run),
        workspace,
        ExecutionEnvironmentConfig::default(),
        options,
    )
    .map_err(|_| unavailable.to_owned())
}

/// One `[mcp_servers.<id>]` table as `ToolSections` renders it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderedServer {
    transport: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    secret_env: BTreeMap<String, String>,
    url: Option<String>,
    bearer_secret: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    secret_headers: BTreeMap<String, String>,
    auth: Option<String>,
    oauth_issuer: Option<String>,
    oauth_client_id: Option<String>,
    oauth_client_secret: Option<String>,
    oauth_callback_port: Option<u16>,
    tools: Option<Vec<String>>,
    #[serde(default)]
    resources: bool,
    startup_seconds: u64,
    call_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct RenderedTables {
    mcp_servers: BTreeMap<String, RenderedServer>,
}

/// Rebuilds one server definition from its frozen canonical table. Every
/// secret it names must be stored now; values are read again at each spawn.
pub(crate) fn server_spec(
    server: &McpServerName,
    rendered: &str,
    secrets: &Arc<dyn NamedSecrets>,
) -> Result<McpServerSpec, String> {
    let invalid = || invalid_definition(server);
    let definition = rendered_server(server, rendered)?;
    let spec = match definition.transport.as_str() {
        "stdio" => {
            let command = definition.command.clone().ok_or_else(invalid)?;
            let environment = definition
                .env
                .iter()
                .map(|(name, value)| {
                    EnvironmentVariableName::new(name.clone())
                        .map(|name| (name, value.clone()))
                        .map_err(|_| invalid())
                })
                .collect::<Result<Vec<_>, _>>()?;
            let spec = McpServerSpec::stdio(server.clone(), command, definition.args.clone())
                .with_environment(environment);
            if definition.secret_env.is_empty() {
                spec
            } else {
                let names = definition
                    .secret_env
                    .iter()
                    .map(|(variable, secret)| {
                        EnvironmentVariableName::new(variable.clone())
                            .map(|variable| (variable, secret.clone()))
                            .map_err(|_| invalid())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                for (_, secret) in &names {
                    require_secret(server, secret, secrets)?;
                }
                let variables = names
                    .iter()
                    .map(|(variable, _)| variable.clone())
                    .collect::<Vec<_>>();
                spec.with_secret_environment(variables, secret_resolver(server, names, secrets))
            }
        }
        "http" => {
            let spec = http_spec(server, &definition, secrets)
                .map_err(|error| error.unwrap_or_else(invalid))?;
            // No usable login refuses before anything is sent.
            if let Some(status) = spec
                .oauth_status()
                .map_err(|error| format!("mcp server {server}: {error}"))?
                && let Some(reason) = login_refusal(&status)
            {
                return Err(format!(
                    "mcp server {server} {reason}; remedy: plak mcp login {server}"
                ));
            }
            spec
        }
        _ => return Err(invalid()),
    };
    let spec = spec
        .with_resources(definition.resources)
        .with_startup_timeout(Duration::from_secs(definition.startup_seconds))
        .with_call_timeout(Duration::from_secs(definition.call_seconds));
    Ok(match definition.tools {
        Some(tools) => spec.with_allowlist(tools),
        None => spec,
    })
}

fn invalid_definition(server: &McpServerName) -> String {
    format!("mcp server {server} has an invalid frozen definition")
}

/// The one `[mcp_servers.<id>]` table of `rendered`, which carries nothing
/// else.
fn rendered_server(server: &McpServerName, rendered: &str) -> Result<RenderedServer, String> {
    let mut tables = crate::frozen_tables::parse::<RenderedTables>(rendered)
        .ok_or_else(|| invalid_definition(server))?;
    tables
        .mcp_servers
        .remove(server.as_str())
        .filter(|_| tables.mcp_servers.is_empty())
        .ok_or_else(|| invalid_definition(server))
}

/// The OAuth settings of a table with `auth = "oauth"`: its login file,
/// issuer pin, pre-registered client (its secret resolved by `client_secret`
/// at each use) and callback port. Failures name `plak mcp login`.
fn oauth_settings(
    server: &McpServerName,
    definition: &RenderedServer,
    secrets: &dyn NamedSecrets,
    client_secret: Option<McpOAuthSecretResolver>,
) -> Result<Option<McpOAuthSettings>, String> {
    match definition.auth.as_deref() {
        None => return Ok(None),
        Some("oauth") => {}
        Some(_) => return Err(invalid_definition(server)),
    }
    let refused = |error: McpError| format!("mcp server {server}: {error}");
    let store = secrets.mcp_login_file(server.as_str()).ok_or_else(|| {
        format!("mcp server {server} uses oauth, and this product stores no MCP logins")
    })?;
    let login = server.clone();
    let mut settings =
        McpOAuthSettings::new(store).with_login_remedy(Arc::new(move |scope| match scope {
            Some(scope) => format!("remedy: plak mcp login {login} --scope \"{scope}\""),
            None => format!("remedy: plak mcp login {login}"),
        }));
    if let Some(issuer) = &definition.oauth_issuer {
        settings = settings.with_issuer(issuer).map_err(refused)?;
    }
    if let Some(client) = &definition.oauth_client_id {
        settings = settings
            .with_client(client.clone(), client_secret)
            .map_err(refused)?;
    }
    if let Some(port) = definition.oauth_callback_port {
        settings = settings.with_callback_port(port);
    }
    Ok(Some(settings))
}

/// An HTTP definition: its URL, literal headers, and the bearer and header
/// secrets resolved by name at each start, or its OAuth login. `Err(None)`
/// is an invalid table.
fn http_spec(
    server: &McpServerName,
    definition: &RenderedServer,
    secrets: &Arc<dyn NamedSecrets>,
) -> Result<McpServerSpec, Option<String>> {
    let url = definition.url.as_deref().ok_or(None)?;
    let refused = |error: McpError| Some(format!("mcp server {server}: {error}"));
    let headers = definition
        .headers
        .iter()
        .map(|(name, value)| {
            Ok((
                HeaderName::try_from(name.as_str()).map_err(|_| None)?,
                HeaderValue::try_from(value.as_str()).map_err(|_| None)?,
            ))
        })
        .collect::<Result<Vec<_>, Option<String>>>()?;
    let mut slots = Vec::new();
    if let Some(secret) = &definition.bearer_secret {
        slots.push((McpHttpSecret::Bearer, secret.clone()));
    }
    for (name, secret) in &definition.secret_headers {
        let name = HeaderName::try_from(name.as_str()).map_err(|_| None)?;
        slots.push((McpHttpSecret::Header(name), secret.clone()));
    }
    for (_, secret) in &slots {
        require_secret(server, secret, secrets).map_err(Some)?;
    }
    let client_secret = match &definition.oauth_client_secret {
        Some(secret) => {
            require_secret(server, secret, secrets).map_err(Some)?;
            Some(oauth_secret_resolver(server, secret.clone(), secrets))
        }
        None => None,
    };
    let oauth =
        oauth_settings(server, definition, secrets.as_ref(), client_secret).map_err(Some)?;
    let mut endpoint = McpHttpEndpoint::new(url)
        .map_err(refused)?
        .with_headers(headers)
        .map_err(refused)?;
    if let Some(settings) = oauth {
        endpoint = endpoint.with_oauth(settings).map_err(refused)?;
    }
    if !slots.is_empty() {
        endpoint = endpoint
            .with_secret_headers(
                slots
                    .iter()
                    .map(|(slot, _)| slot.clone())
                    .collect::<Vec<_>>(),
                header_resolver(server, slots, secrets),
            )
            .map_err(refused)?;
    }
    Ok(McpServerSpec::http(server.clone(), endpoint))
}

/// Why a stored login cannot serve a bind, `None` when it can (a login
/// another process is refreshing included).
fn login_refusal(status: &McpOAuthStatus) -> Option<&'static str> {
    Some(match status {
        McpOAuthStatus::NotLoggedIn => "is not logged in",
        McpOAuthStatus::Quarantined => "has a login whose renewal outcome is unknown",
        McpOAuthStatus::OtherServer => "has a login made for another URL, issuer or client",
        McpOAuthStatus::WrittenByNewerBuild => "has a login written by a newer build",
        McpOAuthStatus::LoggedIn { .. } | McpOAuthStatus::Refreshing => return None,
        _ => "has a login that cannot be used",
    })
}

fn header_resolver(
    server: &McpServerName,
    slots: Vec<(McpHttpSecret, String)>,
    secrets: &Arc<dyn NamedSecrets>,
) -> McpHttpSecretResolver {
    let server = server.clone();
    let secrets = Arc::clone(secrets);
    Arc::new(move || {
        slots
            .iter()
            .map(|(slot, secret)| {
                read_secret(&server, secret, &secrets)
                    .map(|value| (slot.clone(), value.into_header()))
                    .map_err(Into::into)
            })
            .collect()
    })
}

fn oauth_secret_resolver(
    server: &McpServerName,
    secret: String,
    secrets: &Arc<dyn NamedSecrets>,
) -> McpOAuthSecretResolver {
    let server = server.clone();
    let secrets = Arc::clone(secrets);
    Arc::new(move || {
        read_secret(&server, &secret, &secrets)
            .map(NamedSecretValue::into_header)
            .map_err(Into::into)
    })
}

fn read_secret(
    server: &McpServerName,
    secret: &str,
    secrets: &Arc<dyn NamedSecrets>,
) -> Result<NamedSecretValue, String> {
    match secrets.read(secret) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Err(format!(
            "mcp server {server}: secret {secret:?} is not set; remedy: plak secret set {secret}"
        )),
        Err(reason) => Err(format!("mcp server {server}: {reason}")),
    }
}

fn require_secret(
    server: &McpServerName,
    secret: &str,
    secrets: &Arc<dyn NamedSecrets>,
) -> Result<(), String> {
    read_secret(server, secret, secrets).map(drop)
}

fn secret_resolver(
    server: &McpServerName,
    names: Vec<(EnvironmentVariableName, String)>,
    secrets: &Arc<dyn NamedSecrets>,
) -> McpSecretResolver {
    let server = server.clone();
    let secrets = Arc::clone(secrets);
    Arc::new(move || {
        names
            .iter()
            .map(|(variable, secret)| {
                read_secret(&server, secret, &secrets)
                    .and_then(|value| {
                        value.into_environment().map_err(|reason| {
                            format!("mcp server {server}: secret {secret:?}: {reason}")
                        })
                    })
                    .map(|value| (variable.clone(), value))
                    .map_err(Into::into)
            })
            .collect()
    })
}

#[cfg(test)]
#[path = "mcp_binding/tests.rs"]
mod tests;
