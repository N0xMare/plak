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
    }))
}

#[test]
fn pending_authority_is_connection_owned_cancelled_on_disconnect_and_not_recovered() {
    let mut logins = Logins::default();
    let (cancel, receiver) = watch::channel(false);
    let (input, pasted) = oneshot::channel();
    let id = logins
        .admit(owner(), "claude/work".into(), cancel, input, progress())
        .expect("admitted");
    assert!(!*receiver.borrow());
    let (cancel, _) = watch::channel(false);
    let (input, _) = oneshot::channel();
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
                progress()
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
    assert!(pasted.blocking_recv().is_err());
    assert!(
        Logins::default().attempts.get(&id).is_none(),
        "restart has no pending authority"
    );
}

#[test]
fn login_start_limits_aggregate_principals_and_stop_at_three_accounts_host_wide() {
    let mut logins = Logins::default();
    for index in 0..3 {
        let (cancel, _) = watch::channel(false);
        let (input, _) = oneshot::channel();
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
                    progress()
                )
                .is_ok()
        );
    }
    let (cancel, _) = watch::channel(false);
    let (input, _) = oneshot::channel();
    assert!(matches!(
        logins.admit(owner(), "codex/work".into(), cancel, input, progress()),
        Err(RequestRefusal::RateLimited { .. })
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
        },
    );
    let (cancel, receiver) = watch::channel(false);
    let (input, _) = oneshot::channel();
    let id = console
        .logins
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .admit(owner(), "claude/work".into(), cancel, input, progress())
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
