use std::os::unix::fs::MetadataExt as _;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(0);

/// Opt-in maintainer capture. All JSON is built from typed catalog facts, never
/// the raw authenticated response, credential store or account-scoped headers.
#[tokio::test]
#[ignore = "live authenticated catalog; maintainer supplies state home and output path"]
async fn capture_codex_authenticated_catalog() -> Result<(), Box<dyn Error>> {
    let state_home =
        std::env::var_os("PLAK_CAPTURE_STATE_HOME").ok_or("PLAK_CAPTURE_STATE_HOME is required")?;
    let output =
        std::env::var_os("PLAK_CAPTURE_OUTPUT").ok_or("PLAK_CAPTURE_OUTPUT is required")?;
    let subscriptions =
        NativeSubscriptions::new(SubscriptionPaths::from_xdg_state_home(state_home)?);
    let _lock = subscriptions
        .acquire_compatibility_lock(NativeProvider::Codex)
        .await?;
    let status = subscriptions.status_unlocked(NativeProvider::Codex).await?;
    let mut captured = None;
    run_classified_model_catalog(NativeProvider::Codex, status, |gate| async {
        let online = subscriptions.codex_online_with_gate(Some(gate), true).await?;
        let resolver = plakous_openai::CodexModelResolver::new(
            online.manager,
            online.client,
            codex_native_http_config()?,
        ).map_err(map_codex_error)?;
        let catalog = resolver.refresh_catalog().await.map_err(map_codex_error)?;
        let mut models = serde_json::Map::new();
        for entry in catalog.entries() {
            let capacity = entry.capacity_facts();
            models.insert(entry.model_id().to_owned(), serde_json::json!({
                "tool_mode": entry.tool_mode(),
                "reasoning": entry.reasoning_effort_options().iter().map(|effort| {
                    format!("{effort:?}").to_ascii_lowercase()
                }).collect::<Vec<_>>(),
                "default_reasoning": entry.default_reasoning_effort().map(|effort| {
                    format!("{effort:?}").to_ascii_lowercase()
                }),
                "context_window": capacity.context_window().map(std::num::NonZeroU32::get),
                "max_context_window": capacity.max_context_window().map(std::num::NonZeroU32::get),
                "effective_context_window_percent": capacity.effective_context_window_percent().get(),
                "auto_compact_token_limit": capacity.auto_compact_token_limit().map(std::num::NonZeroU32::get),
                "use_responses_lite": entry.uses_responses_lite(),
                "service_tiers": entry.service_tiers().iter().map(|tier| serde_json::json!({
                    "id": tier.id(), "name": tier.name(), "description": tier.description(),
                })).collect::<Vec<_>>(),
            }));
        }
        captured = Some(models);
        Ok(Vec::new())
    }).await?;
    let document = serde_json::json!({
        "schema": "plak.provider-authority.authenticated-catalog-observation.v1",
        "provider": "openai",
        "vertical": "codex-oauth",
        "client_version": plakous_openai::OPENAI_CODEX_REFERENCE_CLIENT_VERSION,
        "account_material_removed": true,
        "capture_procedure": "PLAK_CAPTURE_STATE_HOME=<Plak XDG state home> PLAK_CAPTURE_OUTPUT=<fixture path> cargo test --locked -p plakous --features native-subscriptions capture_codex_authenticated_catalog -- --ignored",
        "models": captured.ok_or("catalog capture did not complete")?,
    });
    let mut bytes = serde_json::to_vec_pretty(&document)?;
    bytes.push(b'\n');
    fs::write(output, bytes)?;
    Ok(())
}
/// Opt-in paid probe, never in a gate tier: one hosted-retrieval side request
/// per (route, tool) on the operator's Codex and SuperGrok logins. A pass is a
/// non-empty answer with at least one source. Run with
/// `PLAK_LIVE_RETRIEVAL_PROBE=1 PLAK_CAPTURE_STATE_HOME=<Plak XDG state home>
/// PLAK_LIVE_CODEX_MODEL=<model> PLAK_LIVE_GROK_MODEL=<model> cargo nextest run
/// -p plakous --features native-subscriptions --run-ignored only
/// -E 'test(live_retrieval_probe)'`; a route whose model variable is unset is
/// skipped.
#[tokio::test]
#[ignore = "paid live probe"]
async fn live_retrieval_probe_subscriptions() -> Result<(), Box<dyn Error>> {
    use plakous_tools::{ExecutionOperationControl, RetrievalRequest};

    if std::env::var("PLAK_LIVE_RETRIEVAL_PROBE").as_deref() != Ok("1") {
        return Err("set PLAK_LIVE_RETRIEVAL_PROBE=1 to spend one probe per tool".into());
    }
    let state_home =
        std::env::var_os("PLAK_CAPTURE_STATE_HOME").ok_or("PLAK_CAPTURE_STATE_HOME is required")?;
    let subscriptions =
        NativeSubscriptions::new(SubscriptionPaths::from_xdg_state_home(state_home)?);
    let web_search = RetrievalRequest::WebSearch {
        query: "Rust programming language latest stable release".to_owned(),
        allowed_domains: Vec::new(),
        blocked_domains: Vec::new(),
        max_uses: 5,
    };
    let x_search = RetrievalRequest::PostSearch {
        query: "Rust programming language release".to_owned(),
        handles: vec!["rustlang".to_owned()],
        from_date: None,
        to_date: None,
    };
    for (provider, model_variable, requests) in [
        (
            NativeProvider::Codex,
            "PLAK_LIVE_CODEX_MODEL",
            vec![web_search.clone()],
        ),
        (
            NativeProvider::Xai,
            "PLAK_LIVE_GROK_MODEL",
            vec![web_search, x_search],
        ),
    ] {
        let Ok(model) = std::env::var(model_variable) else {
            eprintln!("{provider:?}: {model_variable} unset, skipped");
            continue;
        };
        let limits =
            SubscriptionRequestLimits::new(2, 2, 4, 1_000_000, Some(Duration::from_secs(300)), 0)?;
        let gate: Arc<dyn SubscriptionRequestGate> =
            Arc::new(InMemorySubscriptionRequestGate::new(
                limits,
                Arc::new(SystemSubscriptionGateClock::default()),
            ));
        let resolved = subscriptions
            .resolve_gated_native_model(
                provider,
                &NativeModelSelector::Exact(ModelId::new(model)?),
                gate,
                SessionLineage::Root,
            )
            .await
            .map_err(|error| format!("{provider:?} resolution failed: {error:?}"))?;
        let port = resolved
            .retrieval
            .clone()
            .ok_or("the route built no retrieval port")?;
        for request in requests {
            let outcome = port
                .retrieve(
                    request,
                    ExecutionOperationControl::new(plakous_model::CancellationToken::new()),
                )
                .await?;
            eprintln!(
                "{provider:?}: sources={} usage={:?}",
                outcome.sources.len(),
                outcome.usage
            );
            assert!(
                !outcome.text.trim().is_empty(),
                "the search answered nothing"
            );
            assert!(!outcome.sources.is_empty(), "the search reported no source");
        }
    }
    Ok(())
}

#[tokio::test]
async fn xai_inference_observer_is_passive_and_receives_exact_request_bytes()
-> Result<(), Box<dyn Error>> {
    struct Observer(Arc<AtomicUsize>);
    impl plakous_http::ExchangeObserver for Observer {
        fn start(&self, body: &[u8]) -> Option<Box<dyn plakous_http::ExchangeObservation>> {
            assert_eq!(body, br#"{"model":"fixture","input":[],"stream":true}"#);
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
            None
        }
    }
    let observed = Arc::new(AtomicUsize::new(0));
    let plain = xai_inference_client(None)?;
    let client = xai_inference_client(Some(Arc::new(Observer(Arc::clone(&observed)))))?;
    assert_eq!(client.behavior_identity(), plain.behavior_identity());
    for client in [plain, client] {
        let request = plakous_http::Request::new(
            plakous_http::Origin::parse("https://example.invalid")?,
            plakous_http::Method::POST,
            "/responses",
            plakous_http::HeaderMap::new(),
            plakous_http::Bytes::from_static(br#"{"model":"fixture","input":[],"stream":true}"#),
            client.limits(),
        )?;
        let cancelled = plakous_http::CancellationToken::new();
        cancelled.cancel();
        let failure = client
            .execute(
                request,
                plakous_http::ExchangeControl::with_cancellation(
                    Instant::now() + Duration::from_secs(1),
                    cancelled,
                ),
            )
            .await
            .expect_err("pre-cancelled request must never connect");
        assert_eq!(failure.kind(), plakous_http::ErrorKind::Cancelled);
        assert_eq!(
            failure.transmission(),
            plakous_http::TransmissionFact::NotStarted
        );
    }
    assert_eq!(observed.load(AtomicOrdering::SeqCst), 1);
    Ok(())
}

#[test]
fn xai_inference_phase_budget_is_the_streaming_idle_bound() {
    let client = xai_inference_client(None).expect("fixed inference transport");
    assert_eq!(
        client.transport_policy(),
        plakous_http::TransportPolicy::WebPkiHttp1Only
    );
    let limits = client.limits();
    let defaults = plakous_http::Limits::default();
    assert_eq!(
        limits.response_head_timeout,
        plakous_http::STREAMING_IDLE_TIMEOUT
    );
    assert_eq!(
        limits.response_body_timeout,
        plakous_http::STREAMING_IDLE_TIMEOUT
    );
    assert_eq!(limits.resolution_timeout, defaults.resolution_timeout);
    assert_eq!(limits.connect_timeout, Duration::from_secs(10));
    assert_eq!(limits.tls_timeout, defaults.tls_timeout);
    assert_eq!(limits.request_write_timeout, Duration::from_secs(30));
    assert_eq!(defaults.response_head_timeout, Duration::from_secs(30));
    assert_eq!(defaults.response_body_timeout, Duration::from_secs(30));
}

#[test]
fn native_inference_configs_match_product_budget_without_changing_api_defaults() {
    let codex = codex_native_http_config().expect("fixed native Codex config");
    let xai = xai_native_http_config().expect("fixed native Grok config");
    assert_eq!(
        codex.operation_timeout(),
        NATIVE_INFERENCE_OPERATION_TIMEOUT
    );
    assert_eq!(xai.operation_timeout(), NATIVE_INFERENCE_OPERATION_TIMEOUT);
    assert_eq!(
        plakous_openai::StandardOpenAiHttpConfig::default().operation_timeout(),
        Duration::from_secs(120)
    );
    assert_eq!(
        plakous_xai::StandardXaiHttpConfig::default().operation_timeout(),
        Duration::from_secs(120)
    );
    let limits = plakous_http::Limits::for_streaming_inference();
    assert_eq!(limits.response_head_timeout, Duration::from_secs(180));
    assert_eq!(limits.response_body_timeout, Duration::from_secs(180));
    assert_eq!(limits.connect_timeout, Duration::from_secs(10));
    assert_eq!(limits.request_write_timeout, Duration::from_secs(30));
}

use super::super::*;
use super::*;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

#[test]
fn client_device_identity_is_durable_and_serializes_concurrent_creators()
-> Result<(), Box<dyn Error>> {
    let root = unique_test_root("client-device-identity")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    paths.prepare()?;
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let paths = paths.clone();
            std::thread::spawn(move || paths.claude_client_device_id())
        })
        .collect();
    let ids: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap())
        .collect();
    assert!(ids.iter().all(|id| id == &ids[0]));
    assert_eq!(ids[0].as_str().len(), 64);
    assert_eq!(paths.claude_client_device_id()?, ids[0]);
    // Replacing account-scoped state has no ownership over installation metadata.
    write_private_atomic(
        &paths.provider_record(NativeProvider::Claude),
        b"synthetic-retired-slot",
    )?;
    remove_private_file_durable(&paths.provider_record(NativeProvider::Claude))?;
    assert_eq!(paths.claude_client_device_id()?, ids[0]);
    write_private_atomic(&paths.layout_root().join("client-device-id"), b"invalid")?;
    assert!(matches!(
        paths.claude_client_device_id(),
        Err(NativeSubscriptionError::UnsafeStorage)
    ));
    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn client_device_identity_rejects_symlink() -> Result<(), Box<dyn Error>> {
    let root = unique_test_root("client-device-symlink")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    paths.prepare()?;
    symlink(
        "missing-target",
        paths.layout_root().join("client-device-id"),
    )?;
    assert!(matches!(
        paths.claude_client_device_id(),
        Err(NativeSubscriptionError::UnsafeStorage)
    ));
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn xai_facade_lists_only_release_enrolled_catalog_rows() {
    let observed = [
        plakous_xai::XAI_GROK_BUILD_MODEL,
        plakous_xai::XAI_GROK_PRIMARY_CODING_MODEL,
        plakous_xai::XAI_GROK_46_CODING_MODEL,
    ];
    let listed = observed
        .into_iter()
        .filter(|model| facade_xai_model_is_selectable(model))
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        [
            plakous_xai::XAI_GROK_PRIMARY_CODING_MODEL,
            plakous_xai::XAI_GROK_46_CODING_MODEL,
        ]
    );
}

#[test]
fn exact_native_tool_mode_requirement_fails_closed() {
    for (provider, facade) in [
        (
            plakous_openai::CodexCatalogToolMode::Direct,
            NativeToolMode::Direct,
        ),
        (
            plakous_openai::CodexCatalogToolMode::CodeMode,
            NativeToolMode::CodeMode,
        ),
        (
            plakous_openai::CodexCatalogToolMode::CodeModeOnly,
            NativeToolMode::CodeModeOnly,
        ),
    ] {
        assert_eq!(facade_native_tool_mode(Some(provider)), Some(facade));
    }
    assert_eq!(facade_native_tool_mode(None), None);

    for mode in [
        NativeToolMode::Direct,
        NativeToolMode::CodeMode,
        NativeToolMode::CodeModeOnly,
    ] {
        assert_eq!(require_exact_native_tool_mode(Some(mode), mode), Ok(()));
    }
    let mismatch = require_exact_native_tool_mode(
        Some(NativeToolMode::CodeMode),
        NativeToolMode::CodeModeOnly,
    )
    .expect_err("hybrid mode must not satisfy code-mode-only");
    assert_eq!(mismatch.required(), NativeToolMode::CodeModeOnly);
    assert_eq!(mismatch.available(), Some(NativeToolMode::CodeMode));

    let missing = require_exact_native_tool_mode(None, NativeToolMode::CodeMode)
        .expect_err("missing or unknown evidence must fail closed");
    assert_eq!(missing.required(), NativeToolMode::CodeMode);
    assert_eq!(missing.available(), None);
}

#[test]
fn xai_callback_correlation_and_expiry_are_actionable_callback_errors() {
    assert_eq!(
        map_xai_error(plakous_xai::XaiNativeAuthError::InvalidCallback),
        NativeSubscriptionError::InvalidCallback
    );
    assert_eq!(
        map_xai_error(plakous_xai::XaiNativeAuthError::LoginExpired),
        NativeSubscriptionError::InvalidCallback
    );
}

#[test]
fn provider_login_stages_map_to_exact_payload_free_facade_failures() {
    for (error, expected) in [
        (
            plakous_openai::NativeCodexAuthError::LoginTokenTransport,
            NativeSubscriptionError::LoginTokenTransport,
        ),
        (
            plakous_openai::NativeCodexAuthError::LoginTokenRejected,
            NativeSubscriptionError::LoginTokenRejected,
        ),
        (
            plakous_openai::NativeCodexAuthError::LoginTokenResponseInvalid,
            NativeSubscriptionError::LoginTokenResponseInvalid,
        ),
        (
            plakous_openai::NativeCodexAuthError::LoginSigningKeys,
            NativeSubscriptionError::LoginSigningKeys,
        ),
        (
            plakous_openai::NativeCodexAuthError::LoginAccountVerification,
            NativeSubscriptionError::LoginAccountVerification,
        ),
        (
            plakous_openai::NativeCodexAuthError::LoginPersistence,
            NativeSubscriptionError::LoginPersistence,
        ),
    ] {
        assert_eq!(map_codex_error(error), expected);
    }
    for (error, expected) in [
        (
            plakous_xai::XaiNativeAuthError::LoginTokenExchange,
            NativeSubscriptionError::LoginTokenExchange,
        ),
        (
            plakous_xai::XaiNativeAuthError::LoginSigningKeys,
            NativeSubscriptionError::LoginSigningKeys,
        ),
        (
            plakous_xai::XaiNativeAuthError::LoginAccountVerification,
            NativeSubscriptionError::LoginAccountVerification,
        ),
        (
            plakous_xai::XaiNativeAuthError::LoginPersistence,
            NativeSubscriptionError::LoginPersistence,
        ),
    ] {
        assert_eq!(map_xai_error(error), expected);
    }
}

#[test]
fn codex_compatible_selection_is_backend_order_and_requires_text_tools_reasoning() {
    assert_eq!(
        first_codex_compatible_model([
            ("image-tools", false, true, true),
            ("text-only", true, false, true),
            ("no-reasoning", true, true, false),
            ("first-compatible", true, true, true),
            ("later-compatible", true, true, true),
        ]),
        Some("first-compatible")
    );
    assert_eq!(
        first_codex_compatible_model([
            ("image-tools", false, true, true),
            ("text-only", true, false, true),
            ("no-reasoning", true, true, false),
        ]),
        None
    );
}

#[cfg(unix)]
#[test]
fn persisted_catalog_roundtrip_is_offline() -> Result<(), Box<dyn Error>> {
    let root = unique_test_root("catalog-roundtrip")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    paths.prepare()?;
    let models = vec![NativeModelSummary {
        exact_id: "gpt-5.6-sol".to_owned(),
        visibility: NativeModelVisibility::List,
        native_tool_mode: Some(NativeToolMode::CodeModeOnly),
        streaming: true,
        tools: true,
        reasoning_efforts: vec![ReasoningEffort::Medium],
        default_reasoning_effort: None,
        service_tiers: Vec::new(),
        default_service_tier: None,
        reported_context_length: None,
    }];
    SubscriptionState::new(paths.clone())
        .persist_catalog_snapshot(NativeProvider::Codex, &models)?;
    let loaded =
        SubscriptionState::new(paths.clone()).load_persisted_catalog(NativeProvider::Codex)?;
    assert_eq!(loaded.models()[0].exact_id(), "gpt-5.6-sol");
    fs::remove_dir_all(root)?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn public_catalog_policy_requires_ready_and_exact_terminal_dispatch_evidence()
-> Result<(), NativeSubscriptionError> {
    let invoked = Arc::new(AtomicUsize::new(0));
    let refresh_invoked = Arc::clone(&invoked);
    let refresh_required = NativeSubscriptionStatus {
        compatibility: CompatibilityStatus::Current,
        credential: NativeAuthStatus::RefreshRequired,
    };
    assert!(matches!(
        run_classified_model_catalog(
            NativeProvider::Codex,
            refresh_required,
            move |_gate| async move {
                refresh_invoked.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(Vec::new())
            },
        )
        .await,
        Err(NativeSubscriptionError::CredentialUnavailable)
    ));
    assert_eq!(invoked.load(AtomicOrdering::SeqCst), 0);

    let quarantined = NativeSubscriptionStatus {
        compatibility: CompatibilityStatus::Current,
        credential: NativeAuthStatus::Quarantined,
    };
    assert!(matches!(
        run_classified_model_catalog(NativeProvider::Codex, quarantined, |_gate| async move {
            Ok(Vec::new())
        },)
        .await,
        Err(NativeSubscriptionError::CredentialUnavailable)
    ));
    let xai_quarantine_invoked = Arc::clone(&invoked);
    assert!(matches!(
        run_classified_model_catalog(NativeProvider::Xai, quarantined, move |_gate| async move {
            xai_quarantine_invoked.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(Vec::new())
        },)
        .await,
        Err(NativeSubscriptionError::Internal)
    ));
    assert_eq!(invoked.load(AtomicOrdering::SeqCst), 1);

    let ready = NativeSubscriptionStatus {
        compatibility: CompatibilityStatus::Current,
        credential: NativeAuthStatus::Ready,
    };
    let catalog_invoked = Arc::clone(&invoked);
    let models =
        run_classified_model_catalog(NativeProvider::Codex, ready, move |gate| async move {
            catalog_invoked.fetch_add(1, AtomicOrdering::SeqCst);
            let mut permit = gate
                .acquire(SubscriptionRequestClass::Catalog)
                .map_err(|_| NativeSubscriptionError::Internal)?;
            permit
                .authorize_dispatch()
                .map_err(|_| NativeSubscriptionError::Internal)?;
            Ok(vec![NativeModelSummary {
                exact_id: "exact-model".to_owned(),
                visibility: NativeModelVisibility::List,
                native_tool_mode: None,
                streaming: true,
                tools: true,
                reasoning_efforts: vec![ReasoningEffort::High, ReasoningEffort::Low],
                default_reasoning_effort: Some(ReasoningEffort::Low),
                service_tiers: vec![NativeServiceTierSummary {
                    id: "priority".to_owned(),
                    name: "Fast".to_owned(),
                    description: "1.5x speed, increased usage".to_owned(),
                }],
                default_service_tier: Some("priority".to_owned()),
                reported_context_length: None,
            }])
        })
        .await?;
    assert_eq!(models.len(), 1);
    assert!(models[0].supports_reasoning());
    assert_eq!(
        models[0].reasoning_efforts(),
        &[ReasoningEffort::High, ReasoningEffort::Low]
    );
    assert_eq!(
        models[0].default_reasoning_effort(),
        Some(ReasoningEffort::Low)
    );
    assert_eq!(models[0].default_service_tier(), Some("priority"));
    assert_eq!(models[0].service_tiers()[0].id(), "priority");
    assert_eq!(models[0].service_tiers()[0].name(), "Fast");
    assert_eq!(
        models[0].service_tiers()[0].description(),
        "1.5x speed, increased usage"
    );
    assert_eq!(invoked.load(AtomicOrdering::SeqCst), 2);

    assert!(matches!(
        run_classified_model_catalog(NativeProvider::Codex, ready, |_gate| async move {
            Ok(Vec::new())
        },)
        .await,
        Err(NativeSubscriptionError::Internal)
    ));
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn claude_foreign_profile_is_retired_by_logout_or_replaced_by_login()
-> Result<(), Box<dyn Error>> {
    let root = unique_test_root("claude-foreign-profile-local-retirement")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    paths.prepare()?;
    let old_profile = plakous_auth::SubscriptionIntegrationProfileId::new(
        "anthropic.claude.oauth.compat.2026-01-01.r1",
    )?;
    let old_store = plakous_auth::PrivateCredentialStore::open_slot(
        paths.provider_record(NativeProvider::Claude),
        old_profile.clone(),
        plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)?,
    )?;
    old_store.initialize_selected(&plakous_auth::CredentialRecord::new(
        old_profile,
        plakous_auth::AccountBinding::new("old-claude-grant")?,
        plakous_auth::CredentialGeneration::new(1)?,
        plakous_auth::CredentialStatus::Ready,
        plakous_auth::SecretBytes::new(b"old-opaque-credential".to_vec()),
    )?)?;
    let notice = NativeSubscriptions::notice(NativeProvider::Claude);
    let now = unix_now()?;
    store_acceptance(&paths, notice, now)?;
    let acceptance = load_acceptance_exact(&paths, notice, now)?;
    arm_login_recovery(&paths, NativeProvider::Claude, &acceptance.generation)?;
    let subscriptions = NativeSubscriptions::new(paths.clone());
    assert_eq!(
        subscriptions.status(NativeProvider::Claude).await,
        Err(NativeSubscriptionError::CredentialProfileChanged)
    );
    assert_eq!(
        subscriptions.logout_local(NativeProvider::Claude).await?,
        NativeAuthStatus::Revoked
    );
    assert!(!paths.login_recovery_path(NativeProvider::Claude).exists());
    assert!(
        !paths
            .login_recovery_epoch_path(NativeProvider::Claude)
            .exists()
    );
    assert_eq!(
        subscriptions.status(NativeProvider::Claude).await?,
        NativeSubscriptionStatus {
            compatibility: CompatibilityStatus::Current,
            credential: NativeAuthStatus::Revoked,
        }
    );
    let pending = subscriptions
        .begin_login(NativeProvider::Claude, NativeLoginMode::Remote)
        .await?;
    assert_eq!(
        pending.instructions().completion(),
        NativeLoginCompletion::AuthorizationCodeOrRedirectUrl
    );
    drop(pending);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn xai_retired_payload_reads_as_profile_changed_and_local_logout_repairs_it()
-> Result<(), Box<dyn Error>> {
    let provider = NativeProvider::Xai;
    let root = unique_test_root("xai-retired-payload-local-retirement")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    paths.prepare()?;
    let notice = NativeSubscriptions::notice(provider);
    let now = unix_now()?;
    store_acceptance(&paths, notice, now)?;
    let profile = plakous_auth::SubscriptionIntegrationProfileId::new(notice.profile_id())?;
    let store = plakous_auth::PrivateCredentialStore::open_slot(
        paths.provider_record(provider),
        profile.clone(),
        plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)?,
    )?;
    // The version 2 payload an earlier build wrote (hex selection epoch).
    let retired = serde_json::to_vec(&serde_json::json!({
        "version": 2,
        "issuer": "https://auth.x.ai",
        "client_id": "retired-client",
        "subject": "retired-subject",
        "scope": "openid",
        "access_token": "retired-access",
        "refresh_token": "retired-refresh",
        "expires_at": now + 3_600,
        "selection_epoch": "ab".repeat(32),
    }))?;
    store.initialize_selected(&plakous_auth::CredentialRecord::new(
        profile,
        plakous_auth::AccountBinding::new("retired-subject")?,
        plakous_auth::CredentialGeneration::new(1)?,
        plakous_auth::CredentialStatus::Ready,
        plakous_auth::SecretBytes::new(retired),
    )?)?;
    let subscriptions = NativeSubscriptions::new(paths.clone());
    // Offline status refuses the record exactly as refresh does; neither
    // reports it Ready, and neither contacts the provider.
    assert_eq!(
        subscriptions.status(provider).await,
        Err(NativeSubscriptionError::CredentialProfileChanged)
    );
    assert_eq!(
        subscriptions.refresh(provider).await,
        Err(NativeSubscriptionError::CredentialProfileChanged)
    );
    assert!(matches!(
        subscriptions.subscription_usage(provider).await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::LoginRequired
            ),
            ..
        }
    ));
    // `plak login` replaces the unusable record itself and says so.
    let pending = subscriptions
        .begin_login(provider, NativeLoginMode::Remote)
        .await?;
    assert!(pending.replaced_unusable_record());
    assert_eq!(
        pending.instructions().completion(),
        NativeLoginCompletion::AuthorizationCodeOrRedirectUrl
    );
    drop(pending);
    assert_eq!(
        subscriptions.status(provider).await?.credential(),
        NativeAuthStatus::Revoked
    );
    let again = subscriptions
        .begin_login(provider, NativeLoginMode::Remote)
        .await?;
    assert!(!again.replaced_unusable_record());
    drop(again);
    fs::remove_dir_all(root)?;
    Ok(())
}

/// The ways a newer build's state reaches this build: a newer envelope, a
/// newer payload version (Codex: a present `version` above 1), a newer or
/// unplaceable compatibility profile, a newer receipt and a newer recovery
/// marker.
#[derive(Clone, Copy, Debug)]
enum NewerState {
    Envelope,
    Payload,
    Profile,
    UnplaceableProfile,
    Receipt,
    RecoveryMarker,
}

/// Each seeded file and the exact bytes it must keep.
type SeededFiles = Vec<(PathBuf, Vec<u8>)>;

fn seed_newer_state(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    shape: NewerState,
) -> Result<SeededFiles, Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt as _;
    let notice = NativeSubscriptions::notice(provider);
    let now = unix_now()?;
    store_acceptance(paths, notice, now)?;
    let current = notice.profile_id();
    let (prefix, _) = current.rsplit_once(".compat.").ok_or("profile grammar")?;
    let newer_profile = format!("{prefix}.compat.2099-01-01.r1");
    let payload_version = if provider == NativeProvider::Codex {
        2
    } else {
        99
    };
    let (format_version, profile, payload) = match shape {
        NewerState::Envelope => (3, current.to_owned(), serde_json::json!({})),
        NewerState::Payload => (
            2,
            current.to_owned(),
            serde_json::json!({ "version": payload_version, "future": true }),
        ),
        NewerState::Profile => (2, newer_profile.clone(), serde_json::json!({})),
        NewerState::UnplaceableProfile => {
            (2, "renamed-newer-profile".to_owned(), serde_json::json!({}))
        }
        NewerState::Receipt | NewerState::RecoveryMarker => {
            (2, current.to_owned(), serde_json::json!({ "version": 0 }))
        }
    };
    let record = serde_json::json!({
        "format_version": format_version,
        "profile_id": profile,
        "slot_id": PRIMARY_SLOT,
        "account_binding": "newer-account",
        "generation": 7,
        "status": "ready",
        "payload": base64_encode(&serde_json::to_vec(&payload)?),
    });
    // Receipt and marker shapes leave the slot signed out, so only that file
    // is newer.
    let mut files = Vec::new();
    if !matches!(shape, NewerState::Receipt | NewerState::RecoveryMarker) {
        files.push((
            paths.provider_record(provider),
            serde_json::to_vec(&record)?,
        ));
    }
    match shape {
        NewerState::Receipt => files.push((
            paths.receipt_path(provider),
            serde_json::to_vec(&serde_json::json!({
                "schema": 3,
                "profile_id": newer_profile,
                "future": true,
            }))?,
        )),
        NewerState::RecoveryMarker => files.push((
            paths.login_recovery_path(provider),
            serde_json::to_vec(&serde_json::json!({
                "schema": "plak.native-login-recovery",
                "version": 3,
                "future": true,
            }))?,
        )),
        _ => {}
    }
    for (path, bytes) in &files {
        fs::write(path, bytes)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(files)
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let value = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |value, (index, byte)| {
                value | (u32::from(*byte) << (16 - 8 * index))
            });
        for index in 0..4 {
            if index <= chunk.len() {
                encoded.push(char::from(
                    ALPHABET[((value >> (18 - 6 * index)) & 63) as usize],
                ));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn newer_build_state_is_refused_untouched_until_a_confirmed_replacement()
-> Result<(), Box<dyn Error>> {
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        for shape in [
            NewerState::Envelope,
            NewerState::Payload,
            NewerState::Profile,
            NewerState::UnplaceableProfile,
            NewerState::Receipt,
            NewerState::RecoveryMarker,
        ] {
            let label = format!("{provider:?} {shape:?}");
            let root = unique_test_root(&format!("newer-{}", label.replace(' ', "-")))?;
            let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
            paths.prepare()?;
            let files = seed_newer_state(&paths, provider, shape)?;
            let subscriptions = NativeSubscriptions::new(paths.clone());
            let untouched = || -> Result<(), Box<dyn Error>> {
                for (path, bytes) in &files {
                    assert_eq!(&fs::read(path)?, bytes, "{label}: {}", path.display());
                }
                Ok(())
            };
            assert_eq!(
                subscriptions.status(provider).await?.credential(),
                NativeAuthStatus::WrittenByNewerBuild,
                "{label}"
            );
            untouched()?;
            assert_eq!(
                subscriptions.refresh(provider).await,
                Err(NativeSubscriptionError::CredentialWrittenByNewerBuild),
                "{label}"
            );
            assert!(
                subscriptions.refresh_now(provider).await.is_err(),
                "{label}"
            );
            untouched()?;
            assert!(
                matches!(
                    subscriptions.subscription_usage(provider).await,
                    SubscriptionUsageRead::Unavailable {
                        reason: SubscriptionUsageUnavailable::CredentialState(
                            SubscriptionUsageCredentialState::WrittenByNewerBuild
                        ),
                        ..
                    }
                ),
                "{label}"
            );
            untouched()?;
            assert_eq!(
                subscriptions.revoke(provider).await,
                Err(NativeSubscriptionError::CredentialWrittenByNewerBuild),
                "{label}"
            );
            assert!(
                matches!(
                    subscriptions
                        .begin_login(provider, NativeLoginMode::Remote)
                        .await,
                    Err(NativeSubscriptionError::CredentialWrittenByNewerBuild)
                ),
                "{label}"
            );
            untouched()?;
            let notice = NativeSubscriptions::notice(provider);
            if matches!(shape, NewerState::Receipt) {
                assert_eq!(
                    subscriptions
                        .acknowledge_exact(provider, notice.notice_revision())
                        .await,
                    Err(NativeSubscriptionError::CredentialWrittenByNewerBuild),
                    "{label}"
                );
                untouched()?;
            }

            // The operator's confirmed replacement retires it; login then opens.
            let replaced = subscriptions.replace_newer_build_state(provider).await?;
            assert!(
                matches!(
                    replaced,
                    NativeAuthStatus::Revoked | NativeAuthStatus::SignedOut
                ) || matches!(shape, NewerState::Receipt | NewerState::RecoveryMarker),
                "{label}: {replaced:?}"
            );
            assert_ne!(
                subscriptions.status(provider).await?.credential(),
                NativeAuthStatus::WrittenByNewerBuild,
                "{label}"
            );
            assert!(!paths.login_recovery_path(provider).exists(), "{label}");
            if matches!(shape, NewerState::Receipt) {
                assert!(!paths.receipt_path(provider).exists(), "{label}");
                subscriptions
                    .acknowledge_exact(provider, notice.notice_revision())
                    .await?;
            }
            fs::remove_dir_all(root)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn local_logout_retires_a_newer_build_record_without_a_prompt() -> Result<(), Box<dyn Error>>
{
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        for shape in [
            NewerState::Envelope,
            NewerState::Payload,
            NewerState::Profile,
        ] {
            let root = unique_test_root(&format!("newer-logout-{provider:?}-{shape:?}"))?;
            let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
            paths.prepare()?;
            seed_newer_state(&paths, provider, shape)?;
            let subscriptions = NativeSubscriptions::new(paths.clone());
            let status = subscriptions.logout_local(provider).await?;
            assert!(
                matches!(
                    status,
                    NativeAuthStatus::Revoked | NativeAuthStatus::SignedOut
                ),
                "{provider:?} {shape:?}: {status:?}"
            );
            assert!(matches!(
                subscriptions.status(provider).await?.credential(),
                NativeAuthStatus::Revoked | NativeAuthStatus::SignedOut
            ));
            fs::remove_dir_all(root)?;
        }
    }
    Ok(())
}

/// Begins and drops one login without provider traffic. Codex remote login
/// requests a device code, so Codex begins its loopback browser login, which
/// unit tests bind on an ephemeral port.
async fn begin_offline_login(
    subscriptions: &NativeSubscriptions,
    provider: NativeProvider,
) -> Result<(), Box<dyn Error>> {
    let (mode, completion) = match provider {
        NativeProvider::Codex => (NativeLoginMode::Browser, NativeLoginCompletion::Automatic),
        NativeProvider::Xai | NativeProvider::Claude => (
            NativeLoginMode::Remote,
            NativeLoginCompletion::AuthorizationCodeOrRedirectUrl,
        ),
    };
    let pending = subscriptions.begin_login(provider, mode).await?;
    assert_eq!(pending.instructions().completion(), completion);
    if provider == NativeProvider::Codex {
        let instructions = pending.instructions();
        let url = instructions.verification_uri();
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A"));
        assert!(!url.contains("localhost%3A1455%2F") && !url.contains("localhost%3A1457%2F"));
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn explicit_login_retires_armed_recovery_before_starting_a_fresh_attempt()
-> Result<(), Box<dyn Error>> {
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        let root = unique_test_root(&format!("logout-empty-recovery-{provider:?}"))?;
        let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
        let notice = NativeSubscriptions::notice(provider);
        let now = unix_now()?;
        store_acceptance(&paths, notice, now)?;
        let acceptance = load_acceptance_exact(&paths, notice, now)?;
        arm_login_recovery(&paths, provider, &acceptance.generation)?;

        let subscriptions = NativeSubscriptions::new(paths.clone());
        assert!(!paths.provider_record(provider).exists());
        assert_eq!(
            subscriptions.status(provider).await?,
            NativeSubscriptionStatus {
                compatibility: CompatibilityStatus::Current,
                credential: NativeAuthStatus::Quarantined,
            }
        );
        // Codex remote login requests a device code from the provider, so
        // this offline test begins its loopback browser login instead.
        begin_offline_login(&subscriptions, provider).await?;
        assert!(!login_recovery_required(&paths, provider)?);
        assert_eq!(
            subscriptions.status(provider).await?,
            NativeSubscriptionStatus {
                compatibility: CompatibilityStatus::Current,
                credential: NativeAuthStatus::SignedOut,
            }
        );
        assert!(!paths.login_recovery_path(provider).exists());
        assert!(!paths.login_recovery_epoch_path(provider).exists());
        assert!(!paths.provider_record(provider).exists());
        assert!(!login_recovery_required(&paths, provider)?);
        assert_eq!(
            subscriptions.status(provider).await?,
            NativeSubscriptionStatus {
                compatibility: CompatibilityStatus::Current,
                credential: NativeAuthStatus::SignedOut,
            }
        );
        begin_offline_login(&subscriptions, provider).await?;
        assert_eq!(
            subscriptions.logout_local(provider).await?,
            NativeAuthStatus::SignedOut
        );

        fs::remove_dir_all(root)?;
    }
    let claude = NativeSubscriptions::notice(NativeProvider::Claude);
    assert!(!claude.is_killed());
    assert_eq!(
        claude.kill_switch_revision(),
        plakous_anthropic::CLAUDE_OAUTH_KILL_SWITCH_REVISION
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn subscription_usage_dispatches_native_providers_without_unqualified_claude()
-> Result<(), Box<dyn Error>> {
    let root = unique_test_root("usage-provider-dispatch")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let subscriptions = NativeSubscriptions::new(paths);
    for provider in [
        NativeProvider::Xai,
        NativeProvider::Claude,
        NativeProvider::Codex,
    ] {
        assert!(
            matches!(
                subscriptions.subscription_usage(provider).await,
                SubscriptionUsageRead::Unavailable { reason, .. }
                    if reason != SubscriptionUsageUnavailable::Unqualified
            ),
            "{provider:?} stayed unqualified"
        );
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

/// The compatibility lock guards its shared parent directory, so a lock
/// held for one provider makes a sibling provider's non-blocking read miss.
/// That miss is contention (`Busy`), never a transport failure, and clears
/// as soon as the holder releases.
#[cfg(unix)]
#[tokio::test]
async fn subscription_usage_reports_busy_while_a_sibling_holds_the_compatibility_lock()
-> Result<(), Box<dyn Error>> {
    let root = unique_test_root("usage-busy-sibling")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    paths.prepare()?;
    let held =
        acquire_private_filesystem_lock(&paths.compatibility_lock_path(NativeProvider::Codex))?;
    let subscriptions = NativeSubscriptions::new(paths);
    assert!(matches!(
        subscriptions.subscription_usage(NativeProvider::Xai).await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::Busy,
            ..
        }
    ));
    drop(held);
    assert!(matches!(
        subscriptions.subscription_usage(NativeProvider::Xai).await,
        SubscriptionUsageRead::Unavailable { reason, .. }
            if reason != SubscriptionUsageUnavailable::Busy
                && reason != SubscriptionUsageUnavailable::Transport(None)
    ));
    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn device_approval_wait_leaves_no_recovery_when_cancelled_or_expired()
-> Result<(), Box<dyn Error>> {
    let root = unique_test_root("device-approval")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let notice = fixture_notice();
    let provider = notice.provider();
    let accepted_at = notice.reviewed_at() + 1;
    store_acceptance(&paths, notice, accepted_at)?;
    let loaded = load_acceptance_exact(&paths, notice, accepted_at + 1)?;
    let consent = PendingCompatibilityConsent {
        state: SubscriptionState::new(paths.clone()),
        notice,
        exact_receipt: loaded.exact_bytes,
        generation: loaded.generation,
    };
    let no_residue = |paths: &SubscriptionPaths| -> Result<(), Box<dyn Error>> {
        assert!(!login_recovery_required(paths, provider)?);
        assert!(!paths.login_recovery_path(provider).exists());
        assert!(!paths.provider_record(provider).exists());
        Ok(())
    };
    let calls = Arc::new(AtomicUsize::new(0));

    // Ctrl-C while the operator has not approved yet drops the wait.
    let cancelled_calls = Arc::clone(&calls);
    let waiting = complete_approved_with_consent(
        &consent,
        || Ok(accepted_at + 2),
        std::future::pending::<Result<(), NativeSubscriptionError>>(),
        move |()| async move {
            cancelled_calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(NativeAuthStatus::Ready)
        },
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), waiting)
            .await
            .is_err()
    );
    no_residue(&paths)?;

    let expired_calls = Arc::clone(&calls);
    assert_eq!(
        complete_approved_with_consent(
            &consent,
            || Ok(accepted_at + 2),
            async { Err::<(), _>(NativeSubscriptionError::DeviceCodeExpired) },
            move |()| async move {
                expired_calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(NativeAuthStatus::Ready)
            },
        )
        .await,
        Err(NativeSubscriptionError::DeviceCodeExpired)
    );
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
    no_residue(&paths)?;

    // An approval reaches the guarded exchange; its failure restores recovery.
    let armed_paths = paths.clone();
    assert_eq!(
        complete_approved_with_consent(
            &consent,
            || Ok(accepted_at + 2),
            async { Ok("approved") },
            |approved| async move {
                assert_eq!(approved, "approved");
                assert!(login_recovery_required(&armed_paths, provider)?);
                Err(NativeSubscriptionError::LoginTokenRejected)
            },
        )
        .await,
        Err(NativeSubscriptionError::LoginTokenRejected)
    );
    no_residue(&paths)?;
    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn dropping_a_browser_completion_while_its_callback_is_pending_leaves_no_recovery()
-> Result<(), Box<dyn Error>> {
    let root = unique_test_root("dropped-browser-completion")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let notice = fixture_notice();
    let provider = notice.provider();
    let accepted_at = notice.reviewed_at() + 1;
    store_acceptance(&paths, notice, accepted_at)?;
    let loaded = load_acceptance_exact(&paths, notice, accepted_at + 1)?;
    let consent = PendingCompatibilityConsent {
        state: SubscriptionState::new(paths.clone()),
        notice,
        exact_receipt: loaded.exact_bytes,
        generation: loaded.generation,
    };
    let armed_paths = paths.clone();
    // The Codex browser loopback waits for its callback inside the guarded
    // operation, after recovery is armed; Ctrl-C or the CLI timeout drops it.
    let waiting = complete_pending_with_consent(
        &consent,
        || Ok(accepted_at + 2),
        move || async move {
            assert!(login_recovery_required(&armed_paths, provider)?);
            std::future::pending::<Result<NativeAuthStatus, NativeSubscriptionError>>().await
        },
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), waiting)
            .await
            .is_err()
    );
    assert!(!login_recovery_required(&paths, provider)?);
    assert!(!paths.provider_record(provider).exists());
    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one sequential test exercises pre-dispatch, post-persistence, recovery, and expiry consent boundaries"
)]
async fn pending_login_revalidates_exact_consent_before_and_after_completion()
-> Result<(), Box<dyn Error>> {
    let root = unique_test_root("pending-consent")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let notice = fixture_notice();
    let accepted_at = notice.reviewed_at() + 1;
    store_acceptance(&paths, notice, accepted_at)?;
    let loaded = load_acceptance_exact(&paths, notice, accepted_at + 1)?;
    let consent = PendingCompatibilityConsent {
        state: SubscriptionState::new(paths.clone()),
        notice,
        exact_receipt: loaded.exact_bytes,
        generation: loaded.generation,
    };

    let calls = Arc::new(AtomicUsize::new(0));
    fs::remove_file(paths.receipt_path(notice.provider()))?;
    let removed_calls = Arc::clone(&calls);
    assert_eq!(
        complete_pending_with_consent(
            &consent,
            || Ok(accepted_at + 2),
            move || async move {
                removed_calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(NativeAuthStatus::Ready)
            },
        )
        .await,
        Err(NativeSubscriptionError::AcceptanceRequired)
    );
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);

    store_acceptance(&paths, notice, accepted_at)?;
    let exact = load_acceptance_exact(&paths, notice, accepted_at + 1)?;
    let callback_error_consent = PendingCompatibilityConsent {
        state: SubscriptionState::new(paths.clone()),
        notice,
        exact_receipt: exact.exact_bytes,
        generation: exact.generation,
    };
    assert_eq!(
        complete_pending_with_consent(
            &callback_error_consent,
            || Ok(accepted_at + 2),
            || async { Err(NativeSubscriptionError::InvalidCallback) },
        )
        .await,
        Err(NativeSubscriptionError::InvalidCallback)
    );
    assert!(!login_recovery_required(&paths, notice.provider())?);

    store_acceptance(&paths, notice, accepted_at)?;
    let exact = load_acceptance_exact(&paths, notice, accepted_at + 1)?;
    let ambiguous_commit_consent = PendingCompatibilityConsent {
        state: SubscriptionState::new(paths.clone()),
        notice,
        exact_receipt: exact.exact_bytes,
        generation: exact.generation,
    };
    let credential_path = paths.provider_record(notice.provider());
    assert_eq!(
        complete_pending_with_consent(
            &ambiguous_commit_consent,
            || Ok(accepted_at + 2),
            move || async move {
                write_private_atomic(&credential_path, br#"{"generation":1}"#)?;
                Err(NativeSubscriptionError::Authentication)
            },
        )
        .await,
        Err(NativeSubscriptionError::Authentication)
    );
    assert!(login_recovery_required(&paths, notice.provider())?);
    retire_login_recovery(&paths, notice.provider())?;
    fs::remove_file(paths.provider_record(notice.provider()))?;

    store_acceptance(&paths, notice, accepted_at)?;
    let exact = load_acceptance_exact(&paths, notice, accepted_at + 1)?;
    let exact_consent = PendingCompatibilityConsent {
        state: SubscriptionState::new(paths.clone()),
        notice,
        exact_receipt: exact.exact_bytes,
        generation: exact.generation,
    };
    store_acceptance(&paths, notice, accepted_at + 1)?;
    let replaced_calls = Arc::clone(&calls);
    assert_eq!(
        complete_pending_with_consent(
            &exact_consent,
            || Ok(accepted_at + 2),
            move || async move {
                replaced_calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(NativeAuthStatus::Ready)
            },
        )
        .await,
        Err(NativeSubscriptionError::AcceptanceRequired)
    );
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);

    store_acceptance(&paths, notice, accepted_at)?;
    let exact = load_acceptance_exact(&paths, notice, accepted_at + 1)?;
    let post_consent = PendingCompatibilityConsent {
        state: SubscriptionState::new(paths.clone()),
        notice,
        exact_receipt: exact.exact_bytes,
        generation: exact.generation,
    };
    let receipt_path = paths.receipt_path(notice.provider());
    let post_calls = Arc::clone(&calls);
    assert_eq!(
        complete_pending_with_consent(
            &post_consent,
            || Ok(accepted_at + 2),
            move || async move {
                post_calls.fetch_add(1, AtomicOrdering::SeqCst);
                fs::remove_file(receipt_path)
                    .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
                Ok(NativeAuthStatus::Ready)
            },
        )
        .await,
        Err(NativeSubscriptionError::CredentialUnavailable)
    );
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    assert!(login_recovery_required(&paths, notice.provider())?);
    let recovery_status = NativeSubscriptions::new(paths.clone())
        .status(notice.provider())
        .await?;
    assert_eq!(recovery_status.credential(), NativeAuthStatus::Quarantined);
    retire_login_recovery(&paths, notice.provider())?;

    store_acceptance(&paths, notice, accepted_at)?;
    let exact = load_acceptance_exact(&paths, notice, accepted_at + 1)?;
    let expiry_consent = PendingCompatibilityConsent {
        state: SubscriptionState::new(paths.clone()),
        notice,
        exact_receipt: exact.exact_bytes,
        generation: exact.generation,
    };
    let mut times = [accepted_at + 2, notice.expires_at()].into_iter();
    let expiry_calls = Arc::clone(&calls);
    assert_eq!(
        complete_pending_with_consent(
            &expiry_consent,
            || times.next().ok_or(NativeSubscriptionError::Internal),
            move || async move {
                expiry_calls.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(NativeAuthStatus::Ready)
            },
        )
        .await,
        Err(NativeSubscriptionError::ProfileInactive)
    );
    assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);

    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn paths_reject_relative_and_parent_traversing_roots() {
    for path in [Path::new(""), Path::new("relative"), Path::new("/")] {
        assert!(matches!(
            SubscriptionPaths::from_xdg_state_home(path),
            Err(NativeSubscriptionError::UnsafeStorage)
        ));
    }
    assert!(matches!(
        SubscriptionPaths::from_xdg_state_home("/tmp/../state"),
        Err(NativeSubscriptionError::UnsafeStorage)
    ));
}

#[cfg(unix)]
#[test]
fn absent_state_root_is_created_componentwise_and_privately() -> Result<(), Box<dyn Error>> {
    let sandbox = unique_test_root("create-root")?;
    let intermediate = sandbox.join("new-parent");
    let root = intermediate.join("state");
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;

    assert_eq!(
        load_acceptance(&paths, fixture_notice(), 1_800_000_001),
        Err(NativeSubscriptionError::AcceptanceRequired)
    );
    for directory in [intermediate, root] {
        let metadata = fs::metadata(directory)?;
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o700);
        assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
    }

    fs::remove_dir_all(sandbox)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn inactive_profile_precedes_missing_receipt() -> Result<(), Box<dyn Error>> {
    let root = unique_test_root("inactive-missing")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let notice = fixture_notice();

    assert_eq!(
        load_acceptance(
            &paths,
            CompatibilityNotice {
                killed: true,
                ..notice
            },
            notice.reviewed_at()
        ),
        Err(NativeSubscriptionError::ProfileInactive)
    );
    assert_eq!(
        load_acceptance(&paths, notice, notice.expires_at()),
        Err(NativeSubscriptionError::ProfileInactive)
    );
    assert!(!paths.receipt_path(NativeProvider::Codex).exists());

    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn exact_receipt_round_trip_and_drift_rejection() -> Result<(), Box<dyn Error>> {
    let root = unique_test_root("receipt")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let notice = fixture_notice();
    store_acceptance(&paths, notice, 1_800_000_001)?;
    assert_eq!(
        load_acceptance(&paths, notice, 1_800_000_002)?,
        1_800_000_001
    );
    let drifted = CompatibilityNotice {
        source_revision: "different-source",
        ..notice
    };
    assert_eq!(
        load_acceptance(&paths, drifted, 1_800_000_002),
        Err(NativeSubscriptionError::AcceptanceRequired)
    );
    let metadata = fs::metadata(paths.receipt_path(NativeProvider::Codex))?;
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
    assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
    for directory in [
        root.clone(),
        root.join("plak"),
        root.join("plak/subscriptions"),
        root.join("plak/subscriptions/codex"),
        root.join("plak/subscriptions/xai"),
        root.join("plak/subscriptions/compatibility"),
    ] {
        let metadata = fs::metadata(directory)?;
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o700);
        assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn same_second_reack_has_a_distinct_generation_and_invalidates_old_handles()
-> Result<(), Box<dyn Error>> {
    let root = unique_test_root("consent-generation")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let notice = fixture_notice();
    let accepted_at = notice.reviewed_at() + 1;

    store_acceptance(&paths, notice, accepted_at)?;
    let first = load_acceptance_exact(&paths, notice, accepted_at)?;
    let old_handle = PendingCompatibilityConsent {
        state: SubscriptionState::new(paths.clone()),
        notice,
        exact_receipt: first.exact_bytes.clone(),
        generation: first.generation.clone(),
    };
    store_acceptance(&paths, notice, accepted_at)?;
    let second = load_acceptance_exact(&paths, notice, accepted_at)?;
    assert_ne!(first.generation, second.generation);
    assert_ne!(first.exact_bytes, second.exact_bytes);
    assert_eq!(
        old_handle.validate_at(accepted_at).await,
        Err(NativeSubscriptionError::AcceptanceRequired)
    );

    let mut missing_generation: serde_json::Value = serde_json::from_slice(&second.exact_bytes)?;
    missing_generation
        .as_object_mut()
        .ok_or("receipt must be an object")?
        .remove("generation");
    write_private_atomic(
        &paths.receipt_path(notice.provider()),
        &serde_json::to_vec(&missing_generation)?,
    )?;
    assert_eq!(
        load_acceptance_exact(&paths, notice, accepted_at).err(),
        Some(NativeSubscriptionError::InvalidReceipt)
    );

    missing_generation["schema"] = serde_json::Value::from(1);
    write_private_atomic(
        &paths.receipt_path(notice.provider()),
        &serde_json::to_vec(&missing_generation)?,
    )?;
    assert_eq!(
        load_acceptance_exact(&paths, notice, accepted_at).err(),
        Some(NativeSubscriptionError::InvalidReceipt)
    );

    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one matrix keeps clear ambiguity, restoration failure, replay, deletion, restart simulation, and cross-instance status on the same v2 protocol"
)]
async fn recovery_v2_rejects_ambiguous_clear_deletion_and_stale_replay_across_instances()
-> Result<(), Box<dyn Error>> {
    let provider = NativeProvider::Codex;

    let clear_root = unique_test_root("recovery-v2-clear")?;
    let clear_paths = SubscriptionPaths::from_xdg_state_home(&clear_root)?;
    let clear_generation = install_ready_login_fixture(&clear_paths, provider, 7)?;
    let operation = arm_login_recovery(&clear_paths, provider, &clear_generation)?;
    assert_eq!(
        clear_login_recovery_with_hook(&clear_paths, provider, &operation, 7, |stage| {
            if stage == AtomicWriteStage::BeforeDirectorySync {
                Err(NativeSubscriptionError::UnsafeStorage)
            } else {
                Ok(())
            }
        },),
        Err(NativeSubscriptionError::UnsafeStorage)
    );
    let restored =
        read_login_recovery_marker(&clear_paths, provider)?.ok_or("armed marker must remain")?;
    assert_eq!(restored.0, operation.marker);
    let second = NativeSubscriptions::new(clear_paths.clone())
        .status(provider)
        .await?;
    assert_eq!(second.credential(), NativeAuthStatus::Quarantined);

    let restoration_root = unique_test_root("recovery-v2-restoration")?;
    let restoration_paths = SubscriptionPaths::from_xdg_state_home(&restoration_root)?;
    let restoration_generation = install_ready_login_fixture(&restoration_paths, provider, 11)?;
    let restoration_operation =
        arm_login_recovery(&restoration_paths, provider, &restoration_generation)?;
    let marker_directory = restoration_paths
        .login_recovery_path(provider)
        .parent()
        .ok_or("marker parent")?
        .to_owned();
    let restoration_result = clear_login_recovery_with_hook(
        &restoration_paths,
        provider,
        &restoration_operation,
        11,
        |stage| {
            if stage == AtomicWriteStage::BeforeDirectorySync {
                fs::set_permissions(&marker_directory, fs::Permissions::from_mode(0o500))
                    .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
                Err(NativeSubscriptionError::UnsafeStorage)
            } else {
                Ok(())
            }
        },
    );
    fs::set_permissions(&marker_directory, fs::Permissions::from_mode(0o700))?;
    assert_eq!(
        restoration_result,
        Err(NativeSubscriptionError::UnsafeStorage)
    );
    let second = NativeSubscriptions::new(restoration_paths.clone())
        .status(provider)
        .await?;
    assert_eq!(second.credential(), NativeAuthStatus::Quarantined);

    let replay_root = unique_test_root("recovery-v2-replay")?;
    let replay_paths = SubscriptionPaths::from_xdg_state_home(&replay_root)?;
    let replay_generation = install_ready_login_fixture(&replay_paths, provider, 17)?;
    let first = arm_login_recovery(&replay_paths, provider, &replay_generation)?;
    clear_login_recovery(&replay_paths, provider, &first, 17)?;
    let old_cleared = fs::read(replay_paths.login_recovery_path(provider))?;
    let second_operation = arm_login_recovery(&replay_paths, provider, &replay_generation)?;
    assert_ne!(
        first.marker.operation_nonce,
        second_operation.marker.operation_nonce
    );
    write_private_atomic(&replay_paths.login_recovery_path(provider), &old_cleared)?;
    let replayed = NativeSubscriptions::new(replay_paths.clone())
        .status(provider)
        .await?;
    assert_eq!(replayed.credential(), NativeAuthStatus::Quarantined);

    let deletion_root = unique_test_root("recovery-v2-deletion")?;
    let deletion_paths = SubscriptionPaths::from_xdg_state_home(&deletion_root)?;
    let deletion_generation = install_ready_login_fixture(&deletion_paths, provider, 23)?;
    let deleted = arm_login_recovery(&deletion_paths, provider, &deletion_generation)?;
    fs::remove_file(deletion_paths.login_recovery_path(provider))?;
    let deleted_status = NativeSubscriptions::new(deletion_paths.clone())
        .status(provider)
        .await?;
    assert_eq!(deleted_status.credential(), NativeAuthStatus::Quarantined);
    write_private_atomic(
        &deletion_paths.login_recovery_path(provider),
        &deleted.marker.cleared(23).to_bytes()?,
    )?;
    let deletion_key = process_quarantine_key(
        &deletion_paths,
        provider,
        deletion_paths.login_recovery_path(provider),
    );
    clear_quarantine(&LOGIN_RECOVERY_QUARANTINE, &deletion_key)?;
    let still_quarantined = NativeSubscriptions::new(deletion_paths.clone())
        .status(provider)
        .await?;
    assert_eq!(
        still_quarantined.credential(),
        NativeAuthStatus::Quarantined
    );

    for root in [clear_root, restoration_root, replay_root, deletion_root] {
        fs::remove_dir_all(root)?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn lock_guard_detects_lock_and_ancestor_replacement_and_serializes_contenders()
-> Result<(), Box<dyn Error>> {
    use std::sync::mpsc;
    use std::thread;

    let root = unique_test_root("lock-replacement")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let provider = NativeProvider::Codex;
    paths.prepare()?;
    let lock_path = paths.compatibility_lock_path(provider);
    let first = acquire_private_filesystem_lock(&lock_path)?;
    let displaced = lock_path.with_extension("displaced");
    fs::rename(&lock_path, &displaced)?;
    fs::write(&lock_path, b"")?;
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))?;

    let contender_path = lock_path.clone();
    let (sender, receiver) = mpsc::channel();
    let contender = thread::spawn(move || {
        let acquired = acquire_private_filesystem_lock(&contender_path);
        let _ = sender.send(acquired.is_ok());
    });
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    assert_eq!(first.verify(), Err(NativeSubscriptionError::UnsafeStorage));
    drop(first);
    assert!(receiver.recv_timeout(Duration::from_secs(2))?);
    contender.join().map_err(|_| "contender panicked")?;

    let second = acquire_private_filesystem_lock(&paths.compatibility_lock_path(provider))?;
    let directory = paths.layout_root().join("compatibility");
    fs::rename(&directory, paths.layout_root().join("compatibility-old"))?;
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    assert_eq!(second.verify(), Err(NativeSubscriptionError::UnsafeStorage));
    drop(second);

    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn receipt_rejects_unsafe_mode_and_future_time() -> Result<(), Box<dyn Error>> {
    let root = unique_test_root("unsafe")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let notice = fixture_notice();
    store_acceptance(&paths, notice, 1_800_000_001)?;
    fs::set_permissions(
        paths.receipt_path(NativeProvider::Codex),
        fs::Permissions::from_mode(0o644),
    )?;
    assert_eq!(
        load_acceptance(&paths, notice, 1_800_000_002),
        Err(NativeSubscriptionError::UnsafeStorage)
    );
    assert_eq!(
        validate_notice_time(notice, 1_800_000_400, 1_800_000_000),
        Err(NativeSubscriptionError::AcceptanceRequired)
    );
    assert_eq!(
        validate_notice_time(notice, notice.reviewed_at(), notice.expires_at()),
        Err(NativeSubscriptionError::ProfileInactive)
    );
    assert_eq!(
        validate_notice_time(notice, notice.expires_at(), notice.expires_at()),
        Err(NativeSubscriptionError::ProfileInactive)
    );
    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn state_root_is_created_componentwise_without_following_symlinks() -> Result<(), Box<dyn Error>> {
    let sandbox = unique_test_root("symlink")?;
    let target = sandbox.join("target");
    fs::create_dir(&target)?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
    let link = sandbox.join("linked-parent");
    symlink(&target, &link)?;

    let paths = SubscriptionPaths::from_xdg_state_home(link.join("state"))?;
    assert_eq!(paths.prepare(), Err(NativeSubscriptionError::UnsafeStorage));
    assert!(!target.join("state").exists());

    fs::remove_file(link)?;
    fs::remove_dir_all(sandbox)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn state_root_accepts_safe_shared_mode_and_rejects_writable_mode() -> Result<(), Box<dyn Error>> {
    let sandbox = unique_test_root("root-mode")?;
    let root = sandbox.join("state");
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;

    paths.prepare()?;
    assert_eq!(
        fs::metadata(root.join("plak"))?.permissions().mode() & 0o7777,
        0o700
    );

    fs::remove_dir_all(root.join("plak"))?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o775))?;
    assert_eq!(paths.prepare(), Err(NativeSubscriptionError::UnsafeStorage));
    assert!(!root.join("plak").exists());

    fs::remove_dir_all(sandbox)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn atomic_receipt_rejects_unsafe_existing_file_without_overwrite() -> Result<(), Box<dyn Error>> {
    let root = unique_test_root("existing-file")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    paths.prepare()?;
    let receipt = paths.receipt_path(NativeProvider::Codex);
    let original = b"unsafe existing receipt";
    fs::write(&receipt, original)?;
    fs::set_permissions(&receipt, fs::Permissions::from_mode(0o644))?;

    assert_eq!(
        store_acceptance(&paths, fixture_notice(), 1_800_000_001),
        Err(NativeSubscriptionError::UnsafeStorage)
    );
    assert_eq!(fs::read(receipt)?, original);

    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn receipt_symlink_is_unsafe_and_never_followed() -> Result<(), Box<dyn Error>> {
    let root = unique_test_root("receipt-symlink")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    paths.prepare()?;
    let target = root.join("outside-receipt");
    let original = b"not a compatibility receipt";
    fs::write(&target, original)?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
    symlink(&target, paths.receipt_path(NativeProvider::Codex))?;

    assert_eq!(
        load_acceptance(&paths, fixture_notice(), 1_800_000_001),
        Err(NativeSubscriptionError::UnsafeStorage)
    );
    assert_eq!(
        store_acceptance(&paths, fixture_notice(), 1_800_000_001),
        Err(NativeSubscriptionError::UnsafeStorage)
    );
    assert_eq!(fs::read(target)?, original);

    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn compatibility_receipt_rejects_hardlinks_and_retained_file_mutation() -> Result<(), Box<dyn Error>>
{
    let root = unique_test_root("receipt-stability")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let notice = fixture_notice();
    store_acceptance(&paths, notice, 1_800_000_001)?;
    let receipt = paths.receipt_path(NativeProvider::Codex);

    let alias = root.join("receipt-hardlink");
    fs::hard_link(&receipt, &alias)?;
    assert_eq!(
        load_acceptance(&paths, notice, 1_800_000_002),
        Err(NativeSubscriptionError::UnsafeStorage)
    );
    fs::remove_file(alias)?;

    assert_eq!(
        read_private_bounded_with_hook(&receipt, || {
            assert!(fs::write(&receipt, b"changed while retained descriptor was open").is_ok());
            assert!(fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).is_ok());
        }),
        Err(NativeSubscriptionError::UnsafeStorage)
    );

    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn compatibility_receipt_rejects_path_replacement_during_read() -> Result<(), Box<dyn Error>> {
    let root = unique_test_root("receipt-path-binding")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let notice = fixture_notice();
    store_acceptance(&paths, notice, 1_800_000_001)?;
    let receipt = paths.receipt_path(NativeProvider::Codex);
    let displaced = root.join("displaced-receipt");

    assert_eq!(
        read_private_bounded_with_hook(&receipt, || {
            assert!(fs::rename(&receipt, &displaced).is_ok());
            assert!(fs::write(&receipt, b"replacement receipt").is_ok());
            assert!(fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).is_ok());
        }),
        Err(NativeSubscriptionError::UnsafeStorage)
    );

    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
fn unique_test_root(label: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = std::env::temp_dir().join(format!(
        "plak-subscription-{label}-{}-{}",
        std::process::id(),
        NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

#[cfg(unix)]
fn install_ready_login_fixture(
    paths: &SubscriptionPaths,
    provider: NativeProvider,
    generation: u64,
) -> Result<String, Box<dyn Error>> {
    paths.prepare()?;
    let notice = NativeSubscriptions::notice(provider);
    let now = unix_now()?;
    store_acceptance(paths, notice, now)?;
    let acceptance = load_acceptance_exact(paths, notice, now)?;
    let profile = plakous_auth::SubscriptionIntegrationProfileId::new(notice.profile_id())?;
    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)?;
    let store = plakous_auth::PrivateCredentialStore::open_slot(
        paths.provider_record(provider),
        profile.clone(),
        slot,
    )?;
    let record = plakous_auth::CredentialRecord::new(
        profile,
        plakous_auth::AccountBinding::new("fixture-account")?,
        plakous_auth::CredentialGeneration::new(generation)?,
        plakous_auth::CredentialStatus::Ready,
        plakous_auth::SecretBytes::new(b"fixture-opaque-credential".to_vec()),
    )?;
    store.initialize_selected(&record)?;
    Ok(acceptance.generation)
}

#[cfg(unix)]
#[test]
fn durable_unknown_after_cleared_recovery_gate_is_xai_only() -> Result<(), Box<dyn Error>> {
    let cases = [
        (NativeProvider::Xai, false),
        (NativeProvider::Codex, true),
        (NativeProvider::Claude, true),
    ];
    for (provider, expect_recovery) in cases {
        let suffix = match provider {
            NativeProvider::Xai => "xai",
            NativeProvider::Codex => "codex",
            NativeProvider::Claude => "claude",
        };
        let root = unique_test_root(&format!("login-recovery-unknown-{suffix}"))?;
        let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
        let credential_generation = 7_u64;
        let consent_generation =
            install_ready_login_fixture(&paths, provider, credential_generation)?;
        let operation = arm_login_recovery(&paths, provider, &consent_generation)?;
        clear_login_recovery(&paths, provider, &operation, credential_generation)?;
        let notice = NativeSubscriptions::notice(provider);
        let profile = plakous_auth::SubscriptionIntegrationProfileId::new(notice.profile_id())?;
        let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)?;
        let store = plakous_auth::PrivateCredentialStore::open_slot(
            paths.provider_record(provider),
            profile,
            slot,
        )?;
        store.mark_refresh_intent(plakous_auth::CredentialGeneration::new(
            credential_generation,
        )?)?;
        let required = login_recovery_required_for_status(
            &paths,
            provider,
            Some(&consent_generation),
            NativeAuthStatus::Quarantined,
        )?;
        assert_eq!(required, expect_recovery);
        fs::remove_dir_all(root)?;
    }
    Ok(())
}

#[test]
fn dispatch_admission_is_one_rule_for_spawn_and_catalog() {
    for provider in [
        NativeProvider::Xai,
        NativeProvider::Codex,
        NativeProvider::Claude,
    ] {
        assert!(dispatch_credential_admitted(
            provider,
            NativeAuthStatus::Ready
        ));
        assert_eq!(
            dispatch_credential_admitted(provider, NativeAuthStatus::Quarantined),
            provider == NativeProvider::Xai
        );
        for refused in [
            NativeAuthStatus::SignedOut,
            NativeAuthStatus::RefreshRequired,
            NativeAuthStatus::Revoked,
        ] {
            assert!(!dispatch_credential_admitted(provider, refused));
        }
    }
}

#[cfg(unix)]
#[test]
fn repin_tolerates_a_concurrent_generation_advance() -> Result<(), Box<dyn Error>> {
    let root = unique_test_root("repin-advance")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let provider = NativeProvider::Xai;
    let consent_generation = install_ready_login_fixture(&paths, provider, 7)?;
    let operation = arm_login_recovery(&paths, provider, &consent_generation)?;
    clear_login_recovery(&paths, provider, &operation, 7)?;
    let (marker, _) = read_login_recovery_marker(&paths, provider)?.ok_or("missing marker")?;
    assert_eq!(marker.credential_generation, Some(7));
    let notice = NativeSubscriptions::notice(provider);
    let profile = plakous_auth::SubscriptionIntegrationProfileId::new(notice.profile_id())?;
    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)?;
    let store = plakous_auth::PrivateCredentialStore::open_slot(
        paths.provider_record(provider),
        profile.clone(),
        slot,
    )?;
    let advance = |from: u64, to: u64| -> Result<(), Box<dyn Error>> {
        store.replace(
            plakous_auth::CredentialGeneration::new(from)?,
            &plakous_auth::CredentialRecord::new(
                profile.clone(),
                plakous_auth::AccountBinding::new("fixture-account")?,
                plakous_auth::CredentialGeneration::new(to)?,
                plakous_auth::CredentialStatus::Ready,
                plakous_auth::SecretBytes::new(b"fixture-opaque-credential".to_vec()),
            )?,
        )?;
        Ok(())
    };
    // A legitimate refresh advanced the durable generation to 8; the gate
    // repins to 8, but another refresh lands (9) between the marker write
    // and the verification.
    advance(7, 8)?;
    let mut injected = false;
    repin_cleared_login_recovery_with_hook(&paths, provider, &marker, 8, || {
        if !injected {
            injected = true;
            advance(8, 9).map_err(|_| NativeSubscriptionError::Internal)?;
        }
        Ok(())
    })?;
    let (repinned, _) =
        read_login_recovery_marker(&paths, provider)?.ok_or("missing repinned marker")?;
    assert_eq!(repinned.state, LoginRecoveryState::Cleared);
    assert_eq!(repinned.credential_generation, Some(9));
    assert!(login_epoch_matches(&paths, provider, &repinned)?);
    // No sticky process-local quarantine: the gate admits the Ready record.
    assert!(!login_recovery_required_for_status(
        &paths,
        provider,
        Some(&consent_generation),
        NativeAuthStatus::Ready,
    )?);
    fs::remove_dir_all(root)?;
    Ok(())
}

fn fixture_notice() -> CompatibilityNotice {
    CompatibilityNotice {
        provider: NativeProvider::Codex,
        profile_id: "fixture.profile",
        source_revision: "fixture-source",
        secondary_source_revision: None,
        notice_revision: "fixture-notice",
        provenance: "pinned_compatibility",
        reviewed_at: 1_800_000_000,
        expires_at: 1_800_010_000,
        kill_switch_revision: "fixture-kill",
        killed: false,
    }
}

#[tokio::test]
async fn turn_scoped_prepare_without_root_does_not_start_inference() -> Result<(), Box<dyn Error>> {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;
    use std::sync::Arc;

    use plakous_model::{
        AttemptId, CancellationToken, Capability, CapabilityConstraints, CapabilitySet,
        ConversationView, FactProvenance, FactSource, FeatureSupport, GenerationLimits,
        LocalDurabilityDisposition, ModelAdapterProfileId, ModelCallContext, ModelErrorKind,
        ModelFamilyId, ModelId, ModelOperationRoot, ModelRequest, OutputBoundDisposition,
        ProtocolId, RequestMetadata, ResolvedModelDescriptor, RouteId, RunId, Support, TurnId,
        ValidationMode,
    };
    use uuid::Uuid;

    let observed = Timestamp::new(1, 0)?;
    let mut features = BTreeMap::new();
    features.insert(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            FactProvenance::new(FactSource::TestFixture, observed, None),
        ),
    );
    let descriptor = crate::test_model::with_context_contract(
        ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("test.protocol")?,
            RouteId::new("test.route")?,
            ModelId::new("test.model")?,
            Some(ModelId::new("test.model")?),
            Some(ModelFamilyId::new("test.family")?),
            ModelAdapterProfileId::new("test.profile")?,
            CapabilitySet::new(features, BTreeMap::new()),
        ),
        observed,
    )?;
    let inner = Arc::new(crate::test_model::scripted_model(
        descriptor.clone(),
        [],
        observed,
    )?);
    let authority = crate::test_model::runtime_authority(&descriptor, observed)?;
    let unwrapped: Arc<dyn LanguageModel> = Arc::clone(&inner) as _;
    let unwrapped_configured = crate::test_model::configured_model(
        Arc::clone(&unwrapped),
        descriptor.clone(),
        authority.clone(),
        std::iter::empty(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero turn-scoped output bound")?,
        ),
    )?;
    let resolved = ResolvedProvider {
        model: ResolvedNativeModel {
            selected_model: descriptor.requested_model().clone(),
            descriptor: descriptor.clone(),
            model: unwrapped,
            evidence: SubscriptionResolutionEvidence {
                account_fingerprint: "fixture-account".to_owned(),
                credential_authority_binding: "fixture-credential".to_owned(),
                credential_generation: 1,
                credential_selection_epoch: "fixture-epoch".to_owned(),
                route_fingerprint: "fixture-route".to_owned(),
                descriptor_digest: "fixture-descriptor".to_owned(),
                dialect: "test.protocol".to_owned(),
                adapter_revision: "fixture-adapter".to_owned(),
                catalog_observed_at: observed,
                catalog_expires_at: observed,
                runtime_admission_witness: None,
                profile_revision: "fixture-profile".to_owned(),
                codex_exact: None,
                xai_exact: None,
            },
            retrieval: None,
            image_gen: None,
        },
        configured: unwrapped_configured,
        native_tool_mode: Some(NativeToolMode::CodeModeOnly),
        reresolution: None,
    };
    let turn_gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
        SubscriptionRequestLimits::new(1, 1, 8, 1_000_000, Some(Duration::from_secs(30)), 0)?,
        Arc::new(SystemSubscriptionGateClock::default()),
    ));
    let wrapped_provider = apply_turn_scoped_provider_gate(resolved, turn_gate)
        .map_err(|error| format!("turn-scoped provider wrapping failed: {error:?}"))?;
    assert_eq!(
        wrapped_provider.native_tool_mode(),
        Some(NativeToolMode::CodeModeOnly),
        "turn-scoped model wrapping must retain already-rotated catalog evidence"
    );
    wrapped_provider.require_native_tool_mode(NativeToolMode::CodeModeOnly)?;

    let model = wrap_turn_scoped_native_model(Arc::clone(&inner) as _);
    let configured = crate::test_model::configured_model(
        Arc::clone(&model),
        descriptor.clone(),
        authority.clone(),
        std::iter::empty(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(128).ok_or("nonzero turn-scoped output bound")?,
        ),
    )?;
    let turn = TurnId::try_from(Uuid::from_u128(2))?;
    let attempt = AttemptId::try_from(Uuid::from_u128(3))?;
    let request = ModelRequest::builder(
        configured.configuration().selected_identity(),
        configured.configuration().effective_identity(),
        ConversationView::default(),
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1))?,
            turn,
            attempt,
            observed,
        ),
    )
    .limits(GenerationLimits::new(NonZeroU32::new(128), Vec::new())?)
    .build()?;
    let plan = descriptor.validate_request(request, ValidationMode::Strict)?;
    let unbound = Arc::clone(&model).prepare_call(
        plan.clone(),
        ModelPreparationContext::new(authority.clone(), observed),
    )?;
    let unbound_start = unbound
        .start(ModelCallContext::new(
            attempt,
            CancellationToken::new(),
            None,
        ))
        .await;
    let Err(unbound_error) = unbound_start else {
        return Err("unbound start must fail closed".into());
    };
    assert_eq!(unbound_error.kind(), ModelErrorKind::ResourceLimit);
    assert!(
        unbound_error
            .message()
            .contains("inference root is unavailable")
    );
    assert_eq!(
        inner.captured_records(),
        [] as [plakous_model::PreparedCallRecord; 0]
    );

    let _root = Arc::clone(&model).begin_operation_root(ModelOperationRoot::Turn(turn))?;
    let bound =
        Arc::clone(&model).prepare_call(plan, ModelPreparationContext::new(authority, observed))?;
    let bound_start = bound
        .start(ModelCallContext::new(
            attempt,
            CancellationToken::new(),
            None,
        ))
        .await;
    assert_ne!(
        bound_start.as_ref().err().map(ModelError::kind),
        Some(ModelErrorKind::ResourceLimit)
    );
    Ok(())
}

/// An earlier build's Codex login whose generation moved after the last
/// pinned marker (a refresh no status read re-pinned) is reported
/// LoginRequired; the recovery gate re-pins it like a ready login instead of
/// setting the sticky process-local quarantine.
#[cfg(unix)]
#[test]
fn login_required_after_an_unpinned_refresh_is_not_sticky_quarantined() -> Result<(), Box<dyn Error>>
{
    let root = unique_test_root("login-required-repin")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let provider = NativeProvider::Codex;
    let consent_generation = install_ready_login_fixture(&paths, provider, 7)?;
    let operation = arm_login_recovery(&paths, provider, &consent_generation)?;
    clear_login_recovery(&paths, provider, &operation, 7)?;
    let notice = NativeSubscriptions::notice(provider);
    let profile = plakous_auth::SubscriptionIntegrationProfileId::new(notice.profile_id())?;
    let slot = plakous_auth::CredentialSlotId::new(PRIMARY_SLOT)?;
    let store = plakous_auth::PrivateCredentialStore::open_slot(
        paths.provider_record(provider),
        profile.clone(),
        slot,
    )?;
    store.replace(
        plakous_auth::CredentialGeneration::new(7)?,
        &plakous_auth::CredentialRecord::new(
            profile,
            plakous_auth::AccountBinding::new("fixture-account")?,
            plakous_auth::CredentialGeneration::new(9)?,
            plakous_auth::CredentialStatus::Ready,
            plakous_auth::SecretBytes::new(b"fixture-opaque-credential".to_vec()),
        )?,
    )?;
    let required = login_recovery_required_for_status(
        &paths,
        provider,
        Some(&consent_generation),
        NativeAuthStatus::LoginRequired,
    )?;
    fs::remove_dir_all(root)?;
    assert!(
        !required,
        "LoginRequired after an unpinned refresh set the sticky login-recovery quarantine; status shows Quarantined"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn status_on_absent_storage_is_signed_out_and_creates_nothing() -> Result<(), Box<dyn Error>>
{
    let root = unique_test_root("absent-status")?;
    let state_home = root.join("missing").join("state");
    let subscriptions =
        NativeSubscriptions::new(SubscriptionPaths::from_xdg_state_home(&state_home)?);
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        assert_eq!(
            subscriptions.status(provider).await?,
            NativeSubscriptionStatus {
                compatibility: CompatibilityStatus::Required,
                credential: NativeAuthStatus::SignedOut,
            }
        );
    }
    assert!(!root.join("missing").exists());

    fs::create_dir_all(&state_home)?;
    fs::set_permissions(&state_home, fs::Permissions::from_mode(0o700))?;
    assert_eq!(
        subscriptions
            .status(NativeProvider::Codex)
            .await?
            .credential(),
        NativeAuthStatus::SignedOut
    );
    assert_eq!(fs::read_dir(&state_home)?.count(), 0);

    let linked = root.join("linked");
    symlink(&state_home, &linked)?;
    let through_link = NativeSubscriptions::new(SubscriptionPaths::from_xdg_state_home(&linked)?);
    assert_eq!(
        through_link.status(NativeProvider::Codex).await,
        Err(NativeSubscriptionError::UnsafeStorage)
    );
    fs::remove_dir_all(root)?;
    Ok(())
}

/// A profile re-review moves `reviewed_at`, so a receipt recorded under the
/// previous review is no longer current. That is acceptance-required, not a
/// fenced credential: the stored login is not quarantined, the operator
/// accepts the notice again without logging in, and the login stays admitted
/// under the new acceptance, for every native vertical.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_profile_re_review_requires_acceptance_and_keeps_the_ready_login()
-> Result<(), Box<dyn Error>> {
    for provider in [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ] {
        let root = unique_test_root(&format!("re-review-{}", provider.as_str()))?;
        let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
        let consent_generation = install_ready_login_fixture(&paths, provider, 3)?;
        let operation = arm_login_recovery(&paths, provider, &consent_generation)?;
        clear_login_recovery(&paths, provider, &operation, 3)?;

        let notice = NativeSubscriptions::notice(provider);
        let receipt_path = paths.receipt_path(provider);
        let mut receipt: serde_json::Value = serde_json::from_slice(&fs::read(&receipt_path)?)?;
        receipt["reviewed_at"] = serde_json::Value::from(notice.reviewed_at() - 864_000);
        receipt["expires_at"] = serde_json::Value::from(notice.expires_at() - 864_000);
        write_private_atomic(&receipt_path, &serde_json::to_vec(&receipt)?)?;
        assert_eq!(
            load_acceptance_exact(&paths, notice, unix_now()?).err(),
            Some(NativeSubscriptionError::AcceptanceRequired)
        );

        let state = SubscriptionState::new(paths.clone());
        let stale_consent = state.stored_acceptance_generation(provider)?;
        assert!(
            !login_recovery_required_for_status(
                &paths,
                provider,
                stale_consent.as_deref(),
                NativeAuthStatus::Ready,
            )?,
            "{provider:?}: a stale acceptance must not fence the stored login"
        );

        let subscriptions = NativeSubscriptions::new(paths.clone());
        if provider == NativeProvider::Codex {
            // The fixture's opaque secret is not a Codex token, so Codex reads
            // it as login-required; what matters is that it is not fenced.
            let before = subscriptions.status(provider).await?;
            assert_eq!(
                (before.compatibility(), before.credential()),
                (
                    CompatibilityStatus::Required,
                    NativeAuthStatus::LoginRequired
                )
            );
        }

        subscriptions
            .acknowledge_exact(provider, notice.notice_revision())
            .await?;
        let accepted = load_acceptance_exact(&paths, notice, unix_now()?)?;
        assert_ne!(accepted.generation, consent_generation);
        assert!(
            !login_recovery_required_for_status(
                &paths,
                provider,
                Some(&accepted.generation),
                NativeAuthStatus::Ready,
            )?,
            "{provider:?}: accepting the re-reviewed notice must keep the login"
        );
        if provider == NativeProvider::Codex {
            let current = NativeSubscriptions::new(paths.clone())
                .status(provider)
                .await?;
            assert_eq!(
                (current.compatibility(), current.credential()),
                (
                    CompatibilityStatus::Current,
                    NativeAuthStatus::LoginRequired
                )
            );
        }
        fs::remove_dir_all(root)?;
    }
    Ok(())
}

/// Accepting a notice moves only a login bound to the acceptance it replaces:
/// a cleared marker bound to some other consent stays fenced.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn accepting_a_notice_does_not_rebind_a_login_from_another_consent()
-> Result<(), Box<dyn Error>> {
    let provider = NativeProvider::Codex;
    let root = unique_test_root("re-review-foreign-consent")?;
    let paths = SubscriptionPaths::from_xdg_state_home(&root)?;
    let consent_generation = install_ready_login_fixture(&paths, provider, 5)?;
    let operation = arm_login_recovery(&paths, provider, &consent_generation)?;
    clear_login_recovery(&paths, provider, &operation, 5)?;
    let notice = NativeSubscriptions::notice(provider);
    store_acceptance(&paths, notice, unix_now()?)?;

    let subscriptions = NativeSubscriptions::new(paths.clone());
    subscriptions
        .acknowledge_exact(provider, notice.notice_revision())
        .await?;
    let marker = read_login_recovery_marker(&paths, provider)?.ok_or("cleared marker")?;
    assert_eq!(marker.0.consent_generation, consent_generation);
    let status = subscriptions.status(provider).await?;
    fs::remove_dir_all(root)?;
    assert_eq!(status.credential(), NativeAuthStatus::Quarantined);
    Ok(())
}
