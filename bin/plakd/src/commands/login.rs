//! Subscription compatibility acceptance and remote login presentation.

use super::application::AccountOps;
use super::support::{profile_date, profile_inactive_failure, require_ready};
use crate::oauth_ui as product_oauth_ui;
use crate::presentation::{CompatibilityState, CredentialState};
use plakous::subscriptions::NativeSubscriptions;
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult};
use std::io::{self, IsTerminal as _};

pub fn ensure_compatibility(
    backend: &mut impl AccountOps,
    provider: &plakous_hosted::account::AccountRef,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<()> {
    accept_compatibility(backend, provider, stdin, stderr)
}

pub(super) fn ensure_catalog_compatibility(
    backend: &mut impl AccountOps,
    provider: &plakous_hosted::account::AccountRef,
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
    accept_compatibility_status(backend, provider, status.compatibility, stdin, stderr)
}

fn accept_compatibility(
    backend: &mut impl AccountOps,
    provider: &plakous_hosted::account::AccountRef,

    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<()> {
    let status = backend.offline_status(provider)?;
    accept_compatibility_status(backend, provider, status.compatibility, stdin, stderr)
}

fn accept_compatibility_status(
    backend: &mut impl AccountOps,
    provider: &plakous_hosted::account::AccountRef,

    compatibility: CompatibilityState,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<()> {
    match compatibility {
        CompatibilityState::Current => Ok(()),
        CompatibilityState::Inactive => Err(profile_inactive_failure(Some(
            super::application::native_provider(provider)?,
        ))),
        CompatibilityState::Unsupported => Err(CliFailure::classified(
            CliFailureKind::Auth,
            format!(
                "native compatibility state for {} is not supported by this build; build a newer plakd",
                provider.as_str()
            ),
        )),
        CompatibilityState::Required => {
            let response = read_exact_compatibility_response(
                backend,
                provider,
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
    provider: &plakous_hosted::account::AccountRef,

    echoed: bool,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
) -> CliResult<String> {
    let notice = backend.notice(provider)?;
    let account = provider.as_str();
    writeln!(
        stderr,
        "{account} uses an experimental revision-pinned compatibility integration."
    )
    .and_then(|()| writeln!(stderr, "profile: {}", notice.profile_id))
    .and_then(|()| writeln!(stderr, "reviewed: {} UTC", profile_date(notice.reviewed_at)))
    .and_then(|()| {
        writeln!(
            stderr,
            "expires: {}; after that this build refuses {account} until a newer build replaces the profile",
            crate::presentation::format_unix_seconds(notice.expires_at)
        )
    })
    .and_then(|()| write!(stderr, "Continue? [y/N] "))
    .and_then(|()| stderr.flush())
    .map_err(|_| CliFailure::new("stderr failed"))?;
    let response = read_notice_answer(account, echoed, stdin, stderr)?;
    if response.eq_ignore_ascii_case("y")
        || response.eq_ignore_ascii_case("yes")
        || response == notice.notice_revision
    {
        Ok(notice.notice_revision.to_owned())
    } else {
        Err(CliFailure::classified(
            CliFailureKind::Auth,
            format!(
                "compatibility confirmation was declined; nothing was sent to {account}; run `plakd login {account}` to review the notice again"
            ),
        ))
    }
}

/// One line answering the compatibility notice, without its line break.
/// `echoed` says whether stdin is a terminal that echoes the answer.
fn read_notice_answer(
    account: &str,

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
    let remedy = format!(
        "accept it once with `plakd models {account}` from a terminal, or pipe `y` to this command"
    );
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
                    writeln!(stderr, "plakd: authorize URL re-copied to clipboard.")
                        .map_err(|_| CliFailure::new("stderr failed"))?;
                }
                product_oauth_ui::CopyResult::Unavailable => {
                    writeln!(
                        stderr,
                        "plakd: clipboard unavailable — mouse-select the authorize URL line above."
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
    provider: &plakous_hosted::account::AccountRef,
    output: &mut dyn io::Write,
) -> CliResult<()> {
    let notice = NativeSubscriptions::notice(super::application::native_provider(provider)?)
        .map_err(|_| {
            let account = provider.vertical().as_str();
            plakous_hosted::capabilities::CapabilityUnavailable {
                id: account,
                kind: plakous_hosted::capabilities::CapabilityKind::Route,
                feature: account,
            }
        })?;
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

    fn answer(input: &[u8], echoed: bool) -> (CliResult<String>, String) {
        let mut stderr = Vec::new();
        let result = read_notice_answer("codex", echoed, &mut io::Cursor::new(input), &mut stderr);
        (result, String::from_utf8(stderr).unwrap())
    }

    #[test]
    fn a_piped_answer_ends_the_prompt_line_and_a_terminal_answer_does_not() {
        let (piped, stderr) = answer(b"y\r\n", false);
        assert_eq!(piped.unwrap(), "y");
        assert_eq!(stderr, "\n");
        let (typed, stderr) = answer(b"y\n", true);
        assert_eq!(typed.unwrap(), "y");
        assert_eq!(stderr, "");
        // Ctrl+D instead of Enter: the terminal echoed no line break.
        let (eof, stderr) = answer(b"y", true);
        assert_eq!(eof.unwrap(), "y");
        assert_eq!(stderr, "\n");
    }

    #[test]
    fn a_closed_stdin_and_an_unreadable_one_refuse_differently() {
        let (closed, stderr) = answer(b"", true);
        assert_eq!(stderr, "\n");
        let closed = closed.unwrap_err();
        assert_eq!(closed.exit_code(), 3);
        assert_eq!(
            closed.message(),
            "the codex compatibility notice needs an answer on stdin and none came; accept it once with `plakd models codex` from a terminal, or pipe `y` to this command"
        );

        let (unreadable, stderr) = answer(b"\xff\n", false);
        assert_eq!(stderr, "\n");
        let unreadable = unreadable.unwrap_err();
        assert_eq!(unreadable.exit_code(), 3);
        assert_eq!(
            unreadable.message(),
            "the answer to the codex compatibility notice could not be read from stdin as text; accept it once with `plakd models codex` from a terminal, or pipe `y` to this command"
        );
    }
}
