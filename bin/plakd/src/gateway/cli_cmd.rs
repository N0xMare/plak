//! `plakd gateway` execution. Account verbs that are not gateway verbs stay on
//! the normal commands and are forwarded when a gateway holds the store.

use crate::command::GatewayCommand;
use crate::home::paths::PlakPaths;
use std::io::Write;

use super::ServerConfig;
use super::address::GatewayAddress;
use super::route::AccountRoute;

pub(crate) fn execute(
    home: &PlakPaths,
    command: GatewayCommand,
    stdout: &mut dyn Write,
) -> Result<(), String> {
    match command {
        GatewayCommand::Init { tcp, key_dir } => write_init(home, tcp.as_deref(), key_dir, stdout),
        GatewayCommand::Serve => super::serve(home),
        GatewayCommand::Install { dry_run } => {
            let executable = std::env::current_exe().map_err(|error| error.to_string())?;
            let path = super::install(&executable, dry_run)?;
            writeln!(stdout, "{}", path.display()).map_err(|error| error.to_string())
        }
        GatewayCommand::Uninstall => {
            let note = super::uninstall()?;
            writeln!(stdout, "{note}").map_err(|error| error.to_string())
        }
        GatewayCommand::Status { dry_run: true } => dry_status(home, stdout),
        GatewayCommand::Status { dry_run: false } => live_status(home, stdout),
        GatewayCommand::CallerAdd {
            id,
            sources,
            admin,
            key_out,
        } => {
            let address = local_service(home)?;
            super::caller_add(&address, &id, &sources, &admin, key_out.as_deref(), stdout)
        }
        GatewayCommand::CallerList => {
            let address = local_service(home)?;
            super::caller_list(&address, stdout)
        }
        GatewayCommand::CallerRemove { id } => {
            let address = local_service(home)?;
            super::caller_remove(&address, &id)
        }
        GatewayCommand::CallerRotate { id } => {
            let address = local_service(home)?;
            super::caller_rotate(&address, &id, stdout)
        }
        GatewayCommand::Disable {
            account,
            caller,
            cut,
        } => {
            let route = super::route_for(home)?;
            let Some(address) = route.remote() else {
                return Err("no gateway is configured or running".to_owned());
            };
            let key = caller_key(home, &route)?;
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
            let address = local_service(home)?;
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
            let address = local_service(home)?;
            super::drain(&address)
        }
        GatewayCommand::Audit { since, caller } => {
            let text = super::audit_text(home, since.as_deref(), caller.as_deref())?;
            if !text.is_empty() {
                writeln!(stdout, "{text}").map_err(|error| error.to_string())?;
            }
            Ok(())
        }
        GatewayCommand::Check => live_check(home, stdout),
    }
}

fn write_init(
    home: &PlakPaths,
    tcp: Option<&str>,
    key_dir: Option<std::path::PathBuf>,
    stdout: &mut dyn Write,
) -> Result<(), String> {
    let config = super::init(home, tcp, key_dir)?;
    writeln!(stdout, "wrote {}", ServerConfig::path(home).display())
        .map_err(|error| error.to_string())?;
    writeln!(stdout, "state_root {}", config.state_root.display())
        .map_err(|error| error.to_string())?;
    writeln!(stdout, "key_dir {}", config.key_dir.display()).map_err(|error| error.to_string())
}

fn live_status(home: &PlakPaths, stdout: &mut dyn Write) -> Result<(), String> {
    let route = super::route_for(home)?;
    if let Some(line) = route.via_line() {
        writeln!(stdout, "{line}").map_err(|error| error.to_string())?;
    }
    let key = caller_key(home, &route)?;
    let text = super::status_text(home, &route, key.as_deref())?;
    writeln!(stdout, "{text}").map_err(|error| error.to_string())
}

fn live_check(home: &PlakPaths, stdout: &mut dyn Write) -> Result<(), String> {
    let route = super::route_for(home)?;
    let Some(address) = route.remote() else {
        return Err("no gateway is configured or running".to_owned());
    };
    if let Some(line) = route.via_line() {
        writeln!(stdout, "{line}").map_err(|error| error.to_string())?;
    }
    let key = caller_key(home, &route)?;
    super::check(address, key.as_deref(), stdout)
}

fn dry_status(home: &PlakPaths, stdout: &mut dyn Write) -> Result<(), String> {
    let config = ServerConfig::load(home)?;
    writeln!(
        stdout,
        "gateway.toml {}\nstate_root {}\nkey_dir {}\nsocket {}",
        ServerConfig::path(home).display(),
        config.state_root.display(),
        config.key_dir.display(),
        config.socket.display()
    )
    .map_err(|error| error.to_string())
}

fn local_service(home: &PlakPaths) -> Result<GatewayAddress, String> {
    match super::route_for(home)? {
        AccountRoute::Configured(address) | AccountRoute::Service(address) => Ok(address),
        AccountRoute::InProcess => {
            Err("no local gateway is running; start it with plakd gateway serve".to_owned())
        }
    }
}

pub(crate) fn caller_key(home: &PlakPaths, route: &AccountRoute) -> Result<Option<String>, String> {
    let tcp = matches!(route.remote(), Some(GatewayAddress::Tcp(_)));
    let config = crate::product_config::ProductConfig::load(home)
        .unwrap_or_else(|_| crate::product_config::ProductConfig::default_product());
    let path = config
        .gateway_key_file
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.secrets_dir().join("gateway.key"));
    if path.is_file() {
        return super::read_key_file(&path).map(Some);
    }
    if tcp {
        return Err(format!(
            "a tcp gateway requires a caller key at {}",
            path.display()
        ));
    }
    Ok(None)
}

pub(crate) fn doctor_line(home: &PlakPaths) -> Result<String, String> {
    let route = super::route_for(home)?;
    match route.remote() {
        None => Ok("gateway: in-process".to_owned()),
        Some(address) => {
            let key = caller_key(home, &route)?;
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
