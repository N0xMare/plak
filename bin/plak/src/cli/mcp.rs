use super::{ArgAction, Command, Subcommand};

#[derive(Debug, Subcommand)]
pub(super) enum McpAction {
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
pub(super) struct McpOAuthFlags {
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

impl McpOAuthFlags {
    fn into_config(self) -> Result<Option<crate::product_config::McpOAuthConfig>, String> {
        let McpOAuthFlags {
            oauth,
            oauth_issuer,
            oauth_client_id,
            oauth_client_secret,
            oauth_callback_port,
        } = self;
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
        Ok(oauth)
    }
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
    pub(super) fn into_command(self) -> Result<Command, String> {
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
                let oauth = oauth.into_config()?;
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
