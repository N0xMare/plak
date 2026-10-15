//! Terminal OAuth workflow over opaque account-operation tokens.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use plakous::subscriptions::{NativeLoginCompletion, NativeLoginMode, NativeProvider};

use super::application::{AccountOps, LoginChallenge, LoginOutcome, LoginResponse};
use super::login::read_remote_authorization_interactive;
use crate::oauth_ui;
use crate::presentation::{CredentialState, MAX_REMOTE_AUTHORIZATION_INPUT_BYTES};
use crate::{CliFailure, CliResult};

/// Machine effects belong to terminal presentation, separate from account authority.
pub(crate) trait LoginUi {
    fn copy_url(&mut self, url: &str) -> oauth_ui::CopyResult;
    fn read_secret(
        &mut self,
        input: &mut dyn io::BufRead,
        stderr: &mut dyn io::Write,
        maximum: usize,
    ) -> CliResult<super::secret_input::SecretLine>;
    fn ssh_session_detected(&self) -> bool;
    fn present_url(
        &mut self,
        out: &mut dyn io::Write,
        url: &str,
        account: &str,
        paste: bool,
    ) -> io::Result<()>;
    fn open_browser(&mut self, url: &str) -> bool;
    fn wait_reminder(&mut self, account: String, waiting: Arc<AtomicBool>);
}

pub(crate) struct SystemLoginUi;

impl LoginUi for SystemLoginUi {
    fn copy_url(&mut self, url: &str) -> oauth_ui::CopyResult {
        oauth_ui::try_copy_to_clipboard(url)
    }
    fn read_secret(
        &mut self,
        input: &mut dyn io::BufRead,
        stderr: &mut dyn io::Write,
        maximum: usize,
    ) -> CliResult<super::secret_input::SecretLine> {
        super::secret_input::read_system(input, stderr, maximum)
    }
    fn ssh_session_detected(&self) -> bool {
        oauth_ui::ssh_session_detected()
    }

    fn present_url(
        &mut self,
        out: &mut dyn io::Write,
        url: &str,
        account: &str,
        paste: bool,
    ) -> io::Result<()> {
        oauth_ui::present_authorize_url(out, url, account, paste)
    }

    fn open_browser(&mut self, url: &str) -> bool {
        crate::terminal_application::request_browser_open(url)
    }

    fn wait_reminder(&mut self, account: String, waiting: Arc<AtomicBool>) {
        oauth_ui::spawn_soft_wait_reminder(account, waiting);
    }
}

pub(crate) fn perform_login(
    accounts: &mut impl AccountOps,
    provider: NativeProvider,
    mode: NativeLoginMode,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn io::Write,
    ui: &mut impl LoginUi,
) -> CliResult<CredentialState> {
    let account = oauth_ui::account_cli_name(provider);
    if mode == NativeLoginMode::Browser && ui.ssh_session_detected() {
        return Err(CliFailure::new(oauth_ui::ssh_local_login_refusal_message(
            provider,
        )));
    }
    let challenge = accounts.begin_login(provider, mode)?;
    if challenge.completion == NativeLoginCompletion::DeviceCode {
        return finish_device_login(accounts, challenge, account, stderr);
    }
    let completion = challenge.completion;
    let authorize_url = challenge.authorize_url;
    let paste_complete = completion != NativeLoginCompletion::Automatic;
    ui.present_url(stderr, &authorize_url, account, paste_complete)
        .map_err(|_| CliFailure::new("stderr failed"))?;

    if mode == NativeLoginMode::Browser {
        let browser_opened = ui.open_browser(&authorize_url);
        if browser_opened {
            writeln!(stderr, "Browser launch requested on this machine.")
                .map_err(|_| CliFailure::new("stderr failed"))?;
        } else {
            writeln!(stderr, "Could not open a local browser (common on SSH / headless hosts).")
                .and_then(|()| writeln!(stderr,
                    "If the browser is not on this machine, Ctrl-C and run:  plak login {account} --remote"))
                .map_err(|_| CliFailure::new("stderr failed"))?;
        }
        if !paste_complete {
            oauth_ui::write_local_wait_progress(
                stderr,
                account,
                browser_opened,
                &oauth_ui::loopback_wait_target(&authorize_url),
            )
            .map_err(|_| CliFailure::new("stderr failed"))?;
        }
    }

    let completed = if paste_complete {
        writeln!(stderr).map_err(|_| CliFailure::new("stderr failed"))?;
        oauth_ui::write_authorization_paste_instructions(stderr, provider, completion)
            .map_err(|_| CliFailure::new("stderr failed"))?;
        let input = read_remote_authorization_interactive(
            stdin,
            stderr,
            &authorize_url,
            MAX_REMOTE_AUTHORIZATION_INPUT_BYTES,
            ui,
        )?;
        writeln!(stderr, "Completing provider authorization...")
            .and_then(|()| stderr.flush())
            .map_err(|_| CliFailure::new("stderr failed"))?;
        accounts.finish_login(challenge.pending, LoginResponse::Pasted(input.as_str()))
    } else {
        let waiting = Arc::new(AtomicBool::new(true));
        ui.wait_reminder(account.to_owned(), waiting.clone());
        let result = accounts.finish_login(
            challenge.pending,
            LoginResponse::Automatic {
                timeout: oauth_ui::LOCAL_HARD_TIMEOUT,
            },
        );
        waiting.store(false, Ordering::Relaxed);
        result
    };
    let outcome = completed.map_err(|base| {
        let message = base.message();
        if mode == NativeLoginMode::Browser
            && ["callback", "login", "expired", "browser"]
                .iter()
                .any(|word| message.contains(word))
        {
            CliFailure::new(format!(
                "{message}\n\n{}",
                oauth_ui::remote_login_hint(provider)
            ))
        } else {
            base
        }
    })?;
    match outcome {
        LoginOutcome::Completed(state) => Ok(state),
        LoginOutcome::Cancelled => Err(CliFailure::new("login cancelled")),
        LoginOutcome::TimedOut => Err(CliFailure::new(oauth_ui::local_login_timeout_message(
            provider,
            oauth_ui::LOCAL_HARD_TIMEOUT,
        ))),
    }
}

/// Grace past the device-code lifetime so the provider's own expiry, not the
/// local wait, normally ends an unapproved login.
const DEVICE_WAIT_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Shows the device code and waits for approval elsewhere; stdin is never read.
fn finish_device_login<A: AccountOps>(
    accounts: &mut A,
    challenge: LoginChallenge<A::PendingLogin>,
    account: &str,
    stderr: &mut dyn io::Write,
) -> CliResult<CredentialState> {
    let Some(device_code) = challenge.device_code else {
        return Err(CliFailure::new("device-code login returned no code"));
    };
    oauth_ui::write_device_code_prompt(
        stderr,
        account,
        &challenge.authorize_url,
        &device_code.user_code,
        device_code.expires_in,
    )
    .map_err(|_| CliFailure::new("stderr failed"))?;
    let outcome = accounts.finish_login(
        challenge.pending,
        LoginResponse::Automatic {
            timeout: device_code.expires_in + DEVICE_WAIT_GRACE,
        },
    )?;
    match outcome {
        LoginOutcome::Completed(state) => Ok(state),
        LoginOutcome::Cancelled => Err(CliFailure::new("login cancelled")),
        LoginOutcome::TimedOut => Err(CliFailure::new(oauth_ui::device_code_expired_message(
            account,
        ))),
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestLoginUi {
    pub(crate) ssh: bool,
    pub(crate) copied_urls: Vec<String>,
    pub(crate) clipboard_available: bool,
    pub(crate) browser_urls: Vec<String>,
    pub(crate) presented_urls: Vec<String>,
    pub(crate) waiting: Option<Arc<AtomicBool>>,
}

#[cfg(test)]
impl LoginUi for TestLoginUi {
    fn copy_url(&mut self, url: &str) -> oauth_ui::CopyResult {
        self.copied_urls.push(url.to_owned());
        if self.clipboard_available {
            oauth_ui::CopyResult::Copied
        } else {
            oauth_ui::CopyResult::Unavailable
        }
    }
    fn read_secret(
        &mut self,
        input: &mut dyn io::BufRead,
        stderr: &mut dyn io::Write,
        maximum: usize,
    ) -> CliResult<super::secret_input::SecretLine> {
        super::secret_input::read_plain(input, stderr, maximum)
    }
    fn ssh_session_detected(&self) -> bool {
        self.ssh
    }
    fn present_url(
        &mut self,
        out: &mut dyn io::Write,
        url: &str,
        _account: &str,
        _paste: bool,
    ) -> io::Result<()> {
        self.presented_urls.push(url.to_owned());
        writeln!(out, "{url}")
    }
    fn open_browser(&mut self, url: &str) -> bool {
        self.browser_urls.push(url.to_owned());
        false
    }
    fn wait_reminder(&mut self, _account: String, waiting: Arc<AtomicBool>) {
        self.waiting = Some(waiting);
    }
}
