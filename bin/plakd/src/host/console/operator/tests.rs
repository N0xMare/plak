use super::*;

fn new_console() -> std::sync::Arc<Console> {
    Console::new(
        crate::home::paths::PlakPaths::explicit(std::path::PathBuf::from("unused")),
        "unused".into(),
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
fn owner() -> HostRequestContext {
    HostRequestContext {
        principal: HostPrincipal::Socket(7),
        connection_id: 1,
        host_generation: 2,
        request_id: 3,
    }
}
fn action() -> OperatorAction {
    OperatorAction::AccountKeySet {
        account_id: "openai/work".into(),
    }
}
fn grant(console: &Console, context: &HostRequestContext) -> String {
    let Ok(HostResponse::OperatorPrepared { consent }) = console.prepare(context.clone(), action())
    else {
        panic!("prepared")
    };
    let Ok(HostResponse::OperatorConfirmed { confirmation, .. }) =
        console.confirm(context, &consent.nonce)
    else {
        panic!("confirmed")
    };
    assert!(matches!(
        console.confirm(context, &consent.nonce),
        Ok(HostResponse::Refused { .. })
    ));
    confirmation
}

#[test]
fn grants_are_one_shot_and_bound_to_connection_principal_generation_and_action() {
    let console = new_console();
    let owner = owner();
    let confirmation = grant(&console, &owner);
    for thief in [
        HostRequestContext {
            connection_id: 9,
            ..owner.clone()
        },
        HostRequestContext {
            principal: HostPrincipal::Socket(8),
            ..owner.clone()
        },
        HostRequestContext {
            host_generation: 3,
            ..owner.clone()
        },
    ] {
        assert!(console.consume(&thief, &action(), &confirmation).is_err());
    }
    assert!(
        console
            .consume(
                &owner,
                &OperatorAction::AccountKeySet {
                    account_id: "openai/other".into()
                },
                &confirmation
            )
            .is_err()
    );
    assert!(console.consume(&owner, &action(), &confirmation).is_ok());
    assert!(console.consume(&owner, &action(), &confirmation).is_err());
}

#[test]
fn expiry_restart_disconnect_and_outstanding_quota_revoke_authority() {
    let console = new_console();
    let owner = owner();
    let confirmation = grant(&console, &owner);
    console
        .operator
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .grants
        .get_mut(&confirmation)
        .expect("grant")
        .expires = Instant::now();
    assert_eq!(
        console.consume(&owner, &action(), &confirmation),
        Err(ReadRefusal::ConfirmationExpired {})
    );
    let confirmation = grant(&console, &owner);
    console.disconnect(&owner.principal, owner.connection_id, owner.host_generation);
    assert!(console.consume(&owner, &action(), &confirmation).is_err());
    assert!(
        new_console()
            .consume(&owner, &action(), &confirmation)
            .is_err()
    );
    for _ in 0..4 {
        assert!(console.prepare(owner.clone(), action()).is_ok());
    }
    assert!(matches!(
        console.prepare(
            HostRequestContext {
                connection_id: 8,
                ..owner
            },
            action()
        ),
        Err(RequestRefusal::RateLimited { .. })
    ));
}

#[test]
fn mutation_quota_is_shared_and_web_writes_are_default_off() {
    let console = new_console();
    let owner = owner();
    let admit = |credential| {
        console
            .operator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .admit_mutation(&owner.principal, credential)
    };
    assert!(admit(true).is_ok());
    assert!(admit(false).is_err());
    console.end_mutation(&owner.principal);
    for _ in 1..10 {
        assert!(admit(true).is_ok());
        console.end_mutation(&owner.principal);
    }
    assert!(admit(true).is_err());
    for _ in 0..30 {
        assert!(admit(false).is_ok());
        console.end_mutation(&owner.principal);
    }
    assert!(admit(false).is_err());
    {
        let mut state = console
            .operator
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let quota = state.quotas.get_mut(&owner.principal).expect("quota");
        for time in quota.credentials.iter_mut().take(5) {
            *time = Instant::now()
                .checked_sub(Duration::from_secs(61))
                .expect("monotonic test clock");
        }
    }
    for _ in 0..5 {
        assert!(admit(true).is_ok());
        console.end_mutation(&owner.principal);
    }
    assert!(admit(true).is_err());
    assert!(matches!(
        console.prepare(
            HostRequestContext {
                principal: HostPrincipal::Web("operator@example.test".into()),
                ..owner
            },
            action()
        ),
        Err(RequestRefusal::CapabilityDisabled { .. })
    ));
}

#[test]
fn refused_mutation_admission_preserves_grant_and_does_not_charge_quota() {
    let console = new_console();
    let owner = owner();
    let first = grant(&console, &owner);
    let permit = console
        .mutation(&owner, &action(), &first)
        .ok()
        .expect("admitted");
    let second = grant(&console, &owner);
    assert!(matches!(
        console.mutation(&owner, &action(), &second),
        Err(super::super::account_writes::MutationRefusal::Quota(
            RequestRefusal::RateLimited { .. }
        ))
    ));
    {
        let state = console
            .operator
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(state.grants.contains_key(&second));
        assert_eq!(state.quotas[&owner.principal].credentials.len(), 1);
    }
    drop(permit);
    assert!(console.mutation(&owner, &action(), &second).is_ok());
    assert!(
        console
            .mutation(&owner, &action(), "invalid-confirmation")
            .is_err()
    );
    assert_eq!(
        console
            .operator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .quotas[&owner.principal]
            .credentials
            .len(),
        2
    );
}

#[test]
fn definition_consent_refuses_reserved_template_names_before_minting_a_grant() {
    let console = new_console();
    for action in [
        OperatorAction::TemplatePut {
            id: "root".into(),
            if_match: "a".repeat(64),
        },
        OperatorAction::ProfilePut {
            id: "roster".into(),
            if_match: "a".repeat(64),
        },
    ] {
        assert!(matches!(
            console.prepare(owner(), action),
            Ok(HostResponse::Refused { .. })
        ));
    }
    assert!(
        console
            .operator
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .grants
            .is_empty()
    );
}
