//! The plakd credential gateway: listeners, caller keys, the kill switch,
//! audit and the admin bridge. The listener relays API-key accounts and the
//! subscription logins through the shared server core. A host that names a
//! gateway sends API-key and subscription routes through it.

mod address;
mod allowance;
mod audit;
mod auth;
mod bridge;
mod callers;
mod cli_cmd;
pub(crate) use cli_cmd::deployment_generation;
pub(crate) mod client;
mod custody;
mod dispatch;
mod flags;
mod holder;
mod http;
mod install;
mod io_file;
mod ops;
mod product;
mod remote_account;
mod route_custody;
mod server_config;
mod service;
#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
mod subscription_custody;
#[cfg(not(any(feature = "codex", feature = "grok", feature = "claude")))]
#[path = "subscription_custody_unavailable.rs"]
mod subscription_custody;

pub(crate) use address::GatewayAddress;
pub(crate) use cli_cmd::{doctor_line, execute};
pub(crate) use install::{GatewayUnit, SYSTEMD_UNIT, launchd_start};
pub(crate) use remote_account::{is_account_command, try_remote_account};
pub(crate) use route_custody::{AdminLink, GatewayUnavailable, HostCustody, verify_holder};
pub(crate) use server_config::{ServerConfig, default_socket};
pub(crate) use service::Gateway;

use std::io::Write;
use std::net::SocketAddr;

use plakous::gateway::protocol::GatewayAccount;
use plakous::gateway::protocol::admin::{AdminRequest, AdminResponse, DisableTarget};
use plakous::gateway::protocol::scope::{CallerId, ScopeSet, SourceNet};

/// Runs `plakd gateway serve` until a signal or an admin drain.
pub(crate) fn serve(home: &crate::home::paths::PlakPaths) -> Result<(), String> {
    let config = ServerConfig::load(home)?;
    ServerConfig::require_host_state_root(&config.state_root)?;
    // The service is the store's custodian: it never relays to a gateway,
    // even when the shared home's config.toml names this one.
    let app = crate::commands::application::PlakousApplication::new(
        config.state_root.clone(),
        None,
        HostCustody::Local,
    )
    .map_err(|error| error.to_string())?
    .with_account_home(Some(home.clone()));
    let gateway = Gateway::open_with(
        home,
        config,
        Box::new(ops::OpsBridge::new(app, home.clone(), fresh_instance()?)),
    )?;
    gateway.serve_foreground()
}

fn fresh_instance() -> Result<plakous::gateway::protocol::UuidText, String> {
    plakous::gateway::protocol::UuidText::new(uuid::Uuid::new_v4().to_string())
        .map_err(|error| error.to_string())
}

pub(crate) fn init(
    home: &crate::home::paths::PlakPaths,
    tcp: Option<&str>,
) -> Result<ServerConfig, String> {
    let listen_tcp = match tcp {
        Some(text) => Some(parse_listen(text)?),
        None => None,
    };
    ServerConfig::init(home, listen_tcp)
}

fn parse_listen(text: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = text
        .parse()
        .map_err(|_| "tcp address must be <ip>:<port>".to_owned())?;
    address::admit_ip(addr.ip(), false).map_err(|error| error.to_string())?;
    if !address::interface_has(addr.ip()) {
        return Err(format!("{addr} is not assigned on this machine"));
    }
    Ok(addr)
}

pub(crate) fn status_text(
    home: &crate::home::paths::PlakPaths,
    address: &GatewayAddress,
    key: Option<&str>,
) -> crate::CliResult<String> {
    let versions = client::versions(address, key)?;
    let mut lines = vec![format!("instance {}", versions.instance.as_str())];
    if let Some(count) = audit::published_failures(
        &server_config::audit_failures_path(home),
        versions.instance.as_str(),
    ) {
        lines.push(format!("audit append failures {count}"));
    }
    lines.push(format!(
        "versions {}",
        versions
            .versions
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",")
    ));
    lines.push(address.label());
    let flags = flags::Flags::load(&server_config::flags_path(home)).ok();
    if let Some(flags) = flags {
        lines.push(format!("flags {}", flags.summary()));
    }
    Ok(lines.join("\n"))
}

pub(crate) fn audit_text(
    home: &crate::home::paths::PlakPaths,
    since: Option<&str>,
    caller: Option<&str>,
) -> Result<String, String> {
    let since = since.map(audit::parse_since).transpose()?;
    let lines = audit::read_lines(&server_config::audit_dir(home), since, caller)
        .map_err(|error| error.to_string())?;
    Ok(lines.join("\n"))
}

pub(crate) fn caller_add(
    address: &GatewayAddress,
    id: &str,
    sources: &[String],
    admin: &[String],
    key_out: Option<&std::path::Path>,
    stdout: &mut dyn Write,
) -> crate::CliResult<()> {
    let caller = CallerId::new(id.to_owned()).map_err(|error| error.to_string())?;
    let mut scope_text = String::from("relay:*");
    for account in admin {
        scope_text.push_str(",admin:");
        scope_text.push_str(account);
    }
    let scopes = ScopeSet::parse(&scope_text).map_err(|error| error.to_string())?;
    let sources = sources
        .iter()
        .map(|source| SourceNet::parse(source).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let response = client::admin::<AdminResponse>(
        address,
        None,
        &AdminRequest::CallerAdd {
            caller,
            scopes,
            sources,
        },
    )?;
    match response {
        AdminResponse::CallerKeyIssued { caller, key } => {
            if let Some(path) = key_out {
                io_file::write_private(path, key.expose().as_bytes())
                    .map_err(|error| error.to_string())?;
                writeln!(stdout, "caller {} key written", caller.as_str())
                    .map_err(|error| error.to_string())?;
            } else {
                writeln!(stdout, "{}", key.expose()).map_err(|error| error.to_string())?;
            }
            Ok(())
        }
        AdminResponse::Refused { refusal } => {
            Err(format!("caller add refused: {refusal:?}").into())
        }
        _ => Err("caller add returned an unexpected response"
            .to_owned()
            .into()),
    }
}

pub(crate) fn caller_list(
    address: &GatewayAddress,
    stdout: &mut dyn Write,
) -> crate::CliResult<()> {
    let response = client::admin::<AdminResponse>(address, None, &AdminRequest::CallerList {})?;
    let AdminResponse::Callers { callers } = response else {
        return Err("caller list returned an unexpected response"
            .to_owned()
            .into());
    };
    for caller in callers {
        let revoked = if caller.revoked_at_unix_ms.is_some() {
            " revoked"
        } else {
            ""
        };
        writeln!(
            stdout,
            "{} {}{revoked}",
            caller.caller.as_str(),
            caller.scopes
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub(crate) fn caller_remove(address: &GatewayAddress, id: &str) -> crate::CliResult<()> {
    let caller = CallerId::new(id.to_owned()).map_err(|error| error.to_string())?;
    let response =
        client::admin::<AdminResponse>(address, None, &AdminRequest::CallerRemove { caller })?;
    match response {
        AdminResponse::CallerRemoved { .. } => Ok(()),
        AdminResponse::Refused { refusal } => Err(format!("caller rm refused: {refusal:?}").into()),
        _ => Err("caller rm returned an unexpected response"
            .to_owned()
            .into()),
    }
}

pub(crate) fn caller_rotate(
    address: &GatewayAddress,
    id: &str,
    stdout: &mut dyn Write,
) -> crate::CliResult<()> {
    let caller = CallerId::new(id.to_owned()).map_err(|error| error.to_string())?;
    let response =
        client::admin::<AdminResponse>(address, None, &AdminRequest::CallerRotate { caller })?;
    match response {
        AdminResponse::CallerKeyIssued { key, .. } => writeln!(stdout, "{}", key.expose())
            .map_err(|error| crate::CliFailure::new(error.to_string())),
        AdminResponse::Refused { refusal } => {
            Err(format!("caller rotate refused: {refusal:?}").into())
        }
        _ => Err("caller rotate returned an unexpected response"
            .to_owned()
            .into()),
    }
}

pub(crate) fn set_flag(
    address: &GatewayAddress,
    key: Option<&str>,
    account: Option<&str>,
    caller: Option<&str>,
    cut: bool,
    enable: bool,
) -> crate::CliResult<()> {
    let target = flag_target(account, caller)?;
    let request = if enable {
        AdminRequest::Enable { target }
    } else {
        AdminRequest::Disable { target, cut }
    };
    let response = client::admin::<AdminResponse>(address, key, &request)?;
    match response {
        AdminResponse::Disabled {} | AdminResponse::Enabled {} => Ok(()),
        AdminResponse::Refused { refusal } => {
            Err(format!("gateway flag refused: {refusal:?}").into())
        }
        _ => Err("gateway flag returned an unexpected response"
            .to_owned()
            .into()),
    }
}

pub(crate) fn drain(address: &GatewayAddress) -> crate::CliResult<()> {
    let response = client::admin::<AdminResponse>(address, None, &AdminRequest::Drain {})?;
    match response {
        AdminResponse::Draining {} => Ok(()),
        AdminResponse::Refused { refusal } => Err(format!("drain refused: {refusal:?}").into()),
        _ => Err("drain returned an unexpected response".to_owned().into()),
    }
}

pub(crate) fn deploy_drain(socket: &std::path::Path, expected: &str) -> crate::CliResult<bool> {
    let address = GatewayAddress::Unix(socket.into());
    if client::versions(&address, None)?.instance.as_str() != expected {
        return Err("gateway holder changed before drain".into());
    }
    match client::admin::<AdminResponse>(&address, None, &AdminRequest::Drain {})? {
        AdminResponse::Draining {} => Ok(true),
        AdminResponse::Refused { .. } => Ok(false),
        _ => Err("gateway drain returned an unexpected response".into()),
    }
}

pub(crate) fn deploy_holder_released(state_root: &std::path::Path) -> crate::CliResult<bool> {
    holder::advertised_socket(&holder::lock_path(state_root))
        .map(|socket| socket.is_none())
        .map_err(|_| "cannot prove the credential holder released its lock".into())
}

pub(crate) fn check(
    address: &GatewayAddress,
    key: Option<&str>,
    stdout: &mut dyn Write,
) -> crate::CliResult<()> {
    let versions = client::versions(address, key)?;
    writeln!(
        stdout,
        "gateway {} protocol {}",
        versions.instance.as_str(),
        versions
            .versions
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
    .map_err(|error| error.to_string())?;
    let accounts = client::admin::<AdminResponse>(address, key, &AdminRequest::List {})?;
    match accounts {
        AdminResponse::Accounts { accounts } => {
            for account in accounts {
                writeln!(stdout, "account {account}").map_err(|error| error.to_string())?;
            }
            Ok(())
        }
        AdminResponse::Refused { refusal } => {
            Err(format!("gateway account list refused: {refusal:?}").into())
        }
        _ => Err("gateway account list returned an unexpected response"
            .to_owned()
            .into()),
    }
}

fn flag_target(account: Option<&str>, caller: Option<&str>) -> Result<DisableTarget, String> {
    match (account, caller) {
        (None, None) => Ok(DisableTarget::All {}),
        (Some(account), None) => Ok(DisableTarget::Account {
            account: GatewayAccount::new(account.to_owned()).map_err(|error| error.to_string())?,
        }),
        (None, Some(caller)) => Ok(DisableTarget::Caller {
            caller: CallerId::new(caller.to_owned()).map_err(|error| error.to_string())?,
        }),
        (Some(_), Some(_)) => Err("disable one of an account or a caller, not both".to_owned()),
    }
}

pub(crate) fn validate_client_config(
    config: &crate::product_config::ProductConfig,
) -> Result<(), String> {
    if let Some(address) = config.gateway_address.as_deref() {
        address::parse(address, config.gateway_allow_non_tailnet)
            .map_err(|error| format!("gateway.address: {error}"))?;
    }
    if let Some(path) = config.gateway_key_file.as_deref() {
        address::absolute_file(std::path::Path::new(path))
            .map_err(|error| format!("gateway.key_file: {error}"))?;
    }
    Ok(())
}

/// Reads the caller key file, which must be a regular file only its owner
/// can read or write. The value is not logged.
pub(crate) fn read_key_file(path: &std::path::Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        return Err("the gateway key file must be a regular file with mode 0600".to_owned());
    }
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let key = text.trim();
    if key.is_empty() {
        return Err("the gateway key file is empty".to_owned());
    }
    Ok(key.to_owned())
}

#[cfg(test)]
mod adoption_tests;
#[cfg(all(test, feature = "codex", feature = "grok"))]
#[path = "../../tests/integration/support/gateway_seed.rs"]
pub(crate) mod gateway_seed;
#[cfg(test)]
pub(crate) mod test_service;
#[cfg(test)]
mod tests;

#[cfg(all(
    test,
    feature = "openai",
    feature = "anthropic",
    feature = "xai-api",
    feature = "openrouter",
    feature = "codex",
    feature = "claude",
    feature = "grok"
))]
mod continuity_tests;

#[cfg(all(
    test,
    feature = "openai",
    feature = "anthropic",
    feature = "xai-api",
    feature = "openrouter",
    feature = "codex",
    feature = "claude",
    feature = "grok"
))]
mod account_choice_tests;
