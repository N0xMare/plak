use super::*;

fn owner() -> HostRequestContext {
    HostRequestContext {
        principal: HostPrincipal::Socket(7),
        connection_id: 3,
        host_generation: 4,
        request_id: 5,
    }
}
fn progress() -> Arc<Mutex<Progress>> {
    Arc::new(Mutex::new(Progress {
        phase: LoginPhase::AwaitingInput,
        provenance_warning: false,
        warning: None,
    }))
}

fn console_at(root: &std::path::Path) -> Arc<Console> {
    Console::new(
        crate::home::paths::PlakPaths::explicit(root.join("home")),
        root.join("state"),
        None,
        plakd_wire_types::console::ConfiguredSwitches {
            web_accounts_read: false,
            web_config_read: false,
            web_credentials_write: false,
            web_config_write: false,
            web_files_read: false,
            web_terminal: false,
        },
    )
}

#[test]
fn pending_authority_is_connection_owned_cancelled_on_disconnect_and_not_recovered() {
    let mut logins = Logins::default();
    let (cancel, receiver) = watch::channel(false);
    let (input, mut pasted) = mpsc::channel(1);
    let id = logins
        .admit(
            owner(),
            "claude/work".into(),
            cancel,
            input,
            progress(),
            || Ok(()),
        )
        .map(|(id, ())| id)
        .ok()
        .expect("admitted");
    assert!(!*receiver.borrow());
    let (cancel, _) = watch::channel(false);
    let (input, _) = mpsc::channel(1);
    assert!(
        logins
            .admit(
                HostRequestContext {
                    connection_id: 9,
                    ..owner()
                },
                "claude/work".into(),
                cancel,
                input,
                progress(),
                || Ok(())
            )
            .is_err()
    );
    logins.disconnect(
        &owner().principal,
        owner().connection_id,
        owner().host_generation,
    );
    assert!(*receiver.borrow());
    assert!(!logins.attempts.contains_key(&id));
    assert!(pasted.blocking_recv().is_none());
    assert!(
        !Logins::default().attempts.contains_key(&id),
        "restart has no pending authority"
    );
}

#[test]
fn login_start_limits_aggregate_principals_and_stop_at_three_accounts_host_wide() {
    let mut logins = Logins::default();
    for index in 0..3 {
        let (cancel, _) = watch::channel(false);
        let (input, _) = mpsc::channel(1);
        let principal = HostPrincipal::Socket(index);
        assert!(
            logins
                .admit(
                    HostRequestContext {
                        principal,
                        ..owner()
                    },
                    format!("claude/slot-{index}"),
                    cancel,
                    input,
                    progress(),
                    || Ok(())
                )
                .is_ok()
        );
    }
    let (cancel, _) = watch::channel(false);
    let (input, _) = mpsc::channel(1);
    assert!(matches!(
        logins.admit(
            owner(),
            "codex/work".into(),
            cancel,
            input,
            progress(),
            || Ok(())
        ),
        Err(MutationRefusal::Quota(RequestRefusal::RateLimited { .. }))
    ));
}

#[tokio::test]
async fn forged_attempt_poll_and_cancel_never_reveal_another_connections_challenge() {
    let root = tempfile::tempdir().expect("home");
    let console = Console::new(
        crate::home::paths::PlakPaths::explicit(root.path().join("home")),
        root.path().join("state"),
        None,
        plakd_wire_types::console::ConfiguredSwitches {
            web_accounts_read: false,
            web_config_read: false,
            web_credentials_write: false,
            web_config_write: false,
            web_files_read: false,
            web_terminal: false,
        },
    );
    let (cancel, receiver) = watch::channel(false);
    let (input, _) = mpsc::channel(1);
    let id = console
        .logins
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .admit(
            owner(),
            "claude/work".into(),
            cancel,
            input,
            progress(),
            || Ok(()),
        )
        .map(|(id, ())| id)
        .ok()
        .expect("admitted");
    for principal in [
        HostRequestContext {
            connection_id: 10,
            ..owner()
        },
        HostRequestContext {
            host_generation: 10,
            ..owner()
        },
        HostRequestContext {
            principal: HostPrincipal::Socket(10),
            ..owner()
        },
    ] {
        let result = Arc::clone(&console)
            .login_request(
                principal,
                HostRequest::AccountLoginPoll {
                    attempt: id.clone(),
                },
            )
            .await;
        assert!(matches!(result, Ok(HostResponse::Refused { .. })));
    }
    assert!(!*receiver.borrow());
    let result = Arc::clone(&console)
        .login_request(owner(), HostRequest::AccountLoginCancel { attempt: id })
        .await;
    assert!(matches!(
        result,
        Ok(HostResponse::LoginProgress {
            phase: LoginPhase::Cancelling,
            ..
        })
    ));
    assert!(*receiver.borrow());
}

fn grant(console: &Console, action: OperatorAction) -> String {
    let Ok(HostResponse::OperatorPrepared { consent }) = console.prepare(owner(), action) else {
        panic!("prepare")
    };
    let Ok(HostResponse::OperatorConfirmed { confirmation, .. }) =
        console.confirm(&owner(), &consent.nonce)
    else {
        panic!("confirm")
    };
    confirmation
}

fn login_grant(console: &Console, account: &str) -> String {
    grant(
        console,
        OperatorAction::AccountLoginStart {
            account_id: account.into(),
            notice_revision: "fixture".into(),
            replace_newer: false,
        },
    )
}

fn fake_login(
    stopped: oneshot::Sender<()>,
) -> impl FnOnce(&Console, LoginWork<'_>) -> Result<(), AccountError> + Send {
    move |_, work| {
        work.progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .phase = LoginPhase::AwaitingInput;
        let _ = work.reply.send(Ok(HostResponse::LoginChallenge {
            attempt: work.id.into(),
            method: LoginMethod::Paste,
            authorize_url: SecretInput::new(Zeroizing::new("https://example.invalid/login".into()))
                .expect("fixture"),
            user_code: None,
            expires_in_ms: 60_000,
            warning: "offline application fixture".into(),
        }));
        let mut pasted = work.pasted;
        let _ = pasted.blocking_recv();
        work.progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .phase = LoginPhase::Cancelled;
        let _ = stopped.send(());
        Ok(())
    }
}

#[tokio::test]
async fn start_login_allows_two_pending_attempts_and_a_key_write_without_burning_refused_admission()
{
    let root = tempfile::tempdir().expect("home");
    let console = console_at(root.path());
    let paths = &console.paths;
    let mut attempts = Vec::new();
    for name in ["first", "second"] {
        let account = format!("claude/{name}");
        let token = login_grant(&console, &account);
        let (stop, stopped) = oneshot::channel();
        let response = Arc::clone(&console)
            .start_login(
                owner(),
                account,
                "fixture".into(),
                false,
                token,
                fake_login(stop),
            )
            .await;
        let Ok(HostResponse::LoginChallenge { attempt, .. }) = response else {
            panic!("pending login")
        };
        attempts.push((attempt, stopped));
    }
    let account = "openai/work";
    let token = grant(
        &console,
        OperatorAction::AccountKeySet {
            account_id: account.into(),
        },
    );
    let result = Arc::clone(&console)
        .account_write(
            owner(),
            HostRequest::AccountKeySet {
                account_id: account.into(),
                secret: SecretInput::new(Zeroizing::new("synthetic-key".into())).expect("fixture"),
                confirmation: token,
            },
        )
        .await;
    assert!(matches!(result, Ok(HostResponse::AccountChanged { .. })));
    assert!(
        crate::home::secrets::resolve_api_key(paths, account)
            .expect("read")
            .is_some()
    );
    let token = login_grant(&console, "claude/third");
    let (stop, _) = oneshot::channel();
    assert!(matches!(
        Arc::clone(&console)
            .start_login(
                owner(),
                "claude/third".into(),
                "fixture".into(),
                false,
                token.clone(),
                fake_login(stop)
            )
            .await,
        Err(RequestRefusal::RateLimited { .. })
    ));
    let (first, stopped) = attempts.remove(0);
    Arc::clone(&console)
        .login_request(owner(), HostRequest::AccountLoginCancel { attempt: first })
        .await
        .expect("cancel");
    stopped.await.expect("worker stopped");
    let (stop, stopped) = oneshot::channel();
    let Ok(HostResponse::LoginChallenge { attempt, .. }) = Arc::clone(&console)
        .start_login(
            owner(),
            "claude/third".into(),
            "fixture".into(),
            false,
            token,
            fake_login(stop),
        )
        .await
    else {
        panic!("refused admission must preserve grant and quota")
    };
    attempts.push((attempt, stopped));
    console.disconnect(
        &owner().principal,
        owner().connection_id,
        owner().host_generation,
    );
    for (_, stopped) in attempts {
        stopped.await.expect("worker stopped");
    }
}

#[tokio::test]
async fn refused_paste_keeps_pending_authority_and_accepts_a_corrected_input() {
    let root = tempfile::tempdir().expect("home");
    let console = console_at(root.path());
    let token = login_grant(&console, "claude/work");
    let (done, finished) = oneshot::channel();
    let response = Arc::clone(&console)
        .start_login(
            owner(),
            "claude/work".into(),
            "fixture".into(),
            false,
            token,
            move |console, work| {
                let app = console.application()?;
                work.progress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .phase = LoginPhase::AwaitingInput;
                let _ = work.reply.send(Ok(HostResponse::LoginChallenge {
                    attempt: work.id.into(),
                    method: LoginMethod::Paste,
                    authorize_url: SecretInput::new(Zeroizing::new(
                        "https://example.invalid/login".into(),
                    ))
                    .expect("fixture"),
                    user_code: None,
                    expires_in_ms: 60_000,
                    warning: "fixture".into(),
                }));
                let value = wait_for_paste(
                    &app,
                    work.pasted,
                    work.cancel,
                    Instant::now() + Duration::from_secs(30),
                    work.progress,
                    |input| {
                        if input == "valid-bound-callback" {
                            Ok(())
                        } else {
                            Err(validation("pasted", ValidationCode::InvalidValue).into())
                        }
                    },
                )?;
                assert_eq!(
                    value.as_ref().map(SecretInput::expose),
                    Some("valid-bound-callback")
                );
                work.progress
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .phase = LoginPhase::Succeeded;
                let _ = done.send(());
                Ok(())
            },
        )
        .await;
    let Ok(HostResponse::LoginChallenge { attempt, .. }) = response else {
        panic!("challenge")
    };
    let submit = |value: &str| HostRequest::AccountLoginComplete {
        attempt: attempt.clone(),
        pasted: SecretInput::new(Zeroizing::new(value.into())).expect("fixture"),
    };
    assert!(matches!(
        Arc::clone(&console)
            .login_request(owner(), submit("wrong-state"))
            .await,
        Ok(HostResponse::Refused {
            reason: ReadRefusal::ValidationFailed { .. }
        })
    ));
    assert!(matches!(
        Arc::clone(&console)
            .login_request(
                owner(),
                HostRequest::AccountLoginPoll {
                    attempt: attempt.clone()
                }
            )
            .await,
        Ok(HostResponse::LoginProgress {
            phase: LoginPhase::AwaitingInput,
            ..
        })
    ));
    assert!(matches!(
        Arc::clone(&console)
            .login_request(owner(), submit("valid-bound-callback"))
            .await,
        Ok(HostResponse::LoginProgress { .. })
    ));
    finished.await.expect("worker finished");
}
