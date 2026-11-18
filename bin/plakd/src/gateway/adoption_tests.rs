//! In-place adoption: the gateway store is the host state home's exact bytes,
//! and a session bound on that path reopens through a separate local custody.

use std::path::Path;

use super::server_config::ServerConfig;
use crate::command::GatewayCommand;
use crate::home::paths::{self, PlakPaths};

fn home() -> (tempfile::TempDir, PlakPaths) {
    PlakPaths::isolated_test_home()
}

fn write_gateway(paths: &PlakPaths, body: &str) {
    std::fs::write(ServerConfig::path(paths), body).unwrap();
}

#[test]
fn an_absent_state_root_is_the_host_state_home() {
    let (_dir, paths) = home();
    let config = ServerConfig::parse(&paths, "").unwrap();
    let host = paths::application_state_home().unwrap();
    assert_eq!(
        config.state_root.as_os_str().as_encoded_bytes(),
        host.as_os_str().as_encoded_bytes()
    );
    ServerConfig::require_host_state_root(&config.state_root).unwrap();
}

#[test]
fn a_state_root_that_differs_by_one_byte_is_refused_without_opening_it() {
    let host = paths::application_state_home().unwrap();
    let mut slash = host.clone().into_os_string();
    slash.push("/");
    let drifted = Path::new(&slash);
    let error = ServerConfig::require_host_state_root(drifted).unwrap_err();
    assert!(
        error.contains("is not the host state home") && error.contains("no canonicalization"),
        "{error}"
    );
    let other = Path::new("/plak-gateway-not-the-host-state");
    assert_ne!(other, host.as_path());
    assert!(ServerConfig::require_host_state_root(other).is_err());
    assert!(!other.exists());
}

#[test]
fn serve_and_dry_status_refuse_a_foreign_state_root_before_creating_it() {
    let (dir, paths) = home();
    let foreign = dir.path().join("not-the-host-state");
    write_gateway(&paths, &format!("state_root = \"{}\"\n", foreign.display()));
    let served = super::serve(&paths).unwrap_err();
    assert!(served.contains("is not the host state home"), "{served}");
    assert!(!foreign.exists(), "serve must not create a foreign store");
    let mut stdout = Vec::new();
    let dry = super::execute(
        &crate::shell::services::Services::for_home(paths.clone()),
        GatewayCommand::Status { dry_run: true },
        &mut stdout,
    )
    .unwrap_err();
    assert!(dry.contains("is not the host state home"), "{dry}");
    assert_eq!(stdout, Vec::<u8>::new());
    assert!(!foreign.exists());
}

#[test]
fn init_writes_no_key_directory_and_a_named_one_is_refused() {
    let (_dir, paths) = home();
    let error = ServerConfig::parse(&paths, "key_dir = \"/tmp/plak-gateway-keys\"\n").unwrap_err();
    assert!(error.contains("key_dir"), "{error}");
    let mut stdout = Vec::new();
    super::execute(
        &crate::shell::services::Services::for_home(paths.clone()),
        GatewayCommand::Init { tcp: None },
        &mut stdout,
    )
    .unwrap();
    let text = String::from_utf8(stdout).unwrap();
    assert!(!text.contains("key_dir"), "{text}");
    assert!(text.contains("state_root "));
    let file = std::fs::read_to_string(ServerConfig::path(&paths)).unwrap();
    assert!(!file.contains("key_dir"), "{file}");
    assert!(file.contains("state_root = \""));
}

#[cfg(all(feature = "codex", feature = "grok"))]
mod equal_binding {
    use plakous::gateway::custody::{CredentialCustodian as _, CustodySlot};
    use plakous::gateway::protocol::facts::CredentialStatus;
    use plakous::gateway::protocol::{GatewayAccount, facts::FactsRequest};
    use plakous::subscriptions::custody::SubscriptionCustodian;
    use plakous::subscriptions::{NativeProvider, NativeSubscriptions, SubscriptionPaths};

    use crate::home::paths::PlakPaths;

    use super::super::gateway_seed::{CODEX_EPOCH, GROK_EPOCH, seed};

    #[test]
    fn a_host_session_reopens_through_gateway_custody_with_equal_codex_and_grok_bindings() {
        assert_eq!(CODEX_EPOCH.len(), 43);
        assert_eq!(GROK_EPOCH.len(), 43);
        let temporary = PlakPaths::isolated_tempdir();
        let state = temporary.path().join("state");
        let paths = PlakPaths::explicit(temporary.path().join("home"));
        std::fs::create_dir_all(paths.home()).unwrap();
        for provider in [NativeProvider::Codex, NativeProvider::Xai] {
            let notice = NativeSubscriptions::notice(provider).unwrap();
            seed(&state, provider, None, notice.reviewed_at() + 3_600);
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (codex_host, grok_host, gateway) = runtime.block_on(async {
            let subscriptions =
                NativeSubscriptions::new(SubscriptionPaths::from_xdg_state_home(&state).unwrap());
            let codex = SubscriptionCustodian::new(subscriptions.clone(), NativeProvider::Codex);
            let grok = SubscriptionCustodian::new(subscriptions, NativeProvider::Xai);
            let codex_facts = codex
                .facts(&CustodySlot::new("codex").unwrap())
                .await
                .unwrap();
            let grok_facts = grok
                .facts(&CustodySlot::new("grok").unwrap())
                .await
                .unwrap();
            let instance =
                plakous::gateway::protocol::UuidText::new(uuid::Uuid::new_v4().to_string())
                    .unwrap();
            let custody = crate::gateway::custody::local_custody(&paths, instance, &state);
            let reply = custody
                .facts(&FactsRequest {
                    accounts: vec![
                        GatewayAccount::new("codex").unwrap(),
                        GatewayAccount::new("grok").unwrap(),
                    ],
                })
                .await
                .unwrap();
            (codex_facts, grok_facts, reply.facts)
        });

        assert_eq!(gateway.len(), 2);
        assert_eq!(
            codex_host.credential_authority.unwrap().to_hex(),
            gateway[0]
                .identity
                .as_ref()
                .unwrap()
                .credential_authority
                .as_str()
        );
        assert_eq!(gateway[0].credential.status, CredentialStatus::Ready);
        assert_eq!(
            grok_host.credential_authority.unwrap().to_hex(),
            gateway[1]
                .identity
                .as_ref()
                .unwrap()
                .credential_authority
                .as_str()
        );
        assert_eq!(gateway[1].credential.status, CredentialStatus::Ready);
        let host_authority = grok_host
            .wire_identity
            .get("grok_authority_id")
            .expect("host grok authority");
        assert_eq!(
            gateway[1].wire_identity.get("grok_authority_id"),
            Some(host_authority.as_str())
        );
        assert_ne!(
            gateway[0]
                .identity
                .as_ref()
                .unwrap()
                .credential_authority
                .as_str(),
            gateway[1]
                .identity
                .as_ref()
                .unwrap()
                .credential_authority
                .as_str()
        );
    }
}
