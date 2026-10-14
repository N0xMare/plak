//! Subscription compatibility acceptance, login/refresh/logout and API-key
//! credential workflows.
//!
//! Every terminal secret read on these paths is cooked-mode only: the TTY is
//! never put into raw mode, and no secret is echoed back or logged.

use std::io::{self, IsTerminal as _};

use crate::terminal_application::read_terminal_secret_line;
use plakous::subscriptions::{NativeLoginMode, NativeProvider, NativeSubscriptions};

use super::application::AccountOps;
use super::support::{
    cli_provider_name, compatibility_status_name, credential_status_name, profile_date,
    profile_inactive_failure, require_locally_retired, require_ready, require_revoked,
};
use crate::home::paths as product_paths;
use crate::home::secrets as product_secrets;
use crate::oauth_ui as product_oauth_ui;
use crate::presentation::{
    CompatibilityState, CredentialState, MAX_REMOTE_AUTHORIZATION_INPUT_BYTES,
};
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult, ProcessOutcome};

/// `plak status <provider>`: offline credential and compatibility posture.
pub fn auth_status(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
    paths: Option<&product_paths::PlakPaths>,
    stdout: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    let status = backend.offline_status(provider)?;
    let repair = if status.credential == CredentialState::Ready {
        String::new()
    } else {
        format!("; run plak login {}", cli_provider_name(provider))
    };
    writeln!(
        stdout,
        "{}: compatibility={}, credential={}{repair}",
        cli_provider_name(provider),
        compatibility_status_name(status.compatibility),
        super::support::oauth_refresh_status_detail(paths, provider, status.credential)
            .unwrap_or_else(|| credential_status_name(status.credential).to_owned())
    )
    .map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

/// `plak login <provider>`: exact acceptance, then the provider login flow.
pub(crate) fn auth_login(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
    mode: NativeLoginMode,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
    ui: &mut impl super::login_flow::LoginUi,
) -> CliResult<ProcessOutcome> {
    accept_compatibility(backend, provider, Asker::Login, stdin, stderr)
        .map_err(|failure| failure.or_kind(CliFailureKind::Auth))?;
    let status = super::login_flow::perform_login(backend, provider, mode, stdin, stderr, ui)?;
    require_ready(provider, status)?;
    writeln!(stderr)
        .and_then(|()| {
            writeln!(
                stderr,
                "{}: login succeeded — credential ready.",
                cli_provider_name(provider)
            )
        })
        .map_err(|_| CliFailure::new("stderr failed"))?;
    Ok(ProcessOutcome::Success)
}

/// `plak refresh <provider>`.
pub fn auth_refresh(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    ensure_compatibility(backend, provider, stdin, stderr)?;
    let status = backend.refresh(provider)?;
    require_ready(provider, status)?;
    writeln!(
        stderr,
        "{}: refresh succeeded — credential ready.",
        cli_provider_name(provider)
    )
    .map_err(|_| CliFailure::new("stderr failed"))?;
    Ok(ProcessOutcome::Success)
}

/// `plak logout <provider> --local`: retire the local credential only.
pub fn auth_logout_local(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
    stderr: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    let status = backend.logout_local(provider)?;
    require_locally_retired(
        status,
        "local logout did not reach a safely retired local credential state",
    )?;
    let outcome = match status {
        CredentialState::SignedOut => "no local credential remains; recovery state cleared",
        CredentialState::Revoked => "local credential revoked; recovery state cleared",
        _ => {
            return Err(CliFailure::new(
                "local credential retirement invariant failed",
            ));
        }
    };
    writeln!(
        stderr,
        "{}: {outcome}; remote credential may remain active",
        cli_provider_name(provider)
    )
    .map_err(|_| CliFailure::new("stderr failed"))?;
    Ok(ProcessOutcome::Success)
}

/// `plak logout <provider>`: revoke the remote credential.
pub fn auth_logout_remote(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    ensure_compatibility(backend, provider, stdin, stderr)?;
    let status = backend.revoke(provider)?;
    require_revoked(
        status,
        "remote revocation did not reach a revoked credential state",
    )?;
    writeln!(
        stderr,
        "{}: remote credential revoked",
        cli_provider_name(provider)
    )
    .map_err(|_| CliFailure::new("stderr failed"))?;
    Ok(ProcessOutcome::Success)
}

/// `plak key set <account>`: cooked-mode hidden read, then a stored key file.
pub fn key_set(
    account: &str,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    let paths = product_paths::PlakPaths::from_env().map_err(CliFailure::from)?;
    write!(stderr, "API key for {account} (input hidden): ")
        .map_err(|_| CliFailure::new("stderr failed"))?;
    stderr
        .flush()
        .map_err(|_| CliFailure::new("stderr failed"))?;
    let key = read_terminal_secret_line(stdin, stderr, MAX_REMOTE_AUTHORIZATION_INPUT_BYTES)
        .map_err(|_| CliFailure::new("could not read API key"))?;
    product_secrets::set_api_key(&paths, account, key.as_str()).map_err(CliFailure::from)?;
    writeln!(
        stderr,
        "stored API key for {account} (fingerprint only on status)"
    )
    .map_err(|_| CliFailure::new("stderr failed"))?;
    Ok(ProcessOutcome::Success)
}

/// `plak key clear <account>`.
pub fn key_clear(account: &str, stdout: &mut dyn io::Write) -> CliResult<ProcessOutcome> {
    let paths = product_paths::PlakPaths::from_env().map_err(CliFailure::from)?;
    let removed = product_secrets::clear_api_key(&paths, account).map_err(CliFailure::from)?;
    writeln!(
        stdout,
        "{}",
        if removed {
            "cleared"
        } else {
            "no key file present"
        }
    )
    .map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

/// `plak key status [account]`: fingerprints only, never key material.
pub fn key_status(
    account: Option<String>,
    stdout: &mut dyn io::Write,
) -> CliResult<ProcessOutcome> {
    let paths = product_paths::PlakPaths::from_env().map_err(CliFailure::from)?;
    let accounts: Vec<String> = match account {
        Some(a) => vec![a],
        None => ["openai", "xai-api", "anthropic", "openrouter"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    };
    let mut table = crate::cli_output::Table::new(["ACCOUNT", "SOURCE", "FINGERPRINT", "ENV"]);
    for a in accounts {
        let status = product_secrets::key_status(&paths, &a).map_err(CliFailure::from)?;
        let src = match status.source {
            product_secrets::KeySource::Env => "env",
            product_secrets::KeySource::File => "file",
            product_secrets::KeySource::Missing => "missing",
        };
        let mut environment = "-".to_owned();
        if let Some(name) = status.env_var {
            environment = format!("{name} (wins over file; Plak does not load .env)");
            if let Some(shadowed) = status.shadowed_file_fingerprint {
                environment.push_str(&format!(" shadowed_file={shadowed}"));
            }
        }
        table
            .push([
                status.account,
                src.to_owned(),
                status.fingerprint.unwrap_or_else(|| "-".to_owned()),
                environment,
            ])
            .map_err(CliFailure::new)?;
    }
    crate::cli_human_output()
        .write_table(stdout, &table)
        .map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(ProcessOutcome::Success)
}

/// The command asking for the compatibility answer. `plak login` is where a
/// refusal elsewhere sends the operator, so its own refusal must not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Asker {
    Login,
    Other,
}

pub fn ensure_compatibility(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<()> {
    accept_compatibility(backend, provider, Asker::Other, stdin, stderr)
}

pub(super) fn ensure_catalog_compatibility(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<()> {
    let status = backend.offline_status(provider)?;
    if !matches!(
        status.credential,
        CredentialState::Ready | CredentialState::RefreshRequired
    ) {
        require_ready(provider, status.credential)?;
    }
    accept_compatibility_status(
        backend,
        provider,
        Asker::Other,
        status.compatibility,
        stdin,
        stderr,
    )
}

fn accept_compatibility(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
    asker: Asker,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<()> {
    let status = backend.offline_status(provider)?;
    accept_compatibility_status(
        backend,
        provider,
        asker,
        status.compatibility,
        stdin,
        stderr,
    )
}

fn accept_compatibility_status(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
    asker: Asker,
    compatibility: CompatibilityState,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<()> {
    match compatibility {
        CompatibilityState::Current => Ok(()),
        CompatibilityState::Inactive => Err(profile_inactive_failure(Some(provider))),
        CompatibilityState::Unsupported => Err(CliFailure::classified(
            CliFailureKind::Auth,
            format!(
                "native compatibility state for {} is not supported by this build; build a newer plak",
                cli_provider_name(provider)
            ),
        )),
        CompatibilityState::Required => {
            let response = read_exact_compatibility_response(
                backend,
                provider,
                asker,
                io::stdin().is_terminal(),
                stdin,
                stderr,
            )?;
            backend.acknowledge_exact(provider, &response)
        }
    }
}

/// The notice shown before an account's first use. It reads one answer from
/// `stdin`, so a non-interactive caller accepts by piping `y` and a closed
/// stdin refuses with the commands that accept it instead. `echoed` says
/// whether stdin is a terminal that echoes the answer and its line break.
fn read_exact_compatibility_response(
    backend: &impl AccountOps,
    provider: NativeProvider,
    asker: Asker,
    echoed: bool,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<String> {
    let notice = backend.notice(provider);
    let account = cli_provider_name(provider);
    writeln!(
        stderr,
        "{account} uses an experimental revision-pinned compatibility integration."
    )
    .and_then(|()| writeln!(stderr, "profile: {}", notice.profile_id))
    .and_then(|()| writeln!(stderr, "reviewed: {} UTC", profile_date(notice.reviewed_at)))
    .and_then(|()| {
        writeln!(
            stderr,
            "expires: {} UTC; after that this build refuses {account} until a newer build replaces the profile",
            profile_date(notice.expires_at)
        )
    })
    .and_then(|()| write!(stderr, "Continue? [y/N] "))
    .and_then(|()| stderr.flush())
    .map_err(|_| CliFailure::new("stderr failed"))?;
    let response = read_notice_answer(account, asker, echoed, stdin, stderr)?;
    if response.eq_ignore_ascii_case("y")
        || response.eq_ignore_ascii_case("yes")
        || response == notice.notice_revision
    {
        Ok(notice.notice_revision.to_owned())
    } else {
        Err(CliFailure::classified(
            CliFailureKind::Auth,
            format!(
                "compatibility confirmation was declined; nothing was sent to {account}; run `plak login {account}` to review the notice again"
            ),
        ))
    }
}

/// One line answering the compatibility notice, without its line break.
/// `echoed` says whether stdin is a terminal that echoes the answer.
fn read_notice_answer(
    account: &str,
    asker: Asker,
    echoed: bool,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<String> {
    let mut response = String::new();
    let read = stdin.read_line(&mut response);
    // Unless the terminal echoed the answer's line break, end the prompt line
    // here or the next line lands on it. An unreadable line at a terminal was
    // still ended with Enter.
    let line_ended = match &read {
        Ok(_) => response.ends_with('\n'),
        Err(_) => true,
    };
    if !(echoed && line_ended) {
        writeln!(stderr).map_err(|_| CliFailure::new("stderr failed"))?;
    }
    let remedy = match asker {
        Asker::Login => "answer `y` from a terminal, or pipe `y` on stdin".to_owned(),
        Asker::Other => {
            format!("accept it once with `plak login {account}`, or pipe `y` to this command")
        }
    };
    let refusal = match read {
        Ok(0) => format!(
            "the {account} compatibility notice needs an answer on stdin and none came; {remedy}"
        ),
        Ok(_) => {
            response.truncate(response.trim_end_matches(['\n', '\r']).len());
            return Ok(response);
        }
        Err(_) => format!(
            "the answer to the {account} compatibility notice could not be read from stdin as text; {remedy}"
        ),
    };
    Err(CliFailure::classified(CliFailureKind::Auth, refusal))
}

/// Cooked-mode remote paste loop (never puts the TTY in raw mode).
///
/// - empty line → cancel
/// - `c` / `copy` → re-copy authorize URL, prompt again
/// - anything else → callback URL / auth code
pub(crate) fn read_remote_authorization_interactive(
    input: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
    authorize_url: &str,
    maximum_bytes: usize,
    ui: &mut impl super::login_flow::LoginUi,
) -> CliResult<super::secret_input::SecretLine> {
    loop {
        let line = ui.read_secret(input, stderr, maximum_bytes)?;
        let trimmed = line.as_str();
        if trimmed.eq_ignore_ascii_case("c") || trimmed.eq_ignore_ascii_case("copy") {
            match ui.copy_url(authorize_url) {
                product_oauth_ui::CopyResult::Copied => {
                    writeln!(stderr, "plak: authorize URL re-copied to clipboard.")
                        .map_err(|_| CliFailure::new("stderr failed"))?;
                }
                product_oauth_ui::CopyResult::Unavailable => {
                    writeln!(
                        stderr,
                        "plak: clipboard unavailable — mouse-select the authorize URL line above."
                    )
                    .map_err(|_| CliFailure::new("stderr failed"))?;
                }
            }
            continue;
        }
        return Ok(line);
    }
}

pub fn print_compatibility_notice(
    provider: NativeProvider,
    output: &mut dyn io::Write,
) -> CliResult<()> {
    let notice = NativeSubscriptions::notice(provider);
    writeln!(output, "provider={}", notice.provider().as_str())
        .and_then(|()| writeln!(output, "profile_id={}", notice.profile_id()))
        .and_then(|()| writeln!(output, "source_revision={}", notice.source_revision()))
        .and_then(|()| {
            writeln!(
                output,
                "secondary_source_revision={}",
                notice.secondary_source_revision().unwrap_or("none")
            )
        })
        .and_then(|()| writeln!(output, "notice_revision={}", notice.notice_revision()))
        .and_then(|()| writeln!(output, "provenance={}", notice.provenance()))
        .and_then(|()| writeln!(output, "reviewed_at={}", notice.reviewed_at()))
        .and_then(|()| writeln!(output, "expires_at={}", notice.expires_at()))
        .and_then(|()| {
            writeln!(
                output,
                "kill_switch_revision={}",
                notice.kill_switch_revision()
            )
        })
        .and_then(|()| writeln!(output, "killed={}", notice.is_killed()))
        .map_err(|_| CliFailure::new("stdout failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(input: &[u8], asker: Asker, echoed: bool) -> (CliResult<String>, String) {
        let mut stderr = Vec::new();
        let result = read_notice_answer(
            "codex",
            asker,
            echoed,
            &mut io::Cursor::new(input),
            &mut stderr,
        );
        (result, String::from_utf8(stderr).unwrap())
    }

    #[test]
    fn a_piped_answer_ends_the_prompt_line_and_a_terminal_answer_does_not() {
        let (piped, stderr) = answer(b"y\r\n", Asker::Other, false);
        assert_eq!(piped.unwrap(), "y");
        assert_eq!(stderr, "\n");
        let (typed, stderr) = answer(b"y\n", Asker::Other, true);
        assert_eq!(typed.unwrap(), "y");
        assert_eq!(stderr, "");
        // Ctrl+D instead of Enter: the terminal echoed no line break.
        let (eof, stderr) = answer(b"y", Asker::Other, true);
        assert_eq!(eof.unwrap(), "y");
        assert_eq!(stderr, "\n");
    }

    #[test]
    fn a_closed_stdin_and_an_unreadable_one_refuse_differently() {
        let (closed, stderr) = answer(b"", Asker::Other, true);
        assert_eq!(stderr, "\n");
        let closed = closed.unwrap_err();
        assert_eq!(closed.exit_code(), 3);
        assert_eq!(
            closed.message(),
            "the codex compatibility notice needs an answer on stdin and none came; accept it once with `plak login codex`, or pipe `y` to this command"
        );

        let (unreadable, stderr) = answer(b"\xff\n", Asker::Other, false);
        assert_eq!(stderr, "\n");
        let unreadable = unreadable.unwrap_err();
        assert_eq!(unreadable.exit_code(), 3);
        assert_eq!(
            unreadable.message(),
            "the answer to the codex compatibility notice could not be read from stdin as text; accept it once with `plak login codex`, or pipe `y` to this command"
        );

        let (login, _) = answer(b"\xff\n", Asker::Login, false);
        let login = login.unwrap_err();
        assert!(
            !login.message().contains("plak login"),
            "{}",
            login.message()
        );
    }
}
