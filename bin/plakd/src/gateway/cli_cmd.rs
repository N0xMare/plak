//! `plakd gateway` execution.

use crate::command::GatewayCommand;
use crate::home::paths::PlakPaths;
use crate::shell::services::Services;
use crate::{CliFailure, CliResult};
use std::io::Write;

use super::AdminLink;
use super::ServerConfig;
use super::address::GatewayAddress;

/// `plakd gateway check --json`: the holder, served versions and instance,
/// and each enrolled account's credential status and generation. It reads
/// facts only; it never refreshes, dispatches or changes a flag.
#[derive(serde::Serialize)]
struct CheckReport {
    address: String,
    instance: String,
    versions: Vec<u16>,
    holder: bool,
    accounts: Vec<CheckAccount>,
}

#[derive(serde::Serialize)]
struct CheckAccount {
    account: String,
    status: plakous::gateway::protocol::facts::CredentialStatus,
    generation: Option<u64>,
}

fn json_check(services: &Services, stdout: &mut dyn Write) -> CliResult<()> {
    use plakous::gateway::protocol::Endpoint;
    use plakous::gateway::protocol::facts::{FactsReply, FactsRequest};

    let link = cli_link(services)?;
    verify_holder(services, &link)?;
    let versions = super::client::versions(&link.address, link.key())?;
    let accounts = super::listed_accounts(&link.address, link.key())?;
    let facts = if accounts.is_empty() {
        Vec::new()
    } else {
        super::client::request::<FactsReply>(
            &link.address,
            link.key(),
            Endpoint::Facts,
            &FactsRequest {
                accounts: accounts.clone(),
            },
        )?
        .facts
    };
    let mut rows = Vec::with_capacity(facts.len());
    for account in &accounts {
        let row = facts
            .iter()
            .find(|row| &row.account == account)
            .ok_or_else(|| CliFailure::new("gateway facts omitted an enrolled account"))?;
        rows.push(CheckAccount {
            account: account.to_string(),
            status: row.credential.status,
            generation: row.credential.generation,
        });
    }
    let report = CheckReport {
        address: link.address.label(),
        instance: versions.instance.as_str().to_owned(),
        versions: versions.versions,
        holder: true,
        accounts: rows,
    };
    serde_json::to_writer(&mut *stdout, &report)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    writeln!(stdout).map_err(|error| CliFailure::new(error.to_string()))
}

pub(crate) fn execute(
    services: &Services,
    command: GatewayCommand,
    stdout: &mut dyn Write,
) -> CliResult<()> {
    let home = services
        .paths()
        .map_err(|failure| failure.message().to_owned())?;
    match command {
        GatewayCommand::Init { tcp } => write_init(home, tcp.as_deref(), stdout),
        GatewayCommand::Serve => super::serve(home).map_err(CliFailure::from),
        GatewayCommand::Install { dry_run } => {
            let state = services
                .home()
                .state()
                .map_err(|failure| failure.message().to_owned())?;
            super::install::install(home, state, dry_run, stdout).map_err(CliFailure::from)
        }
        GatewayCommand::Uninstall => {
            let note = super::install::uninstall()?;
            writeln!(stdout, "{note}").map_err(|error| CliFailure::new(error.to_string()))
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
            let link = AdminLink::for_cli(home)?;
            super::set_flag(
                &link.address,
                link.key(),
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
                writeln!(stdout, "{text}").map_err(|error| CliFailure::new(error.to_string()))?;
            }
            Ok(())
        }
        GatewayCommand::Check { json: false } => live_check(services, stdout),
        GatewayCommand::Check { json: true } => json_check(services, stdout),
    }
}

fn write_init(home: &PlakPaths, tcp: Option<&str>, stdout: &mut dyn Write) -> CliResult<()> {
    let config = super::init(home, tcp)?;
    writeln!(stdout, "wrote {}", ServerConfig::path(home).display())
        .map_err(|error| CliFailure::new(error.to_string()))?;
    writeln!(stdout, "state_root {}", config.state_root.display())
        .map_err(|error| CliFailure::new(error.to_string()))
}

fn live_status(services: &Services, stdout: &mut dyn Write) -> CliResult<()> {
    let home = services
        .paths()
        .map_err(|failure| failure.message().to_owned())?;
    let link = AdminLink::for_cli(home)?;
    let text = super::status_text(home, &link.address, link.key())?;
    writeln!(stdout, "via gateway {}\n{text}", link.address.label())
        .map_err(|error| CliFailure::new(error.to_string()))
}

fn live_check(services: &Services, stdout: &mut dyn Write) -> CliResult<()> {
    let link = cli_link(services)?;
    verify_holder(services, &link)?;
    let mut checked = Vec::new();
    super::check(&link.address, link.key(), &mut checked)?;
    writeln!(stdout, "via gateway {}", link.address.label())
        .map_err(|error| CliFailure::new(error.to_string()))?;
    stdout
        .write_all(&checked)
        .map_err(|error| CliFailure::new(error.to_string()))
}

fn dry_status(home: &PlakPaths, stdout: &mut dyn Write) -> CliResult<()> {
    let config = ServerConfig::load(home)?;
    ServerConfig::require_host_state_root(&config.state_root)?;
    writeln!(
        stdout,
        "gateway.toml {}\nstate_root {}\nsocket {}",
        ServerConfig::path(home).display(),
        config.state_root.display(),
        config.socket.display()
    )
    .map_err(|error| CliFailure::new(error.to_string()))
}

fn local_service(services: &Services) -> Result<GatewayAddress, String> {
    let home = services
        .paths()
        .map_err(|failure| failure.message().to_owned())?;
    Ok(AdminLink::local(home)?.address)
}

fn cli_link(services: &Services) -> Result<AdminLink, String> {
    let home = services
        .paths()
        .map_err(|failure| failure.message().to_owned())?;
    AdminLink::for_cli(home)
}

/// A configured unix gateway must hold this home's state store.
fn verify_holder(services: &Services, link: &AdminLink) -> CliResult<()> {
    let state = services
        .home()
        .state()
        .map_err(|failure| failure.message().to_owned())?;
    if !super::verify_holder(&link.address, state)? {
        return Err(super::GatewayUnavailable::Unreachable(&link.address).failure());
    }
    Ok(())
}

/// The gateway's address and instance. A gateway
/// that does not answer, or that does not hold this home's store, is an
/// error the doctor counts as a failure.
pub(crate) fn doctor_line(services: &Services) -> Result<String, String> {
    let link = cli_link(services)?;
    verify_holder(services, &link).map_err(|failure| failure.to_string())?;
    let versions = super::client::versions(&link.address, link.key())
        .map_err(|failure| failure.to_string())?;
    Ok(format!(
        "gateway: {} {}",
        link.address.label(),
        versions.instance.as_str()
    ))
}
