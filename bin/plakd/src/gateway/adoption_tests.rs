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
    assert!(
        dry.message().contains("is not the host state home"),
        "{dry}"
    );
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
    use std::sync::Arc;

    use plakous::gateway::custody::{CredentialCustodian as _, CustodySlot};
    use plakous::gateway::protocol::facts::{
        AccountFacts, CredentialStatus, FactsReply, FactsRequest,
    };
    use plakous::gateway::protocol::{Endpoint, GatewayAccount, HexDigest, call};
    use plakous::gateway::server::{RelayServer, http1_upstream};
    use plakous::gateway::{GatewayEndpoint, InMemoryGateway, RelayClient};
    use plakous::model::{Digest, DigestDomain};
    use plakous::subscriptions::custody::SubscriptionCustodian;
    use plakous::subscriptions::{NativeProvider, NativeSubscriptions, SubscriptionPaths};
    use sha2::{Digest as _, Sha256};

    use super::super::address::GatewayAddress;
    use super::super::gateway_seed::{primary_record, seed};
    use super::super::product::ProductGateway;

    #[test]
    fn one_product_gateway_serves_both_subscription_authorities() {
        let (_temporary, paths) = super::home();
        let gateway_state = super::super::tests::state_for(&paths);
        let state = gateway_state.config.state_root.clone();
        for provider in [NativeProvider::Codex, NativeProvider::Xai] {
            let notice = NativeSubscriptions::notice(provider).unwrap();
            seed(&state, provider, None, notice.reviewed_at() + 3_600);
        }

        let body = call::encode(&FactsRequest {
            accounts: ["codex", "grok"]
                .map(|kind| GatewayAccount::new(kind).unwrap())
                .to_vec(),
        })
        .unwrap();
        let server = RelayServer::new(
            Arc::new(ProductGateway::new(&paths, Arc::clone(&gateway_state))),
            http1_upstream().unwrap(),
            gateway_state.instance.clone(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (codex_host, grok_host, gateway, legacy) = runtime.block_on(async {
            let (gateway, legacy) =
                versioned_facts(server, paths.home().join("test.sock"), body).await;
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
            (codex_facts, grok_facts, gateway.facts, legacy.facts)
        });
        assert_eq!(gateway.len(), 2);
        for (current, direct) in gateway.iter().zip([&codex_host, &grok_host]) {
            assert_eq!(current.credential.status, CredentialStatus::Ready);
            assert_eq!(
                current
                    .identity
                    .as_ref()
                    .unwrap()
                    .credential_authority
                    .as_str(),
                direct.credential_authority.unwrap().to_hex(),
            );
        }
        assert_eq!(
            gateway[1].wire_identity.get("grok_authority_id"),
            grok_host
                .wire_identity
                .get("grok_authority_id")
                .map(String::as_str),
        );
        assert_legacy(&state, &legacy);
        for (current, legacy) in gateway.iter().zip(legacy) {
            let current_id = current.identity.as_ref().unwrap();
            let legacy_id = legacy.identity.unwrap();
            assert_eq!(current_id.account_binding, legacy_id.account_binding);
            assert_ne!(
                current_id.credential_authority,
                legacy_id.credential_authority
            );
            assert_ne!(
                current.credential.selection_epoch_digest,
                legacy.credential.selection_epoch_digest
            );
        }
    }

    async fn versioned_facts(
        server: RelayServer<ProductGateway>,
        socket: std::path::PathBuf,
        body: Vec<u8>,
    ) -> (FactsReply, FactsReply) {
        use plakous::gateway::server::Peer;
        let in_process = server.clone();
        let endpoint = GatewayEndpoint::InMemory(InMemoryGateway::new(move |io| {
            let server = in_process.clone();
            tokio::spawn(async move {
                server.serve(io, Peer::InProcess).await.unwrap();
            });
        }));
        let relay = RelayClient::new(endpoint, None).unwrap();
        let control = RelayClient::call_control();
        assert_eq!(relay.version(&control).await.unwrap(), 2);
        let gateway: FactsReply = call::decode(
            &relay
                .call(Endpoint::Facts, body.clone(), &control)
                .await
                .unwrap(),
        )
        .unwrap();

        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let accepted = tokio::spawn(async move {
            let (io, _) = listener.accept().await.unwrap();
            server
                .serve(io, Peer::Unix { same_user: true })
                .await
                .unwrap();
        });
        let legacy = tokio::task::spawn_blocking(move || {
            let reply = super::super::client::exchange(
                &GatewayAddress::Unix(socket),
                None,
                "POST",
                "/v1/facts",
                &body,
            )
            .unwrap();
            assert_eq!(reply.status, 200);
            call::decode::<FactsReply>(&reply.body).unwrap()
        })
        .await
        .unwrap();
        accepted.await.unwrap();

        (gateway, legacy)
    }

    fn assert_legacy(state: &std::path::Path, legacy: &[AccountFacts]) {
        let codex_path = primary_record(state, NativeProvider::Codex);
        let codex_profile = NativeSubscriptions::notice(NativeProvider::Codex).unwrap();
        let codex_legacy = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plak-native-codex-authority",
                codex_profile.profile_id().as_bytes(),
                codex_path.as_os_str().as_encoded_bytes(),
            ],
        )
        .unwrap();
        assert_eq!(
            legacy[0].identity.as_ref().unwrap().credential_authority,
            HexDigest::from_bytes(codex_legacy.bytes())
        );
        let grok_path = primary_record(state, NativeProvider::Xai);
        let mut grok_legacy = Sha256::new();
        grok_legacy.update(grok_path.as_os_str().as_encoded_bytes());
        grok_legacy.update(b"\0xai-route-credential-authority-v2\0primary");
        let grok_legacy = format!(
            "plak-native-xai-route:{}",
            plakous::model::hex_lower(&grok_legacy.finalize()),
        );
        assert_eq!(
            legacy[1].wire_identity.get("grok_authority_id"),
            Some(grok_legacy.as_str())
        );
    }
}
