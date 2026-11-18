use super::*;
use plakous_model::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
pub(in crate::subscriptions) const MODEL: &str = "gpt-5.6-luna";

pub(in crate::subscriptions) struct CatalogFixture {
    pub(in crate::subscriptions) client: plakous_http::Client,
    pub(in crate::subscriptions) origin: plakous_openai::EndpointOrigin,
    pub(in crate::subscriptions) gets: Arc<AtomicUsize>,
    pub(in crate::subscriptions) server: tokio::task::JoinHandle<TestResult>,
}

async fn catalog_server(statuses: Vec<u16>) -> TestResult<CatalogFixture> {
    scripted_server(statuses, ReadPath::Catalog).await
}
pub(in crate::subscriptions) async fn usage_server(
    statuses: Vec<u16>,
) -> TestResult<CatalogFixture> {
    scripted_server(statuses, ReadPath::Usage).await
}
enum ReadPath {
    Catalog,
    Usage,
}

async fn scripted_server(statuses: Vec<u16>, path: ReadPath) -> TestResult<CatalogFixture> {
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
            assert!(head.starts_with(match path {
                ReadPath::Catalog => b"GET /backend-api/codex/models?client_version=".as_slice(),
                ReadPath::Usage => b"GET /backend-api/wham/usage HTTP/1.1".as_slice(),
            }));
            count.fetch_add(1, Ordering::SeqCst);
            // The provider's existing catalog fixture, with one exact direct row.
            let body = match path {
                ReadPath::Usage => serde_json::to_vec(
                    &serde_json::json!({"rate_limit":{"primary_window":{"used_percent":12,"limit_window_seconds":18_000,"reset_at":1_800_000_000}}}),
                )?,
                ReadPath::Catalog => serde_json::to_vec(&serde_json::json!({"models": [{
                    "slug": MODEL, "visibility": "list", "priority": 1,
                    "supported_in_api": true, "tool_mode": "direct",
                    "supported_reasoning_levels": [{"effort": "low"}],
                    "shell_type": "shell_command", "input_modalities": ["text"],
                    "context_window": 272_000, "max_context_window": 1_000_000,
                    "effective_context_window_percent": 95, "auto_compact_token_limit": 240_000
                }]}))?,
            };
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

pub(in crate::subscriptions) fn seed(paths: &SubscriptionPaths, now: u64) -> TestResult {
    paths.prepare()?;
    store_acceptance(
        paths,
        NativeSubscriptions::notice(NativeProvider::Codex).unwrap(),
        now,
    )?;
    let profile = plakous_auth::SubscriptionIntegrationProfileId::new(
        plakous_openai::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
    )?;
    let store = plakous_auth::PrivateCredentialStore::open_slot(
        paths.provider_record(NativeProvider::Codex),
        profile.clone(),
        slot_of(paths)?,
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
        NativeSubscriptions::notice(NativeProvider::Codex).unwrap(),
        now,
    )?;
    let operation = arm_login_recovery(paths, NativeProvider::Codex, &consent.generation)?;
    clear_login_recovery(paths, NativeProvider::Codex, &operation, 1)?;
    Ok(())
}

/// The paths of `provider`'s named account `name` under `paths`, with its
/// private directory created as a login creates it.
pub(in crate::subscriptions) fn named_account(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    name: &str,
) -> TestResult<SubscriptionPaths> {
    use std::os::unix::fs::PermissionsExt as _;
    paths.prepare()?;
    let named = paths.for_named_account(provider, name)?;
    let slot = named.named.as_ref().ok_or("named slot")?;
    let accounts = paths.state_root.join("plak/subscriptions/accounts");
    for directory in [
        accounts.clone(),
        accounts.join(slot.vertical),
        accounts.join(slot.vertical).join(&slot.name),
    ] {
        if !directory.exists() {
            fs::create_dir(&directory)?;
        }
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(named)
}

/// The credential slot id of `paths`: the primary slot, or a named one's.
fn slot_of(paths: &SubscriptionPaths) -> TestResult<plakous_auth::CredentialSlotId> {
    let token = paths.named.as_ref().map_or_else(
        || PRIMARY_SLOT.to_owned(),
        |slot| format!("named-{}", slot.name),
    );
    Ok(plakous_auth::CredentialSlotId::new(token)?)
}

/// The seeded Codex login's store, as another holder opens it.
pub(in crate::subscriptions) fn codex_store(
    paths: &SubscriptionPaths,
) -> TestResult<plakous_auth::PrivateCredentialStore> {
    Ok(plakous_auth::PrivateCredentialStore::open_slot(
        paths.provider_record(NativeProvider::Codex),
        plakous_auth::SubscriptionIntegrationProfileId::new(
            plakous_openai::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
        )?,
        slot_of(paths)?,
    )?)
}

/// Seeds a consented Ready SuperGrok login for `subject` at the provider's
/// test time, with its login recovery cleared.
#[cfg(feature = "grok")]
pub(in crate::subscriptions) fn seed_grok(paths: &SubscriptionPaths, subject: &str) -> TestResult {
    let provider = NativeProvider::Xai;
    let notice = NativeSubscriptions::notice(provider).unwrap();
    let now = notice.reviewed_at() + 60;
    paths.prepare()?;
    store_acceptance(paths, notice, now)?;
    let profile = plakous_auth::SubscriptionIntegrationProfileId::new(notice.profile_id())?;
    let store = plakous_auth::PrivateCredentialStore::open_slot(
        paths.provider_record(provider),
        profile.clone(),
        slot_of(paths)?,
    )?;
    let payload = serde_json::to_vec(&serde_json::json!({
        "version": 3,
        "issuer": "https://auth.x.ai",
        "client_id": "b1a00492-073a-47ea-816f-4c329264a828",
        "subject": subject,
        "scope": "openid profile email offline_access grok-cli:access api:access",
        "access_token": "opaque-seeded-grok-access",
        "refresh_token": "opaque-seeded-grok-refresh",
        "expires_at": now + 3_600,
        "selection_epoch": plakous_auth::generate_login_selection_epoch()?,
    }))?;
    store.initialize_selected(&plakous_auth::CredentialRecord::new(
        profile,
        plakous_auth::AccountBinding::new(subject)?,
        plakous_auth::CredentialGeneration::new(1)?,
        plakous_auth::CredentialStatus::Ready,
        plakous_auth::SecretBytes::new(payload),
    )?)?;
    let consent = load_acceptance_exact(paths, notice, now)?;
    let operation = arm_login_recovery(paths, provider, &consent.generation)?;
    clear_login_recovery(paths, provider, &operation, 1)?;
    Ok(())
}

/// A SuperGrok selection of `model`.
#[cfg(feature = "grok")]
pub(in crate::subscriptions) fn grok_selection(
    model: &str,
) -> TestResult<SelectedProviderConfiguration> {
    Ok(SelectedProviderConfiguration::new(
        native_provider_route_selection(NativeProvider::Xai, ModelId::new(model)?)?,
        ProviderBehaviorConfiguration {
            reasoning_summary: ReasoningSummaryDisposition::Concise,
            parallel_tools: ParallelToolDisposition::Disabled,
            output_bound: OutputBoundDisposition::Enforced(
                std::num::NonZeroU32::new(
                    plakous_xai::grok_max_output_tokens(model).ok_or("grok output ceiling")?,
                )
                .ok_or("grok output ceiling")?,
            ),
            retention: RetentionDisposition::RouteContract {
                profile: ModelAdapterProfileId::new(plakous_xai::XAI_GROK_COMPATIBILITY_PROFILE)?,
                revision: ProviderProfileRevisionId::new(
                    plakous_xai::XAI_RESPONSES_ADAPTER_REVISION,
                )?,
            },
            ..selection()?.behavior().clone()
        },
    )?)
}

/// The account UUID of the seeded Claude login.
#[cfg(feature = "claude")]
pub(in crate::subscriptions) const CLAUDE_ACCOUNT: &str = "12345678-1234-4234-8234-123456789abc";

/// Seeds a consented Ready Claude login with `access` and `refresh` at the
/// provider's test time, with its login recovery cleared.
#[cfg(feature = "claude")]
pub(in crate::subscriptions) fn seed_claude(
    paths: &SubscriptionPaths,
    access: &str,
    refresh: &str,
) -> TestResult {
    let provider = NativeProvider::Claude;
    let notice = NativeSubscriptions::notice(provider).unwrap();
    let now = notice.reviewed_at() + 60;
    paths.prepare()?;
    store_acceptance(paths, notice, now)?;
    let profile = plakous_auth::SubscriptionIntegrationProfileId::new(notice.profile_id())?;
    let store = plakous_auth::PrivateCredentialStore::open_slot(
        paths.provider_record(provider),
        profile.clone(),
        slot_of(paths)?,
    )?;
    let client_id = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
    // The grant lineage digest the provider derives from the refresh token.
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
        )?
    );
    let payload = serde_json::to_vec(&serde_json::json!({
        "account_uuid": CLAUDE_ACCOUNT,
        "version": 3,
        "client_id": client_id,
        "scope": "user:inference",
        "access_token": access,
        "refresh_token": refresh,
        "expires_at": now + 3_600,
        "grant_binding": grant,
    }))?;
    store.initialize_selected(&plakous_auth::CredentialRecord::new(
        profile,
        plakous_auth::AccountBinding::new(grant)?,
        plakous_auth::CredentialGeneration::new(1)?,
        plakous_auth::CredentialStatus::Ready,
        plakous_auth::SecretBytes::new(payload),
    )?)?;
    let consent = load_acceptance_exact(paths, notice, now)?;
    let operation = arm_login_recovery(paths, provider, &consent.generation)?;
    clear_login_recovery(paths, provider, &operation, 1)?;
    Ok(())
}

/// The stored generation of the Claude login under `paths`.
#[cfg(feature = "claude")]
pub(in crate::subscriptions) fn claude_generation(paths: &SubscriptionPaths) -> TestResult<u64> {
    Ok(claude_stored(paths)?.0)
}

/// The stored generation and status of the Claude login under `paths`.
#[cfg(feature = "claude")]
pub(in crate::subscriptions) fn claude_stored(
    paths: &SubscriptionPaths,
) -> TestResult<(u64, plakous_auth::CredentialStatus)> {
    let provider = NativeProvider::Claude;
    let store = plakous_auth::PrivateCredentialStore::open_slot(
        paths.provider_record(provider),
        plakous_auth::SubscriptionIntegrationProfileId::new(
            NativeSubscriptions::notice(provider).unwrap().profile_id(),
        )?,
        slot_of(paths)?,
    )?;
    let fence = store.load()?.ok_or("no Claude record")?;
    Ok((fence.generation().get(), fence.status()))
}

/// A tool-free Claude selection of `model`, with the product's Claude
/// behavior otherwise.
#[cfg(feature = "claude")]
pub(in crate::subscriptions) fn claude_selection(
    model: &str,
) -> TestResult<SelectedProviderConfiguration> {
    let route = native_provider_route_selection(NativeProvider::Claude, ModelId::new(model)?)?;
    let retention = RetentionDisposition::RouteContract {
        profile: route.integration_profile().clone(),
        revision: route.integration_revision().clone(),
    };
    Ok(SelectedProviderConfiguration::new(
        route,
        ProviderBehaviorConfiguration {
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::Omit,
            output_bound: OutputBoundDisposition::Enforced(
                std::num::NonZeroU32::new(plakous_anthropic::claude_max_output_tokens(model))
                    .ok_or("claude output ceiling")?,
            ),
            storage: ProviderStorageDisposition::OmittedRouteContract,
            cache: CachePolicy::Enabled {
                requirement: CapabilityRequirement::Required,
            },
            retention,
            ..selection()?.behavior().clone()
        },
    )?)
}

pub(in crate::subscriptions) fn selection() -> TestResult<SelectedProviderConfiguration> {
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
