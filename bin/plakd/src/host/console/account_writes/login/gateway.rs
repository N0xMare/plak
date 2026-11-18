//! A host login whose credential the gateway holds.
//!
//! The gateway runs the provider flow. This worker only shows the challenge
//! and forwards paste, poll and cancel.

use std::time::{Duration, Instant};

use plakd_wire_types::console::{
    HostResponse, LoginMethod as HostLoginMethod, LoginPhase as HostLoginPhase, ReadRefusal,
    SecretInput, ValidationCode,
};
use plakous::gateway::protocol::admin::{
    AdminRefusal, AdminRequest, AdminResponse, LoginMethod, LoginPhase,
};
use plakous::gateway::protocol::{BoundedText, GatewayAccount, SecretText, UuidText};
use tokio::sync::{mpsc, watch};
use zeroize::Zeroizing;

use super::super::{AccountError, failed, validation};
use super::{Completion, LIFETIME, LoginWork, Progress};
use crate::gateway::AdminLink;

pub(super) fn run(link: &AdminLink, work: LoginWork<'_>) -> Result<(), AccountError> {
    let LoginWork {
        id,
        account,
        notice_revision,
        replace_newer,
        mut cancel,
        mut pasted,
        progress,
        reply,
    } = work;
    let mut reply = Some(reply);
    let opened = match open(link, account, notice_revision, replace_newer) {
        Ok(opened) => opened,
        Err(reason) => {
            if let Some(reply) = reply.take() {
                let _ = reply.send(Err(reason));
            }
            return Err(failed(false));
        }
    };
    let expires = Instant::now() + opened.expires.min(LIFETIME);
    let response = HostResponse::LoginChallenge {
        attempt: id.into(),
        method: opened.method,
        authorize_url: secret(&opened.authorize_url)?,
        user_code: opened
            .user_code
            .as_deref()
            .map(|code| secret(code))
            .transpose()?,
        expires_in_ms: u64::try_from(opened.expires.as_millis()).unwrap_or(0),
        warning: opened.warning,
    };
    set_phase(
        progress,
        if opened.device {
            HostLoginPhase::Exchanging
        } else {
            HostLoginPhase::AwaitingInput
        },
    );
    if reply
        .take()
        .is_none_or(|reply| reply.send(Ok(response)).is_err())
    {
        let _ = cancel_attempt(link, &opened.attempt);
        return Err(failed(false));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|_| failed(false))?;
    runtime.block_on(finish(
        link,
        &opened.attempt,
        opened.device,
        expires,
        &mut cancel,
        &mut pasted,
        progress,
    ))
}

struct Opened {
    attempt: UuidText,
    method: HostLoginMethod,
    device: bool,
    authorize_url: String,
    user_code: Option<Zeroizing<String>>,
    expires: Duration,
    warning: String,
}

fn open(
    link: &AdminLink,
    account: &str,
    notice_revision: &str,
    replace_newer: bool,
) -> Result<Opened, AccountError> {
    let account = GatewayAccount::new(account.to_owned()).map_err(|_| failed(false))?;
    let notice_revision =
        BoundedText::new(notice_revision.to_owned()).map_err(|_| failed(false))?;
    match link
        .call(&AdminRequest::LoginStart {
            account,
            notice_revision,
            replace_newer,
        })
        .map_err(|_| failed(false))?
    {
        AdminResponse::Challenge {
            attempt,
            method,
            authorize_url,
            user_code,
            expires_in_ms,
            warning,
        } => Ok(Opened {
            attempt,
            device: method == LoginMethod::DeviceCode,
            method: match method {
                LoginMethod::DeviceCode => HostLoginMethod::DeviceCode,
                LoginMethod::Paste => HostLoginMethod::Paste,
            },
            authorize_url: authorize_url.expose().to_owned(),
            user_code: user_code.map(|code| Zeroizing::new(code.expose().to_owned())),
            expires: Duration::from_millis(expires_in_ms),
            warning: challenge_warning(warning.as_ref().map(BoundedText::as_str)),
        }),
        AdminResponse::Refused { refusal } => Err(from_refusal(&refusal)),
        _ => Err(failed(false)),
    }
}

fn challenge_warning(extra: Option<&str>) -> String {
    let mut warning = "Only enter this code on the provider's page you explicitly opened. Never share it. This logs in the serving machine, not your browser's machine.".to_owned();
    if let Some(extra) = extra {
        warning.push(' ');
        warning.push_str(extra);
    }
    warning
}

async fn finish(
    link: &AdminLink,
    attempt: &UuidText,
    device: bool,
    expires: Instant,
    cancel: &mut watch::Receiver<bool>,
    pasted: &mut mpsc::Receiver<Completion>,
    progress: &std::sync::Mutex<Progress>,
) -> Result<(), AccountError> {
    if device {
        return poll_device(link, attempt, expires, cancel, progress).await;
    }
    let Some(value) = wait_for_paste(pasted, cancel, expires, progress).await? else {
        let _ = cancel_attempt(link, attempt);
        return Ok(());
    };
    set_phase(progress, HostLoginPhase::Exchanging);
    match link
        .call(&AdminRequest::LoginComplete {
            attempt: attempt.clone(),
            pasted: SecretText::new(value.into_zeroizing()).map_err(|_| failed(false))?,
        })
        .map_err(|_| failed(true))?
    {
        AdminResponse::Progress { phase, warning, .. } => {
            apply_phase(progress, phase, warning.as_ref().map(BoundedText::as_str));
            Ok(())
        }
        AdminResponse::Refused { refusal } => Err(from_refusal(&refusal)),
        _ => Err(failed(false)),
    }
}

async fn poll_device(
    link: &AdminLink,
    attempt: &UuidText,
    expires: Instant,
    cancel: &mut watch::Receiver<bool>,
    progress: &std::sync::Mutex<Progress>,
) -> Result<(), AccountError> {
    loop {
        if cancelled(cancel) || Instant::now() >= expires {
            let _ = cancel_attempt(link, attempt);
            set_phase(
                progress,
                if Instant::now() >= expires {
                    HostLoginPhase::Expired
                } else {
                    HostLoginPhase::Cancelled
                },
            );
            return Ok(());
        }
        match link
            .call(&AdminRequest::LoginPoll {
                attempt: attempt.clone(),
            })
            .map_err(|_| failed(true))?
        {
            AdminResponse::Progress { phase, warning, .. } => match phase {
                LoginPhase::Succeeded
                | LoginPhase::Failed
                | LoginPhase::Cancelled
                | LoginPhase::Expired
                | LoginPhase::OutcomeUnknown => {
                    apply_phase(progress, phase, warning.as_ref().map(BoundedText::as_str));
                    return Ok(());
                }
                _ => {}
            },
            AdminResponse::Refused {
                refusal: AdminRefusal::AttemptLimit {},
            } => {}
            AdminResponse::Refused { refusal } => return Err(from_refusal(&refusal)),
            _ => return Err(failed(false)),
        }
        tokio::select! {
            biased;
            _ = cancel.changed() => {}
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

async fn wait_for_paste(
    pasted: &mut mpsc::Receiver<Completion>,
    cancel: &mut watch::Receiver<bool>,
    expires: Instant,
    progress: &std::sync::Mutex<Progress>,
) -> Result<Option<SecretInput>, AccountError> {
    loop {
        if cancelled(cancel) {
            set_phase(progress, HostLoginPhase::Cancelled);
            return Ok(None);
        }
        let completion = tokio::select! {
            biased;
            _ = cancel.changed() => None,
            value = tokio::time::timeout(expires.saturating_duration_since(Instant::now()), pasted.recv()) => {
                value.ok().flatten()
            }
        };
        let Some(completion) = completion else {
            set_phase(
                progress,
                if Instant::now() >= expires {
                    HostLoginPhase::Expired
                } else {
                    HostLoginPhase::Cancelled
                },
            );
            return Ok(None);
        };
        if !acceptable(completion.pasted.expose()) {
            set_phase(progress, HostLoginPhase::AwaitingInput);
            let _ = completion.reply.send(Err(
                validation("pasted", ValidationCode::InvalidValue).into()
            ));
            continue;
        }
        if completion.reply.send(Ok(())).is_err() {
            return Ok(None);
        }
        return Ok(Some(completion.pasted));
    }
}

fn acceptable(input: &str) -> bool {
    !input.is_empty()
        && input.len() <= crate::presentation::MAX_REMOTE_AUTHORIZATION_INPUT_BYTES
        && !input.chars().any(char::is_control)
}

fn cancelled(cancel: &watch::Receiver<bool>) -> bool {
    *cancel.borrow()
}

fn cancel_attempt(link: &AdminLink, attempt: &UuidText) -> Result<(), AccountError> {
    match link
        .call(&AdminRequest::LoginCancel {
            attempt: attempt.clone(),
        })
        .map_err(|_| failed(false))?
    {
        AdminResponse::Progress { .. } | AdminResponse::Refused { .. } => Ok(()),
        _ => Err(failed(false)),
    }
}

fn apply_phase(progress: &std::sync::Mutex<Progress>, phase: LoginPhase, warning: Option<&str>) {
    use std::sync::PoisonError;
    let mut progress = progress.lock().unwrap_or_else(PoisonError::into_inner);
    progress.phase = match phase {
        LoginPhase::Starting => HostLoginPhase::Starting,
        LoginPhase::AwaitingInput => HostLoginPhase::AwaitingInput,
        LoginPhase::Exchanging => HostLoginPhase::Exchanging,
        LoginPhase::Cancelling => HostLoginPhase::Cancelling,
        LoginPhase::Succeeded => HostLoginPhase::Succeeded,
        LoginPhase::Failed => HostLoginPhase::Failed,
        LoginPhase::OutcomeUnknown => HostLoginPhase::OutcomeUnknown,
        LoginPhase::Cancelled => HostLoginPhase::Cancelled,
        LoginPhase::Expired => HostLoginPhase::Expired,
    };
    if let Some(warning) = warning {
        progress.warning = Some(warning.to_owned());
    }
}

fn set_phase(progress: &std::sync::Mutex<Progress>, phase: HostLoginPhase) {
    use std::sync::PoisonError;
    progress
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .phase = phase;
}

fn secret(value: &str) -> Result<SecretInput, AccountError> {
    SecretInput::new(Zeroizing::new(value.to_owned())).map_err(|_| failed(false))
}

fn from_refusal(refusal: &AdminRefusal) -> AccountError {
    match refusal {
        AdminRefusal::DeviceLoginUnavailable {} => ReadRefusal::DeviceLoginUnavailable {}.into(),
        AdminRefusal::WrittenByNewerBuild {} => {
            validation("account_id", ValidationCode::InvalidValue).into()
        }
        AdminRefusal::NoticeMismatch {} => {
            validation("notice_revision", ValidationCode::InvalidValue).into()
        }
        AdminRefusal::PasteInvalid {} => validation("pasted", ValidationCode::InvalidValue).into(),
        AdminRefusal::OutcomeUnknown {} => failed(true),
        _ => failed(false),
    }
}
