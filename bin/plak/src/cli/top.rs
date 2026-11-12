use super::{
    Command, HostAction, NativeLoginMode, RunPrompt, StateAction, TopCommand, TreeAction,
    WireAction, finish_agent, finish_continue_session, finish_run, parse_oauth_account,
};

impl TopCommand {
    #[allow(
        clippy::too_many_lines,
        reason = "The exhaustive top-level grammar maps each clap variant once, with run help and session-binding refusals in their original precedence."
    )]
    pub(super) fn into_command(self) -> Result<Command, String> {
        match self {
            Self::Accounts { action } => match action {
                super::AccountAction::List => Ok(Command::AccountsList),
                super::AccountAction::Rm { account, confirmed } => Ok(Command::AccountsRemove {
                    account: account.parse().map_err(
                        |error: plakous_hosted::capabilities::AccountAdmissionError| {
                            error.to_string()
                        },
                    )?,
                    confirmed,
                }),
            },
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
                        account: Some(account.parse().map_err(
                            |error: plakous_hosted::capabilities::AccountAdmissionError| {
                                error.to_string()
                            },
                        )?),
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
            } => action.into_command(crate::home::paths::SecretShelf::Named, "secret"),
            Self::Runner { action: None } => Ok(Command::CommandHelp {
                command: "runner".to_owned(),
            }),
            Self::Runner {
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
            } => Ok(Command::CompatibilityNotice(
                parse_oauth_account(&provider)?
                    .vertical()
                    .capability()
                    .native_provider()
                    .ok_or("compatibility requires a subscription account")?,
            )),
            Self::Models { provider: None } => Ok(Command::CommandHelp {
                command: "models".to_owned(),
            }),
            Self::Models {
                provider: Some(value),
            } => {
                let account: super::AccountRef = value.parse().map_err(
                    |error: plakous_hosted::capabilities::AccountAdmissionError| error.to_string(),
                )?;
                if account.vertical() == super::Vertical::OpenRouter {
                    Ok(Command::ModelsOpenRouter(account))
                } else if account.vertical().is_api() {
                    Ok(Command::ModelsPlatform(account))
                } else {
                    Ok(Command::Models(account))
                }
            }
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
                        limits,
                    );
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
                stdio_wire,
            } => finish_agent(
                account.or(provider),
                model,
                effort,
                composition,
                protocol,
                session,
            )
            .map(|command| command.speaking_wire(stdio_wire.as_deref() == Some("v2"))),
            Self::Resume { session } => Ok(Command::Resume {
                session_id: session,
                handoff: None,
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
            Self::Tree { action: None }
            | Self::Tree {
                action: Some(TreeAction::Release { tree_id: None, .. }),
            } => Ok(Command::CommandHelp {
                command: "tree".to_owned(),
            }),
            Self::Tree {
                action:
                    Some(TreeAction::Release {
                        tree_id: Some(tree_id),
                        force,
                    }),
            } => Ok(Command::TreeRelease { tree_id, force }),
            Self::Usage {
                account,
                session_id,
            } => Ok(Command::Usage {
                account: account
                    .map(|value| {
                        value.parse().map_err(
                            |error: plakous_hosted::capabilities::AccountAdmissionError| {
                                error.to_string()
                            },
                        )
                    })
                    .transpose()?,
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
            Self::Host {
                action:
                    Some(HostAction::Install {
                        web,
                        ui,
                        dry_run,
                        dev_login,
                    }),
                ..
            } => {
                if dev_login.is_some() {
                    return Err("plak host install never accepts --dev-login".into());
                }
                Ok(Command::HostInstall(crate::command::HostInstall {
                    web,
                    ui,
                    dry_run,
                }))
            }
            Self::Host {
                action: Some(HostAction::Connect { stdio, auto_start }),
                ..
            } => {
                if !stdio {
                    return Err("plak host connect requires --stdio".to_owned());
                }
                Ok(Command::HostConnect { auto_start })
            }
            Self::Host {
                action:
                    Some(
                        action @ (HostAction::StartAuto
                        | HostAction::SuperviseAuto
                        | HostAction::ServeAuto),
                    ),
                ..
            } => Ok(Command::Host(crate::command::HostCommand {
                start: match action {
                    HostAction::StartAuto => crate::command::HostStart::Helper,
                    HostAction::SuperviseAuto => crate::command::HostStart::Supervisor,
                    _ => crate::command::HostStart::Detached,
                },
                ..crate::command::HostCommand::default()
            })),
            Self::Host {
                action:
                    Some(HostAction::Upgrade {
                        expect_binary,
                        wait,
                        abandon,
                    }),
                ..
            } => Ok(Command::HostUpgrade {
                expect_binary,
                abandon,
                wait: std::time::Duration::from_secs(wait),
            }),
            Self::Host {
                action: None,
                web,
                ui,
                dev_login,
                idle_release,
            } => Ok(Command::Host(crate::command::HostCommand {
                start: crate::command::HostStart::Foreground,
                web: web.map(|listen| crate::command::HostWeb {
                    listen,
                    ui,
                    dev_login,
                }),
                idle_release: idle_release.map(std::time::Duration::from_secs),
            })),
            Self::Wire {
                action: WireAction::Schema,
            } => Ok(Command::WireSchema),
            Self::Version {
                host_upgrade_probe: true,
                host_upgrade_service,
                ..
            } => Ok(Command::HostUpgradeProbe {
                service: host_upgrade_service,
            }),
            Self::Version {
                host_upgrade_capacity: Some(path),
                ..
            } => Ok(Command::HostUpgradeCapacity { path }),
            Self::Version {
                write_binary_manifest: Some(directory),
                ..
            } => Ok(Command::WriteBinaryManifest { directory }),
            Self::Version {
                carry_probe: Some(path),
                ..
            } => Ok(Command::CarryProbe { path }),
            Self::Version {
                reopen_probe: Some(session),
                host_upgrade_reopen,
                ..
            } => Ok(Command::ReopenProbe {
                session,
                host_upgrade_reopen,
            }),
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
