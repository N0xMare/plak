//! Cutover continuity on one home: the credentials a host admits with the
//! store in-process are the ones it admits through `plakd gateway serve`, and
//! a host that names the gateway never opens its own subscription store.

use std::sync::Arc;

use plakous::api_key::{ApiKeyRequest, CredentialProviderTag};
use plakous::gateway::protocol::GatewayAccount;
use plakous::gateway::protocol::admin::{AdminRequest, AdminResponse};
use plakous::gateway::protocol::facts::{CredentialStatus, KeySource};
use plakous::subscriptions::{NativeAuthStatus, NativeProvider};
use plakous_hosted::account::AccountRef;
use plakous_hosted::platform::{
    AnthropicCredentials as _, OpenAiCredentials as _, XaiCredentials as _,
};

use super::server_config::ServerConfig;
use super::service::Gateway;
use crate::home::paths::PlakPaths;

struct Selection {
    provider: CredentialProviderTag,
    origin: &'static str,
    reference: Option<String>,
}

impl ApiKeyRequest for Selection {
    fn provider(&self) -> CredentialProviderTag {
        self.provider
    }

    fn origin(&self) -> &str {
        self.origin
    }

    fn credential_reference(&self) -> Option<&str> {
        self.reference.as_deref()
    }
}

fn selection(provider: CredentialProviderTag, origin: &'static str) -> Selection {
    Selection {
        provider,
        origin,
        reference: None,
    }
}

fn account(text: &str) -> AccountRef {
    text.parse().unwrap()
}

/// Each API-key kind's generation grant as the host's route admits it, after
/// the attempt's preparation. A grant is the binding's credential input.
#[derive(PartialEq)]
struct Grants {
    openai: plakous::openai::OpenAiKeyGeneration,
    anthropic: plakous::api_key::ApiKeyGenerationGrant,
    xai: plakous::xai::XaiPlatformKeyGeneration,
    openrouter: plakous::openrouter::OpenRouterKeyGeneration,
}

async fn grants(paths: &PlakPaths) -> Grants {
    let platform = crate::platform_product::ProductPlatformCredentials::shared(paths);
    let openrouter = crate::openrouter_product::ProductOpenRouterCredentials::shared(paths);
    let openai = platform.openai(&account("openai")).unwrap();
    openai.prepare().await.unwrap();
    let reference =
        plakous::openai::CredentialReference::application(account("openai").credential_reference())
            .unwrap();
    let anthropic = platform.anthropic(&account("anthropic")).unwrap();
    anthropic.prepare().await.unwrap();
    let xai = platform.xai(&account("xai-api")).unwrap();
    xai.prepare().await.unwrap();
    let router = openrouter.source(&account("openrouter")).unwrap();
    router.prepare().await.unwrap();
    Grants {
        openai: openai
            .generation_grant(&Selection {
                reference: Some(reference.as_str().to_owned()),
                ..selection(
                    CredentialProviderTag::OpenAiPlatform,
                    "https://api.openai.com/",
                )
            })
            .unwrap(),
        anthropic: anthropic
            .generation_grant(&selection(
                CredentialProviderTag::AnthropicPlatform,
                plakous::anthropic::ANTHROPIC_PLATFORM_ORIGIN,
            ))
            .unwrap(),
        xai: xai
            .generation_grant(&selection(
                CredentialProviderTag::XaiPlatform,
                "https://api.x.ai",
            ))
            .unwrap(),
        openrouter: router
            .generation_grant(&selection(
                CredentialProviderTag::OpenRouterPlatform,
                "https://openrouter.ai",
            ))
            .unwrap(),
    }
}

/// Runs `plakd gateway serve` over `paths` with its store at
/// `gateway_state`, exactly as the service runs, until an admin drain.
fn serve(
    paths: &PlakPaths,
    gateway_state: &std::path::Path,
) -> (
    std::path::PathBuf,
    std::thread::JoinHandle<Result<(), String>>,
) {
    let socket = paths.home().join("gw.sock");
    std::fs::create_dir_all(gateway_state).unwrap();
    let text = format!(
        "socket = \"{}\"\nstate_root = \"{}\"\n",
        socket.display(),
        gateway_state.display(),
    );
    let config = ServerConfig::parse(paths, &text).unwrap();
    let instance =
        plakous::gateway::protocol::UuidText::new(uuid::Uuid::new_v4().to_string()).unwrap();
    let app = crate::commands::application::PlakousApplication::new(
        gateway_state.to_path_buf(),
        None,
        None,
    )
    .unwrap()
    .with_account_home(Some(paths.clone()));
    let bridge = super::ops::OpsBridge::new(app, paths.clone(), instance);
    let gateway = Gateway::open_with(paths, config, Box::new(bridge)).unwrap();
    let served = std::thread::spawn(move || gateway.serve_foreground());
    while !socket.exists() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    (socket, served)
}

/// `plakd gateway drain`: the service stops accepting and exits cleanly.
fn drain(socket: &std::path::Path, served: std::thread::JoinHandle<Result<(), String>>) {
    let address = super::GatewayAddress::Unix(socket.to_path_buf());
    let response: AdminResponse =
        super::client::admin(&address, None, &AdminRequest::Drain {}).unwrap();
    assert!(
        matches!(response, AdminResponse::Draining {}),
        "{response:?}"
    );
    served.join().unwrap().unwrap();
}

/// Key status through the gateway's admin verbs names the key's source.
fn assert_key_status(socket: &std::path::Path) {
    let address = super::GatewayAddress::Unix(socket.to_path_buf());
    let response: AdminResponse = super::client::admin(
        &address,
        None,
        &AdminRequest::Status {
            accounts: vec![GatewayAccount::new("openai").unwrap()],
        },
    )
    .unwrap();
    let AdminResponse::Status { rows } = response else {
        panic!("status: {response:?}");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].facts.credential.status, CredentialStatus::Ready);
    assert_eq!(
        rows[0].facts.key.as_ref().map(|key| key.source),
        Some(KeySource::File)
    );
}

/// A gateway host's roots answer every subscription kind from the gateway
/// and never create the host's own subscription store.
fn assert_gateway_roots(
    runtime: &tokio::runtime::Runtime,
    paths: &PlakPaths,
    host_state: &std::path::Path,
) {
    std::fs::create_dir_all(host_state).unwrap();
    let records = Arc::new(crate::store::session_records::SqliteSessionStore::new(
        paths.clone(),
    ));
    let roots = crate::commands::support::product_state_roots_from(
        paths,
        host_state.to_path_buf(),
        None,
        records,
        &plakous_hosted::agent::ProcessJobs::default(),
    )
    .unwrap();
    for (name, provider) in [
        ("codex", NativeProvider::Codex),
        ("claude", NativeProvider::Claude),
        ("grok", NativeProvider::Xai),
    ] {
        let subscriptions = roots.native_subscriptions_for(&account(name)).unwrap();
        let status = runtime.block_on(subscriptions.status(provider)).unwrap();
        assert_eq!(status.credential(), NativeAuthStatus::SignedOut, "{name}");
    }
    assert!(
        !host_state.join("plak/subscriptions").exists(),
        "a gateway host must not open or create its own subscription store"
    );
}

#[test]
fn a_gateway_host_admits_the_in_process_key_generations_and_opens_no_store() {
    let directory = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(directory.path().join("home"));
    std::fs::create_dir_all(paths.home()).unwrap();
    for (name, key) in [
        ("openai", "synthetic-openai-key"),
        ("anthropic", "synthetic-anthropic-key"),
        ("xai-api", "synthetic-xai-key"),
        ("openrouter", "synthetic-openrouter-key"),
    ] {
        crate::home::secrets::set_api_key(&paths, name, zeroize::Zeroizing::new(key.to_owned()))
            .unwrap();
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    // The build before the flip: no gateway, keys read in this process.
    let before = runtime.block_on(grants(&paths));

    let (socket, served) = serve(&paths, &directory.path().join("gateway-state"));
    std::fs::write(
        paths.config_file(),
        format!("[gateway]\naddress = \"unix:{}\"\n", socket.display()),
    )
    .unwrap();

    // The same home after the flip: every key stays at the gateway.
    let after = runtime.block_on(grants(&paths));
    assert!(
        before == after,
        "a gateway host must admit the generations it admitted in-process"
    );
    let audited = std::fs::read_dir(super::server_config::audit_dir(&paths))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(
        audited.matches("\"verb\":\"facts\"").count() >= 4,
        "every key generation after the flip must come from the gateway's facts"
    );
    assert_key_status(&socket);
    assert_gateway_roots(&runtime, &paths, &directory.path().join("host-state"));

    drain(&socket, served);
}
