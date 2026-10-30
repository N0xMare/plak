//! Clap-backed CLI parsing for the Plak application.

pub(crate) mod render;

use std::ffi::OsString;

use clap::{ArgAction, Parser, Subcommand};
use plakous::subscriptions::{NativeLoginMode, NativeProvider};

use crate::command::{Command, NATIVE_RUN_MAX_PROMPT_BYTES, RunEffort};

/// Parse argv tokens (without binary name) into a product [`Command`].
pub(crate) fn parse(arguments: impl IntoIterator<Item = OsString>) -> crate::CliResult<Command> {
    parse_with_image_capability(arguments, crate::build_features::require_feature("images"))
}

pub(super) fn parse_with_image_capability(
    arguments: impl IntoIterator<Item = OsString>,
    images_available: Result<(), plakous_hosted::capabilities::CapabilityUnavailable>,
) -> crate::CliResult<Command> {
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

    // Bare `plak` → TUI.
    if values.is_empty() {
        return Ok(Command::Tui {
            session_id: None,
            resume_picker: false,
        });
    }

    match values.as_slice() {
        [single] if single == "--help" || single == "-h" || single == "help" => {
            return Ok(Command::Help);
        }
        [single] if single == "--version" || single == "-V" || single == "version" => {
            return Ok(Command::Version);
        }
        [single] if single == "--resume" => {
            return Ok(Command::Resume { session_id: None });
        }
        _ => {}
    }

    // `plak help <cmd>`
    if values.first().map(String::as_str) == Some("help") {
        if values.len() == 1 {
            return Ok(Command::Help);
        }
        return Ok(Command::CommandHelp {
            command: values[1].clone(),
        });
    }

    // Recursive: `plak <cmd> --help` / `-h` — only for commands this build actually owns.
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

    // Hidden: `/reload` onto a build with another state key execs
    // `plak --import-resume <file>`.
    if values.first().map(String::as_str) == Some("--import-resume") {
        return match values.as_slice() {
            [_, path] if !path.starts_with('-') => Ok(Command::ImportResume { path: path.into() }),
            _ => Err("plak --import-resume takes exactly one export file".to_owned()),
        }
        .map_err(usage);
    }

    // `plak --resume [uuid]`
    if values.first().map(String::as_str) == Some("--resume") {
        return match values.as_slice() {
            [_] => Ok(Command::Resume { session_id: None }),
            [_, session_id] if !session_id.starts_with('-') => Ok(Command::Resume {
                session_id: Some(session_id.clone()),
            }),
            [_, option] if option.starts_with('-') => Err(
                "unknown option for `plak --resume`; expected an optional session UUID".to_owned(),
            ),
            _ => Err("plak --resume accepts at most one session UUID".to_owned()),
        }
        .map_err(usage);
    }

    let cli = Cli::try_parse_from(values.iter().map(String::as_str)).map_err(|error| {
        let verb = values.first().filter(|verb| is_known_command(verb));
        if verb.is_some_and(|verb| verb == "run")
            && error.kind() == clap::error::ErrorKind::UnknownArgument
            && !values.iter().any(|value| value == "--")
        {
            return "put the prompt after `--`: plak run ... -- '<prompt>'; use `plak run --help`"
                .to_owned();
        }
        let message = clap_error(error);
        match verb {
            Some(verb) => message.replace("<command>", verb),
            None => message.replace("plak <command> --help", "plak help"),
        }
    }).map_err(usage)?;
    let account = match cli.command.as_ref() {
        Some(
            TopCommand::Login { account, .. }
            | TopCommand::Logout { account, .. }
            | TopCommand::Status { account }
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
        ) => account.as_deref().or(provider.as_deref()),
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
    };
    if account == Some("xai") {
        return Err(usage(
            "`xai` is ambiguous: use `grok` (SuperGrok subscription) or `xai-api` (API key)"
                .to_owned(),
        ));
    }
    if let Some(TopCommand::Run { image, .. }) = &cli.command {
        crate::build_features::admit_image_arguments(image, images_available)?;
    }
    cli.into_command().map_err(usage)
}

#[cfg(test)]
#[test]
fn hidden_reload_and_export_version_queries_are_accepted() {
    for query in ["--reload-probe", "--export-version"] {
        assert!(parse([OsString::from("version"), OsString::from(query)]).is_ok());
    }
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
            "unknown command; run `plak help` for the command list".to_owned()
        }
        ErrorKind::UnknownArgument => {
            "unknown option or unexpected argument; use `plak <command> --help`".to_owned()
        }
        ErrorKind::MissingRequiredArgument => {
            "missing required argument; use `plak <command> --help`".to_owned()
        }
        ErrorKind::TooManyValues => {
            "too many values for an argument; use `plak <command> --help`".to_owned()
        }
        ErrorKind::TooFewValues | ErrorKind::NoEquals => {
            "missing value for an option; use `plak <command> --help`".to_owned()
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            "invalid argument value; use `plak <command> --help`".to_owned()
        }
        ErrorKind::ArgumentConflict => {
            "conflicting arguments; use `plak <command> --help`".to_owned()
        }
        _ => "could not parse command; run `plak help` for usage".to_owned(),
    }
}

fn is_known_command(name: &str) -> bool {
    matches!(
        name,
        "login"
            | "logout"
            | "status"
            | "refresh"
            | "key"
            | "secret"
            | "mcp"
            | "run"
            | "agent"
            | "resume"
            | "session"
            | "state"
            | "usage"
            | "doctor"
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
    name = "plak",
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
            None => Ok(Command::Tui {
                session_id: None,
                resume_picker: false,
            }),
            Some(command) => command.into_command(),
        }
    }
}

#[derive(Debug, Subcommand)]
enum TopCommand {
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
    /// Credential readiness.
    Status {
        account: Option<String>,
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
        #[arg(long)]
        account: Option<String>,
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
        #[arg(long = "resume-tree", action = ArgAction::SetTrue)]
        resume_tree: bool,
        /// Attach an image file (png, jpeg, gif, webp; at most 3.75 MiB). Repeatable.
        #[arg(long = "image", value_name = "PATH")]
        image: Vec<std::path::PathBuf>,
        #[arg(last = true, allow_hyphen_values = true)]
        prompt: Vec<String>,
    },
    /// Long-lived JSONL host client. Stdio is the default pipe.
    Agent {
        #[arg(long)]
        account: Option<String>,
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
        #[arg(long = "resume-tree", action = ArgAction::SetTrue)]
        resume_tree: bool,
    },
    Resume {
        session: Option<String>,
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
    Doctor,
    State {
        #[command(subcommand)]
        action: Option<StateAction>,
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
        /// Hidden: print only this build's state key (`/reload` asks it).
        #[arg(long = "state-key", action = ArgAction::SetTrue, hide = true)]
        state_key: bool,
        #[arg(long, hide = true, conflicts_with_all = ["state_key", "reload_probe", "export_version"])]
        write_binary_manifest: Option<std::path::PathBuf>,
        #[arg(long, hide = true, conflicts_with_all = ["state_key", "reload_probe", "export_version", "write_binary_manifest"])]
        carry_probe: Option<std::path::PathBuf>,
        #[arg(long, action = ArgAction::SetTrue, hide = true, conflicts_with_all = ["state_key", "export_version"])]
        reload_probe: bool,
        #[arg(long, action = ArgAction::SetTrue, hide = true, conflicts_with = "state_key")]
        export_version: bool,
    },
    Help,
}

impl TopCommand {
    fn into_command(self) -> Result<Command, String> {
        match self {
            Self::Login { account: None, .. } => Ok(Command::CommandHelp {
                command: "login".to_owned(),
            }),
            Self::Login {
                account: Some(account),
                remote,
            } => Ok(Command::AuthLogin {
                provider: parse_oauth_account(&account)?,
                mode: if remote {
                    NativeLoginMode::Remote
                } else {
                    NativeLoginMode::Browser
                },
            }),
            Self::Logout { account: None, .. } => Ok(Command::CommandHelp {
                command: "logout".to_owned(),
            }),
            Self::Logout {
                account: Some(account),
                local,
            } => Ok(Command::AuthLogout {
                provider: parse_oauth_account(&account)?,
                local_only: local,
            }),
            Self::Status { account: None } => Ok(Command::StatusAll),
            Self::Status {
                account: Some(account),
            } => {
                if crate::product_secrets::is_api_key_account(&account) {
                    Ok(Command::KeyStatus {
                        account: Some(account),
                    })
                } else {
                    Ok(Command::AuthStatus(parse_oauth_account(&account)?))
                }
            }
            Self::Refresh { account: None } => Ok(Command::CommandHelp {
                command: "refresh".to_owned(),
            }),
            Self::Refresh {
                account: Some(account),
            } => Ok(Command::AuthRefresh(parse_oauth_account(&account)?)),
            Self::Key { action: None } => Ok(Command::CommandHelp {
                command: "key".to_owned(),
            }),
            Self::Key {
                action: Some(action),
            } => action.into_command(),
            Self::Secret { action: None } => Ok(Command::CommandHelp {
                command: "secret".to_owned(),
            }),
            Self::Secret {
                action: Some(action),
            } => action.into_command(),
            Self::Mcp { action: None } => Ok(Command::CommandHelp {
                command: "mcp".to_owned(),
            }),
            Self::Mcp {
                action: Some(action),
            } => action.into_command(),
            #[cfg(feature = "maintainer")]
            Self::Compatibility { provider: None } => Ok(Command::CommandHelp {
                command: "compatibility".to_owned(),
            }),
            #[cfg(feature = "maintainer")]
            Self::Compatibility {
                provider: Some(provider),
            } => Ok(Command::CompatibilityNotice(parse_oauth_account(
                &provider,
            )?)),
            Self::Models { provider: None } => Ok(Command::CommandHelp {
                command: "models".to_owned(),
            }),
            Self::Models {
                provider: Some(provider),
            } if provider == "openrouter" => Ok(Command::ModelsOpenRouter),
            Self::Models {
                provider: Some(provider),
            } => match plakous_hosted::binding::PlatformProvider::for_account(&provider) {
                Some(provider) => Ok(Command::ModelsPlatform(provider)),
                None => Ok(Command::Models(parse_oauth_account(&provider)?)),
            },
            Self::Run {
                account,
                provider,
                model,
                effort,
                composition,
                protocol,
                session,
                print_session_id,
                max_turns,
                timeout,
                leave_jobs_running,
                resume_tree,
                image,
                prompt,
            } => {
                let limits = plakous_hosted::runtime::RunBounds {
                    max_turns,
                    leave_jobs_running,
                    deadline: timeout
                        .map(|seconds| {
                            std::time::Instant::now()
                                .checked_add(std::time::Duration::from_secs(seconds))
                                .ok_or_else(|| "timeout is too large".to_owned())
                        })
                        .transpose()?,
                };
                // Bare `plak run` prints run help (no flags, no prompt).
                if account.is_none()
                    && provider.is_none()
                    && model.is_none()
                    && effort.is_none()
                    && composition.is_none()
                    && protocol.is_none()
                    && session.is_none()
                    && !print_session_id
                    && !resume_tree
                    && !leave_jobs_running
                    && max_turns == 25
                    && timeout.is_none()
                    && image.is_empty()
                    && prompt.is_empty()
                {
                    return Ok(Command::CommandHelp {
                        command: "run".to_owned(),
                    });
                }
                if let Some(session_id) = session {
                    if account.is_some()
                        || provider.is_some()
                        || model.is_some()
                        || effort.is_some()
                        || composition.is_some()
                        || protocol.is_some()
                    {
                        return Err(
                            "run --session uses the recorded binding and composition; omit --account/--model/--effort/--composition/--protocol"
                                .to_owned(),
                        );
                    }
                    return finish_continue_session(
                        session_id,
                        RunPrompt {
                            words: prompt,
                            images: image,
                        },
                        print_session_id,
                        resume_tree,
                        limits,
                    );
                }
                if resume_tree {
                    return Err("run --resume-tree requires --session".to_owned());
                }
                let account = account.or(provider).ok_or_else(|| {
                    "run requires --account <codex|grok|claude|openrouter|openai|anthropic|xai-api>".to_owned()
                })?;
                let model = model.ok_or_else(|| "run requires --model".to_owned())?;
                finish_run(
                    account,
                    model,
                    effort,
                    composition,
                    protocol,
                    RunPrompt {
                        words: prompt,
                        images: image,
                    },
                    print_session_id,
                    limits,
                )
            }
            Self::Agent {
                account,
                provider,
                model,
                effort,
                composition,
                protocol,
                session,
                resume_tree,
            } => finish_agent(
                account.or(provider),
                model,
                effort,
                composition,
                protocol,
                session,
                resume_tree,
            ),
            Self::Resume { session } => Ok(Command::Resume {
                session_id: session,
            }),
            #[cfg(feature = "maintainer")]
            Self::Fork { session: None, .. } => Ok(Command::CommandHelp {
                command: "fork".to_owned(),
            }),
            #[cfg(feature = "maintainer")]
            Self::Fork {
                session: Some(session),
                confirmed,
            } => Ok(Command::Fork {
                session_id: session,
                confirmed,
            }),
            #[cfg(feature = "maintainer")]
            Self::Compact { session: None, .. } => Ok(Command::CommandHelp {
                command: "compact".to_owned(),
            }),
            #[cfg(feature = "maintainer")]
            Self::Compact {
                session: Some(session),
                confirmed,
            } => Ok(Command::Compact {
                session_id: session,
                confirmed,
            }),
            Self::Config { action: None } => Ok(Command::CommandHelp {
                command: "config".to_owned(),
            }),
            Self::Config {
                action: Some(action),
            } => action.into_command(),
            Self::Doctor => Ok(Command::Doctor),
            Self::State { action: None } => Ok(Command::CommandHelp {
                command: "state".to_owned(),
            }),
            Self::State {
                action:
                    Some(StateAction::PruneLegacy {
                        yes,
                        forget_missing_launches,
                        prune_orphaned_references,
                    }),
            } => Ok(Command::StatePruneLegacy {
                yes,
                forget_missing_launches,
                prune_orphaned_references,
            }),
            Self::Usage {
                account,
                session_id,
            } => Ok(Command::Usage {
                account,
                session_id,
            }),
            Self::Session { action: None } => Ok(Command::CommandHelp {
                command: "session".to_owned(),
            }),
            Self::Session {
                action: Some(action),
            } => action.into_command(),
            Self::Telemetry { action: None } => Ok(Command::CommandHelp {
                command: "telemetry".to_owned(),
            }),
            Self::Telemetry {
                action: Some(action),
            } => action.into_command(),
            Self::Version {
                write_binary_manifest: Some(directory),
                ..
            } => Ok(Command::WriteBinaryManifest { directory }),
            Self::Version {
                carry_probe: Some(path),
                ..
            } => Ok(Command::CarryProbe { path }),
            Self::Version {
                reload_probe: true, ..
            } => Ok(Command::ReloadProbe),
            Self::Version {
                export_version: true,
                ..
            } => Ok(Command::ExportVersion),
            Self::Version {
                state_key: true, ..
            } => Ok(Command::StateKey),
            Self::Version { .. } => Ok(Command::Version),
            Self::Help => Ok(Command::Help),
        }
    }
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
                Ok(Command::KeySet { account })
            }
            Self::Clear {
                account: Some(account),
            } => {
                if !crate::product_secrets::is_api_key_account(&account) {
                    return Err("key clear requires openai|xai-api|anthropic|openrouter".to_owned());
                }
                Ok(Command::KeyClear { account })
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
                Ok(Command::KeyStatus { account })
            }
        }
    }
}

#[derive(Debug, Subcommand)]
enum SecretAction {
    Set { name: Option<String> },
    Rm { name: Option<String> },
    List,
}

impl SecretAction {
    fn into_command(self) -> Result<Command, String> {
        match self {
            // A bare leaf routes to the parent group's help.
            Self::Set { name: None } | Self::Rm { name: None } => Ok(Command::CommandHelp {
                command: "secret".to_owned(),
            }),
            Self::Set { name: Some(name) } => {
                crate::product_secrets::validate_secret_name(&name)?;
                Ok(Command::SecretSet { name })
            }
            Self::Rm { name: Some(name) } => {
                crate::product_secrets::validate_secret_name(&name)?;
                Ok(Command::SecretRm { name })
            }
            Self::List => Ok(Command::SecretList),
        }
    }
}

#[derive(Debug, Subcommand)]
enum McpAction {
    Add {
        id: Option<String>,
        #[arg(long, value_parser = ["user", "global", "project"])]
        scope: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long = "bearer-secret", value_name = "SECRET_NAME")]
        bearer_secret: Option<String>,
        #[arg(long = "header", value_name = "NAME=VALUE")]
        header: Vec<String>,
        #[arg(long = "header-secret", value_name = "NAME=SECRET_NAME")]
        header_secret: Vec<String>,
        #[command(flatten)]
        oauth: Box<McpOAuthFlags>,
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(long = "secret-env", value_name = "KEY=SECRET_NAME")]
        secret_env: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        tools: Option<Vec<String>>,
        #[arg(long, action = ArgAction::SetTrue)]
        resources: bool,
        #[arg(long = "startup-seconds")]
        startup_seconds: Option<u16>,
        #[arg(long = "call-seconds")]
        call_seconds: Option<u16>,
        #[arg(long, action = ArgAction::SetTrue)]
        replace: bool,
        #[arg(last = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    List,
    Show {
        id: Option<String>,
    },
    Test {
        id: Option<String>,
    },
    Login {
        id: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, action = ArgAction::SetTrue)]
        remote: bool,
    },
    Logout {
        id: Option<String>,
        #[arg(long, action = ArgAction::SetTrue)]
        local: bool,
    },
    Prompts {
        id: Option<String>,
        prompt: Option<String>,
        #[arg(long = "arg", value_name = "NAME=VALUE")]
        arg: Vec<String>,
    },
    Remove {
        id: Option<String>,
        #[arg(long, value_parser = ["user", "global", "project"])]
        scope: Option<String>,
        #[arg(long, action = ArgAction::SetTrue)]
        force: bool,
    },
    Enable {
        id: Option<String>,
        #[arg(long)]
        template: Option<String>,
        #[arg(long = "tool")]
        tool: Vec<String>,
    },
    Disable {
        id: Option<String>,
        #[arg(long)]
        template: Option<String>,
    },
}

/// `mcp add`'s OAuth flags.
#[derive(Debug, clap::Args)]
struct McpOAuthFlags {
    #[arg(long, action = ArgAction::SetTrue)]
    oauth: bool,
    #[arg(long = "oauth-issuer", value_name = "URL")]
    oauth_issuer: Option<String>,
    #[arg(long = "oauth-client-id", value_name = "ID")]
    oauth_client_id: Option<String>,
    #[arg(long = "oauth-client-secret", value_name = "SECRET_NAME")]
    oauth_client_secret: Option<String>,
    #[arg(long = "oauth-callback-port", value_name = "PORT")]
    oauth_callback_port: Option<u16>,
}

fn write_scope(
    scope: Option<&str>,
) -> Result<Option<crate::product_config::ConfigWriteScope>, String> {
    scope
        .map(|scope| {
            crate::product_config::ConfigWriteScope::parse(scope)
                .ok_or_else(|| "--scope must be user|global|project".to_owned())
        })
        .transpose()
}

/// `KEY=VALUE` pairs of one repeated flag; a key given twice is refused.
fn pairs(
    flag: &str,
    values: Vec<String>,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut out = std::collections::BTreeMap::new();
    for value in values {
        let Some((key, value)) = value.split_once('=') else {
            return Err(format!(
                "{flag} takes NAME=VALUE; one value has no `=` (it is not repeated here)"
            ));
        };
        if out.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(format!("{flag} names {key} twice"));
        }
    }
    Ok(out)
}

/// Every secret an `mcp add` names must be a secret *name*; a rejected one
/// may be a pasted value, so it is never repeated back.
fn secret_references_are_names(
    transport: &crate::product_config::McpTransport,
) -> Result<(), String> {
    let named: Vec<(&str, &String)> = match transport {
        crate::product_config::McpTransport::Stdio { secret_env, .. } => secret_env
            .values()
            .map(|name| ("--secret-env", name))
            .collect(),
        crate::product_config::McpTransport::Http {
            bearer_secret,
            secret_headers,
            oauth,
            ..
        } => bearer_secret
            .iter()
            .map(|name| ("--bearer-secret", name))
            .chain(
                secret_headers
                    .values()
                    .map(|name| ("--header-secret", name)),
            )
            .chain(
                oauth
                    .iter()
                    .filter_map(|oauth| oauth.client.as_ref()?.secret.as_ref())
                    .map(|name| ("--oauth-client-secret", name)),
            )
            .collect(),
    };
    for (flag, name) in named {
        if crate::product_secrets::validate_secret_name(name).is_err() {
            return Err(format!(
                "{flag} takes the name of a stored secret ([a-z][a-z0-9-]{{0,31}}), never its value; the value given is not shown. Store the value with `plak secret set <name>`"
            ));
        }
    }
    Ok(())
}

impl McpAction {
    fn into_command(self) -> Result<Command, String> {
        use crate::commands::mcp::McpCommand;
        let help = || {
            Ok(Command::CommandHelp {
                command: "mcp".to_owned(),
            })
        };
        let command = match self {
            Self::Add { id: None, .. }
            | Self::Show { id: None }
            | Self::Test { id: None }
            | Self::Login { id: None, .. }
            | Self::Logout { id: None, .. }
            | Self::Prompts { id: None, .. }
            | Self::Remove { id: None, .. }
            | Self::Enable { id: None, .. }
            | Self::Disable { id: None, .. } => return help(),
            Self::Add {
                id: Some(id),
                scope,
                url,
                bearer_secret,
                header,
                header_secret,
                oauth,
                env,
                secret_env,
                tools,
                resources,
                startup_seconds,
                call_seconds,
                replace,
                command,
            } => {
                let McpOAuthFlags {
                    oauth,
                    oauth_issuer,
                    oauth_client_id,
                    oauth_client_secret,
                    oauth_callback_port,
                } = *oauth;
                let oauth_given = oauth_issuer.is_some()
                    || oauth_client_id.is_some()
                    || oauth_client_secret.is_some()
                    || oauth_callback_port.is_some();
                if oauth_given && !oauth {
                    return Err("the --oauth-* flags require --oauth".to_owned());
                }
                // The load parser checks the rest (issuer, client and port
                // rules) before anything is written.
                let oauth = oauth.then(|| crate::product_config::McpOAuthConfig {
                    issuer: oauth_issuer,
                    client: oauth_client_id.map(|id| crate::product_config::McpOAuthClient {
                        id,
                        secret: oauth_client_secret.clone(),
                    }),
                    callback_port: oauth_callback_port,
                });
                if oauth
                    .as_ref()
                    .is_some_and(|oauth| oauth.client.is_none() && oauth_client_secret.is_some())
                {
                    return Err("--oauth-client-secret requires --oauth-client-id".to_owned());
                }
                let transport = match (url, command.split_first()) {
                    (Some(_), Some(_)) => {
                        return Err(
                            "mcp add takes either --url or a command after `--`, not both"
                                .to_owned(),
                        );
                    }
                    (None, None) => {
                        return Err("mcp add needs a command after `--` (stdio) or --url (http): plak mcp add <id> -- <command> [args...]".to_owned());
                    }
                    (Some(url), None) => {
                        for (flag, given) in [
                            ("--env", !env.is_empty()),
                            ("--secret-env", !secret_env.is_empty()),
                        ] {
                            if given {
                                return Err(format!("{flag} applies only to a stdio server"));
                            }
                        }
                        crate::product_config::McpTransport::Http {
                            url,
                            bearer_secret,
                            headers: pairs("--header", header)?,
                            secret_headers: pairs("--header-secret", header_secret)?,
                            oauth,
                        }
                    }
                    (None, Some((program, args))) => {
                        for (flag, given) in [
                            ("--bearer-secret", bearer_secret.is_some()),
                            ("--header", !header.is_empty()),
                            ("--header-secret", !header_secret.is_empty()),
                            ("--oauth", oauth.is_some()),
                        ] {
                            if given {
                                return Err(format!("{flag} applies only to an http server"));
                            }
                        }
                        crate::product_config::McpTransport::Stdio {
                            command: program.clone(),
                            args: args.to_vec(),
                            env: pairs("--env", env)?,
                            secret_env: pairs("--secret-env", secret_env)?,
                        }
                    }
                };
                secret_references_are_names(&transport)?;
                McpCommand::Add {
                    id,
                    scope: write_scope(scope.as_deref())?.unwrap_or_default(),
                    server: crate::product_config::McpServerConfig {
                        transport,
                        tools,
                        resources,
                        startup_seconds: startup_seconds
                            .unwrap_or(crate::product_config::DEFAULT_MCP_STARTUP_SECONDS),
                        call_seconds: call_seconds
                            .unwrap_or(crate::product_config::DEFAULT_MCP_CALL_SECONDS),
                    },
                    replace,
                }
            }
            Self::List => McpCommand::List,
            Self::Show { id: Some(id) } => McpCommand::Show { id },
            Self::Test { id: Some(id) } => McpCommand::Test { id },
            Self::Login {
                id: Some(id),
                scope,
                remote,
            } => McpCommand::Login { id, scope, remote },
            Self::Logout {
                id: Some(id),
                local,
            } => McpCommand::Logout { id, local },
            Self::Prompts {
                id: Some(id),
                prompt,
                arg,
            } => {
                if prompt.is_none() && !arg.is_empty() {
                    return Err("mcp prompts --arg needs a prompt name".to_owned());
                }
                McpCommand::Prompts {
                    id,
                    prompt,
                    arguments: pairs("--arg", arg)?,
                }
            }
            Self::Remove {
                id: Some(id),
                scope,
                force,
            } => McpCommand::Remove {
                id,
                scope: write_scope(scope.as_deref())?,
                force,
            },
            Self::Enable {
                id: Some(id),
                template,
                tool,
            } => McpCommand::Enable {
                id,
                template: template.ok_or("mcp enable needs --template <name>")?,
                tools: tool,
            },
            Self::Disable {
                id: Some(id),
                template,
            } => McpCommand::Disable {
                id,
                template: template.ok_or("mcp disable needs --template <name>")?,
            },
        };
        Ok(Command::Mcp(command))
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
                Err("config set requires a value: plak config set <key> <value>".to_owned())
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
enum SessionAction {
    /// List sessions for the current workspace.
    List,
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
    },
    /// Reclaim superseded checkpoint history of a session, or with --all of
    /// every session in this home not open in a running plak, and finish
    /// interrupted deletes. Never refused because the store is full.
    Gc {
        #[arg(conflicts_with = "all")]
        id: Option<String>,
        /// Every session in this home, across workspaces.
        #[arg(long, action = ArgAction::SetTrue)]
        all: bool,
    },
}

impl SessionAction {
    fn into_command(self) -> Result<Command, String> {
        match self {
            Self::List => Ok(Command::SessionList),
            // A bare leaf routes to the parent group's help.
            Self::Show { id: None }
            | Self::Log { id: None, .. }
            | Self::Recover { id: None }
            | Self::Export { id: None, .. }
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
            Self::Import { path: Some(path) } => Ok(Command::SessionImport { path }),
            Self::Delete {
                id: Some(id),
                yes,
                abandon_children,
            } => Ok(Command::SessionDelete {
                id,
                yes,
                abandon_children,
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

/// The prompt words after `--` and the `--image` paths of one `plak run`.
struct RunPrompt {
    words: Vec<String>,
    images: Vec<std::path::PathBuf>,
}

impl RunPrompt {
    /// Joins the words and reads every image; a refusal names the file.
    fn into_draft(self) -> Result<(String, Vec<plakous_host_protocol::DraftImage>), String> {
        if self.images.len() > plakous_hosted::attachments::MAX_IMAGES {
            return Err(format!(
                "run accepts at most {} --image files",
                plakous_hosted::attachments::MAX_IMAGES
            ));
        }
        let images = self
            .images
            .iter()
            .map(|path| {
                plakous_hosted::attachments::read_image(path).map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let text = self.words.join(" ");
        plakous_hosted::attachments::check_message_budget(&text, &images)
            .map_err(|error| format!("run refused: {error}"))?;
        for image in images
            .iter()
            .filter(|image| image.source_dimensions.is_some())
        {
            eprintln!(
                "plak: attached {}",
                plakous_hosted::attachments::chip(image)
            );
        }
        Ok((text, images))
    }
}

fn finish_continue_session(
    session_id: String,
    prompt: RunPrompt,
    print_session_id: bool,
    resume_tree: bool,
    limits: plakous_hosted::runtime::RunBounds,
) -> Result<Command, String> {
    if session_id.trim().is_empty() {
        return Err("run --session requires an exact session uuid".to_owned());
    }
    let (prompt, images) = prompt.into_draft()?;
    if prompt.trim().is_empty() && images.is_empty() {
        return Err("run requires a non-empty prompt after `--`".to_owned());
    }
    if prompt.len() > NATIVE_RUN_MAX_PROMPT_BYTES {
        return Err("run prompt exceeds the maximum supported size".to_owned());
    }
    Ok(Command::ContinueSession {
        limits,
        session_id,
        prompt: plakous_host_protocol::Draft::new(prompt, images),
        print_session_id,
        resume_tree,
    })
}

fn finish_run(
    account: String,
    model: String,
    effort: Option<String>,
    composition: Option<String>,
    protocol: Option<String>,
    prompt: RunPrompt,
    print_session_id: bool,
    limits: plakous_hosted::runtime::RunBounds,
) -> Result<Command, String> {
    if model.is_empty() {
        return Err("run requires --model".to_owned());
    }
    let (prompt, images) = prompt.into_draft()?;
    if let Some(provider) = plakous_hosted::binding::PlatformProvider::for_account(&account) {
        if protocol.is_some() {
            return Err("--protocol is available only with --account openrouter".into());
        }
        let effort = effort
            .as_deref()
            .map(|value| RunEffort::parse(value).ok_or_else(crate::command::run_effort_usage))
            .transpose()?
            .map(RunEffort::as_reasoning_effort);
        let composition = composition
            .as_deref()
            .map(crate::product_config::ProductCompositionSelection::parse)
            .transpose()?
            .unwrap_or(crate::product_config::ProductCompositionSelection::FollowCatalog);
        crate::platform_catalog::validate(provider, &model, effort, composition)?;
        if (prompt.trim().is_empty() && images.is_empty())
            || prompt.len() > NATIVE_RUN_MAX_PROMPT_BYTES
        {
            return Err("run requires a non-empty prompt within the supported size bound".into());
        }
        return Ok(Command::RunPlatform {
            limits,
            provider,
            model,
            effort: effort.ok_or_else(crate::command::run_effort_usage)?,
            composition,
            prompt: plakous_host_protocol::Draft::new(prompt, images),
            print_session_id,
        });
    }
    if account == "openrouter" {
        let effort = effort
            .map(|value| {
                RunEffort::parse(&value)
                    .map(RunEffort::as_reasoning_effort)
                    .ok_or_else(crate::command::run_effort_usage)
            })
            .transpose()?;
        let composition = composition
            .map(|value| crate::product_config::ProductCompositionSelection::parse(&value))
            .transpose()?
            .unwrap_or(crate::product_config::ProductCompositionSelection::FollowCatalog);
        if composition == crate::product_config::ProductCompositionSelection::NativePtc {
            return Err(plakous_hosted::binding::OPENROUTER_NATIVE_PTC_REFUSAL.to_owned());
        }
        let selection = protocol
            .as_deref()
            .map(crate::product_config::ProductProtocolSelection::parse)
            .transpose()?
            .unwrap_or_default();
        let wire = crate::product_config::ProductWireBinding::resolve(selection);
        if prompt.trim().is_empty() && images.is_empty() {
            return Err("run requires a non-empty prompt after `--`".to_owned());
        }
        if prompt.len() > NATIVE_RUN_MAX_PROMPT_BYTES {
            return Err("run prompt exceeds the maximum supported size".to_owned());
        }
        return Ok(Command::RunOpenRouter {
            limits,
            model,
            effort,
            composition,
            wire,
            prompt: plakous_host_protocol::Draft::new(prompt, images),
            print_session_id,
        });
    }
    if protocol.is_some() {
        return Err("--protocol is available only with --account openrouter".to_owned());
    }
    let provider = parse_oauth_account(&account)?;
    // No silent default: the catalog decides whether an effort is required, and
    // `plak run` says so by name when it is (see `PlakousApplication::run`).
    let effort = effort
        .map(|value| RunEffort::parse(&value).ok_or_else(crate::command::run_effort_usage))
        .transpose()?;
    let composition = composition
        .map(|value| crate::product_config::ProductCompositionSelection::parse(&value))
        .transpose()?;
    if prompt.trim().is_empty() && images.is_empty() {
        return Err("run requires a non-empty prompt after `--`".to_owned());
    }
    if prompt.len() > NATIVE_RUN_MAX_PROMPT_BYTES {
        return Err("run prompt exceeds the maximum supported size".to_owned());
    }
    Ok(Command::Run {
        limits,
        provider,
        model,
        effort: effort.map(RunEffort::as_reasoning_effort),
        composition,
        prompt: plakous_host_protocol::Draft::new(prompt, images),
        print_session_id,
    })
}

fn finish_agent(
    account: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    composition: Option<String>,
    protocol: Option<String>,
    session: Option<String>,
    resume_tree: bool,
) -> Result<Command, String> {
    if let Some(session_id) = session {
        if account.is_some()
            || model.is_some()
            || effort.is_some()
            || composition.is_some()
            || protocol.is_some()
        {
            return Err(
                "agent --session uses the recorded binding and composition; omit --account/--model/--effort/--composition/--protocol"
                    .to_owned(),
            );
        }
        if session_id.trim().is_empty() {
            return Err("agent --session requires an exact session uuid".to_owned());
        }
        return Ok(Command::Agent {
            session_id: Some(session_id),
            provider: None,
            platform: None,
            wire: None,
            model: None,
            effort: None,
            effort_explicit: false,
            composition: None,
            resume_tree,
        });
    }
    if resume_tree {
        return Err("agent --resume-tree requires --session".to_owned());
    }
    if let Some(account) = account.as_deref() {
        if let Some(platform) = plakous_hosted::binding::PlatformProvider::for_account(account) {
            if protocol.is_some() {
                return Err("--protocol is available only with --account openrouter".into());
            }
            let effort = effort
                .as_deref()
                .map(|value| {
                    RunEffort::parse(value)
                        .map(RunEffort::as_reasoning_effort)
                        .ok_or_else(crate::command::run_effort_usage)
                })
                .transpose()?;
            let composition = composition
                .as_deref()
                .map(crate::product_config::ProductCompositionSelection::parse)
                .transpose()?;
            if plakous_hosted::capabilities::require(account).is_ok() {
                if model
                    .as_deref()
                    .is_some_and(|model| !crate::platform_catalog::enrolled(platform, model))
                {
                    return Err("API model is not enrolled for the selected account".into());
                }
                if let (Some(model), Some(composition)) = (model.as_deref(), composition) {
                    plakous_hosted::platform::api_composition(platform, model, composition)
                        .map_err(str::to_owned)?;
                }
                if let (Some(model), Some(effort)) = (model.as_deref(), effort) {
                    plakous_hosted::platform::bundled_catalog(platform, "offline")
                        .admit(model, Some(effort))
                        .map_err(|error| error.to_string())?;
                }
            }
            return Ok(Command::Agent {
                session_id: None,
                provider: None,
                platform: Some(platform),
                wire: None,
                model,
                effort,
                effort_explicit: effort.is_some(),
                composition,
                resume_tree: false,
            });
        }
        if account == "openrouter" {
            let effort = effort
                .map(|value| {
                    RunEffort::parse(&value)
                        .map(RunEffort::as_reasoning_effort)
                        .ok_or_else(crate::command::run_effort_usage)
                })
                .transpose()?;
            let effort_explicit = effort.is_some();
            let composition = composition
                .map(|value| crate::product_config::ProductCompositionSelection::parse(&value))
                .transpose()?
                .unwrap_or(crate::product_config::ProductCompositionSelection::FollowCatalog);
            if composition == crate::product_config::ProductCompositionSelection::NativePtc {
                return Err(plakous_hosted::binding::OPENROUTER_NATIVE_PTC_REFUSAL.to_owned());
            }
            let selection = protocol
                .as_deref()
                .map(crate::product_config::ProductProtocolSelection::parse)
                .transpose()?
                .unwrap_or_default();
            let wire = crate::product_config::ProductWireBinding::resolve(selection);
            return Ok(Command::Agent {
                session_id: None,
                provider: None,
                platform: None,
                wire: Some(wire),
                model,
                effort,
                effort_explicit,
                composition: Some(composition),
                resume_tree: false,
            });
        }
        if protocol.is_some() {
            return Err("--protocol is available only with --account openrouter".to_owned());
        }
    } else if protocol.is_some() {
        return Err("--protocol is available only with --account openrouter".to_owned());
    }
    let provider = account.as_deref().map(parse_oauth_account).transpose()?;
    let effort_explicit = effort.is_some();
    let effort = match effort {
        None => None,
        Some(value) => Some(
            RunEffort::parse(&value)
                .ok_or_else(crate::command::run_effort_usage)?
                .as_reasoning_effort(),
        ),
    };
    let composition = composition
        .map(|value| crate::product_config::ProductCompositionSelection::parse(&value))
        .transpose()?;
    Ok(Command::Agent {
        session_id: None,
        provider,
        platform: None,
        wire: None,
        model,
        effort,
        effort_explicit,
        composition,
        resume_tree: false,
    })
}

fn parse_oauth_account(value: &str) -> Result<NativeProvider, String> {
    match value {
        "codex" => Ok(NativeProvider::Codex),
        "grok" | "supergrok" => Ok(NativeProvider::Xai),
        "claude" => Ok(NativeProvider::Claude),
        _ => Err(format!(
            "unknown OAuth account `{value}`; expected codex|grok|claude (API keys: plak key set …)"
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
