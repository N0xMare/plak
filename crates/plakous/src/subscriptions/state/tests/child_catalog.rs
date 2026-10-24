use super::*;
use plakous_model::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
const MODEL: &str = "gpt-5.6-luna";

struct CatalogFixture {
    client: plakous_http::Client,
    origin: plakous_openai::EndpointOrigin,
    gets: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<TestResult>,
}

async fn catalog_server(statuses: Vec<u16>) -> TestResult<CatalogFixture> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()])?;
    let certificate = cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        signing_key.serialize_der(),
    ));
    let server = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key)?;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.clone())?;
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let identity = plakous_http::TlsConfigIdentity::from_parts(1, &[certificate.as_ref()])?;
    let client = plakous_http::Client::with_tls_config_and_identity(
        plakous_http::Limits::default(),
        tls,
        identity,
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin =
        plakous_openai::EndpointOrigin::parse(&format!("https://{}/", listener.local_addr()?))?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let gets = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&gets);
    let task = tokio::spawn(async move {
        for status in statuses {
            let (stream, _) = listener.accept().await?;
            let mut stream = acceptor.accept(stream).await?;
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(stream.read_u8().await?);
            }
            assert!(head.starts_with(b"GET /backend-api/codex/models?client_version="));
            count.fetch_add(1, Ordering::SeqCst);
            // The provider's existing catalog fixture, with one exact direct row.
            let body = serde_json::to_vec(&serde_json::json!({"models": [{
                "slug": MODEL, "visibility": "list", "priority": 1,
                "supported_in_api": true, "tool_mode": "direct",
                "supported_reasoning_levels": [{"effort": "low"}],
                "shell_type": "shell_command", "input_modalities": ["text"],
                "context_window": 272_000, "max_context_window": 1_000_000,
                "effective_context_window_percent": 95, "auto_compact_token_limit": 240_000
            }]}))?;
            stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await?;
            stream.write_all(&body).await?;
            stream.shutdown().await?;
        }
        Ok(())
    });
    Ok(CatalogFixture {
        client,
        origin,
        gets,
        server: task,
    })
}

fn seed(paths: &SubscriptionPaths, now: u64) -> TestResult {
    paths.prepare()?;
    store_acceptance(
        paths,
        NativeSubscriptions::notice(NativeProvider::Codex),
        now,
    )?;
    let profile = plakous_auth::SubscriptionIntegrationProfileId::new(
        plakous_openai::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
    )?;
    let store = plakous_auth::PrivateCredentialStore::open_slot(
        paths.provider_record(NativeProvider::Codex),
        profile.clone(),
        plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)?,
    )?;
    let payload = serde_json::to_vec(&serde_json::json!({
        "access_token": "opaque-seeded-access", "refresh_token": "opaque-seeded-refresh",
        "id_token": "opaque-seeded-id", "subject": "fixture-subject", "account_id": "fixture-account",
        "scopes": ["openid", "profile", "email", "offline_access", "api.connectors.read", "api.connectors.invoke"],
        "access_expires_at": now + 3600,
        "selection_epoch": "seeded-login-selection-epoch-0123456789abcd"
    }))?;
    store.initialize_selected(&plakous_auth::CredentialRecord::new(
        profile,
        plakous_auth::AccountBinding::new("fixture-account")?,
        plakous_auth::CredentialGeneration::new(1)?,
        plakous_auth::CredentialStatus::Ready,
        plakous_auth::SecretBytes::new(payload),
    )?)?;
    let consent = load_acceptance_exact(
        paths,
        NativeSubscriptions::notice(NativeProvider::Codex),
        now,
    )?;
    let operation = arm_login_recovery(paths, NativeProvider::Codex, &consent.generation)?;
    clear_login_recovery(paths, NativeProvider::Codex, &operation, 1)?;
    Ok(())
}

fn selection() -> TestResult<SelectedProviderConfiguration> {
    Ok(SelectedProviderConfiguration::new(
        native_provider_route_selection(NativeProvider::Codex, ModelId::new(MODEL)?)?,
        ProviderBehaviorConfiguration {
            processing_tier: ProcessingTierDisposition::Standard,
            tool_mode: ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Enabled(ReasoningEffort::Low),
            reasoning_summary: ReasoningSummaryDisposition::Auto,
            reasoning_include: ReasoningIncludeDisposition::EncryptedContent,
            replay: ReplayDisposition::StrictEncrypted,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::NoRequestBound,
            storage: ProviderStorageDisposition::DisabledOnWire,
            retention: RetentionDisposition::RouteContract {
                profile: ModelAdapterProfileId::new(
                    plakous_openai::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
                )?,
                revision: ProviderProfileRevisionId::new(
                    plakous_openai::OPENAI_RESPONSES_ADAPTER_REVISION,
                )?,
            },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::Ephemeral,
        },
    )?)
}

#[tokio::test]
async fn spawned_child_resolves_through_the_real_codex_catalog_and_releases_backoff_authority()
-> TestResult {
    for statuses in [vec![503, 200], vec![200], vec![401]] {
        let CatalogFixture {
            client,
            origin,
            gets,
            server,
        } = catalog_server(statuses.clone()).await?;
        let root = tempfile::tempdir()?;
        let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
        seed(&paths, unix_now()?)?;
        let mut subscriptions = NativeSubscriptions::new(paths);
        subscriptions.codex_test_transport = Some((client, origin));
        let unlocked = subscriptions.clone();
        let waits = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&waits);
        subscriptions.child_backoff = Some(Arc::new(move || {
            let unlocked = unlocked.clone();
            let observed = Arc::clone(&observed);
            Box::pin(async move {
                assert!(
                    unlocked
                        .try_acquire_compatibility_lock(NativeProvider::Codex)
                        .await
                        .expect("read exact compatibility lock")
                        .is_some()
                );
                observed.fetch_add(1, Ordering::SeqCst);
            })
        }));
        let limits =
            SubscriptionRequestLimits::new(8, 3, 4, 1_000_000, Some(Duration::from_secs(300)), 0)?;
        let result = subscriptions
            .resolve_spawned_child_provider(
                selection()?,
                limits,
                Instant::now() + Duration::from_secs(120),
            )
            .await;
        assert_eq!(
            result.is_ok(),
            statuses.last() == Some(&200),
            "{statuses:?}: {:?}",
            result.err()
        );
        tokio::time::timeout(Duration::from_secs(5), server).await???;
        assert_eq!(gets.load(Ordering::SeqCst), statuses.len());
        assert_eq!(
            waits.load(Ordering::SeqCst),
            usize::from(statuses.len() == 2)
        );
    }
    Ok(())
}
