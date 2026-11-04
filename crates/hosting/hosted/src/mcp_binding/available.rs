//! Binding the MCP servers a node's template names: definitions from the
//! frozen canonical `[mcp_servers.<id>]` tables, secret values through the
//! product's named-secret port, and one process-wide server pool.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use plakous::mcp::{
    HeaderName, HeaderValue, McpError, McpErrorKind, McpHttpEndpoint, McpHttpSecret,
    McpHttpSecretResolver, McpLease, McpOAuthSecretResolver, McpOAuthSettings, McpOAuthStatus,
    McpPool, McpPrompt, McpPromptExpansion, McpSecretResolver, McpServerSpec, McpToolPack,
    McpToolRecord, McpToolSelection, exposed_tool_name,
};
use plakous::model::ToolName;
use plakous::tools::runner::{RunnerBackend, process_runner, runner_environment};
use plakous::tools::{
    EnvironmentVariableName, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentScope, McpServerName, ToolPack, ToolRegistration, WorkspacePolicy,
};
use serde::Deserialize;

use crate::frozen_tools::McpPlacement;
use crate::tool_plan::{SelectedTool, TemplateToolSelection};

fn mcp_failure(error: McpError) -> crate::CliFailure {
    let advice = if let Some(login) = error.login_requirement() {
        Some(plakous_host_protocol::refusal::RefusalAdvice::McpLogin {
            server: login.server.clone(),
            scopes: login.scopes.clone(),
        })
    } else {
        error.secret_requirement().map(|secret| {
            plakous_host_protocol::refusal::RefusalAdvice::SecretMissing {
                server: secret.server.clone(),
                name: secret.name.clone(),
            }
        })
    };
    crate::CliFailure::new(error.to_string()).with_optional_advice(advice)
}

fn boxed_secret_error(failure: crate::CliFailure) -> Box<dyn std::error::Error + Send + Sync> {
    if let Some(plakous_host_protocol::refusal::RefusalAdvice::SecretMissing { server, name }) =
        failure.advice()
    {
        return Box::new(plakous::mcp::McpNamedSecretMissing::new(
            server.clone(),
            name.clone(),
            failure.to_string(),
        ));
    }
    Box::new(failure)
}

/// The MCP name of the resources tool a `resources = true` table adds.
const READ_RESOURCE: &str = "read_resource";

use super::{NamedSecretValue, NamedSecrets};

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
    /// Where each bound server runs.
    pub(crate) placements: BTreeMap<McpServerName, McpPlacement>,
}

/// Where one server's process runs: its table's `placement`, and the runner
/// this process runs its sessions on.
#[derive(Clone)]
pub(crate) enum ServerSite {
    /// Beside the agent; secrets from the host's named secrets.
    Host,
    /// `placement = "runner"` with no runner selected: this machine is the
    /// node's runner, so the server runs here with secrets from its runner
    /// shelf, never the host's named secrets.
    LocalRunner,
    /// `placement = "runner"` on the selected runner: the server runs there
    /// and the runner resolves its secrets by name from its own store.
    Runner(Arc<RunnerBackend>),
}

impl ServerSite {
    /// The site of a server placed `placement` when `runner` is the
    /// process's selected runner.
    fn of(placement: Option<McpPlacement>, runner: Option<Arc<RunnerBackend>>) -> Self {
        match (placement, runner) {
            (Some(McpPlacement::Runner), Some(runner)) => Self::Runner(runner),
            (Some(McpPlacement::Runner), None) => Self::LocalRunner,
            (Some(McpPlacement::Host) | None, _) => Self::Host,
        }
    }

    const fn placement(&self) -> McpPlacement {
        match self {
            Self::Host => McpPlacement::Host,
            Self::LocalRunner | Self::Runner(_) => McpPlacement::Runner,
        }
    }
}

/// The runner this process runs its sessions on, if any.
fn selected_runner() -> Option<Arc<RunnerBackend>> {
    process_runner().map(plakous::tools::runner::ProcessRunner::backend)
}

/// The process's MCP pool and the executor its servers run on. Sessions bind
/// and call from executors that may end before a server should; the pool's
/// own outlives them all.
fn pool() -> Result<(&'static McpPool, &'static tokio::runtime::Runtime), crate::CliFailure> {
    static POOL: OnceLock<Result<(McpPool, tokio::runtime::Runtime), crate::CliFailure>> =
        OnceLock::new();
    crate::capabilities::require_feature("mcp").map_err(crate::CliFailure::from)?;
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
) -> Result<BoundMcpTools, crate::CliFailure> {
    if selection.mcp_servers().next().is_none() {
        return Ok(BoundMcpTools::empty());
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
) -> Result<BoundMcpTools, crate::CliFailure> {
    let mut bound = BoundMcpTools::empty();
    let servers = selection.mcp_servers().cloned().collect::<Vec<_>>();
    let runner = selected_runner();
    let (mut host, mut remote) = (None, None);
    for server in servers {
        let rendered = tool_sections
            .get(&format!("mcp_servers.{server}"))
            .ok_or_else(|| {
                format!(
                    "mcp server {server} is not configured; remedy: add [mcp_servers.{server}] \
                     to config.toml or remove mcp:{server} from the template's tools"
                )
            })?;
        let (spec, site) = placed_server_spec(&server, rendered, &secrets, runner.clone())?;
        // One environment per site and bind; the pool keys its entries by
        // the environment's runner and root.
        let slot = if matches!(site, ServerSite::Runner(_)) {
            &mut remote
        } else {
            &mut host
        };
        let environment = match slot {
            Some(environment) => Arc::clone(environment),
            None => Arc::clone(slot.insert(Arc::new(service_environment(workspace_root, &site)?))),
        };
        let remedy =
            format!("remedy: fix the server or remove mcp:{server} from the template's tools");
        // A refused login already names the more specific login remedy.
        let refused = |error: McpError, remedy: &str| {
            if error.kind() == McpErrorKind::Authorization {
                mcp_failure(error)
            } else {
                let failure = mcp_failure(error);
                crate::CliFailure::new(format!("{failure}; {remedy}"))
                    .with_optional_advice(failure.advice().cloned())
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
        bound.add(&server, &pack, records, &|error| refused(error, &remedy))?;
        bound.placements.insert(server, site.placement());
    }
    Ok(bound)
}

impl BoundMcpTools {
    fn empty() -> Self {
        Self {
            names: Vec::new(),
            registrations: Vec::new(),
            notices: Vec::new(),
            records: Vec::new(),
            placements: BTreeMap::new(),
        }
    }

    /// Each frozen record with the placement its server ran under.
    pub(crate) fn placed_records(&self) -> Vec<(McpToolRecord, McpPlacement)> {
        self.records
            .iter()
            .map(|record| {
                let placement = self
                    .placements
                    .get(record.server())
                    .copied()
                    .unwrap_or_default();
                (record.clone(), placement)
            })
            .collect()
    }

    fn add(
        &mut self,
        server: &McpServerName,
        pack: &McpToolPack,
        records: Vec<McpToolRecord>,
        refused: &dyn Fn(McpError) -> crate::CliFailure,
    ) -> Result<(), crate::CliFailure> {
        self.names.extend(
            pack.exposed_names()
                .map_err(refused)?
                .into_iter()
                .map(|name| (server.clone(), name)),
        );
        self.records.extend(records);
        self.registrations.extend(
            pack.registrations()
                .map_err(|error| {
                    format!("mcp server {server} tools could not be registered: {error}")
                })?
                .into_iter()
                .map(ToolRegistration::nested_eligible),
        );
        Ok(())
    }
}

/// The node's MCP tools as an exact-reopen verification compiles them:
/// rebuilt from the session's frozen records with no server behind them.
/// Nothing is started, connected or leased, so no MCP login is renewed; the
/// definitions and binding identities are the ones a leasing reopen builds.
pub(crate) fn recorded_mcp_tools(
    selection: &TemplateToolSelection,
    tool_sections: &BTreeMap<String, String>,
    secrets: &Arc<dyn NamedSecrets>,
    frozen: Option<&[McpToolRecord]>,
) -> Result<BoundMcpTools, crate::CliFailure> {
    let mut bound = BoundMcpTools::empty();
    if selection.mcp_servers().next().is_none() {
        return Ok(bound);
    }
    crate::capabilities::require_feature("mcp").map_err(crate::CliFailure::from)?;
    let frozen = frozen.ok_or_else(|| {
        "the session's mcp tools were never frozen, so a reopen would list its servers again"
            .to_owned()
    })?;
    for server in selection.mcp_servers() {
        let rendered = tool_sections
            .get(&format!("mcp_servers.{server}"))
            .ok_or_else(|| {
                format!(
                    "mcp server {server} is not configured; remedy: add [mcp_servers.{server}] \
                     to config.toml or remove mcp:{server} from the template's tools"
                )
            })?;
        let (spec, site) = placed_server_spec(server, rendered, secrets, selected_runner())?;
        let records = frozen
            .iter()
            .filter(|record| record.server() == server)
            .cloned()
            .collect::<Vec<_>>();
        let refused = |error: McpError| {
            let failure = mcp_failure(error);
            crate::CliFailure::new(format!("mcp server {server}: {failure}"))
                .with_optional_advice(failure.advice().cloned())
        };
        let pack = McpToolPack::unbound(server.clone(), spec.resources(), records.clone())
            .map_err(refused)?;
        bound.add(server, &pack, records, &refused)?;
        bound.placements.insert(server.clone(), site.placement());
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
) -> Result<McpProbe, crate::CliFailure> {
    let server = McpServerName::new(server.to_owned())
        .map_err(|_| format!("{server:?} is not a valid MCP server id"))?;
    let (spec, site) = placed_server_spec(&server, rendered, &secrets, selected_runner())?;
    let allowlist = crate::frozen_tables::parse::<RenderedTables>(rendered)
        .and_then(|mut tables| tables.mcp_servers.remove(server.as_str()))
        .and_then(|definition| definition.tools);
    let (listing, listed, resources) =
        on_probe_lease(spec, &site, workspace_root, async |lease| {
            let listing = lease
                .freeze(&McpToolSelection::All)
                .await
                .map_err(mcp_failure)?;
            let listed = lease.listed_tool_names().await.map_err(mcp_failure)?;
            let resources = if lease.spec().resources() {
                let resources = lease.list_resources().await.map_err(mcp_failure)?;
                let templates = lease.list_resource_templates().await.map_err(mcp_failure)?;
                Some((resources.len(), templates.len()))
            } else {
                None
            };
            Ok((listing, listed, resources))
        })?;
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
        let exposed = exposed_tool_name(&server, READ_RESOURCE).map_err(mcp_failure)?;
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
}

/// The prompts one server lists, from a pool of its own started the way a
/// session bind starts it.
///
/// # Errors
///
/// Returns the one-line cause a session bind would report, or the server's
/// refusal of `prompts/list`.
pub fn list_mcp_prompts(
    server: &str,
    rendered: &str,
    workspace_root: &std::path::Path,
    secrets: Arc<dyn NamedSecrets>,
) -> Result<Vec<McpPrompt>, crate::CliFailure> {
    let server = McpServerName::new(server.to_owned())
        .map_err(|_| format!("{server:?} is not a valid MCP server id"))?;
    let (spec, site) = placed_server_spec(&server, rendered, &secrets, selected_runner())?;
    on_probe_lease(spec, &site, workspace_root, async |lease| {
        lease.list_prompts().await.map_err(mcp_failure)
    })
}

/// One server prompt with `arguments`, flattened into the user message a
/// prompt invocation sends, from a pool of its own.
///
/// # Errors
///
/// As [`list_mcp_prompts`], and an unknown prompt, a wrong argument, or a
/// refused, failed or oversized expansion.
pub fn get_mcp_prompt(
    server: &str,
    rendered: &str,
    workspace_root: &std::path::Path,
    secrets: Arc<dyn NamedSecrets>,
    prompt: &str,
    arguments: &BTreeMap<String, String>,
) -> Result<McpPromptExpansion, crate::CliFailure> {
    let server = McpServerName::new(server.to_owned())
        .map_err(|_| format!("{server:?} is not a valid MCP server id"))?;
    let (spec, site) = placed_server_spec(&server, rendered, &secrets, selected_runner())?;
    on_probe_lease(spec, &site, workspace_root, async |lease| {
        lease
            .get_prompt(prompt, arguments)
            .await
            .map_err(mcp_failure)
    })
}

/// Runs `work` on a lease of `spec` from a pool of its own, then stops the
/// server.
fn on_probe_lease<T>(
    spec: McpServerSpec,
    site: &ServerSite,
    workspace_root: &std::path::Path,
    work: impl AsyncFnOnce(&McpLease) -> Result<T, crate::CliFailure>,
) -> Result<T, crate::CliFailure> {
    crate::capabilities::require_feature("mcp").map_err(crate::CliFailure::from)?;
    let environment = Arc::new(service_environment(workspace_root, site)?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("plak-mcp-probe")
        .enable_all()
        .build()
        .map_err(|_| "mcp server executor could not be started".to_owned())?;
    let pool = McpPool::new().running_on(runtime.handle().clone());
    runtime.block_on(async {
        let outcome = async {
            let lease = pool.lease(spec, environment).await.map_err(mcp_failure)?;
            work(&lease).await
        }
        .await;
        pool.shutdown().await;
        outcome
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
) -> Result<Option<McpOAuthStatus>, crate::CliFailure> {
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
        .map_err(|error| crate::CliFailure::new(format!("mcp server {server}: {error}")))
}

/// The definition `plak mcp login` and `logout` run against: one table with
/// `auth = "oauth"`, whose login need not exist yet. A configured client
/// secret is resolved only when authentication needs it, never while
/// building the definition for a local logout.
///
/// # Errors
///
/// Returns a one-line reason for an invalid table, a table without OAuth,
/// or a product without MCP logins.
pub fn mcp_oauth_spec(
    server: &str,
    rendered: &str,
    secrets: Arc<dyn NamedSecrets>,
) -> Result<McpServerSpec, crate::CliFailure> {
    let server = McpServerName::new(server.to_owned())
        .map_err(|_| format!("{server:?} is not a valid MCP server id"))?;
    let definition = rendered_server(&server, rendered)?;
    if definition.auth.as_deref() != Some("oauth") {
        return Err(crate::CliFailure::new(format!(
            "mcp server {server} does not use oauth; add auth = \"oauth\" to [mcp_servers.{server}]"
        )));
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

/// The environment a new pool entry for this workspace spawns in, on the
/// server's site. A service outlives the bind that started it, so its scope
/// is a run of its own. On a runner the workspace is named as the runner
/// names it, and the runner's own process options apply.
fn service_environment(
    workspace_root: &std::path::Path,
    site: &ServerSite,
) -> Result<ExecutionEnvironment, crate::CliFailure> {
    let unavailable = "mcp server environment could not be prepared for this workspace";
    if let ServerSite::Runner(runner) = site {
        let run = plakous::model::RunId::try_from(uuid::Uuid::new_v4())
            .map_err(|_| unavailable.to_owned())?;
        return runner_environment(
            ExecutionEnvironmentScope::Run(run),
            workspace_root,
            ".",
            ExecutionEnvironmentConfig::default(),
            Arc::clone(runner),
        )
        .map_err(|_| crate::CliFailure::new(unavailable));
    }
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
    .map_err(|_| crate::CliFailure::new(unavailable))
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
    placement: Option<McpPlacement>,
    startup_seconds: u64,
    call_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct RenderedTables {
    mcp_servers: BTreeMap<String, RenderedServer>,
}

/// Rebuilds one server definition from its frozen canonical table, placed
/// as the process places it.
#[cfg(test)]
pub(crate) fn server_spec(
    server: &McpServerName,
    rendered: &str,
    secrets: &Arc<dyn NamedSecrets>,
) -> Result<McpServerSpec, crate::CliFailure> {
    placed_server_spec(server, rendered, secrets, selected_runner()).map(|(spec, _)| spec)
}

/// Rebuilds one server definition from its frozen canonical table, and
/// where it runs when `runner` is the process's selected runner. Every
/// secret it names must be stored now on the shelf its site reads; values
/// are read again at each spawn. A server on a selected runner names its
/// secrets to the runner, which checks them at the spawn: no value is read
/// on the host.
pub(crate) fn placed_server_spec(
    server: &McpServerName,
    rendered: &str,
    secrets: &Arc<dyn NamedSecrets>,
    runner: Option<Arc<RunnerBackend>>,
) -> Result<(McpServerSpec, ServerSite), crate::CliFailure> {
    let invalid = || invalid_definition(server);
    let definition = rendered_server(server, rendered)?;
    let site = ServerSite::of(definition.placement, runner);
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
            } else if matches!(site, ServerSite::Runner(_)) {
                let names = definition
                    .secret_env
                    .iter()
                    .map(|(variable, secret)| {
                        EnvironmentVariableName::new(variable.clone())
                            .map(|variable| (variable, secret.clone()))
                            .map_err(|_| invalid())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                spec.with_runner_secret_environment(names)
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
                let shelf = Shelf::of(&site);
                for (_, secret) in &names {
                    require_secret(server, secret, secrets, shelf)?;
                }
                let variables = names
                    .iter()
                    .map(|(variable, _)| variable.clone())
                    .collect::<Vec<_>>();
                spec.with_secret_environment(
                    variables,
                    secret_resolver(server, names, secrets, shelf),
                )
            }
        }
        // A runner-placed server speaks stdio: it runs where the runner's
        // processes run, and an HTTP server runs nowhere of ours.
        "http" if site.placement() == McpPlacement::Runner => return Err(invalid()),
        "http" => {
            if let Some(secret) = &definition.oauth_client_secret {
                require_secret(server, secret, secrets, Shelf::Named)?;
            }
            let spec = http_spec(server, &definition, secrets)
                .map_err(|error| error.unwrap_or_else(invalid))?;
            // No usable login refuses before anything is sent.
            if let Some(status) = spec
                .oauth_status()
                .map_err(|error| format!("mcp server {server}: {error}"))?
                && let Some(reason) = login_refusal(&status)
            {
                return Err(crate::CliFailure::new(format!(
                    "mcp server {server} {reason}; authenticate this server"
                ))
                .with_advice(
                    plakous_host_protocol::refusal::RefusalAdvice::McpLogin {
                        server: server.to_string(),
                        scopes: Vec::new(),
                    },
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
    let spec = match definition.tools {
        Some(tools) => spec.with_allowlist(tools),
        None => spec,
    };
    Ok((spec, site))
}

fn invalid_definition(server: &McpServerName) -> crate::CliFailure {
    crate::CliFailure::new(format!(
        "mcp server {server} has an invalid frozen definition"
    ))
}

/// The one `[mcp_servers.<id>]` table of `rendered`, which carries nothing
/// else.
fn rendered_server(
    server: &McpServerName,
    rendered: &str,
) -> Result<RenderedServer, crate::CliFailure> {
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
/// at each use) and callback port. Failures retain typed server/scope facts.
fn oauth_settings(
    server: &McpServerName,
    definition: &RenderedServer,
    secrets: &dyn NamedSecrets,
    client_secret: Option<McpOAuthSecretResolver>,
) -> Result<Option<McpOAuthSettings>, crate::CliFailure> {
    match definition.auth.as_deref() {
        None => return Ok(None),
        Some("oauth") => {}
        Some(_) => return Err(invalid_definition(server)),
    }
    let refused = |error: McpError| mcp_failure(error).with_context(format!("mcp server {server}"));
    let store = secrets.mcp_login_file(server.as_str()).ok_or_else(|| {
        format!("mcp server {server} uses oauth, and this product stores no MCP logins")
    })?;
    let mut settings = McpOAuthSettings::new(store);
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
) -> Result<McpServerSpec, Option<crate::CliFailure>> {
    let url = definition.url.as_deref().ok_or(None)?;
    let refused =
        |error: McpError| Some(mcp_failure(error).with_context(format!("mcp server {server}")));
    let headers = definition
        .headers
        .iter()
        .map(|(name, value)| {
            Ok((
                HeaderName::try_from(name.as_str()).map_err(|_| None)?,
                HeaderValue::try_from(value.as_str()).map_err(|_| None)?,
            ))
        })
        .collect::<Result<Vec<_>, Option<crate::CliFailure>>>()?;
    let mut slots = Vec::new();
    if let Some(secret) = &definition.bearer_secret {
        slots.push((McpHttpSecret::Bearer, secret.clone()));
    }
    for (name, secret) in &definition.secret_headers {
        let name = HeaderName::try_from(name.as_str()).map_err(|_| None)?;
        slots.push((McpHttpSecret::Header(name), secret.clone()));
    }
    for (_, secret) in &slots {
        require_secret(server, secret, secrets, Shelf::Named).map_err(Some)?;
    }
    let client_secret = definition
        .oauth_client_secret
        .as_ref()
        .map(|secret| oauth_secret_resolver(server, secret.clone(), secrets));
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
                read_secret(&server, secret, &secrets, Shelf::Named)
                    .map(|value| (slot.clone(), value.into_header()))
                    .map_err(boxed_secret_error)
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
        read_secret(&server, &secret, &secrets, Shelf::Named)
            .map(NamedSecretValue::into_header)
            .map_err(boxed_secret_error)
    })
}

/// Which of the product's secret shelves a host-side spawn reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Shelf {
    /// The host's named secrets (`plak secret set`).
    Named,
    /// This machine's runner secrets (`plak runner secret set`).
    Runner,
}

impl Shelf {
    const fn of(site: &ServerSite) -> Self {
        match site {
            ServerSite::Host => Self::Named,
            ServerSite::LocalRunner | ServerSite::Runner(_) => Self::Runner,
        }
    }
}

fn read_secret(
    server: &McpServerName,
    secret: &str,
    secrets: &Arc<dyn NamedSecrets>,
    shelf: Shelf,
) -> Result<NamedSecretValue, crate::CliFailure> {
    let read = match shelf {
        Shelf::Named => secrets.read(secret),
        Shelf::Runner => secrets.read_runner(secret),
    };
    match read {
        Ok(Some(value)) => Ok(value),
        Ok(None) if shelf == Shelf::Runner => Err(crate::CliFailure::new(format!(
            "mcp server {server}: runner secret {secret:?} is not set; supply the runner secret"
        ))
        .with_advice(
            plakous_host_protocol::refusal::RefusalAdvice::SecretMissing {
                server: server.to_string(),
                name: secret.to_owned(),
            },
        )),
        Ok(None) => Err(crate::CliFailure::new(format!(
            "mcp server {server}: secret {secret:?} is not set; supply the named secret"
        ))
        .with_advice(
            plakous_host_protocol::refusal::RefusalAdvice::SecretMissing {
                server: server.to_string(),
                name: secret.to_owned(),
            },
        )),
        Err(reason) => Err(crate::CliFailure::new(format!(
            "mcp server {server}: {reason}"
        ))),
    }
}

fn require_secret(
    server: &McpServerName,
    secret: &str,
    secrets: &Arc<dyn NamedSecrets>,
    shelf: Shelf,
) -> Result<(), crate::CliFailure> {
    read_secret(server, secret, secrets, shelf).map(drop)
}

fn secret_resolver(
    server: &McpServerName,
    names: Vec<(EnvironmentVariableName, String)>,
    secrets: &Arc<dyn NamedSecrets>,
    shelf: Shelf,
) -> McpSecretResolver {
    let server = server.clone();
    let secrets = Arc::clone(secrets);
    Arc::new(move || {
        names
            .iter()
            .map(|(variable, secret)| {
                read_secret(&server, secret, &secrets, shelf)
                    .and_then(|value| {
                        value.into_environment().map_err(|reason| {
                            crate::CliFailure::new(format!(
                                "mcp server {server}: secret {secret:?}: {reason}"
                            ))
                        })
                    })
                    .map(|value| (variable.clone(), value))
                    .map_err(boxed_secret_error)
            })
            .collect()
    })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "runner_tests.rs"]
mod runner_tests;
