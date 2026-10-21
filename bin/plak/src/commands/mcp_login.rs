//! `plak mcp login|logout`: the OAuth login of an `auth = "oauth"` MCP
//! server, stored in the home's secrets. Only this command starts a login;
//! a session never does.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use plakous::mcp::{
    McpError, McpLocalRetirement, McpLoginOptions, McpLogoutOutcome, McpOAuthStatus, McpPool,
    McpRemoteLogin, McpServerSpec,
};
use plakous_hosted::error::{CliFailure, CliResult, ProcessOutcome};

use super::login_flow::LoginUi;
use crate::home::paths::PlakPaths;
use crate::home::secrets;
use crate::oauth_ui;
use crate::presentation::MAX_REMOTE_AUTHORIZATION_INPUT_BYTES;
use crate::product_config::ProductConfig;

/// The client name a login registers under with an authorization server.
const CLIENT_NAME: &str = "Plak";

/// `plak mcp login <id> [--scope <scopes>] [--remote]`.
pub(crate) fn login(
    paths: &PlakPaths,
    id: &str,
    scope: Option<&str>,
    remote: bool,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
    ui: &mut impl LoginUi,
) -> CliResult<ProcessOutcome> {
    if !remote && ui.ssh_session_detected() {
        return Err(CliFailure::new(format!(
            "SSH session detected — local browser login is not available here.\n\
             \n\
             Re-run with the explicit remote flag:\n\
             \n\
               plak mcp login {id} --remote\n\
             \n\
             {}",
            remote_follow_up()
        )));
    }
    let config = ProductConfig::load(paths).map_err(CliFailure::new)?;
    let spec = oauth_spec(paths, &config, id)?;
    let mut options = McpLoginOptions::new().with_client_name(CLIENT_NAME);
    if let Some(scope) = scope {
        options = options.with_scope(scope).map_err(failure)?;
    }
    if let Some(url) = &config.mcp_client_id_metadata_url {
        options = options
            .with_client_id_metadata_document(url)
            .map_err(failure)?;
    }
    let label = format!("mcp server {id}");
    // Terminal input runs its own executor, so it is read between, never
    // inside, the login's steps.
    let runtime = runtime()?;
    let login = runtime
        .block_on(McpPool::new().login(&spec, options))
        .map_err(failure)?;
    // What the login trusts, shown before the user approves anything.
    writeln!(
        stderr,
        "{label}: authorization server {}",
        super::mcp::shown(login.issuer())
    )
    .and_then(|()| match login.scope() {
        Some(scope) => writeln!(
            stderr,
            "{label}: requesting scope \"{}\"",
            super::mcp::shown(scope)
        ),
        None => writeln!(stderr, "{label}: requesting no scope"),
    })
    .map_err(|_| stderr_failed())?;
    paths.ensure_mcp_login_dir(id).map_err(CliFailure::new)?;
    let status = if remote {
        match runtime.block_on(login.remote()).map_err(failure)? {
            McpRemoteLogin::Device(device) => {
                oauth_ui::write_device_code_prompt(
                    stderr,
                    &label,
                    device.verification_uri(),
                    device.user_code(),
                    device.expires_in(),
                )
                .map_err(|_| stderr_failed())?;
                runtime.block_on(device.complete()).map_err(failure)?
            }
            McpRemoteLogin::Paste(paste) => {
                if paste.device_skipped_by_policy() {
                    writeln!(stderr, "{label}: advertised device endpoint skipped by transport or client-secret origin policy; falling back to paste-back login")
                        .map_err(|_| stderr_failed())?;
                }
                if let Some(status) = paste.device_refusal_status() {
                    writeln!(stderr, "{label}: device authorization refused (HTTP {status}); falling back to paste-back login")
                        .map_err(|_| stderr_failed())?;
                }
                ui.present_url(stderr, paste.authorization_url(), &label, true)
                    .map_err(|_| stderr_failed())?;
                writeln!(stderr)
                    .and_then(|()| {
                        writeln!(
                            stderr,
                            "After authorizing, the browser is sent to {} and cannot load it.",
                            paste.redirect_uri()
                        )
                    })
                    .and_then(|()| writeln!(stderr, "At the prompt:"))
                    .and_then(|()| {
                        writeln!(
                            stderr,
                            "  • paste the full URL from the address bar, then Enter"
                        )
                    })
                    .and_then(|()| {
                        writeln!(
                            stderr,
                            "  • or type  c  then Enter to re-copy the authorize URL"
                        )
                    })
                    .and_then(|()| writeln!(stderr, "  • empty line cancels"))
                    .map_err(|_| stderr_failed())?;
                let pasted = super::login::read_remote_authorization_interactive(
                    stdin,
                    stderr,
                    paste.authorization_url(),
                    MAX_REMOTE_AUTHORIZATION_INPUT_BYTES,
                    ui,
                )?;
                if pasted.as_str().is_empty() {
                    return Err(CliFailure::new("login cancelled"));
                }
                runtime
                    .block_on(paste.complete(pasted.as_str()))
                    .map_err(failure)?
            }
        }
    } else {
        let local = runtime.block_on(login.local()).map_err(failure)?;
        ui.present_url(stderr, local.authorization_url(), &label, false)
            .map_err(|_| stderr_failed())?;
        let opened = ui.open_browser(local.authorization_url());
        if opened {
            writeln!(stderr, "Browser launch requested on this machine.")
        } else {
            writeln!(
                stderr,
                "No local browser was opened. Open the URL above on this machine, or cancel (Ctrl-C) and use:  plak mcp login {id} --remote"
            )
        }
        .and_then(|()| writeln!(stderr, "Plak is listening for: {}", local.redirect_uri()))
        .and_then(|()| {
            writeln!(
                stderr,
                "  timeout at {}s · Ctrl-C cancels",
                oauth_ui::LOCAL_HARD_TIMEOUT.as_secs()
            )
        })
        .and_then(|()| stderr.flush())
        .map_err(|_| stderr_failed())?;
        let waiting = Arc::new(AtomicBool::new(true));
        ui.wait_reminder(label.clone(), Arc::clone(&waiting));
        let completed = runtime.block_on(local.complete(oauth_ui::LOCAL_HARD_TIMEOUT));
        waiting.store(false, Ordering::Relaxed);
        completed.map_err(|error| {
            CliFailure::new(format!(
                "{label}: {error}\n\n\
                 If you are on SSH, headless, or the browser is elsewhere, run:\n\
                 \n\
                   plak mcp login {id} --remote\n\
                 \n\
                 {}",
                remote_follow_up()
            ))
        })?
    };
    writeln!(
        stderr,
        "{label}: {}",
        super::mcp::shown(&logged_in(&status))
    )
    .map_err(|_| stderr_failed())?;
    Ok(ProcessOutcome::Success)
}

/// `plak mcp logout <id> [--local]`.
pub(crate) fn logout(
    paths: &PlakPaths,
    id: &str,
    local: bool,
    stdout: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    let config = ProductConfig::load(paths).map_err(CliFailure::new)?;
    let spec = oauth_spec(paths, &config, id)?;
    let outcome = runtime()?
        .block_on(McpPool::new().logout(&spec, local))
        .map_err(|error| {
            CliFailure::new(format!(
                "mcp server {id}: {error}; `plak mcp logout {id} --local` retires it here without revoking"
            ))
        })?;
    let remains = "the grant may remain active at the authorization server";
    let line = match outcome {
        McpLogoutOutcome::NotLoggedIn => {
            format!("mcp server {id} is not logged in; nothing to log out")
        }
        McpLogoutOutcome::Revoked => {
            format!("logged out of mcp server {id}; the authorization server revoked the grant")
        }
        McpLogoutOutcome::RetiredLocally(reason) => {
            let why = match reason {
                McpLocalRetirement::Requested => "not revoked (--local)",
                McpLocalRetirement::NoRevocationEndpoint => {
                    "not revoked: its authorization server offers no revocation"
                }
                McpLocalRetirement::Unusable => {
                    "not revoked: the login is not usable (quarantined, expired or unreadable)"
                }
                McpLocalRetirement::DefinitionChanged => {
                    "not revoked: the server definition changed"
                }
                _ => "not revoked",
            };
            format!("retired the login of mcp server {id} here, {why}; {remains}")
        }
        _ => format!("mcp server {id}: logged out"),
    };
    writeln!(stdout, "{line}").map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

fn remote_follow_up() -> &'static str {
    "Then open the URL it prints in a browser on any device: enter the one-time\n\
     code it shows, or paste back the URL the browser was sent to."
}

/// The server's definition for a login or logout; refuses an unknown id and
/// a table without `auth = "oauth"`.
fn oauth_spec(paths: &PlakPaths, config: &ProductConfig, id: &str) -> CliResult<McpServerSpec> {
    let rendered = config.tool_sections.render_server(id).ok_or_else(|| {
        CliFailure::new(format!(
            "no MCP server {id}; `plak mcp list` shows the configured servers"
        ))
    })?;
    plakous_hosted::mcp_binding::mcp_oauth_spec(
        id,
        &rendered,
        secrets::ProductNamedSecrets::shared(paths),
    )
    .map_err(CliFailure::new)
}

fn logged_in(status: &McpOAuthStatus) -> String {
    match status {
        McpOAuthStatus::LoggedIn {
            scope, refreshable, ..
        } => {
            let mut line = match scope {
                Some(scope) => format!("logged in with scope \"{scope}\""),
                None => "logged in".to_owned(),
            };
            if !refreshable {
                line.push_str(
                    "; the server issued no refresh token, so log in again when the token expires",
                );
            }
            line
        }
        _ => "the login was stored but cannot be used; `plak mcp show` shows its state".to_owned(),
    }
}

fn runtime() -> CliResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("plak-mcp-login")
        .enable_all()
        .build()
        .map_err(|_| CliFailure::new("the login executor could not be started"))
}

fn failure(error: McpError) -> CliFailure {
    CliFailure::new(error.to_string())
}

fn stderr_failed() -> CliFailure {
    CliFailure::new("stderr failed")
}
