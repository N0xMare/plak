#![allow(clippy::unwrap_used, reason = "offline subscription store fixture")]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use plakous::model::{FixedClock, Timestamp};
use plakous::subscriptions::{NativeProvider, NativeSubscriptions, SubscriptionPaths};

pub(crate) const CODEX_EPOCH: &str = "seeded-login-selection-epoch-0123456789abcd";
pub(crate) const GROK_EPOCH: &str = "grok-login-selection-epoch-0123456789abcdef";

pub(crate) fn seed(state: &Path, provider: NativeProvider, name: Option<&str>, expires_at: u64) {
    let paths = SubscriptionPaths::from_xdg_state_home(state).unwrap();
    let notice = NativeSubscriptions::notice(provider).unwrap();
    let clock = Arc::new(FixedClock::new(
        Timestamp::new(notice.reviewed_at() + 60, 0).unwrap(),
    ));
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let default = NativeSubscriptions::new(paths).with_clock(clock);
    let subscriptions = name.map_or(default.clone(), |name| {
        let named = default.for_named_account(provider, name).unwrap();
        executor
            .block_on(named.create_account_for_login(provider))
            .unwrap()
            .unwrap()
    });
    executor
        .block_on(subscriptions.acknowledge_exact(provider, notice.notice_revision()))
        .unwrap()
        .unwrap();

    let (account, payload) = payload(provider, expires_at);
    let profile =
        plakous::auth::SubscriptionIntegrationProfileId::new(notice.profile_id()).unwrap();
    let root = state.join("plak/subscriptions");
    let (root, record, slot_name) = name.map_or_else(
        || {
            let record = root.join(provider.as_str()).join("credential.json");
            (root.clone(), record, "primary".to_owned())
        },
        |name| {
            let vertical = if provider == NativeProvider::Xai {
                "grok"
            } else {
                provider.as_str()
            };
            let root = root.join("accounts").join(vertical).join(name);
            let record = root.join("credential.json");
            (root, record, format!("named-{name}"))
        },
    );
    let slot = plakous::auth::PrivateCredentialStore::open_slot(
        &record,
        profile.clone(),
        plakous::auth::CredentialSlotId::new(slot_name).unwrap(),
    )
    .unwrap();
    slot.initialize_selected(
        &plakous::auth::CredentialRecord::new(
            profile,
            plakous::auth::AccountBinding::new(account).unwrap(),
            plakous::auth::CredentialGeneration::new(1).unwrap(),
            plakous::auth::CredentialStatus::Ready,
            plakous::auth::SecretBytes::new(serde_json::to_vec(&payload).unwrap()),
        )
        .unwrap(),
    )
    .unwrap();
    clear_recovery(&root, provider);
}

fn payload(provider: NativeProvider, expires_at: u64) -> (String, serde_json::Value) {
    use serde_json::json;

    match provider {
        NativeProvider::Codex => (
            "fixture-account".to_owned(),
            json!({
                "access_token": "opaque-seeded-access",
                "refresh_token": "opaque-seeded-refresh",
                "id_token": "opaque-seeded-id",
                "subject": "fixture-subject",
                "account_id": "fixture-account",
                "scopes": ["openid", "profile", "email", "offline_access", "api.connectors.read", "api.connectors.invoke"],
                "access_expires_at": expires_at,
                "selection_epoch": CODEX_EPOCH
            }),
        ),
        NativeProvider::Xai => (
            "fixture-grok-subject".to_owned(),
            json!({
                "version": 3,
                "issuer": "https://auth.x.ai",
                "client_id": "b1a00492-073a-47ea-816f-4c329264a828",
                "subject": "fixture-grok-subject",
                "scope": "openid profile email offline_access grok-cli:access api:access",
                "access_token": "opaque-seeded-grok-access",
                "refresh_token": "opaque-seeded-grok-refresh",
                "expires_at": expires_at,
                "selection_epoch": GROK_EPOCH
            }),
        ),
        NativeProvider::Claude => {
            use plakous::model::{Digest, DigestDomain};

            let client_id = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
            let refresh = "opaque-seeded-claude-refresh";
            let grant = format!(
                "grant-{}",
                Digest::sha256(
                    DigestDomain::ProviderAuthorityBinding,
                    1,
                    &[
                        b"anthropic-claude-oauth-grant-lineage",
                        client_id.as_bytes(),
                        refresh.as_bytes(),
                    ],
                )
                .unwrap()
            );
            let payload = json!({
                "account_uuid": "12345678-1234-4234-8234-123456789abc",
                "version": 3,
                "client_id": client_id,
                "scope": "user:inference",
                "access_token": "opaque-seeded-claude-access",
                "refresh_token": refresh,
                "expires_at": expires_at,
                "grant_binding": grant,
            });
            (grant, payload)
        }
        _ => unreachable!("only native subscription routes are seeded"),
    }
}

fn clear_recovery(root: &Path, provider: NativeProvider) {
    let compatibility = root.join("compatibility");
    let receipt = std::fs::read(compatibility.join(format!("{}.json", provider.as_str()))).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&receipt).unwrap();
    let generation = parsed["generation"].as_str().unwrap();
    assert_eq!(generation.len(), 64);
    let nonce = "0123456789abcdef".repeat(4);
    let marker = format!(
        "{{\"schema\":\"plak.native-login-recovery\",\"version\":2,\"state\":\"cleared\",\"operation_nonce\":\"{nonce}\",\"consent_generation\":\"{generation}\",\"credential_generation\":1}}\n"
    );
    for name in ["login-recovery", "login-recovery-epoch"] {
        let path = compatibility.join(format!("{}.{name}", provider.as_str()));
        std::fs::write(&path, marker.as_bytes()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
