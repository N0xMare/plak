//! Clap-backed CLI parsing for the Plak application.

mod finish;
mod mcp;
pub(crate) mod render;
mod top;

use finish::{finish_agent, finish_continue_session, finish_run};
use mcp::McpAction;

use std::ffi::OsString;

use clap::{ArgAction, Parser, Subcommand};
use plakous::subscriptions::NativeLoginMode;
#[cfg(all(test, feature = "maintainer"))]
use plakous::subscriptions::NativeProvider;

use crate::command::{Command, NATIVE_RUN_MAX_PROMPT_BYTES, RunEffort};
use plakous_hosted::account::{AccountRef, Vertical};

/// Parse argv tokens (without binary name) into a product [`Command`].
pub(crate) fn parse(arguments: impl IntoIterator<Item = OsString>) -> crate::CliResult<Command> {
    let usage = |message| crate::CliFailure::classified(crate::CliFailureKind::Usage, message);
    let values = arguments
        .into_iter()
        .map(|value| {
            value
                .into_string()
                .map_err(|_| "command arguments must be valid UTF-8".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(usage)?;

    // An operator must choose a command before any product state is opened.
    if values.is_empty() {
        return Ok(Command::Help);
    }

    match values.as_slice() {
        [single] if single == "--help" || single == "-h" || single == "help" => {
            return Ok(Command::Help);
        }
        [single] if single == "--version" || single == "-V" || single == "version" => {
            return Ok(Command::Version);
        }
        _ => {}
    }

    // `plakd help <cmd>`
    if values.first().map(String::as_str) == Some("help") {
        if values.len() == 1 {
            return Ok(Command::Help);
        }
        return Ok(Command::CommandHelp {
            command: values[1].clone(),
        });
    }

    // Recursive: `plakd <cmd> --help` / `-h` — only for commands this build actually owns.
    if let Some(cmd) = values.first()
        && values
            .iter()
            .take_while(|v| *v != "--")
            .any(|v| v == "--help" || v == "-h")
        && is_known_command(cmd)
    {
        return Ok(Command::CommandHelp {
            command: cmd.clone(),
        });
    }

    let cli = Cli::try_parse_from(values.iter().map(String::as_str)).map_err(|error| {
        let verb = values.first().filter(|verb| is_known_command(verb));
        if verb.is_some_and(|verb| verb == "run")
            && error.kind() == clap::error::ErrorKind::UnknownArgument
            && !values.iter().any(|value| value == "--")
        {
            return "put the prompt after `--`: plakd run ... -- '<prompt>'; use `plakd run --help`"
                .to_owned();
        }
        let message = clap_error(error);
        match verb {
            Some(verb) => message.replace("<command>", verb),
            None => message.replace("plakd <command> --help", "plakd help"),
        }
    }).map_err(usage)?;
    let account = command_account(cli.command.as_ref());
    if account == Some("xai") {
        return Err(usage(
            "`xai` is ambiguous: use `grok` (SuperGrok subscription) or `xai-api` (API key)"
                .to_owned(),
        ));
    }
    cli.into_command().map_err(usage)
}

fn command_account(command: Option<&TopCommand>) -> Option<&str> {
    match command {
        Some(
            TopCommand::Login { account, .. }
            | TopCommand::Logout { account, .. }
            | TopCommand::Status { account, .. }
            | TopCommand::Refresh { account }
            | TopCommand::Usage { account, .. },
        ) => account.as_deref(),
        Some(
            TopCommand::Run {
                account, provider, ..
            }
            | TopCommand::Agent {
                account, provider, ..
            },
        ) => account
            .iter()
            .find(|flag| !flag.contains('='))
            .map(String::as_str)
            .or(provider.as_deref()),
        Some(TopCommand::Models { provider }) => provider.as_deref(),
        Some(TopCommand::Key {
            action:
                Some(
                    KeyAction::Set { account }
                    | KeyAction::Clear { account }
                    | KeyAction::Status { account },
                ),
        }) => account.as_deref(),
        _ => None,
    }
}

#[cfg(test)]
#[test]
fn hidden_reload_and_export_version_queries_are_accepted() {
    for query in ["--reload-probe", "--export-version"] {
        assert!(parse([OsString::from("version"), OsString::from(query)]).is_ok());
    }
    assert!(matches!(
        parse(
            [
                "version",
                "--reopen-probe",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
            ]
            .map(OsString::from)
        ),
        Ok(Command::ReopenProbe { .. })
    ));
}

#[cfg(all(test, feature = "maintainer"))]
#[test]
fn maintainer_session_rehearsal_is_accepted() {
    assert!(
        parse(
            [
                "session",
                "rehearse",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
            ]
            .map(OsString::from)
        )
        .is_ok()
    );
}

fn parse_max_turns(value: &str) -> Result<u8, String> {
    value
        .parse::<u8>()
        .ok()
        .filter(|value| (1..=100).contains(value))
        .ok_or_else(|| "max-turns must be 1..=100".to_owned())
}

fn clap_error(error: clap::Error) -> String {
    use clap::error::ErrorKind;

    match error.kind() {
        ErrorKind::InvalidSubcommand => {
            "unknown command; run `plakd help` for the command list".to_owned()
        }
        ErrorKind::UnknownArgument => {
            "unknown option or unexpected argument; use `plakd <command> --help`".to_owned()
        }
        ErrorKind::MissingRequiredArgument => {
            "missing required argument; use `plakd <command> --help`".to_owned()
        }
        ErrorKind::TooManyValues => {
            "too many values for an argument; use `plakd <command> --help`".to_owned()
        }
        ErrorKind::TooFewValues | ErrorKind::NoEquals => {
            "missing value for an option; use `plakd <command> --help`".to_owned()
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            "invalid argument value; use `plakd <command> --help`".to_owned()
        }
        ErrorKind::ArgumentConflict => {
            "conflicting arguments; use `plakd <command> --help`".to_owned()
        }
        _ => "could not parse command; run `plakd help` for usage".to_owned(),
    }
}

fn is_known_command(name: &str) -> bool {
    matches!(
        name,
        "accounts"
            | "login"
            | "logout"
            | "status"
            | "refresh"
            | "key"
            | "secret"
            | "mcp"
            | "runner"
            | "host"
            | "run"
            | "agent"
            | "wire"
            | "session"
            | "state"
            | "store"
            | "tree"
            | "usage"
            | "doctor"
            | "gateway"
            | "config"
            | "telemetry"
            | "models"
            | "help"
            | "version"
    ) || is_maintainer_command(name)
}

#[cfg(feature = "maintainer")]
fn is_maintainer_command(name: &str) -> bool {
    matches!(name, "compatibility" | "compact" | "fork")
}

#[cfg(not(feature = "maintainer"))]
fn is_maintainer_command(_name: &str) -> bool {
    false
}

#[derive(Debug, Parser)]
#[command(
    name = "plakd",
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true,
    infer_subcommands = false
)]
struct Cli {
    #[arg(short = 'h', long = "help", action = ArgAction::SetTrue, hide = true)]
    help: bool,
    #[arg(short = 'V', long = "version", action = ArgAction::SetTrue, hide = true)]
    version: bool,
    #[command(subcommand)]
    command: Option<TopCommand>,
}

impl Cli {
    fn into_command(self) -> Result<Command, String> {
        if self.help {
            return Ok(Command::Help);
        }
        if self.version {
            return Ok(Command::Version);
        }
        match self.command {
            None => Ok(Command::Help),
            Some(command) => command.into_command(),
        }
    }
}

#[derive(Debug, Subcommand)]
enum TopCommand {
    #[command(hide = true)]
    Snapshot {
        #[command(subcommand)]
        action: crate::snapshot_install::Action,
    },
    #[command(hide = true)]
    Deploy {
        #[command(subcommand)]
        action: crate::deploy::Action,
    },
    #[command(hide = true)]
    DeployCanary(crate::deploy::canary::Options),
    #[command(hide = true)]
    DeployTarget,
    #[command(hide = true)]
    DeployCredentialGeneration {
        account: String,
    },
    /// List or remove local account selections.
    Accounts {
        #[command(subcommand)]
        action: AccountAction,
    },
    /// OAuth / subscription login.
    Login {
        /// Account id (`codex` | `grok` | `claude`). Omit to print usage.
        account: Option<String>,
        /// The browser is on another machine: codex shows a one-time device
        /// code; grok and claude take the code their page displays.
        #[arg(long, action = ArgAction::SetTrue)]
        remote: bool,
    },
    /// OAuth logout.
    Logout {
        /// Account id (`codex` | `grok` | `claude`). Omit to print usage.
        account: Option<String>,
        #[arg(long, action = ArgAction::SetTrue)]
        local: bool,
    },
    /// Runtime store headroom and credential readiness.
    Status {
        account: Option<String>,
        /// Print local runtime store headroom as JSON without contacting the gateway.
        #[arg(long, conflicts_with = "account")]
        json: bool,
    },
    /// Refresh OAuth credential.
    Refresh {
        /// Account id (`codex` | `grok` | `claude`). Omit to print usage.
        account: Option<String>,
    },
    /// API key management.
    Key {
        #[command(subcommand)]
        action: Option<KeyAction>,
    },
    /// Named secrets that config.toml refers to by name.
    Secret {
        #[command(subcommand)]
        action: Option<SecretAction>,
    },
    /// MCP servers: add, inspect, test and enable them on templates.
    Mcp {
        #[command(subcommand)]
        action: Option<McpAction>,
    },
    /// This machine's runner daemon, and this host's runners.
    Runner {
        #[command(subcommand)]
        action: Option<RunnerAction>,
    },
    #[cfg(feature = "maintainer")]
    Compatibility {
        /// Account id. Omit to print usage.
        provider: Option<String>,
    },
    /// Read-only catalog discovery: available in every build.
    Models {
        /// Account id (`codex` | `grok` | `claude` | `openrouter` | `openai` | `anthropic` | `xai-api`). Omit to print usage.
        provider: Option<String>,
    },
    Run {
        /// Root account, or repeatable `<kind>=<ref>` for a profile kind.
        #[arg(long = "account")]
        account: Vec<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        composition: Option<String>,
        #[arg(long)]
        protocol: Option<String>,
        #[arg(long = "session")]
        session: Option<String>,
        #[arg(long, action = ArgAction::SetTrue)]
        print_session_id: bool,
        #[arg(long, default_value_t = 25, value_parser = parse_max_turns)]
        max_turns: u8,
        #[arg(long)]
        timeout: Option<u64>,
        /// Leave leftover background jobs alive at exit, giving up ownership.
        #[arg(long, action = ArgAction::SetTrue)]
        leave_jobs_running: bool,
        /// Attach an image file (png, jpeg, gif, webp; at most 3.75 MiB). Repeatable.
        #[arg(long = "image", value_name = "PATH")]
        image: Vec<std::path::PathBuf>,
        #[arg(last = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// Long-lived JSONL host client. Stdio is the default pipe.
    Agent {
        /// Root account, or repeatable `<kind>=<ref>` for a profile kind.
        #[arg(long = "account")]
        account: Vec<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        composition: Option<String>,
        #[arg(long)]
        protocol: Option<String>,
        #[arg(long = "session")]
        session: Option<String>,
        /// The stdio protocol: `v1` (JSONL, the default) or `v2` (the
        /// framed protocol over the hidden, unsupported single-session entrypoint;
        /// supported wire v2 clients use the host socket or stdio bridge).
        #[arg(long = "wire", hide = true, value_parser = ["v1", "v2"])]
        stdio_wire: Option<String>,
    },
    /// The per-user host process, serving sessions on a private control socket.
    #[command(args_conflicts_with_subcommands = true)]
    Host {
        #[command(subcommand)]
        action: Option<HostAction>,
        /// Also serve the sessions' wire v2 at `/wire`, and the web UI, on
        /// `unix:<path>` (the `tailscale serve unix:` deployment) or, in dev
        /// mode, `127.0.0.1:<port>`.
        #[arg(long)]
        web: Option<String>,
        /// The web UI's build directory, with its `csp.json`.
        #[arg(long, requires = "web")]
        ui: Option<std::path::PathBuf>,
        /// Dev mode, loopback only: every web request is this login.
        #[arg(long, requires = "web")]
        dev_login: Option<String>,
        /// Seconds an idle session stays resident (300).
        #[arg(long, hide = true)]
        idle_release: Option<u64>,
    },
    /// Inspect this build's wire v2 integration contract.
    Wire {
        #[command(subcommand)]
        action: Option<WireAction>,
    },
    #[cfg(feature = "maintainer")]
    Fork {
        /// Source session uuid. Omit to print usage.
        session: Option<String>,
        #[arg(long = "yes", action = ArgAction::SetTrue)]
        confirmed: bool,
    },
    #[cfg(feature = "maintainer")]
    Compact {
        /// Session uuid. Omit to print usage.
        session: Option<String>,
        #[arg(long = "yes", action = ArgAction::SetTrue)]
        confirmed: bool,
    },
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Credential gateway: the only process that opens the credential store
    /// when it is serving.
    Gateway {
        #[command(subcommand)]
        action: Option<GatewayAction>,
    },
    Doctor,
    /// Back up the current runtime, product and agent-tree stores.
    Store {
        #[command(subcommand)]
        action: Option<StoreAction>,
    },
    State {
        #[command(subcommand)]
        action: Option<StateAction>,
    },
    Tree {
        #[command(subcommand)]
        action: Option<TreeAction>,
    },
    Usage {
        #[arg(long, conflicts_with = "session_id")]
        account: Option<String>,
        #[arg(long = "session")]
        session_id: Option<String>,
    },
    Session {
        #[command(subcommand)]
        action: Option<SessionAction>,
    },
    Telemetry {
        #[command(subcommand)]
        action: Option<TelemetryAction>,
    },
    Version {
        #[arg(long, hide = true, conflicts_with_all = ["state_key", "reload_probe", "export_version", "write_binary_manifest", "carry_probe", "reopen_probe", "host_upgrade_reopen", "host_upgrade_probe", "host_upgrade_capacity", "host_upgrade_service"])]
        build_info: bool,
        #[arg(long, hide = true, conflicts_with_all = ["state_key", "reload_probe", "export_version", "write_binary_manifest", "carry_probe", "reopen_probe", "host_upgrade_capacity"])]
        host_upgrade_probe: bool,
        #[arg(long, hide = true, requires = "host_upgrade_probe")]
        host_upgrade_service: Option<std::path::PathBuf>,
        #[arg(long, hide = true, conflicts_with_all = ["state_key", "reload_probe", "export_version", "write_binary_manifest", "carry_probe", "reopen_probe"])]
        host_upgrade_capacity: Option<std::path::PathBuf>,
        /// Hidden: print only this build's state key (`/reload` asks it).
        #[arg(long = "state-key", action = ArgAction::SetTrue, hide = true)]
        state_key: bool,
        #[arg(long, hide = true, conflicts_with_all = ["state_key", "reload_probe", "export_version"])]
        write_binary_manifest: Option<std::path::PathBuf>,
        #[arg(long, hide = true, conflicts_with_all = ["state_key", "reload_probe", "export_version", "write_binary_manifest"])]
        carry_probe: Option<std::path::PathBuf>,
        #[arg(long, action = ArgAction::SetTrue, hide = true, conflicts_with_all = ["state_key", "export_version"])]
        reload_probe: bool,
        /// Hidden: whether an exact reopen of this session would attach on
        /// this build (`/reload` asks it before teardown).
        #[arg(long, hide = true, conflicts_with_all = ["state_key", "reload_probe", "export_version", "write_binary_manifest", "carry_probe"])]
        reopen_probe: Option<String>,
        #[arg(long, hide = true, requires = "reopen_probe")]
        host_upgrade_reopen: bool,
        #[arg(long, hide = true, requires = "host_upgrade_reopen")]
        child_binding_probe: bool,
        #[arg(long, hide = true, requires = "host_upgrade_reopen")]
        store_probe: Option<std::path::PathBuf>,
        #[arg(long, action = ArgAction::SetTrue, hide = true, conflicts_with = "state_key")]
        export_version: bool,
    },
    Help,
}

#[derive(Debug, clap::Subcommand)]
enum AccountAction {
    /// List local accounts without reading credential values.
    List,
    /// Remove an already signed-out named slot, or clear a named API key.
    Rm {
        account: String,
        #[arg(long = "yes")]
        confirmed: bool,
    },
}

#[derive(Debug, Subcommand)]
enum GatewayAction {
    /// Write gateway.toml and create the gateway directory. Never touches the store.
    Init {
        #[arg(long, value_name = "IP:PORT")]
        tcp: Option<String>,
    },
    /// Foreground service. Takes the exclusive holder lock, then listens.
    Serve,
    /// Write a user service definition without starting it.
    Install {
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove the user service definition. Does not stop a running service.
    Uninstall,
    /// Versions, listeners and flags. `--dry-run` prints paths without the lock.
    Status {
        #[arg(long)]
        dry_run: bool,
    },
    Caller {
        #[command(subcommand)]
        action: Option<GatewayCallerAction>,
    },
    /// Engage the kill switch. Remote admin may do this; `--cut` cancels in-flight relays.
    Disable {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        caller: Option<String>,
        #[arg(long)]
        cut: bool,
    },
    /// Clear the kill switch. Local administrator only.
    Enable {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        caller: Option<String>,
    },
    /// Stop accepting, wait for in-flight work, then exit. Local administrator only.
    Drain,
    /// Print audit metadata. Never prints secrets.
    Audit {
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        caller: Option<String>,
    },
    /// Negotiate a version and list accounts.
    Check,
}

#[derive(Debug, Subcommand)]
enum GatewayCallerAction {
    Add {
        id: String,
        #[arg(long = "source", required = true)]
        sources: Vec<String>,
        #[arg(long = "admin")]
        admin: Vec<String>,
        #[arg(long)]
        key_out: Option<std::path::PathBuf>,
    },
    List,
    Rm {
        id: String,
    },
    Rotate {
        id: String,
    },
}

#[derive(Debug, clap::Subcommand)]
enum HostAction {
    /// Upgrade the running host within its current state key.
    Upgrade {
        #[arg(long, required_unless_present_any = ["abandon", "wait"])]
        expect_binary: Option<String>,
        #[arg(long, hide = true)]
        abandon: Option<String>,
        #[arg(long, hide = true)]
        wait: Option<String>,
        /// Same-key boundary drain: default 60s; 0, integer ms/s/m/h, or idle.
        #[arg(long)]
        drain: Option<crate::command::HostDrain>,
        /// Save a private, size-bounded SQLite backup before requesting handoff.
        #[arg(long)]
        backup_store: bool,
        #[arg(long, hide = true, requires = "inspect_source")]
        inspect_runner: Vec<String>,
        #[arg(long, hide = true, requires = "inspect_runner")]
        inspect_source: Option<String>,
    },
    /// Inspect an inherited upgrade verdict or clear a checked recovery fence.
    UpgradeVerdict {
        /// Clear this exact fenced verdict after stopping the host and checking the stores.
        #[arg(long, value_name = "ID")]
        acknowledge: Option<uuid::Uuid>,
    },
    /// Relay wire bytes to this home's host without consuming its handshake.
    Connect {
        #[arg(long, required = true)]
        stdio: bool,
        /// Start an absent local host, unless its user unit is installed.
        #[arg(long)]
        auto_start: bool,
    },
    #[command(name = "__start-auto", hide = true)]
    StartAuto,
    #[command(name = "__supervise-auto", hide = true)]
    SuperviseAuto,
    #[command(name = "__serve-auto", hide = true)]
    ServeAuto,
    /// Write a systemd user unit or macOS LaunchAgent without starting it.
    Install {
        #[arg(long, value_name = "unix:ABSOLUTE_PATH")]
        web: String,
        #[arg(long, value_name = "ABSOLUTE_DIR")]
        ui: std::path::PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, hide = true, num_args = 0..=1, default_missing_value = "")]
        dev_login: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum WireAction {
    /// Print this build's wire v2 JSON Schema, with its protocol version.
    Schema,
}

#[derive(Debug, Subcommand)]
enum KeyAction {
    Set { account: Option<String> },
    Clear { account: Option<String> },
    Status { account: Option<String> },
}

impl KeyAction {
    fn into_command(self) -> Result<Command, String> {
        match self {
            // A bare leaf routes to the parent group's help.
            Self::Set { account: None } | Self::Clear { account: None } => {
                Ok(Command::CommandHelp {
                    command: "key".to_owned(),
                })
            }
            Self::Set {
                account: Some(account),
            } => {
                if !crate::product_secrets::is_api_key_account(&account) {
                    return Err("key set requires openai|xai-api|anthropic|openrouter".to_owned());
                }
                Ok(Command::KeySet {
                    account: account.parse().map_err(
                        |error: plakous_hosted::capabilities::AccountAdmissionError| {
                            error.to_string()
                        },
                    )?,
                })
            }
            Self::Clear {
                account: Some(account),
            } => {
                if !crate::product_secrets::is_api_key_account(&account) {
                    return Err("key clear requires openai|xai-api|anthropic|openrouter".to_owned());
                }
                Ok(Command::KeyClear {
                    account: account.parse().map_err(
                        |error: plakous_hosted::capabilities::AccountAdmissionError| {
                            error.to_string()
                        },
                    )?,
                })
            }
            Self::Status { account } => {
                if account
                    .as_deref()
                    .is_some_and(|account| !crate::product_secrets::is_api_key_account(account))
                {
                    return Err(
                        "key status requires openai|xai-api|anthropic|openrouter".to_owned()
                    );
                }
                Ok(Command::KeyStatus {
                    account: account
                        .map(|value| {
                            value.parse().map_err(
                                |error: plakous_hosted::capabilities::AccountAdmissionError| {
                                    error.to_string()
                                },
                            )
                        })
                        .transpose()?,
                })
            }
        }
    }
}

#[derive(Debug, Subcommand)]
enum RunnerAction {
    Serve {
        /// Serve this home's home-local runner, which carries its own host's
        /// processes, instead of the machine's runner for paired hosts.
        /// Hidden: only a host starts it.
        #[arg(long, hide = true, action = ArgAction::SetTrue)]
        local: bool,
    },
    Connect {
        /// Bridge standard input and output to the daemon (what a host's
        /// ssh runs).
        #[arg(long, action = ArgAction::SetTrue)]
        stdio: bool,
        /// Select an already installed immutable runner binary before connecting.
        #[arg(long, requires = "stdio")]
        binary: Option<String>,
    },
    Allow {
        host_id: Option<String>,
    },
    Ps {
        name: Option<String>,
    },
    Pair {
        name: Option<String>,
    },
    /// Remove this host's pairing pin. Refuses while a job on that runner is still running.
    Unpair {
        name: Option<String>,
        /// Drop the pin even when a job has no terminal evidence, and name the processes left running.
        #[arg(long, action = ArgAction::SetTrue)]
        abandon_jobs: bool,
    },
    /// This machine's runner secrets, which an MCP server placed on this
    /// runner reads.
    Secret {
        #[command(subcommand)]
        action: Option<SecretAction>,
    },
}

impl RunnerAction {
    fn into_command(self) -> Result<Command, String> {
        use crate::commands::runner::RunnerCommand;
        let help = || {
            Ok(Command::CommandHelp {
                command: "runner".to_owned(),
            })
        };
        let runner_secret = crate::home::paths::SecretShelf::Runner;
        Ok(Command::Runner(match self {
            Self::Secret {
                action: Some(action),
            } => return action.into_command(runner_secret, "runner"),
            Self::Secret { action: None } => return help(),
            Self::Serve { local: false } => RunnerCommand::Serve,
            Self::Serve { local: true } => RunnerCommand::ServeLocal,
            Self::Connect {
                stdio: true,
                binary,
            } => RunnerCommand::Connect { binary },
            // A bare leaf routes to the parent group's help.
            Self::Connect { stdio: false, .. }
            | Self::Allow { host_id: None }
            | Self::Pair { name: None }
            | Self::Unpair { name: None, .. } => {
                return help();
            }
            Self::Allow {
                host_id: Some(host_id),
            } => RunnerCommand::Allow { host_id },
            Self::Ps { name } => {
                if let Some(name) = &name {
                    crate::product_config::validate_runner_name(name)?;
                }
                RunnerCommand::Ps { name }
            }
            Self::Pair { name: Some(name) } => {
                crate::product_config::validate_runner_name(&name)?;
                RunnerCommand::Pair { name }
            }
            Self::Unpair {
                name: Some(name),
                abandon_jobs,
            } => {
                crate::product_config::validate_runner_name(&name)?;
                RunnerCommand::Unpair { name, abandon_jobs }
            }
        }))
    }
}

#[derive(Debug, Subcommand)]
enum SecretAction {
    Set { name: Option<String> },
    Rm { name: Option<String> },
    List,
}

impl SecretAction {
    /// The command on `kind`; a bare leaf routes to `group`'s help.
    fn into_command(
        self,
        kind: crate::home::paths::SecretShelf,
        group: &str,
    ) -> Result<Command, String> {
        match self {
            Self::Set { name: None } | Self::Rm { name: None } => Ok(Command::CommandHelp {
                command: group.to_owned(),
            }),
            Self::Set { name: Some(name) } => {
                crate::product_secrets::validate_secret_name(&name)?;
                Ok(Command::SecretSet { shelf: kind, name })
            }
            Self::Rm { name: Some(name) } => {
                crate::product_secrets::validate_secret_name(&name)?;
                Ok(Command::SecretRm { shelf: kind, name })
            }
            Self::List => Ok(Command::SecretList { shelf: kind }),
        }
    }
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    Show,
    Set {
        key: Option<String>,
        value: Option<String>,
        /// Which file the write lands in: `user` (the global home) or
        /// `project` (the trusted `.plak/` override).
        #[arg(long, value_parser = ["user", "global", "project"])]
        scope: Option<String>,
    },
    Edit,
    Init,
    /// Admit the discovered project `.plak` configuration for this and later
    /// sessions.
    Trust,
    /// Stop applying the discovered project `.plak` configuration.
    Untrust,
}

impl ConfigAction {
    fn into_command(self) -> Result<Command, String> {
        match self {
            Self::Show => Ok(Command::ConfigShow),
            // A bare leaf routes to the parent group's help.
            Self::Set { key: None, .. } => Ok(Command::CommandHelp {
                command: "config".to_owned(),
            }),
            Self::Set { value: None, .. } => {
                Err("config set requires a value: plakd config set <key> <value>".to_owned())
            }
            Self::Set {
                key: Some(key),
                value: Some(value),
                scope,
            } => {
                let scope = scope
                    .as_deref()
                    .map_or(
                        Some(crate::product_config::ConfigWriteScope::User),
                        crate::product_config::ConfigWriteScope::parse,
                    )
                    .ok_or_else(|| "--scope must be user|global|project".to_owned())?;
                Ok(Command::ConfigSet { key, value, scope })
            }
            Self::Edit => Ok(Command::ConfigEdit),
            Self::Init => Ok(Command::ConfigInit),
            Self::Trust => Ok(Command::ConfigTrust),
            Self::Untrust => Ok(Command::ConfigUntrust),
        }
    }
}

#[derive(Debug, Subcommand)]
enum TelemetryAction {
    Status {
        /// Also print the recommended read-only SQL views.
        #[arg(long, action = ArgAction::SetTrue)]
        views: bool,
    },
    Sql {
        query: Option<String>,
        #[arg(long, action = ArgAction::SetTrue)]
        json: bool,
    },
    Prune {
        #[arg(long = "max-age-days")]
        max_age_days: Option<u64>,
        #[arg(long = "max-bytes")]
        max_bytes: Option<u64>,
        #[arg(long = "yes", action = ArgAction::SetTrue)]
        confirmed: bool,
    },
}

impl TelemetryAction {
    fn into_command(self) -> Result<Command, String> {
        match self {
            Self::Status { views } => Ok(Command::TelemetryStatus { views }),
            // A bare leaf routes to the parent group's help.
            Self::Sql { query: None, .. } => Ok(Command::CommandHelp {
                command: "telemetry".to_owned(),
            }),
            Self::Sql {
                query: Some(query),
                json,
            } => Ok(Command::TelemetrySql { query, json }),
            Self::Prune {
                max_age_days,
                max_bytes,
                confirmed,
            } => {
                if max_age_days.is_none() && max_bytes.is_none() && !confirmed {
                    return Err(
                        "telemetry prune deletes whole sessions; pass --max-age-days N, --max-bytes N, or --yes to prune to the configured retention"
                            .to_owned(),
                    );
                }
                Ok(Command::TelemetryPrune {
                    max_age_days,
                    max_bytes,
                })
            }
        }
    }
}

#[derive(Debug, Subcommand)]
enum StoreAction {
    /// Copy one consistent SQLite snapshot into a new private directory.
    Backup { directory: std::path::PathBuf },
}

#[derive(Debug, Subcommand)]
enum StateAction {
    PruneLegacy {
        #[arg(long)]
        yes: bool,
        /// Forget ownership inventories whose launch repository is missing.
        #[arg(long)]
        forget_missing_launches: bool,
        /// Delete orphaned session references, which may be the only surviving exports.
        #[arg(long)]
        prune_orphaned_references: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TreeAction {
    /// Abort a pending child retirement before archive, with the host stopped.
    AbortRetirement { tree_id: String, child: String },
    /// Release an agent tree's custody without its holder (break-glass).
    Release {
        tree_id: Option<String>,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TraceReceiptAction {
    /// Record an attestation after verifying the durable remote archive checksum.
    Record {
        /// Exact child address from the trace source.
        id: String,
        /// Attempt ID from the trace source.
        #[arg(long)]
        attempt: String,
        /// Canonical head from the trace source.
        #[arg(long)]
        head: String,
        /// Runtime revision from the trace source.
        #[arg(long)]
        revision: u64,
        /// Lowercase hex SHA-256 of the verified archive artifact.
        #[arg(long)]
        sha256: String,
        /// Opaque archive identifier (the receipt ID).
        #[arg(long)]
        archive: String,
    },
}

impl TraceReceiptAction {
    fn into_command(self) -> Command {
        let Self::Record {
            id,
            attempt,
            head,
            revision,
            sha256,
            archive,
        } = self;
        Command::SessionTraceReceipt(crate::command::TraceReceiptRecord {
            id,
            attempt,
            head,
            revision,
            sha256,
            archive,
        })
    }
}

#[derive(Debug, Subcommand)]
enum SessionAction {
    #[command(hide = true)]
    CheckBinding {
        #[arg(long)]
        all: bool,
        #[arg(long, hide = true, requires = "all")]
        summary: bool,
        #[arg(long, hide = true, requires = "all")]
        store_probe: Option<std::path::PathBuf>,
    },
    /// List sessions for the current workspace.
    List {
        #[arg(long)]
        include_archived: bool,
    },
    /// Set a home-local title override, or clear it with --clear.
    Rename {
        id: Option<String>,
        title: Option<String>,
        #[arg(long, conflicts_with = "title")]
        clear: bool,
    },
    /// Make an archived session writable again, without starting work.
    Unarchive { id: Option<String> },
    /// Hide a dormant, quiescent session and make it read-only.
    Archive {
        id: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Show session summary + event counts.
    Show { id: Option<String> },
    /// Print agent-readable event log (JSONL).
    Log {
        id: Option<String>,
        /// Filter one kind namespace (e.g. `tool` includes `tool.started`).
        #[arg(long)]
        kind: Option<String>,
        /// Number of trailing events (default 100).
        #[arg(long, default_value_t = 100)]
        tail: usize,
        /// Emit raw JSONL only (no headers).
        #[arg(long, action = ArgAction::SetTrue)]
        json: bool,
    },
    /// Recover an interrupted turn: reopen the session, record once what the
    /// turn left unobserved, and release it again.
    Recover { id: Option<String> },
    /// Offline reload checks; never constructs a credentialed provider.
    #[cfg(feature = "maintainer")]
    #[command(hide = true)]
    Rehearse { id: String },
    /// Write the portable export document of a settled session.
    Export {
        id: Option<String>,
        /// Where to write it; default a new private file under `~/.plak/carry/`.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Write the retained canonical trace of one settled child.
    Trace {
        id: Option<String>,
        /// Where to write it; default a new private file under `~/.plak/traces/`.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// List eligible child trace revisions across this Plak home.
    Traces {
        #[arg(long, required = true)]
        settled: bool,
        /// Include children settled at or after this RFC 3339 timestamp.
        #[arg(long, value_parser = parse_trace_since)]
        since: Option<time::OffsetDateTime>,
        #[arg(long, required = true)]
        json: bool,
    },
    /// Record a verified child trace archive receipt.
    #[command(subcommand)]
    TraceReceipt(TraceReceiptAction),
    /// Create a new session from an export document.
    Import { path: Option<std::path::PathBuf> },
    /// Delete a session and all of its retained history. Refused while it is
    /// open, while its agent tree has running children, or while other
    /// sessions continue from it; never refused because the store is full.
    Delete {
        id: Option<String>,
        /// Skip the interactive confirmation.
        #[arg(long, action = ArgAction::SetTrue)]
        yes: bool,
        /// Abandon unresolved child obligations instead of running recovery.
        #[arg(long, action = ArgAction::SetTrue)]
        abandon_children: bool,
        /// Delete even when a job has no terminal evidence, and name the processes left running.
        #[arg(long, action = ArgAction::SetTrue)]
        abandon_jobs: bool,
    },
    /// Reclaim superseded checkpoint history of a session, or with --all of
    /// every session in this home not open in a running plakd, and finish
    /// interrupted deletes. Never refused because the store is full.
    Gc {
        #[arg(conflicts_with = "all")]
        id: Option<String>,
        /// Every session in this home, across workspaces.
        #[arg(long, action = ArgAction::SetTrue)]
        all: bool,
    },
    /// Ask a running interactive session to reload onto the installed build.
    Reload {
        id: Option<String>,
        /// The installed build's binary id the reload must land on.
        #[arg(long)]
        expect_binary: Option<String>,
    },
}

impl SessionAction {
    fn into_command(self) -> Result<Command, String> {
        match self {
            Self::List { include_archived } => Ok(Command::SessionList { include_archived }),
            Self::CheckBinding {
                all: true,
                summary,
                store_probe,
            } => Ok(Command::SessionCheckBinding {
                summary,
                store_probe,
            }),
            Self::CheckBinding { all: false, .. } => Ok(Command::CommandHelp {
                command: "session".to_owned(),
            }),
            Self::Rename {
                id: Some(id),
                title,
                clear,
            } => {
                if title.is_none() && !clear {
                    return Err("session rename needs a title or --clear".to_owned());
                }
                Ok(Command::SessionRename { id, title })
            }
            Self::Unarchive { id: Some(id) } => Ok(Command::SessionUnarchive { id }),
            Self::Archive { id: Some(id), yes } => Ok(Command::SessionArchive { id, yes }),
            // A bare leaf routes to the parent group's help.
            Self::Show { id: None }
            | Self::Rename { id: None, .. }
            | Self::Unarchive { id: None }
            | Self::Archive { id: None, .. }
            | Self::Log { id: None, .. }
            | Self::Recover { id: None }
            | Self::Export { id: None, .. }
            | Self::Trace { id: None, .. }
            | Self::Import { path: None }
            | Self::Delete { id: None, .. }
            | Self::Gc {
                id: None,
                all: false,
            } => Ok(Command::CommandHelp {
                command: "session".to_owned(),
            }),
            Self::Show { id: Some(id) } => Ok(Command::SessionShow { id }),
            Self::Recover { id: Some(id) } => Ok(Command::SessionRecover { id }),
            #[cfg(feature = "maintainer")]
            Self::Rehearse { id } => Ok(Command::SessionRehearse { id }),
            Self::Export { id: Some(id), out } => Ok(Command::SessionExport { id, out }),
            Self::Trace { id: Some(id), out } => Ok(Command::SessionTrace { id, out }),
            Self::Traces {
                settled: true,
                since,
                json: true,
            } => Ok(Command::SessionTraces { since }),
            Self::Traces { .. } => Err("session traces requires --settled and --json".to_owned()),
            Self::TraceReceipt(action) => Ok(action.into_command()),
            Self::Reload {
                id: Some(id),
                expect_binary: Some(expect_binary),
            } => Ok(Command::SessionReloadRequest { id, expect_binary }),
            Self::Reload { .. } => Ok(Command::CommandHelp {
                command: "session".to_owned(),
            }),
            Self::Import { path: Some(path) } => Ok(Command::SessionImport { path }),
            Self::Delete {
                id: Some(id),
                yes,
                abandon_children,
                abandon_jobs,
            } => Ok(Command::SessionDelete {
                id,
                yes,
                abandon_children,
                abandon_jobs,
            }),
            Self::Gc { all: true, .. } => Ok(Command::SessionGc { id: None }),
            Self::Gc { id, all: false } => Ok(Command::SessionGc { id }),
            Self::Log {
                id: Some(id),
                kind,
                tail,
                json,
            } => {
                if tail == 0 {
                    return Err("session log --tail must be >= 1".to_owned());
                }
                Ok(Command::SessionLog {
                    id,
                    kind,
                    tail,
                    json,
                })
            }
        }
    }
}

fn parse_trace_since(value: &str) -> Result<time::OffsetDateTime, String> {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map_err(|_| "expected an RFC 3339 timestamp".to_owned())
}

/// The prompt words after `--` and the `--image` paths of one `plakd run`.
struct RunPrompt {
    words: Vec<String>,
    images: Vec<std::path::PathBuf>,
}

fn parse_oauth_account(value: &str) -> Result<AccountRef, String> {
    let account: AccountRef = value
        .parse()
        .map_err(|error: plakous_hosted::capabilities::AccountAdmissionError| error.to_string())?;
    if account.vertical().is_api() {
        return Err(format!(
            "account `{value}` is not an OAuth account; use plakd key set for API keys"
        ));
    }
    Ok(account)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
