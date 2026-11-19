//! Account commands when a gateway holds the store.
//!
//! The command prints `via gateway <label>` and sends the admin verb.

use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use plakous::gateway::protocol::admin::{
    AdminRefusal, AdminRequest, AdminResponse, LoginMethod, LoginPhase, LogoutMode,
};
use plakous::gateway::protocol::facts::{CredentialStatus, KeySource};
use plakous::gateway::protocol::{BoundedText, GatewayAccount, SecretText};
use plakous::subscriptions::{NativeLoginMode, NativeSubscriptions};
use plakous_hosted::account::AccountRef;
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult, ProcessOutcome};

use super::AdminLink;
use super::address::GatewayAddress;
use super::client;
use crate::command::Command;
use crate::commands::login::read_remote_authorization_interactive;
use crate::commands::login_flow::{LoginUi as _, SystemLoginUi};
use crate::presentation::MAX_REMOTE_AUTHORIZATION_INPUT_BYTES;
use crate::shell::services::Services;
use crate::terminal_application::read_terminal_secret_line;

/// Forwards an account command to the separate gateway.
/// `Ok(None)` means the command runs in this process: it is not an account
/// command, or it dispatches model traffic (`run`, `continue`, `models`),
/// whose routes take this process's gateway custody themselves.
pub(crate) fn try_remote_account(
    services: &Services,
    command: &Command,
    stdin: &mut dyn io::BufRead,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> CliResult<Option<ProcessOutcome>> {
    if !is_account_command(command) {
        return Ok(None);
    }
    let link = AdminLink::for_cli(services.paths()?).map_err(CliFailure::new)?;
    let address = &link.address;
    let key = link.key();
    client::versions(address, key).map_err(CliFailure::new)?;
    writeln!(stdout, "via gateway {}", address.label())
        .map_err(|_| CliFailure::new("stdout failed"))?;
    dispatch(address, key, command, stdin, stdout, stderr).map(Some)
}

pub(crate) fn is_account_command(command: &Command) -> bool {
    matches!(
        command,
        Command::AuthStatus(_)
            | Command::AuthLogin { .. }
            | Command::AuthRefresh(_)
            | Command::AuthLogout { .. }
            | Command::KeySet { .. }
            | Command::KeyClear { .. }
            | Command::KeyStatus { .. }
            | Command::AccountsList
            | Command::AccountsRemove { .. }
            | Command::StatusAll
            | Command::Usage {
                session_id: None,
                ..
            }
    )
}

fn dispatch(
    address: &GatewayAddress,
    key: Option<&str>,
    command: &Command,
    stdin: &mut dyn io::BufRead,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    match command {
        Command::AccountsList => list_accounts(address, key, stdout),
        Command::AccountsRemove { account, confirmed } => remove_account(
            address,
            key,
            account.as_str(),
            *confirmed,
            stdin,
            stdout,
            stderr,
        ),
        Command::KeySet { account } => key_set(address, key, account.as_str(), stdin, stderr),
        Command::KeyClear { account } => {
            changed(
                address,
                key,
                AdminRequest::KeyClear {
                    account: account_of(account.as_str())?,
                },
            )?;
            writeln!(stdout, "cleared").map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        Command::KeyStatus { account } => status(
            address,
            key,
            account.as_ref().map(AccountRef::as_str),
            stdout,
        ),
        Command::AuthStatus(account) => status(address, key, Some(account.as_str()), stdout),
        Command::StatusAll => status(address, key, None, stdout),
        Command::AuthRefresh(account) => refresh(address, key, account.as_str(), stdout),
        Command::AuthLogout {
            provider,
            local_only,
        } => logout(address, key, provider.as_str(), *local_only, stdout),
        Command::AuthLogin { provider, mode } => {
            login(address, key, provider.as_str(), *mode, stdin, stderr)
        }
        Command::Usage { account, .. } => usage(address, key, account.as_ref(), stdout),
        _ => Ok(ProcessOutcome::Success),
    }
}

fn list_accounts(
    address: &GatewayAddress,
    key: Option<&str>,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    for account in listed_accounts(address, key)? {
        writeln!(stdout, "{account}").map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}

fn listed_accounts(address: &GatewayAddress, key: Option<&str>) -> CliResult<Vec<GatewayAccount>> {
    match ask(address, key, AdminRequest::List {})? {
        AdminResponse::Accounts { accounts } => Ok(accounts),
        other => unexpected(other),
    }
}

fn remove_account(
    address: &GatewayAddress,
    key: Option<&str>,
    account: &str,
    confirmed: bool,
    stdin: &mut dyn io::BufRead,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    if !confirmed {
        writeln!(stderr, "Remove {account}? Type {account} to confirm:")
            .map_err(|_| CliFailure::new("stderr failed"))?;
        let mut answer = String::new();
        stdin
            .read_line(&mut answer)
            .map_err(|_| CliFailure::new("stdin failed"))?;
        if answer.trim() != account {
            return Err(CliFailure::new(
                "account removal was not confirmed; nothing changed",
            ));
        }
    }
    changed(
        address,
        key,
        AdminRequest::Remove {
            account: account_of(account)?,
        },
    )?;
    writeln!(stdout, "{account}: removed").map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

fn key_set(
    address: &GatewayAddress,
    key: Option<&str>,
    account: &str,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    write!(stderr, "API key for {account} (input hidden): ")
        .map_err(|_| CliFailure::new("stderr failed"))?;
    stderr
        .flush()
        .map_err(|_| CliFailure::new("stderr failed"))?;
    let secret = read_terminal_secret_line(stdin, stderr, MAX_REMOTE_AUTHORIZATION_INPUT_BYTES)
        .map_err(|_| CliFailure::new("could not read API key"))?;
    let secret = SecretText::new(zeroize::Zeroizing::new(secret.as_str().to_owned()))
        .map_err(|error| CliFailure::new(error.to_string()))?;
    changed(
        address,
        key,
        AdminRequest::KeySet {
            account: account_of(account)?,
            secret,
        },
    )?;
    writeln!(
        stderr,
        "stored API key for {account} (fingerprint only on status)"
    )
    .map_err(|_| CliFailure::new("stderr failed"))?;
    Ok(ProcessOutcome::Success)
}

fn status(
    address: &GatewayAddress,
    key: Option<&str>,
    account: Option<&str>,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    account_rows(address, key, account, stdout, status_cell)
}

fn status_cell(
    address: &GatewayAddress,
    key: Option<&str>,
    account: &GatewayAccount,
) -> CliResult<String> {
    let response = ask(
        address,
        key,
        AdminRequest::Status {
            accounts: vec![account.clone()],
        },
    )?;
    let AdminResponse::Status { rows } = response else {
        return unexpected(response);
    };
    let [row] = rows.as_slice() else {
        return Err(CliFailure::new("the gateway did not return one status row"));
    };
    if row.facts.account != *account {
        return Err(CliFailure::new(
            "the gateway returned status for a different account",
        ));
    }
    let source =
        row.facts
            .key
            .as_ref()
            .map_or_else(String::new, |key| match (&key.source, &key.env_var) {
                (KeySource::Environment, Some(variable)) => {
                    format!(" source=environment ({})", variable.as_str())
                }
                (KeySource::Environment, None) => " source=environment".to_owned(),
                (KeySource::File, _) => " source=file".to_owned(),
                (KeySource::Missing, _) => " source=missing".to_owned(),
            });
    // Same fingerprint as in-process key status, for comparison across custody flips.
    let fingerprint = row
        .facts
        .key
        .as_ref()
        .and_then(|key| key.fingerprint.as_ref())
        .map_or_else(String::new, |fingerprint| {
            format!(" fingerprint={}", fingerprint.as_str())
        });
    Ok(format!(
        "credential={}{source}{fingerprint}",
        credential_word(row.facts.credential.status)
    ))
}

fn refresh(
    address: &GatewayAddress,
    key: Option<&str>,
    account: &str,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    match ask(
        address,
        key,
        AdminRequest::Refresh {
            account: account_of(account)?,
        },
    )? {
        AdminResponse::Refreshed { facts } => {
            writeln!(
                stdout,
                "{account}: credential={}",
                credential_word(facts.credential.status)
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        other => unexpected(other),
    }
}

fn logout(
    address: &GatewayAddress,
    key: Option<&str>,
    account: &str,
    local_only: bool,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let mode = if local_only {
        LogoutMode::Local
    } else {
        LogoutMode::Remote
    };
    match ask(
        address,
        key,
        AdminRequest::Logout {
            account: account_of(account)?,
            mode,
        },
    )? {
        AdminResponse::Changed {
            remote_grant_may_remain,
            ..
        } => {
            let tail = if remote_grant_may_remain == Some(true) {
                "; the remote grant may remain active"
            } else {
                ""
            };
            writeln!(stdout, "{account}: logged out{tail}")
                .map_err(|_| CliFailure::new("stdout failed"))?;
            Ok(ProcessOutcome::Success)
        }
        other => unexpected(other),
    }
}

fn usage(
    address: &GatewayAddress,
    key: Option<&str>,
    account: Option<&AccountRef>,
    stdout: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    account_rows(
        address,
        key,
        account.map(AccountRef::as_str),
        stdout,
        usage_cell,
    )
}

fn usage_cell(
    address: &GatewayAddress,
    key: Option<&str>,
    account: &GatewayAccount,
) -> CliResult<String> {
    match ask(
        address,
        key,
        AdminRequest::Allowance {
            account: account.clone(),
        },
    )? {
        AdminResponse::Allowance {
            unavailable, view, ..
        } => Ok(unavailable
            .map(|text| text.as_str().to_owned())
            .or_else(|| view.map(|value| value.to_string()))
            .unwrap_or_else(|| "unavailable".to_owned())),
        other => unexpected(other),
    }
}

fn account_rows(
    address: &GatewayAddress,
    key: Option<&str>,
    account: Option<&str>,
    stdout: &mut dyn Write,
    cell: impl Fn(&GatewayAddress, Option<&str>, &GatewayAccount) -> CliResult<String>,
) -> CliResult<ProcessOutcome> {
    let all = account.is_none();
    let accounts = match account {
        Some(account) => vec![account_of(account)?],
        None => listed_accounts(address, key)?,
    };
    for account in accounts {
        let detail = match cell(address, key, &account) {
            Ok(detail) => detail,
            Err(error) if all => format!("error={}", error.message()),
            Err(error) => return Err(error),
        };
        writeln!(stdout, "{account}: {detail}").map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}

fn login(
    address: &GatewayAddress,
    key: Option<&str>,
    account: &str,
    mode: NativeLoginMode,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    if mode == NativeLoginMode::Browser {
        writeln!(
            stderr,
            "{account}: the gateway runs the remote login; a browser callback cannot reach its store"
        )
        .map_err(|_| CliFailure::new("stderr failed"))?;
    }
    let revision = accepted_notice(account, stdin, stderr)?;
    let mut replace_newer = false;
    loop {
        let response = ask(
            address,
            key,
            AdminRequest::LoginStart {
                account: account_of(account)?,
                notice_revision: bounded(&revision)?,
                replace_newer,
            },
        )?;
        if !replace_newer
            && matches!(
                &response,
                AdminResponse::Refused {
                    refusal: AdminRefusal::WrittenByNewerBuild {},
                }
            )
        {
            writeln!(stderr, "a newer build wrote this login. Replace it? [y/N]")
                .map_err(|_| CliFailure::new("stderr failed"))?;
            let mut answer = String::new();
            stdin
                .read_line(&mut answer)
                .map_err(|_| CliFailure::new("stdin failed"))?;
            if !answer.trim().eq_ignore_ascii_case("y") {
                return Err(CliFailure::classified(
                    CliFailureKind::Auth,
                    "login was not replaced; nothing changed",
                ));
            }
            replace_newer = true;
            continue;
        }
        return finish_login(address, key, response, mode, stdin, stderr);
    }
}

fn finish_login(
    address: &GatewayAddress,
    key: Option<&str>,
    response: AdminResponse,
    mode: NativeLoginMode,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let AdminResponse::Challenge {
        attempt,
        method,
        authorize_url,
        user_code,
        warning,
        ..
    } = response
    else {
        return unexpected(response);
    };
    if let Some(warning) = warning {
        writeln!(stderr, "{}", warning.as_str()).map_err(|_| CliFailure::new("stderr failed"))?;
    }
    let mut ui = SystemLoginUi;
    ui.present_url(
        stderr,
        authorize_url.expose(),
        "gateway",
        method == LoginMethod::Paste,
    )
    .map_err(|_| CliFailure::new("stderr failed"))?;
    if mode == NativeLoginMode::Browser && !ui.ssh_session_detected() {
        let _ = ui.open_browser(authorize_url.expose());
    }
    if let Some(code) = user_code.as_ref() {
        writeln!(stderr, "code {}", code.expose()).map_err(|_| CliFailure::new("stderr failed"))?;
    }
    if method == LoginMethod::Paste {
        write!(stderr, "paste the provider response (input hidden): ")
            .map_err(|_| CliFailure::new("stderr failed"))?;
        stderr
            .flush()
            .map_err(|_| CliFailure::new("stderr failed"))?;
        let pasted = read_remote_authorization_interactive(
            stdin,
            stderr,
            authorize_url.expose(),
            MAX_REMOTE_AUTHORIZATION_INPUT_BYTES,
            &mut ui,
        )?;
        let pasted = SecretText::new(zeroize::Zeroizing::new(pasted.as_str().to_owned()))
            .map_err(|error| CliFailure::new(error.to_string()))?;
        return finish_progress(ask(
            address,
            key,
            AdminRequest::LoginComplete { attempt, pasted },
        )?);
    }
    let waiting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    ui.wait_reminder("gateway".to_owned(), waiting.clone());
    let result = (|| {
        for _ in 0..900 {
            thread::sleep(Duration::from_millis(1100));
            let progress = ask(
                address,
                key,
                AdminRequest::LoginPoll {
                    attempt: attempt.clone(),
                },
            )?;
            if terminal_phase(&progress) {
                return finish_progress(progress);
            }
        }
        let _ = ask(address, key, AdminRequest::LoginCancel { attempt });
        Err(CliFailure::classified(
            CliFailureKind::Auth,
            "the login attempt expired",
        ))
    })();
    waiting.store(false, std::sync::atomic::Ordering::Release);
    result
}

fn finish_progress(response: AdminResponse) -> CliResult<ProcessOutcome> {
    match response {
        AdminResponse::Progress {
            phase: LoginPhase::Succeeded,
            ..
        } => Ok(ProcessOutcome::Success),
        AdminResponse::Progress { phase, warning, .. } => Err(CliFailure::classified(
            CliFailureKind::Auth,
            format!(
                "login {phase:?}{}",
                warning
                    .map(|text| format!(" ({})", text.as_str()))
                    .unwrap_or_default()
            ),
        )),
        other => unexpected(other),
    }
}

fn terminal_phase(response: &AdminResponse) -> bool {
    matches!(
        response,
        AdminResponse::Progress {
            phase: LoginPhase::Succeeded
                | LoginPhase::Failed
                | LoginPhase::OutcomeUnknown
                | LoginPhase::Cancelled
                | LoginPhase::Expired,
            ..
        } | AdminResponse::Refused { .. }
    )
}

fn accepted_notice(
    account: &str,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn Write,
) -> CliResult<String> {
    let parsed: AccountRef = account.parse().map_err(
        |error: plakous_hosted::capabilities::AccountAdmissionError| {
            CliFailure::new(error.to_string())
        },
    )?;
    let provider = crate::commands::application::native_provider(&parsed)?;
    let notice = NativeSubscriptions::notice(provider)
        .map_err(|error| CliFailure::classified(CliFailureKind::Auth, error.to_string()))?;
    writeln!(
        stderr,
        "{account} compatibility profile {} revision {}. Continue? [y/N]",
        notice.profile_id(),
        notice.notice_revision()
    )
    .map_err(|_| CliFailure::new("stderr failed"))?;
    let mut answer = String::new();
    stdin
        .read_line(&mut answer)
        .map_err(|_| CliFailure::new("stdin failed"))?;
    if answer.trim().eq_ignore_ascii_case("y") || answer.trim() == notice.notice_revision() {
        Ok(notice.notice_revision().to_owned())
    } else {
        Err(CliFailure::classified(
            CliFailureKind::Auth,
            format!("compatibility confirmation was declined; nothing was sent to {account}"),
        ))
    }
}

fn changed(address: &GatewayAddress, key: Option<&str>, request: AdminRequest) -> CliResult<()> {
    match ask(address, key, request)? {
        AdminResponse::Changed { .. } => Ok(()),
        other => unexpected(other),
    }
}

fn ask(
    address: &GatewayAddress,
    key: Option<&str>,
    request: AdminRequest,
) -> CliResult<AdminResponse> {
    client::admin(address, key, &request).map_err(CliFailure::new)
}

fn account_of(account: &str) -> CliResult<GatewayAccount> {
    GatewayAccount::new(account.to_owned()).map_err(|error| CliFailure::new(error.to_string()))
}

fn bounded(text: &str) -> CliResult<BoundedText<128>> {
    BoundedText::new(text.to_owned()).map_err(|error| CliFailure::new(error.to_string()))
}

fn unexpected<T>(response: AdminResponse) -> CliResult<T> {
    match response {
        AdminResponse::Refused { refusal } => Err(CliFailure::classified(
            CliFailureKind::Auth,
            refusal_text(&refusal),
        )),
        _ => Err(CliFailure::new(
            "the gateway returned an unexpected admin response",
        )),
    }
}

fn refusal_text(refusal: &AdminRefusal) -> String {
    match refusal {
        AdminRefusal::EnvironmentOverride { variable } => {
            format!(
                "the gateway environment variable {} overrides the stored key",
                variable.as_str()
            )
        }
        AdminRefusal::LocalAdminRequired {} => {
            "replacing an account or this verb needs the local administrator".to_owned()
        }
        other => format!("{other:?}").to_lowercase().replace(' ', "_"),
    }
}

fn credential_word(status: CredentialStatus) -> &'static str {
    match status {
        CredentialStatus::SignedOut => "signed-out",
        CredentialStatus::Ready => "ready",
        CredentialStatus::RefreshRequired => "refresh-required",
        CredentialStatus::Refreshing => "refreshing",
        CredentialStatus::Quarantined => "quarantined",
        CredentialStatus::LoginRequired => "login-required",
        CredentialStatus::ProfileChanged => "profile-changed",
        CredentialStatus::WrittenByNewerBuild => "written-by-newer-build",
        CredentialStatus::Revoked => "revoked",
    }
}
