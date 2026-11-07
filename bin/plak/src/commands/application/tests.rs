#![allow(
    clippy::too_many_lines,
    reason = "Credential matrix scenarios exercise stored-state, login, refresh, and refusal behavior without live providers."
)]

use plakous::subscriptions::NativeSubscriptionError;

use plakous::subscriptions::{NativeAuthStatus, NativeProvider};

use super::{
    OfflineReadError, Revocation, StoredCredential, revocation_from, stored_credential_from,
};
use crate::commands::support::native_subscription_failure;
use crate::presentation::CredentialState;

#[test]
fn named_removal_refusals_are_typed_and_never_offer_a_default_or_remote_action() {
    use plakous_hosted::capabilities::AccountAdmissionError;
    let root = tempfile::tempdir().unwrap();
    let app = super::PlakousApplication::for_test_accounts(root.path().join("state")).unwrap();
    let account = "claude/work".parse().unwrap();
    for (error, next_step) in [
        (NativeSubscriptionError::AccountBusy, "busy"),
        (NativeSubscriptionError::AccountSignedIn, "log out first"),
    ] {
        let failure = app.account_failure(error, &account);
        assert_eq!(failure.exit_code(), 3);
        assert!(failure.message().contains("claude/work"));
        assert!(failure.message().contains(next_step));
        assert!(!failure.message().contains("plak logout claude`"));
        assert!(!failure.message().contains("plak login claude"));
        assert!(failure.remedy().is_none());
        assert!(matches!(
            std::error::Error::source(&failure)
                .unwrap()
                .downcast_ref::<AccountAdmissionError>(),
            Some(
                AccountAdmissionError::AccountBusy { .. }
                    | AccountAdmissionError::AccountSignedIn { .. }
            )
        ));
    }
}

/// Every stored-credential state (including records written by an earlier
/// build) x every account command: one verdict, a remedy that never names
/// the command that observed it, and no remedy chain that loops.
#[test]
fn stored_credential_matrix_is_consistent_and_never_loops() {
    use super::{AccountCommand as C, CredentialVerdict as V, LoginReason};
    use crate::presentation::{CliStatus, CompatibilityState as K};
    let read_of = |compatibility, credential| {
        Ok(CliStatus {
            compatibility,
            credential,
        })
    };
    let states: Vec<(&str, Result<CliStatus, OfflineReadError>)> = vec![
        (
            "signed out",
            read_of(K::Current, CredentialState::SignedOut),
        ),
        (
            "revoked tombstone",
            read_of(K::Current, CredentialState::Revoked),
        ),
        ("ready", read_of(K::Current, CredentialState::Ready)),
        (
            "ready, acceptance required",
            read_of(K::Required, CredentialState::Ready),
        ),
        (
            "needs refresh",
            read_of(K::Current, CredentialState::RefreshRequired),
        ),
        (
            "quarantined",
            read_of(K::Current, CredentialState::Quarantined),
        ),
        (
            "previous-build codex format",
            read_of(K::Current, CredentialState::LoginRequired),
        ),
        (
            "inactive profile",
            read_of(K::Inactive, CredentialState::Ready),
        ),
        (
            "unsupported platform",
            read_of(K::Current, CredentialState::Unsupported),
        ),
        (
            "previous-build grok/claude format or foreign profile, read offline",
            read_of(K::Required, CredentialState::ProfileChanged),
        ),
        (
            "previous-build grok/claude format or foreign profile",
            Err(OfflineReadError::Subscription(
                NativeSubscriptionError::CredentialProfileChanged,
            )),
        ),
        // A newer envelope, payload version (Codex `version: 2`), newer
        // or unplaceable profile id, receipt or recovery marker all read
        // as this one status offline and this one refusal online.
        (
            "newer-build record",
            read_of(K::Current, CredentialState::WrittenByNewerBuild),
        ),
        (
            "newer-build receipt",
            read_of(K::Required, CredentialState::WrittenByNewerBuild),
        ),
        (
            "newer-build record refused online",
            Err(OfflineReadError::Subscription(
                NativeSubscriptionError::CredentialWrittenByNewerBuild,
            )),
        ),
    ];
    let commands = [
        C::Status,
        C::Login,
        C::Logout,
        C::LogoutLocal,
        C::Refresh,
        C::Usage,
        C::Run,
    ];
    let names = |command| match command {
        C::Status => "plak status",
        C::Login => "plak login",
        C::Logout => "plak logout",
        C::LogoutLocal => "--local",
        C::Refresh => "plak refresh",
        C::Usage => "plak usage",
        C::Run => "plak run",
    };
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        for (label, fixture) in &states {
            let read = match fixture {
                Ok(state) => Ok(*state),
                Err(OfflineReadError::Subscription(error)) => {
                    Err(OfflineReadError::Subscription(error.clone()))
                }
                Err(OfflineReadError::Execution) => Err(OfflineReadError::Execution),
                Err(OfflineReadError::Selection(_)) => {
                    panic!("matrix fixtures use native default selections")
                }
            };
            let verdict = V::classify(read).unwrap_or_else(|_| panic!("{label}"));
            // Logout's view is the same verdict.
            let stored = verdict.stored();
            match verdict {
                V::Usable(_) | V::NeedsRefresh(_) => {
                    assert!(matches!(stored, StoredCredential::Usable(_)), "{label}");
                }
                V::SignedOut => assert_eq!(stored, StoredCredential::Absent, "{label}"),
                _ => assert_eq!(stored, StoredCredential::Unusable, "{label}"),
            }
            if label.starts_with("newer-build") {
                assert_eq!(verdict, V::WrittenByNewerBuild, "{label}");
                for after in commands {
                    let remedy = verdict
                        .remedy(
                            &super::super::support::cli_provider_name(provider)
                                .parse()
                                .unwrap(),
                            after,
                        )
                        .unwrap_or_default();
                    assert!(remedy.contains("newer plak build"), "{label}: {remedy}");
                }
            }
            if matches!(verdict, V::NeedsLogin(_)) {
                assert!(matches!(
                    verdict,
                    V::NeedsLogin(LoginReason::RetiredFormat | LoginReason::ProfileChanged)
                ));
            }
            for after in commands {
                let remedy = verdict.remedy(
                    &super::super::support::cli_provider_name(provider)
                        .parse()
                        .unwrap(),
                    after,
                );
                if let Some(remedy) = &remedy {
                    assert!(
                        !remedy.contains(names(after)),
                        "{provider:?} {label} after {after:?}: {remedy}"
                    );
                }
                // Follow the remedy assuming each step fails: the next
                // remedy never repeats the command that just failed.
                let mut previous = after;
                let mut current = remedy;
                for _ in 0..4 {
                    let Some(text) = current else { break };
                    let next = if text.contains("--local") {
                        C::LogoutLocal
                    } else if text.starts_with("run plak refresh") {
                        C::Refresh
                    } else if text.starts_with("run plak login") {
                        C::Login
                    } else {
                        break;
                    };
                    assert_ne!(next, previous, "{provider:?} {label}: {text} loops");
                    current = verdict.remedy(
                        &super::super::support::cli_provider_name(provider)
                            .parse()
                            .unwrap(),
                        next,
                    );
                    previous = next;
                }
            }
        }
    }
}

#[test]
fn logout_reads_a_foreign_or_retired_record_as_unusable_not_a_failure() {
    assert!(matches!(
        stored_credential_from(Err(OfflineReadError::Subscription(
            NativeSubscriptionError::CredentialProfileChanged
        ))),
        Ok(StoredCredential::Unusable)
    ));
    assert!(matches!(
        stored_credential_from(Err(OfflineReadError::Subscription(
            NativeSubscriptionError::UnsafeStorage
        ))),
        Err(OfflineReadError::Subscription(
            NativeSubscriptionError::UnsafeStorage
        ))
    ));
}

#[test]
fn no_remedy_names_the_logout_or_refresh_that_produced_it() {
    use NativeSubscriptionError as E;
    let state = std::path::Path::new("/nonexistent-plak-state");
    assert_eq!(
        revocation_from(Ok(NativeAuthStatus::Revoked)),
        Ok(Revocation::Revoked(CredentialState::Revoked))
    );
    for error in [
        E::UnsafeStorage,
        E::CredentialProfileChanged,
        E::CredentialWrittenByNewerBuild,
        E::UnsupportedPlatform,
        E::AcceptanceRequired,
        E::ProfileInactive,
        E::InvalidReceipt,
        E::CredentialUnavailable,
        E::Authentication,
        E::AuthenticationBinding,
        E::AuthenticationProviderRejected,
        E::AuthenticationValidation,
        E::AuthenticationAccountChanged,
        E::LoginTokenExchange,
        E::LoginTokenTransport,
        E::LoginTokenRejected,
        E::LoginTokenResponseInvalid,
        E::LoginSigningKeys,
        E::LoginAccountVerification,
        E::LoginPersistence,
        E::InvalidCallback,
        E::DeviceLoginUnavailable,
        E::DeviceCodeExpired,
        E::ModelUnavailable,
        E::CatalogCredentialRejected,
        E::CatalogAuthorizationHeaderRejected,
        E::CatalogAccountHeaderRejected,
        E::CatalogRefreshRejected,
        E::Transport(None),
        E::Internal,
    ] {
        for provider in [
            NativeProvider::Codex,
            NativeProvider::Xai,
            NativeProvider::Claude,
        ] {
            let message = native_subscription_failure(error.clone(), Some(provider), state)
                .message()
                .to_owned();
            assert!(!message.contains("plak refresh"), "{error:?}: {message}");
            if let Err(error) = revocation_from(Err(error.clone())) {
                assert!(!message.contains("plak logout"), "{error:?}: {message}");
            }
        }
    }
}
