//! `plakd gateway` execution. Account verbs that are not gateway verbs stay on
//! the normal commands and are forwarded when a gateway holds the store.

use crate::command::GatewayCommand;
use crate::home::paths::PlakPaths;
use crate::shell::services::Services;
use std::io::Write;

use super::ServerConfig;
use super::address::GatewayAddress;
use super::route::AccountRoute;

pub(crate) fn execute(
    services: &Services,
    command: GatewayCommand,
    stdout: &mut dyn Write,
) -> Result<(), String> {
    let home = services
        .paths()
        .map_err(|failure| failure.message().to_owned())?;
    match command {
        GatewayCommand::Init { tcp } => write_init(home, tcp.as_deref(), stdout),
        GatewayCommand::Serve => super::serve(home),
        GatewayCommand::Install { dry_run } => {
            let executable = std::env::current_exe().map_err(|error| error.to_string())?;
            let state = services
                .home()
                .state()
                .map_err(|failure| failure.message().to_owned())?;
            let path = super::install(&executable, home.home(), state, dry_run)?;
            writeln!(stdout, "{}", path.display()).map_err(|error| error.to_string())
        }
        GatewayCommand::Uninstall => {
            let note = super::uninstall()?;
            writeln!(stdout, "{note}").map_err(|error| error.to_string())
        }
        GatewayCommand::Status { dry_run: true } => dry_status(home, stdout),
        GatewayCommand::Status { dry_run: false } => live_status(services, stdout),
        GatewayCommand::CallerAdd {
            id,
            sources,
            admin,
            key_out,
        } => {
            let address = local_service(services)?;
            super::caller_add(&address, &id, &sources, &admin, key_out.as_deref(), stdout)
        }
        GatewayCommand::CallerList => {
            let address = local_service(services)?;
            super::caller_list(&address, stdout)
        }
        GatewayCommand::CallerRemove { id } => {
            let address = local_service(services)?;
            super::caller_remove(&address, &id)
        }
        GatewayCommand::CallerRotate { id } => {
            let address = local_service(services)?;
            super::caller_rotate(&address, &id, stdout)
        }
        GatewayCommand::Disable {
            account,
            caller,
            cut,
        } => {
            let (route, key) = route(services)?;
            let Some(address) = route.remote() else {
                return Err("no gateway is configured or running".to_owned());
            };
            super::set_flag(
                address,
                key.as_deref(),
                account.as_deref(),
                caller.as_deref(),
                cut,
                false,
            )
        }
        GatewayCommand::Enable { account, caller } => {
            let address = local_service(services)?;
            super::set_flag(
                &address,
                None,
                account.as_deref(),
                caller.as_deref(),
                false,
                true,
            )
        }
        GatewayCommand::Drain => {
            let address = local_service(services)?;
            super::drain(&address)
        }
        GatewayCommand::Audit { since, caller } => {
            let text = super::audit_text(home, since.as_deref(), caller.as_deref())?;
            if !text.is_empty() {
                writeln!(stdout, "{text}").map_err(|error| error.to_string())?;
            }
            Ok(())
        }
        GatewayCommand::Check => live_check(services, stdout),
    }
}

fn write_init(home: &PlakPaths, tcp: Option<&str>, stdout: &mut dyn Write) -> Result<(), String> {
    let config = super::init(home, tcp)?;
    writeln!(stdout, "wrote {}", ServerConfig::path(home).display())
        .map_err(|error| error.to_string())?;
    writeln!(stdout, "state_root {}", config.state_root.display())
        .map_err(|error| error.to_string())
}

fn live_status(services: &Services, stdout: &mut dyn Write) -> Result<(), String> {
    let (route, key) = route(services)?;
    if let Some(line) = route.via_line() {
        writeln!(stdout, "{line}").map_err(|error| error.to_string())?;
    }
    let home = services
        .paths()
        .map_err(|failure| failure.message().to_owned())?;
    let text = super::status_text(home, &route, key.as_deref())?;
    writeln!(stdout, "{text}").map_err(|error| error.to_string())
}

fn live_check(services: &Services, stdout: &mut dyn Write) -> Result<(), String> {
    let (route, key) = route(services)?;
    let Some(address) = route.remote() else {
        return Err("no gateway is configured or running".to_owned());
    };
    if let Some(line) = route.via_line() {
        writeln!(stdout, "{line}").map_err(|error| error.to_string())?;
    }
    verify_holder(services)?;
    super::check(address, key.as_deref(), stdout)
}

fn dry_status(home: &PlakPaths, stdout: &mut dyn Write) -> Result<(), String> {
    let config = ServerConfig::load(home)?;
    ServerConfig::require_host_state_root(&config.state_root)?;
    writeln!(
        stdout,
        "gateway.toml {}\nstate_root {}\nsocket {}",
        ServerConfig::path(home).display(),
        config.state_root.display(),
        config.socket.display()
    )
    .map_err(|error| error.to_string())
}

fn local_service(services: &Services) -> Result<GatewayAddress, String> {
    match route(services)?.0 {
        AccountRoute::Configured(address) | AccountRoute::Service(address) => Ok(address),
        AccountRoute::InProcess => {
            Err("no local gateway is running; start it with plakd gateway serve".to_owned())
        }
    }
}

/// The route for a gateway verb and the caller key it presents: the
/// configured gateway's key, or none for a discovered same-user service.
fn route(services: &Services) -> Result<(AccountRoute, Option<String>), String> {
    let custody = services
        .custody()
        .map_err(|failure| failure.message().to_owned())?;
    let home = services
        .paths()
        .map_err(|failure| failure.message().to_owned())?;
    let route = super::route_for(home, &custody)?;
    let key = match route {
        AccountRoute::Configured(_) => custody
            .admin()
            .and_then(super::AdminLink::key)
            .map(str::to_owned),
        AccountRoute::Service(_) | AccountRoute::InProcess => None,
    };
    Ok((route, key))
}

/// A configured unix gateway must hold this home's state store.
fn verify_holder(services: &Services) -> Result<(), String> {
    let custody = services
        .custody()
        .map_err(|failure| failure.message().to_owned())?;
    let state = services
        .home()
        .state()
        .map_err(|failure| failure.message().to_owned())?;
    custody.verify_holder(state)
}

/// `gateway: in-process`, or the gateway's address and instance. A gateway
/// that does not answer, or that does not hold this home's store, is an
/// error the doctor counts as a failure.
pub(crate) fn doctor_line(services: &Services) -> Result<String, String> {
    let (route, key) = route(services).map_err(|error| format!("gateway: {error}"))?;
    match route.remote() {
        None => Ok("gateway: in-process".to_owned()),
        Some(address) => {
            if matches!(route, AccountRoute::Configured(_)) {
                verify_holder(services).map_err(|error| format!("gateway: {error}"))?;
            }
            match super::client::versions(address, key.as_deref()) {
                Ok(versions) => Ok(format!(
                    "gateway: {} {}",
                    address.label(),
                    versions.instance.as_str()
                )),
                Err(error) => Err(format!("gateway: {error}")),
            }
        }
    }
}
