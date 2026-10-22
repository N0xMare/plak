//! Clap-backed CLI parsing for the Plak application.

pub(crate) mod render;

use std::ffi::OsString;

use clap::{ArgAction, Parser, Subcommand};
use plakous::subscriptions::{NativeLoginMode, NativeProvider};

use crate::command::{Command, NATIVE_RUN_MAX_PROMPT_BYTES, RunEffort};

/// Parse argv tokens (without binary name) into a product [`Command`].
pub(crate) fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let values = arguments
        .into_iter()
        .map(|value| {
            value
                .into_string()
                .map_err(|_| "command arguments must be valid UTF-8".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;

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
        };
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
        };
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
    })?;
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
        return Err(
            "`xai` is ambiguous: use `grok` (SuperGrok subscription) or `xai-api` (API key)"
                .to_owned(),
        );
    }
    cli.into_command()
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
                resume_tree,
                image,
                prompt,
            } => {
                let limits = plakous_hosted::runtime::RunBounds {
                    max_turns,
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
    /// Read bounded excerpts from a version 2 session archive without import.
    Reference {
        path: Option<std::path::PathBuf>,
        #[arg(long)]
        search: Option<String>,
        #[arg(long, default_value_t = 20)]
        tail: usize,
    },
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
            | Self::Reference { path: None, .. }
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
            Self::Reference {
                path: Some(path),
                search,
                tail,
            } => {
                if tail == 0 {
                    return Err("session reference --tail must be >= 1".to_owned());
                }
                Ok(Command::SessionReference { path, search, tail })
            }
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
        for image in images
            .iter()
            .filter(|image| image.source_dimensions.is_some())
        {
            eprintln!(
                "plak: attached {}",
                plakous_hosted::attachments::chip(image)
            );
        }
        Ok((self.words.join(" "), images))
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
mod tests {
    #[test]
    fn run_image_flags_attach_sniffed_files_and_name_the_refused_file() {
        let dir = tempfile::tempdir().unwrap();
        let shot = dir.path().join("shot.png");
        std::fs::write(&shot, plakous_hosted::attachments::png(4, 3, 10)).unwrap();
        let notes = dir.path().join("notes.png");
        std::fs::write(&notes, "plain text").unwrap();
        let run = |images: &[&std::path::Path], account: &str| {
            let mut arguments = vec![
                OsString::from("run"),
                "--account".into(),
                account.into(),
                "--model".into(),
                "m".into(),
            ];
            for image in images {
                arguments.push("--image".into());
                arguments.push(image.as_os_str().to_owned());
            }
            arguments.extend(["--".into(), "what is this?".into()]);
            parse(arguments)
        };
        let command = run(&[&shot, &shot], "grok").unwrap();
        let Command::Run { prompt, .. } = command else {
            panic!("expected run")
        };
        assert_eq!(prompt.text, "what is this?");
        assert_eq!(prompt.images.len(), 2);
        assert_eq!(prompt.images[0].media_type, "image/png");
        assert_eq!(prompt.images[0].name, "shot.png");
        let Command::RunOpenRouter { prompt, .. } = run(&[&shot], "openrouter").unwrap() else {
            panic!("expected openrouter run")
        };
        assert_eq!(prompt.images.len(), 1);

        // Images alone are a prompt.
        let image_only = parse(vec![
            OsString::from("run"),
            "--account".into(),
            "grok".into(),
            "--model".into(),
            "m".into(),
            "--image".into(),
            shot.as_os_str().to_owned(),
        ]);
        let Ok(Command::Run { prompt, .. }) = image_only else {
            panic!("expected an image-only run, got {image_only:?}")
        };
        assert_eq!(prompt.text, "");
        assert_eq!(prompt.images.len(), 1);

        let error = run(&[&notes], "grok").unwrap_err();
        assert!(
            error.contains("notes.png") && error.contains("not a supported image"),
            "{error}"
        );
        let missing = dir.path().join("missing.png");
        let error = run(&[&missing], "grok").unwrap_err();
        assert!(
            error.contains("missing.png") && error.contains("cannot be read"),
            "{error}"
        );
        let nine = vec![shot.as_path(); 9];
        let error = run(&nine, "grok").unwrap_err();
        assert!(error.contains("at most 8"), "{error}");
    }

    #[test]
    fn publication_help_after_separator_is_prompt_text() {
        let command = parse(
            [
                "run",
                "--account",
                "grok",
                "--model",
                "test",
                "--",
                "--help",
            ]
            .map(OsString::from),
        )
        .unwrap();
        assert!(matches!(command, Command::Run { prompt, .. } if prompt == "--help"));
    }

    #[test]
    fn publication_api_invalid_effort_names_ladder() {
        let error = parse(
            [
                "run",
                "--account",
                "openai",
                "--model",
                "gpt-5",
                "--effort",
                "bogus",
                "--",
                "task",
            ]
            .map(OsString::from),
        )
        .unwrap_err();
        assert_eq!(error, crate::command::run_effort_usage());
    }

    #[test]
    fn publication_parse_errors_name_verb_without_echoing_arguments() {
        let error = parse(["run", "secret-prompt"].map(OsString::from)).unwrap_err();
        assert!(error.contains("put the prompt after `--`"));
        assert!(!error.contains("secret-prompt"));
        let error = parse(["session", "--unknown"].map(OsString::from)).unwrap_err();
        assert!(error.contains("plak session --help"));
        assert!(!error.contains("<command>"));
    }

    #[test]
    fn publication_every_visible_clap_flag_is_on_verb_help_page() {
        use clap::CommandFactory as _;
        fn check(command: &clap::Command, page: &str) {
            for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
                if let Some(long) = arg.get_long() {
                    assert!(
                        page.contains(&format!("--{long}")),
                        "{} missing --{long}",
                        command.get_name()
                    );
                }
            }
            for subcommand in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
                check(subcommand, page);
            }
        }
        for command in Cli::command()
            .get_subcommands()
            .filter(|sub| !sub.is_hide_set())
        {
            let mut page = Vec::new();
            render::command_help(command.get_name(), &mut page).unwrap();
            check(command, &String::from_utf8(page).unwrap());
        }
    }

    #[test]
    fn run_accepts_turn_and_wall_clock_bounds() {
        let args = [
            "run",
            "--account",
            "grok",
            "--model",
            "test",
            "--max-turns",
            "2",
            "--timeout",
            "3",
            "--",
            "task",
        ]
        .map(str::to_owned);
        assert!(parse(args.into_iter().map(OsString::from)).is_ok());
        for bound in ["0", "101"] {
            assert!(
                parse(
                    [
                        "run",
                        "--account",
                        "grok",
                        "--model",
                        "test",
                        "--max-turns",
                        bound,
                        "--",
                        "task"
                    ]
                    .into_iter()
                    .map(OsString::from)
                )
                .is_err()
            );
        }
    }
    use super::*;
    use crate::command::Command;
    use proptest::prelude::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(|s| OsString::from(*s)).collect()
    }

    #[test]
    fn bare_plak_opens_tui() {
        assert!(matches!(
            parse(Vec::<OsString>::new()),
            Ok(Command::Tui { .. })
        ));
    }

    #[test]
    fn help_and_login_parse() {
        assert!(matches!(parse(["help".into()]), Ok(Command::Help)));
        assert!(matches!(
            parse(["login".into(), "grok".into()]),
            Ok(Command::AuthLogin {
                provider: NativeProvider::Xai,
                ..
            })
        ));
        assert!(matches!(
            parse(["login".into(), "claude".into()]),
            Ok(Command::AuthLogin {
                provider: NativeProvider::Claude,
                ..
            })
        ));
        assert!(matches!(
            parse(["login".into(), "--help".into()]),
            Ok(Command::CommandHelp { command }) if command == "login"
        ));
    }

    #[test]
    fn clap_routes_status() {
        assert!(matches!(
            parse(["status".into(), "codex".into()]),
            Ok(Command::AuthStatus(NativeProvider::Codex))
        ));
    }

    #[test]
    fn run_session_rejects_conflicting_binding_flags() {
        let err = parse([
            "run".into(),
            "--session".into(),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            "--account".into(),
            "codex".into(),
            "--".into(),
            "hello".into(),
        ]);
        let Err(message) = err else {
            panic!("session plus account must fail closed");
        };
        assert!(message.contains("recorded binding"));
    }

    #[test]
    fn run_session_resume_tree_is_explicit() {
        let cmd = parse([
            "run".into(),
            "--session".into(),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            "--resume-tree".into(),
            "--".into(),
            "continue".into(),
        ])
        .unwrap();
        match cmd {
            Command::ContinueSession {
                resume_tree: true,
                prompt,
                ..
            } => assert_eq!(prompt, "continue"),
            _ => panic!("expected continue with resume_tree"),
        }
    }

    #[test]
    fn run_resume_tree_without_session_fails() {
        let err = parse([
            "run".into(),
            "--account".into(),
            "codex".into(),
            "--model".into(),
            "m".into(),
            "--resume-tree".into(),
            "--".into(),
            "hello".into(),
        ]);
        let Err(message) = err else {
            panic!("--resume-tree without --session must fail closed");
        };
        assert!(message.contains("--session"));
    }

    #[test]
    fn agent_stdio_is_the_default_pipe() {
        let cmd = parse(["agent".into()]).unwrap();
        match cmd {
            Command::Agent {
                session_id: None,
                resume_tree: false,
                ..
            } => {}
            _ => panic!("expected bare agent"),
        }
    }

    #[test]
    fn agent_session_resume_tree_is_explicit() {
        let cmd = parse([
            "agent".into(),
            "--session".into(),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            "--resume-tree".into(),
        ])
        .unwrap();
        match cmd {
            Command::Agent {
                session_id: Some(id),
                resume_tree: true,
                ..
            } => assert_eq!(id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
            _ => panic!("expected agent continue with resume_tree"),
        }
    }

    #[test]
    fn agent_resume_tree_without_session_fails() {
        let err = parse([
            "agent".into(),
            "--account".into(),
            "codex".into(),
            "--model".into(),
            "m".into(),
            "--resume-tree".into(),
        ]);
        let Err(message) = err else {
            panic!("agent --resume-tree without --session must fail closed");
        };
        assert!(message.contains("--session"));
    }

    #[test]
    fn openrouter_agent_resolves_auto_once_and_defaults_follow_catalog() {
        let command = parse(args(&[
            "agent",
            "--account",
            "openrouter",
            "--model",
            "vendor/model:free",
        ]))
        .unwrap();
        assert!(matches!(
            command,
            Command::Agent {
                provider: None,
                platform: None,
                wire: Some(crate::product_config::ProductWireBinding {
                    selection: crate::product_config::ProductProtocolSelection::Auto,
                    resolved: crate::product_config::ProductResolvedProtocol::Chat,
                    ..
                }),
                model: Some(model),
                effort: None,
                effort_explicit: false,
                composition:
                    Some(crate::product_config::ProductCompositionSelection::FollowCatalog),
                ..
            } if model == "vendor/model:free"
        ));
    }

    #[test]
    fn openrouter_agent_protocol_and_composition_admission_is_closed_pre_io() {
        for (protocol, resolved) in [
            (
                "responses",
                crate::product_config::ProductResolvedProtocol::Responses,
            ),
            (
                "messages",
                crate::product_config::ProductResolvedProtocol::Messages,
            ),
        ] {
            let command = parse(args(&[
                "agent",
                "--account",
                "openrouter",
                "--model",
                "vendor/model",
                "--protocol",
                protocol,
            ]))
            .unwrap();
            assert!(matches!(
                command,
                Command::Agent {
                    wire: Some(wire),
                    ..
                } if wire.resolved == resolved
            ));
        }
        let native_ptc = parse(args(&[
            "agent",
            "--account",
            "openrouter",
            "--model",
            "vendor/model",
            "--composition",
            "native-ptc",
        ]))
        .unwrap_err();
        assert!(
            native_ptc.contains("OpenRouter has no native programmatic tool calling")
                && native_ptc.contains("use js-ptc, py-ptc or direct"),
            "{native_ptc}"
        );
        let legacy_native = parse(args(&[
            "agent",
            "--account",
            "openrouter",
            "--model",
            "vendor/model",
            "--composition",
            "native",
        ]))
        .unwrap_err();
        assert!(legacy_native.contains("direct|plakous-default|native-ptc|js-ptc|py-ptc"));
        let oauth_protocol = parse(args(&[
            "agent",
            "--account",
            "codex",
            "--model",
            "gpt-5.3-codex-spark",
            "--protocol",
            "chat",
        ]))
        .unwrap_err();
        assert!(oauth_protocol.contains("only with --account openrouter"));
    }

    #[test]
    fn openrouter_agent_account_parses_without_oauth_provider() {
        let command = parse(args(&[
            "agent",
            "--account",
            "openrouter",
            "--model",
            "vendor/model",
        ]))
        .unwrap();
        assert!(matches!(
            command,
            Command::Agent {
                provider: None,
                platform: None,
                wire: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn clap_routes_run_with_delimiter() {
        let cmd = parse([
            "run".into(),
            "--account".into(),
            "codex".into(),
            "--model".into(),
            "gpt-5.3-codex-spark".into(),
            "--".into(),
            "hello".into(),
        ])
        .unwrap();
        match cmd {
            Command::Run {
                provider,
                model,
                prompt,
                ..
            } => {
                assert_eq!(provider, NativeProvider::Codex);
                assert_eq!(model, "gpt-5.3-codex-spark");
                assert_eq!(prompt, "hello");
            }
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn openrouter_run_resolves_auto_once_and_defaults_follow_catalog() {
        let command = parse(args(&[
            "run",
            "--account",
            "openrouter",
            "--model",
            "vendor/model:free",
            "--",
            "hello",
        ]))
        .unwrap();
        assert!(matches!(
            command,
            Command::RunOpenRouter {
                model,
                effort: None,
                composition: crate::product_config::ProductCompositionSelection::FollowCatalog,
                wire: crate::product_config::ProductWireBinding {
                    selection: crate::product_config::ProductProtocolSelection::Auto,
                    resolved: crate::product_config::ProductResolvedProtocol::Chat,
                    ..
                },
                prompt,
                ..
            } if model == "vendor/model:free" && prompt == "hello"
        ));
    }

    #[test]
    fn openrouter_protocol_and_composition_admission_is_closed_pre_io() {
        for (protocol, resolved) in [
            (
                "responses",
                crate::product_config::ProductResolvedProtocol::Responses,
            ),
            (
                "messages",
                crate::product_config::ProductResolvedProtocol::Messages,
            ),
        ] {
            let command = parse(args(&[
                "run",
                "--account",
                "openrouter",
                "--model",
                "vendor/model",
                "--protocol",
                protocol,
                "--",
                "hello",
            ]))
            .unwrap();
            assert!(matches!(
                command,
                Command::RunOpenRouter { wire, .. } if wire.resolved == resolved
            ));
        }
        let native_ptc = parse(args(&[
            "run",
            "--account",
            "openrouter",
            "--model",
            "vendor/model",
            "--composition",
            "native-ptc",
            "--",
            "hello",
        ]))
        .unwrap_err();
        assert!(
            native_ptc.contains("OpenRouter has no native programmatic tool calling")
                && native_ptc.contains("use js-ptc, py-ptc or direct"),
            "{native_ptc}"
        );
        let legacy_native = parse(args(&[
            "run",
            "--account",
            "openrouter",
            "--model",
            "vendor/model",
            "--composition",
            "native",
            "--",
            "hello",
        ]))
        .unwrap_err();
        assert!(legacy_native.contains("direct|plakous-default|native-ptc|js-ptc|py-ptc"));
        let oauth_protocol = parse(args(&[
            "run",
            "--account",
            "codex",
            "--model",
            "gpt-5.3-codex-spark",
            "--protocol",
            "chat",
            "--",
            "hello",
        ]))
        .unwrap_err();
        assert!(oauth_protocol.contains("only with --account openrouter"));
    }

    #[test]
    fn run_composition_selector_accepts_closed_cli_set() {
        for (value, expected) in [
            (
                "direct",
                crate::product_config::ProductCompositionSelection::Direct,
            ),
            (
                "plakous-default",
                crate::product_config::ProductCompositionSelection::FollowCatalog,
            ),
            (
                "native-ptc",
                crate::product_config::ProductCompositionSelection::NativePtc,
            ),
            (
                "js-ptc",
                crate::product_config::ProductCompositionSelection::JsPtc,
            ),
            (
                "py-ptc",
                crate::product_config::ProductCompositionSelection::PyPtc,
            ),
        ] {
            let command = parse(args(&[
                "run",
                "--account",
                "grok",
                "--model",
                "grok-4.6",
                "--effort",
                "low",
                "--composition",
                value,
                "--",
                "hello",
            ]))
            .unwrap();
            assert!(matches!(
                command,
                Command::Run {
                    composition: Some(actual),
                    ..
                } if actual == expected
            ));
        }
        assert!(
            parse(args(&[
                "run",
                "--account",
                "grok",
                "--model",
                "grok-4.6",
                "--composition",
                "py",
                "--",
                "hello",
            ]))
            .unwrap_err()
            .contains("direct|plakous-default|native-ptc|js-ptc|py-ptc")
        );
        assert!(
            parse(args(&[
                "run",
                "--account",
                "grok",
                "--model",
                "grok-4.6",
                "--composition",
                "javascript,python",
                "--",
                "hello",
            ]))
            .unwrap_err()
            .contains("direct|plakous-default|native-ptc|js-ptc|py-ptc")
        );
    }

    #[test]
    fn clap_routes_claude_oauth_run_sonnet_5_low() {
        let cmd = parse([
            "run".into(),
            "--account".into(),
            "claude".into(),
            "--model".into(),
            "claude-sonnet-5".into(),
            "--effort".into(),
            "low".into(),
            "--".into(),
            "pong".into(),
        ])
        .unwrap();
        match cmd {
            Command::Run {
                provider,
                model,
                effort,
                prompt,
                ..
            } => {
                assert_eq!(provider, NativeProvider::Claude);
                assert_eq!(model, "claude-sonnet-5");
                assert_eq!(effort, Some(plakous::model::ReasoningEffort::Low));
                assert_eq!(prompt, "pong");
            }
            _ => panic!("expected claude run"),
        }
    }

    #[test]
    fn prune_legacy_missing_launches_requires_explicit_flag() {
        assert!(
            parse(args(&[
                "state",
                "prune-legacy",
                "--yes",
                "--forget-missing-launches"
            ]))
            .is_ok()
        );
    }

    /// P0: product argv matrix — each public command parses (or documented reject).
    #[test]
    #[allow(
        clippy::type_complexity,
        reason = "table of argv slices → predicate is clearer than a macro here"
    )]
    fn product_command_argv_matrix() {
        let cases: &[(&[&str], fn(&Command) -> bool)] = &[
            (&["help"], |c| matches!(c, Command::Help)),
            (&["version"], |c| matches!(c, Command::Version)),
            (&["-V"], |c| matches!(c, Command::Version)),
            (&["--version"], |c| matches!(c, Command::Version)),
            (&["-h"], |c| matches!(c, Command::Help)),
            (&["doctor"], |c| matches!(c, Command::Doctor)),
            (&["state", "prune-legacy"], |c| {
                matches!(
                    c,
                    Command::StatePruneLegacy {
                        yes: false,
                        forget_missing_launches: false,
                        prune_orphaned_references: false
                    }
                )
            }),
            (&["state", "prune-legacy", "--yes"], |c| {
                matches!(
                    c,
                    Command::StatePruneLegacy {
                        yes: true,
                        forget_missing_launches: false,
                        prune_orphaned_references: false
                    }
                )
            }),
            (&["config", "show"], |c| matches!(c, Command::ConfigShow)),
            (&["config", "init"], |c| matches!(c, Command::ConfigInit)),
            (
                &["config", "set", "account", "grok"],
                |c| matches!(c, Command::ConfigSet { key, value, scope } if key == "account" && value == "grok" && *scope == crate::product_config::ConfigWriteScope::User),
            ),
            (
                &["config", "set", "tui.theme", "light", "--scope", "project"],
                |c| matches!(c, Command::ConfigSet { scope, .. } if *scope == crate::product_config::ConfigWriteScope::Project),
            ),
            (&["config", "trust"], |c| matches!(c, Command::ConfigTrust)),
            (&["config", "untrust"], |c| {
                matches!(c, Command::ConfigUntrust)
            }),
            (&["session", "list"], |c| matches!(c, Command::SessionList)),
            (
                &["session", "show", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"],
                |c| matches!(c, Command::SessionShow { id } if id.starts_with("aaaa")),
            ),
            (
                &["session", "log", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"],
                |c| matches!(c, Command::SessionLog { id, .. } if id.starts_with("aaaa")),
            ),
            (
                &["session", "export", "aaaa", "--out", "/tmp/x.json"],
                |c| matches!(c, Command::SessionExport { id, out: Some(out) } if id == "aaaa" && out.as_os_str() == "/tmp/x.json"),
            ),
            (&["session", "export", "aaaa"], |c| {
                matches!(c, Command::SessionExport { out: None, .. })
            }),
            (
                &["session", "import", "/tmp/x.json"],
                |c| matches!(c, Command::SessionImport { path } if path.as_os_str() == "/tmp/x.json"),
            ),
            (&["session", "export"], |c| {
                matches!(c, Command::CommandHelp { .. })
            }),
            (
                &["session", "delete", "aaaa"],
                |c| matches!(c, Command::SessionDelete { id, yes: false, abandon_children: false } if id == "aaaa"),
            ),
            (&["session", "delete", "aaaa", "--yes"], |c| {
                matches!(c, Command::SessionDelete { yes: true, .. })
            }),
            (&["session", "delete"], |c| {
                matches!(c, Command::CommandHelp { .. })
            }),
            (
                &["session", "gc", "aaaa"],
                |c| matches!(c, Command::SessionGc { id: Some(id) } if id == "aaaa"),
            ),
            (&["session", "gc", "--all"], |c| {
                matches!(c, Command::SessionGc { id: None })
            }),
            (&["session", "gc"], |c| {
                matches!(c, Command::CommandHelp { .. })
            }),
            (
                &["version", "--write-binary-manifest", "/tmp/snapshot"],
                |c| matches!(c, Command::WriteBinaryManifest { directory } if directory.as_os_str() == "/tmp/snapshot"),
            ),
            (&["version", "--state-key"], |c| {
                matches!(c, Command::StateKey)
            }),
            (
                &["--import-resume", "/tmp/x.json"],
                |c| matches!(c, Command::ImportResume { path } if path.as_os_str() == "/tmp/x.json"),
            ),
            (&["status"], |c| matches!(c, Command::StatusAll)),
            (&["models", "claude"], |c| {
                matches!(c, Command::Models(NativeProvider::Claude))
            }),
            (&["models", "codex"], |c| {
                matches!(c, Command::Models(NativeProvider::Codex))
            }),
            (&["status", "codex"], |c| {
                matches!(c, Command::AuthStatus(NativeProvider::Codex))
            }),
            (&["login", "codex"], |c| {
                matches!(
                    c,
                    Command::AuthLogin {
                        provider: NativeProvider::Codex,
                        ..
                    }
                )
            }),
            (&["login", "claude"], |c| {
                matches!(
                    c,
                    Command::AuthLogin {
                        provider: NativeProvider::Claude,
                        mode: NativeLoginMode::Browser,
                    }
                )
            }),
            (&["login", "claude", "--remote"], |c| {
                matches!(
                    c,
                    Command::AuthLogin {
                        provider: NativeProvider::Claude,
                        mode: NativeLoginMode::Remote,
                    }
                )
            }),
            (&["logout", "grok", "--local"], |c| {
                matches!(
                    c,
                    Command::AuthLogout {
                        provider: NativeProvider::Xai,
                        local_only: true
                    }
                )
            }),
            (&["logout", "supergrok", "--local"], |c| {
                matches!(
                    c,
                    Command::AuthLogout {
                        provider: NativeProvider::Xai,
                        local_only: true
                    }
                )
            }),
            (&["refresh", "grok"], |c| {
                matches!(c, Command::AuthRefresh(NativeProvider::Xai))
            }),
            (&["key", "status"], |c| {
                matches!(c, Command::KeyStatus { account: None })
            }),
            (
                &["key", "status", "openai"],
                |c| matches!(c, Command::KeyStatus { account: Some(a) } if a == "openai"),
            ),
            (
                &["key", "clear", "openai"],
                |c| matches!(c, Command::KeyClear { account } if account == "openai"),
            ),
            (
                &["key", "set", "xai-api"],
                |c| matches!(c, Command::KeySet { account } if account == "xai-api"),
            ),
            (
                &["secret", "set", "github"],
                |c| matches!(c, Command::SecretSet { name } if name == "github"),
            ),
            (
                &["secret", "rm", "linear-ws"],
                |c| matches!(c, Command::SecretRm { name } if name == "linear-ws"),
            ),
            (&["secret", "list"], |c| matches!(c, Command::SecretList)),
            (
                &["secret"],
                |c| matches!(c, Command::CommandHelp { command } if command == "secret"),
            ),
            (
                &["secret", "set"],
                |c| matches!(c, Command::CommandHelp { command } if command == "secret"),
            ),
            (&["resume"], |c| {
                matches!(c, Command::Resume { session_id: None })
            }),
            (
                &["resume", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"],
                |c| matches!(c, Command::Resume { session_id: Some(id) } if id.starts_with("aaaa")),
            ),
            (&["--resume"], |c| {
                matches!(c, Command::Resume { session_id: None })
            }),
            (
                &["usage", "--session", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"],
                |c| {
                    matches!(
                        c,
                        Command::Usage {
                            session_id: Some(_),
                            ..
                        }
                    )
                },
            ),
            (&["usage"], |c| {
                matches!(
                    c,
                    Command::Usage {
                        account: None,
                        session_id: None
                    }
                )
            }),
            (
                &["run", "--account", "grok", "--model", "m", "--", "hi"],
                |c| matches!(c, Command::Run { provider: NativeProvider::Xai, prompt, .. } if prompt == "hi"),
            ),
            (
                &[
                    "run",
                    "--session",
                    "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                    "--",
                    "continue",
                ],
                |c| {
                    matches!(
                        c,
                        Command::ContinueSession { session_id, prompt, .. }
                            if session_id.starts_with("aaaa") && prompt == "continue"
                    )
                },
            ),
            // Recursive help
            (
                &["config", "--help"],
                |c| matches!(c, Command::CommandHelp { command } if command == "config"),
            ),
            (
                &["help", "run"],
                |c| matches!(c, Command::CommandHelp { command } if command == "run"),
            ),
        ];

        for (argv, check) in cases {
            let parsed = parse(args(argv));
            assert!(parsed.is_ok(), "expected Ok for {argv:?}, got {parsed:?}");
            let cmd = parsed.unwrap();
            assert!(check(&cmd), "predicate failed for {argv:?} → {cmd:?}");
        }
    }

    /// `telemetry prune` mutates the store, so a bare invocation refuses.
    #[test]
    fn telemetry_prune_requires_a_bound_or_an_explicit_yes() {
        assert!(
            parse(args(&["telemetry", "prune"]))
                .unwrap_err()
                .contains("--yes")
        );
        assert!(matches!(
            parse(args(&["telemetry", "prune", "--yes"])),
            Ok(Command::TelemetryPrune {
                max_age_days: None,
                max_bytes: None
            })
        ));
        assert!(matches!(
            parse(args(&["telemetry", "prune", "--max-age-days", "7"])),
            Ok(Command::TelemetryPrune {
                max_age_days: Some(7),
                ..
            })
        ));
    }

    /// `plak config path` is retired: `config show` prints `config_path=` first.
    #[test]
    fn config_path_is_retired_in_favour_of_config_show() {
        assert!(parse(args(&["config", "path"])).is_err());
        assert!(matches!(
            parse(args(&["config", "show"])),
            Ok(Command::ConfigShow)
        ));
    }

    #[test]
    fn secret_values_never_come_from_argv_and_names_follow_the_grammar() {
        for argv in [
            &["secret", "set", "github", "ghp-value"][..],
            &["secret", "set", "Bad"][..],
            &["secret", "rm", "../x"][..],
            &["secret", "list", "github"][..],
        ] {
            assert!(parse(args(argv)).is_err(), "must reject {argv:?}");
        }
    }

    #[test]
    fn mcp_add_takes_secret_names_never_values_and_one_transport() {
        use crate::commands::mcp::McpCommand;
        use crate::product_config::McpTransport;
        let Ok(Command::Mcp(McpCommand::Add { id, server, .. })) = parse(args(&[
            "mcp",
            "add",
            "github",
            "--secret-env",
            "GITHUB_PERSONAL_ACCESS_TOKEN=github",
            "--tools",
            "get_me,search_code",
            "--",
            "docker",
            "run",
            "-i",
            "--rm",
            "img",
        ])) else {
            panic!("stdio add must parse");
        };
        assert_eq!(id, "github");
        assert_eq!(
            server.tools.as_deref(),
            Some(&["get_me".to_owned(), "search_code".to_owned()][..])
        );
        let McpTransport::Stdio {
            command,
            args: argv,
            secret_env,
            ..
        } = server.transport
        else {
            panic!("expected stdio");
        };
        assert_eq!(command, "docker");
        assert_eq!(argv, ["run", "-i", "--rm", "img"]);
        assert_eq!(secret_env["GITHUB_PERSONAL_ACCESS_TOKEN"], "github");
        let Ok(Command::Mcp(McpCommand::Add { server, .. })) = parse(args(&[
            "mcp",
            "add",
            "docs",
            "--url",
            "https://docs.test/mcp",
            "--oauth",
            "--oauth-issuer",
            "https://auth.test",
            "--oauth-client-id",
            "plak",
            "--oauth-client-secret",
            "docs-client",
            "--oauth-callback-port",
            "4321",
            "--resources",
        ])) else {
            panic!("oauth add must parse");
        };
        assert!(server.resources);
        let McpTransport::Http { oauth, .. } = server.transport else {
            panic!("expected http");
        };
        assert_eq!(
            oauth,
            Some(crate::product_config::McpOAuthConfig {
                issuer: Some("https://auth.test".to_owned()),
                client: Some(crate::product_config::McpOAuthClient {
                    id: "plak".to_owned(),
                    secret: Some("docs-client".to_owned()),
                }),
                callback_port: Some(4321),
            })
        );
        assert!(matches!(
            parse(args(&[
                "mcp",
                "add",
                "gh",
                "--url",
                "https://x.test/mcp",
                "--bearer-secret",
                "github"
            ])),
            Ok(Command::Mcp(McpCommand::Add { .. }))
        ));
        for argv in [
            &["mcp", "add", "gh"][..],
            &["mcp", "add", "gh", "--url", "https://x.test", "--", "cmd"][..],
            &[
                "mcp",
                "add",
                "gh",
                "--url",
                "https://x.test",
                "--env",
                "A=b",
            ][..],
            &["mcp", "add", "gh", "--bearer-secret", "s", "--", "cmd"][..],
            &["mcp", "add", "gh", "--secret-env", "NOEQUALS", "--", "cmd"][..],
            &["mcp", "enable", "gh"][..],
            &["mcp", "add", "gh", "--oauth", "--", "cmd"][..],
            &[
                "mcp",
                "add",
                "gh",
                "--url",
                "https://x.test",
                "--oauth-issuer",
                "https://auth.test",
            ][..],
        ] {
            assert!(parse(args(argv)).is_err(), "must reject {argv:?}");
        }
        for argv in [
            &[
                "mcp",
                "add",
                "gh",
                "--url",
                "https://x.test",
                "--bearer-secret",
                "ghp_Pasted_Value",
            ][..],
            &[
                "mcp",
                "add",
                "gh",
                "--secret-env",
                "T=ghp_Pasted_Value",
                "--",
                "cmd",
            ][..],
            &[
                "mcp",
                "add",
                "gh",
                "--url",
                "https://x.test",
                "--header-secret",
                "X-Key=ghp_Pasted_Value",
            ][..],
            &[
                "mcp",
                "add",
                "gh",
                "--url",
                "https://x.test",
                "--header",
                "ghp_Pasted_Value",
            ][..],
            &[
                "mcp",
                "add",
                "gh",
                "--url",
                "https://x.test",
                "--oauth",
                "--oauth-issuer",
                "https://auth.test",
                "--oauth-client-id",
                "plak",
                "--oauth-client-secret",
                "ghp_Pasted_Value",
            ][..],
        ] {
            let refused = parse(args(argv)).unwrap_err();
            assert!(!refused.contains("Pasted"), "{refused}");
        }
        assert!(matches!(
            parse(args(&["mcp"])),
            Ok(Command::CommandHelp { command }) if command == "mcp"
        ));
        assert!(matches!(
            parse(args(&["mcp", "add", "--help"])),
            Ok(Command::CommandHelp { command }) if command == "mcp"
        ));
    }

    #[test]
    fn resume_aliases_accept_at_most_one_session_id() {
        let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        for argv in [["resume", session_id], ["--resume", session_id]] {
            assert!(matches!(
                parse(args(&argv)),
                Ok(Command::Resume { session_id: Some(actual) }) if actual == session_id
            ));
        }

        for argv in [
            &["resume", session_id, "extra"][..],
            &["--resume", session_id, "extra"][..],
            &["--resume", "--unknown"][..],
        ] {
            assert!(parse(args(argv)).is_err(), "must reject {argv:?}");
        }
    }

    #[test]
    fn top_level_init_and_setup_are_absent_but_config_init_remains() {
        for retired in ["init", "setup"] {
            assert_eq!(
                parse(args(&[retired])).unwrap_err(),
                "unknown command; run `plak help` for the command list"
            );
        }
        assert!(matches!(
            parse(args(&["config", "init"])),
            Ok(Command::ConfigInit)
        ));
    }

    #[test]
    fn parser_errors_preserve_useful_categories_without_payloads() {
        assert_eq!(
            parse(args(&["config", "set", "tui.theme"])).unwrap_err(),
            "config set requires a value: plak config set <key> <value>"
        );
        assert_eq!(
            parse(args(&["not-a-command"])).unwrap_err(),
            "unknown command; run `plak help` for the command list"
        );
        assert_eq!(
            parse(args(&["doctor", "--sk-live-secret"])).unwrap_err(),
            "unknown option or unexpected argument; use `plak doctor --help`"
        );
    }

    #[test]
    fn session_delete_abandon_children_is_an_explicit_delete_only_flag() {
        let parsed =
            parse(["session", "delete", "aaaa", "--abandon-children", "--yes"].map(OsString::from));
        assert!(parsed.is_ok(), "{parsed:?}");
        assert!(matches!(
            parsed,
            Ok(Command::SessionDelete {
                yes: true,
                abandon_children: true,
                ..
            })
        ));
        assert!(
            parse(["session", "show", "aaaa", "--abandon-children"].map(OsString::from)).is_err()
        );
        assert!(
            parse(["session", "gc", "aaaa", "--abandon-children"].map(OsString::from)).is_err()
        );
    }

    #[test]
    fn clap_long_options_accept_equals_syntax() {
        assert!(matches!(
            parse(args(&[
                "run",
                "--account=codex",
                "--model=gpt-5.3-codex-spark",
                "--",
                "hello"
            ])),
            Ok(Command::Run { model, prompt, .. })
                if model == "gpt-5.3-codex-spark" && prompt == "hello"
        ));
    }

    #[cfg(not(feature = "maintainer"))]
    #[test]
    fn product_command_argv_rejects_maintainer_verbs() {
        let rejected: &[&[&str]] = &[
            &["compact", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "--yes"],
            &["fork", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "--yes"],
            &["rebind", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "--yes"],
            &["compatibility", "codex"],
            &["update"],
        ];
        for argv in rejected {
            assert!(
                parse(args(argv)).is_err(),
                "release parser must reject {argv:?}"
            );
        }
    }

    #[cfg(feature = "maintainer")]
    #[test]
    #[allow(
        clippy::type_complexity,
        reason = "table of argv slices → predicate is clearer than a macro here"
    )]
    fn product_command_maintainer_argv_matrix() {
        let cases: &[(&[&str], fn(&Command) -> bool)] = &[
            (&["compatibility", "codex"], |c| {
                matches!(c, Command::CompatibilityNotice(NativeProvider::Codex))
            }),
            (
                &["fork", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "--yes"],
                |c| {
                    matches!(
                        c,
                        Command::Fork {
                            confirmed: true,
                            ..
                        }
                    )
                },
            ),
            (
                &["compact", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "--yes"],
                |c| {
                    matches!(
                        c,
                        Command::Compact {
                            confirmed: true,
                            ..
                        }
                    )
                },
            ),
        ];
        for (argv, check) in cases {
            let parsed = parse(args(argv));
            assert!(parsed.is_ok(), "expected Ok for {argv:?}, got {parsed:?}");
            let cmd = parsed.unwrap();
            assert!(check(&cmd), "predicate failed for {argv:?} → {cmd:?}");
        }
        assert!(parse(args(&["update"])).is_err());
    }

    /// Every remedy string any product surface can print must name a command
    /// this build actually has.
    ///
    /// The TUI prints these into the Models readiness header, the Usage
    /// not-connected lines, the notes and `/help`. Before this test they were
    /// free-form strings, and `plak login openrouter` — an OAuth verb on an
    /// API-key account — shipped to operators as advice that only produces a
    /// refusal.
    #[test]
    fn every_tui_remedy_names_a_command_this_build_has() {
        for remedy in crate::tui::remedy_catalog() {
            let mut tokens = remedy.split_whitespace();
            assert_eq!(
                tokens.next(),
                Some("plak"),
                "a remedy is a command line starting with the binary: {remedy}"
            );
            let argv: Vec<&str> = tokens.collect();
            let parsed = parse(args(&argv));
            assert!(
                parsed.is_ok(),
                "remedy `{remedy}` is not a command this build accepts: {:?}",
                parsed.err()
            );
            // A remedy that only prints its own usage has not told the operator
            // what to run, so `CommandHelp` is as much a failure as a parse
            // error.
            assert!(
                !matches!(parsed, Ok(Command::CommandHelp { .. })),
                "remedy `{remedy}` resolves to a usage page, not an action"
            );
        }
    }

    /// The canonical verbs are `plak login` / `plak logout` / `plak key`. The
    /// legacy `plak auth` tree is retired: it names no product surface and is
    /// rejected like any other unknown command.
    #[test]
    fn the_legacy_auth_tree_is_retired_and_the_canonical_verbs_remain() {
        assert_eq!(
            parse(args(&["auth", "login", "codex"])).unwrap_err(),
            "unknown command; run `plak help` for the command list"
        );
        assert!(parse(args(&["login", "codex"])).is_ok());
        assert!(parse(args(&["logout", "codex"])).is_ok());
        assert!(parse(args(&["key", "set", "openrouter"])).is_ok());
        // The defect this closes: an OAuth verb on an API-key account.
        assert!(parse(args(&["login", "openrouter"])).is_err());
    }

    #[test]
    fn product_argv_rejects_garbage_without_reflecting_payloads() {
        let rejected: &[&[&str]] = &[
            &["not-a-command"],
            &["update"],                     // dropped until public releases exist
            &["login", "openai"],            // API key account, not OAuth
            &["run", "--account", "codex"],  // missing --model and prompt
            &["config", "set", "tui.theme"], // missing value
        ];
        for argv in rejected {
            let err = parse(args(argv)).unwrap_err();
            // Redaction: do not echo huge/secret-looking blobs; errors stay short.
            assert!(err.len() < 500, "error too chatty: {err}");
            assert!(!err.contains("sk-live"), "{err}");
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Arbitrary short argv tokens never panic and either parse or return stable Err.
        #[test]
        fn arbitrary_short_argv_never_panics(
            tokens in proptest::collection::vec("[a-zA-Z0-9_\\-]{0,16}", 0..6)
        ) {
            let argv: Vec<OsString> = tokens.into_iter().map(OsString::from).collect();
            let _ = parse(argv);
        }
    }
}
