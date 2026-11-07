//! API catalog admission end to end: exact reopen from stored evidence, the
//! cache file contract and the failed-listing interval.
use super::*;
use crate::platform::listing::{
    api_model_catalog_observed, cache_path, cached_for_test, read_cache, write_cache,
};
use crate::platform::{ApiCatalogSource, ApiModelLists, ApiWindow, PlatformAdmission};
use plakous::agent::ReasoningEffort;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
    OpenSessionRequest, SessionBackend, SessionOperationId,
};

fn spec<'a>(
    directory: &'a std::path::Path,
    workers: &'a FixtureWorkers,
    extensions: &'a crate::tools::ProductToolExtensions,
    opening: bool,
) -> ApiRuntimeSpec<'a> {
    ApiRuntimeSpec {
        job_config: plakous::tools::JobConfig::default(),
        account: PlatformProvider::Anthropic.account().parse().unwrap(),
        sidecars: workers,
        product_home: Some(directory),
        literal_model: "claude-haiku-5-5",
        effort: Some(ReasoningEffort::Medium),
        composition: CompositionSelection::Direct,
        wire: PlatformProvider::Anthropic.protocol(),
        workspace_root: directory,
        cwd: directory,
        plan_state_authority: "fixture-session",
        opening_durable_session: opening,
        tool_fence: crate::agent::SessionToolFence::Full,
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: extensions,
        telemetry: None,
        spawned_child: false,
    }
}

mod listing_cache {
    use super::*;
    use crate::platform::listing::CachedRows;
    use plakous::api_key::{
        ApiKey, ApiKeyError, ApiKeyGenerationGrant, ApiKeyRequest, ApiKeySource,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PROVIDERS: [PlatformProvider; 3] = [
        PlatformProvider::Anthropic,
        PlatformProvider::OpenAi,
        PlatformProvider::Xai,
    ];
    const NOW: u64 = 2_000_000_000;
    const KEY: &str = "fixture-key-one";

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Failure {
        None,
        Source,
        Fingerprint,
    }

    struct ListingCredentials {
        reads: Arc<AtomicUsize>,
        failure: Failure,
    }

    impl ListingCredentials {
        fn new(failure: Failure) -> Self {
            Self {
                reads: Arc::new(AtomicUsize::new(0)),
                failure,
            }
        }

        fn wrap<S: ?Sized>(
            &self,
            provider: PlatformProvider,
            source: CliResult<Arc<S>>,
        ) -> CliResult<Arc<ListingSource<S>>> {
            if self.failure == Failure::Source {
                return Err(CliFailure::new(format!(
                    "{} selected source failed",
                    provider.account()
                )));
            }
            Ok(Arc::new(ListingSource {
                source: source?,
                reads: Arc::clone(&self.reads),
                fail_fingerprint: self.failure == Failure::Fingerprint,
            }))
        }
    }

    struct ListingSource<S: ?Sized> {
        source: Arc<S>,
        reads: Arc<AtomicUsize>,
        fail_fingerprint: bool,
    }

    impl<R, A, S> ApiKeySource<R, A> for ListingSource<S>
    where
        R: ApiKeyRequest + ?Sized,
        S: ApiKeySource<R, A> + ?Sized,
    {
        fn authority(&self) -> &A {
            self.source.authority()
        }

        fn generation_grant(
            &self,
            request: &dyn ApiKeyRequest,
        ) -> Result<ApiKeyGenerationGrant<A>, ApiKeyError> {
            let previous = self.reads.fetch_add(1, Ordering::SeqCst);
            // A fetch re-resolves after fingerprinting. Refuse that read before
            // constructing HTTP, so every live-path probe remains offline.
            if self.fail_fingerprint || previous != 0 {
                return Err(ApiKeyError::Unavailable);
            }
            self.source.generation_grant(request)
        }

        fn api_key(
            &self,
            request: &R,
            expected: &ApiKeyGenerationGrant<A>,
        ) -> Result<ApiKey, ApiKeyError> {
            self.source.api_key(request, expected)
        }
    }

    impl AnthropicCredentials for ListingCredentials {
        fn anthropic(
            &self,
            _account: &crate::account::AccountRef,
        ) -> CliResult<Arc<plakous::anthropic::AnthropicKeySource>> {
            Ok(self.wrap(
                PlatformProvider::Anthropic,
                Credentials(KEY).anthropic(&"anthropic".parse().unwrap()),
            )?)
        }
    }

    impl OpenAiCredentials for ListingCredentials {
        fn openai(
            &self,
            _account: &crate::account::AccountRef,
        ) -> CliResult<Arc<plakous::openai::OpenAiKeySource>> {
            Ok(self.wrap(
                PlatformProvider::OpenAi,
                Credentials(KEY).openai(&"openai".parse().unwrap()),
            )?)
        }
    }

    impl XaiCredentials for ListingCredentials {
        fn xai(
            &self,
            _account: &crate::account::AccountRef,
        ) -> CliResult<Arc<plakous::xai::XaiPlatformKeySource>> {
            Ok(self.wrap(
                PlatformProvider::Xai,
                Credentials(KEY).xai(&"xai-api".parse().unwrap()),
            )?)
        }
    }

    fn cache_fixture(provider: PlatformProvider) -> (String, serde_json::Value) {
        let (fingerprint, rows) = match provider {
            PlatformProvider::Anthropic => (
                plakous::anthropic::anthropic_api_key_fingerprint(
                    Credentials(KEY)
                        .anthropic(&"anthropic".parse().unwrap())
                        .unwrap()
                        .as_ref(),
                )
                .unwrap(),
                CachedRows::Anthropic(
                    plakous::anthropic::anthropic_api_bundled_listing().unwrap()[..1].to_vec(),
                ),
            ),
            PlatformProvider::OpenAi => (
                plakous::openai::openai_api_key_fingerprint(
                    &openai::openai_credential_reference(&"openai".parse().unwrap()).unwrap(),
                    Credentials(KEY)
                        .openai(&"openai".parse().unwrap())
                        .unwrap()
                        .as_ref(),
                )
                .unwrap(),
                CachedRows::OpenAi(
                    plakous::openai::openai_api_bundled_listing().unwrap()[..1].to_vec(),
                ),
            ),
            PlatformProvider::Xai => (
                plakous::xai::xai_api_key_fingerprint(
                    Credentials(KEY)
                        .xai(&"xai-api".parse().unwrap())
                        .unwrap()
                        .as_ref(),
                )
                .unwrap(),
                CachedRows::Xai(plakous::xai::xai_api_bundled_listing().unwrap()[..1].to_vec()),
            ),
        };
        let document = serde_json::json!({
            "version": 2,
            "account": provider.account(),
            "key_fingerprint": fingerprint,
            "fetched_at": NOW - 60,
            "rows": rows,
        });
        (fingerprint, document)
    }

    fn write_fixture(path: &std::path::Path, document: &serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(document).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn fresh_and_recent_failure_caches_are_used_on_every_platform() {
        for provider in PROVIDERS {
            for age in [60, 6 * 60 * 60 + 10] {
                let directory = tempfile::tempdir().unwrap();
                let credentials = ListingCredentials::new(Failure::None);
                let (fingerprint, mut document) = cache_fixture(provider);
                document["fetched_at"] = (NOW - age).into();
                let path = cache_path(
                    directory.path(),
                    &provider.account().parse().unwrap(),
                    &fingerprint,
                );
                write_fixture(&path, &document);
                if age > 60 {
                    std::fs::write(path.with_extension("failed"), (NOW - 60).to_string()).unwrap();
                }
                let catalog = api_model_catalog_observed(
                    provider,
                    &provider.account().parse().unwrap(),
                    &credentials,
                    Some(directory.path()),
                    ApiModelLists::Live,
                    NOW,
                    None,
                )
                .await;
                assert_eq!(
                    catalog.source,
                    ApiCatalogSource::Cached { age_seconds: age }
                );
                assert_eq!(catalog.models.len(), 1, "{provider:?}");
                assert_eq!(credentials.reads.load(Ordering::SeqCst), 1, "{provider:?}");
            }
        }
    }

    #[tokio::test]
    async fn live_stale_cache_without_a_failure_marker_attempts_a_fetch_on_every_platform() {
        let mut failures = Vec::new();
        for provider in PROVIDERS {
            let directory = tempfile::tempdir().unwrap();
            let credentials = ListingCredentials::new(Failure::None);
            let (fingerprint, mut document) = cache_fixture(provider);
            document["fetched_at"] = 1.into();
            let path = cache_path(
                directory.path(),
                &provider.account().parse().unwrap(),
                &fingerprint,
            );
            write_fixture(&path, &document);
            let catalog = api_model_catalog_observed(
                provider,
                &provider.account().parse().unwrap(),
                &credentials,
                Some(directory.path()),
                ApiModelLists::Live,
                NOW,
                None,
            )
            .await;
            assert_eq!(
                catalog.source,
                ApiCatalogSource::Cached {
                    age_seconds: NOW - 1
                }
            );
            assert_eq!(catalog.models.len(), 1, "{provider:?}");
            let reads = credentials.reads.load(Ordering::SeqCst);
            if reads != 2 || !path.with_extension("failed").exists() {
                failures.push(format!("{provider:?}: fetch not attempted, reads={reads}"));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[tokio::test]
    async fn source_and_fingerprint_failures_keep_exact_diagnostics_on_every_platform() {
        let mut failures = Vec::new();
        for provider in PROVIDERS {
            for failure in [Failure::Source, Failure::Fingerprint] {
                let credentials = ListingCredentials::new(failure);
                let catalog = api_model_catalog_observed(
                    provider,
                    &provider.account().parse().unwrap(),
                    &credentials,
                    None,
                    ApiModelLists::Offline,
                    NOW,
                    None,
                )
                .await;
                let expected = if failure == Failure::Source {
                    format!("{} selected source failed", provider.account())
                } else {
                    match provider {
                        PlatformProvider::Anthropic => "API-key credential source is unavailable",
                        PlatformProvider::OpenAi => {
                            "OpenAI Request error: OpenAI model list credential is unavailable"
                        }
                        PlatformProvider::Xai => "xAI model list credential is unavailable",
                    }
                    .to_owned()
                };
                let expected = ApiCatalogSource::Bundled { reason: expected };
                if catalog.source != expected {
                    failures.push(format!(
                        "{provider:?}: {:?}, expected {expected:?}",
                        catalog.source
                    ));
                }
                assert!(!catalog.models.is_empty(), "{provider:?}");
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[tokio::test]
    async fn corrupt_cache_envelopes_are_ignored_offline_on_every_platform() {
        for provider in PROVIDERS {
            let (fingerprint, document) = cache_fixture(provider);
            let mut wrong_version = document.clone();
            wrong_version["version"] = 999.into();
            let mut wrong_account = document;
            wrong_account["account"] = "other-account".into();
            for bytes in [
                b"not JSON".to_vec(),
                serde_json::to_vec(&wrong_version).unwrap(),
                serde_json::to_vec(&wrong_account).unwrap(),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let credentials = ListingCredentials::new(Failure::None);
                let path = cache_path(
                    directory.path(),
                    &provider.account().parse().unwrap(),
                    &fingerprint,
                );
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, bytes).unwrap();
                let catalog = api_model_catalog_observed(
                    provider,
                    &provider.account().parse().unwrap(),
                    &credentials,
                    Some(directory.path()),
                    ApiModelLists::Offline,
                    NOW,
                    None,
                )
                .await;
                assert_eq!(
                    catalog.source,
                    ApiCatalogSource::Bundled {
                        reason: "no cached model list".into()
                    }
                );
                assert!(!catalog.models.is_empty(), "{provider:?}");
                assert_eq!(credentials.reads.load(Ordering::SeqCst), 1, "{provider:?}");
            }
        }
    }
}

/// An Anthropic API session created under the full window reopens exactly
/// from the evidence the facade reader returns, even after the product default
/// moved back to the working window, and keeps the full window. Once the
/// provider's published facts move (here via the cache), the same reopen is
/// refused before compiling with the rebind remedy.
#[tokio::test]
async fn exact_reopen_from_stored_evidence_keeps_window_and_refuses_moved_facts() {
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("uninvoked-worker");
    std::fs::write(&worker, b"offline artifact identity fixture").unwrap();
    let workers = FixtureWorkers(worker);
    let extensions = crate::tools::ProductToolExtensions::default();
    let sessions = AgentSessions::new(
        SessionBackend::sqlite(directory.path().join("sessions.sqlite3")).unwrap(),
    )
    .unwrap();
    let credentials = Credentials("fixture-key-one");
    let created_admission = PlatformAdmission {
        lists: ApiModelLists::Offline,
        window: ApiWindow::Full,
        created_evidence: None,
    };
    let runtime = construct_platform_runtime_with(
        PlatformProvider::Anthropic,
        &credentials,
        &spec(directory.path(), &workers, &extensions, false),
        None,
        &created_admission,
        review_clock_for(PlatformProvider::Anthropic),
    )
    .await
    .unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("probe:create").unwrap(),
            runtime,
        ))
        .await
        .unwrap()
    else {
        panic!("not created");
    };
    let locator = session.locator();
    session.release().await.unwrap();

    let stored = sessions
        .created_provider_semantic_evidence(locator)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&stored);
    assert!(text.contains(";window=1000000;"), "{text}");

    let reopen_admission = PlatformAdmission {
        lists: ApiModelLists::Offline,
        window: ApiWindow::Working,
        created_evidence: Some(stored.clone()),
    };
    let runtime = construct_platform_runtime_with(
        PlatformProvider::Anthropic,
        &credentials,
        &spec(directory.path(), &workers, &extensions, true),
        None,
        &reopen_admission,
        review_clock_for(PlatformProvider::Anthropic),
    )
    .await
    .unwrap();
    let reopened = sessions
        .open(OpenSessionRequest::new(
            SessionOperationId::new("probe:reopen").unwrap(),
            locator,
            runtime,
        ))
        .await
        .unwrap();
    let OpenSessionOutcome::Opened { session, .. } = reopened else {
        panic!("unchanged facts did not reopen");
    };
    session.release().await.unwrap();

    // The provider now publishes a smaller output limit for Haiku.
    let fingerprint = plakous::anthropic::anthropic_api_key_fingerprint(
        Credentials("fixture-key-one")
            .anthropic(&"anthropic".parse().unwrap())
            .unwrap()
            .as_ref(),
    )
    .unwrap();
    let mut listed = plakous::anthropic::anthropic_api_bundled_listing().unwrap();
    listed
        .iter_mut()
        .find(|row| row.id == "claude-haiku-5-5")
        .unwrap()
        .max_tokens = Some(64_000);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let path = cache_path(
        directory.path(),
        &"anthropic".parse().unwrap(),
        &fingerprint,
    );
    write_cache(
        &path,
        &cached_for_test(PlatformProvider::Anthropic, &fingerprint, now, listed),
    )
    .unwrap();
    let Err(refusal) = construct_platform_runtime_with(
        PlatformProvider::Anthropic,
        &credentials,
        &spec(directory.path(), &workers, &extensions, true),
        None,
        &reopen_admission,
        review_clock_for(PlatformProvider::Anthropic),
    )
    .await
    else {
        panic!("moved facts reopened");
    };
    let message = refusal.message().to_owned();
    assert!(
        message.contains("changed since this session was created"),
        "{message}"
    );
    assert!(message.contains("rebind"), "{message}");
}

/// The cache is a provider list too; it passes the same id and duplicate
/// checks as a fetched page instead of being trusted as written.
#[test]
fn cache_rows_are_validated_like_a_fetched_page() {
    let directory = tempfile::tempdir().unwrap();
    let provider = PlatformProvider::Anthropic;
    let mut listed = plakous::anthropic::anthropic_api_bundled_listing().unwrap();
    let mut evil = listed[0].clone();
    evil.id = "claude-haiku-5-5\u{1b}[2J\tusable".to_owned();
    listed.push(evil);
    listed.push(listed[0].clone());
    let path = cache_path(directory.path(), &provider.account().parse().unwrap(), "fp");
    write_cache(&path, &cached_for_test(provider, "fp", 1, listed)).unwrap();
    assert_eq!(
        read_cache(&path, provider, &provider.account().parse().unwrap(), "fp"),
        None,
        "a cache with an unsafe id and a repeated id was accepted"
    );
}

/// The cache file is owner-only regardless of the process umask.
#[test]
fn cache_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir().unwrap();
    let provider = PlatformProvider::Anthropic;
    let path = cache_path(directory.path(), &provider.account().parse().unwrap(), "fp");
    let listed = plakous::anthropic::anthropic_api_bundled_listing().unwrap();
    write_cache(&path, &cached_for_test(provider, "fp", 1, listed)).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode & 0o077, 0, "cache file mode {mode:o}");
    for created in [
        path.parent().unwrap(),
        path.parent().unwrap().parent().unwrap(),
    ] {
        let mode = std::fs::metadata(created).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o077, 0, "{} mode {mode:o}", created.display());
    }
}

/// A symlink at the cache path is never followed: the target is not read.
#[test]
fn cache_read_does_not_follow_a_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let provider = PlatformProvider::Anthropic;
    let target = directory.path().join("elsewhere.json");
    let listed = plakous::anthropic::anthropic_api_bundled_listing().unwrap();
    write_cache(&target, &cached_for_test(provider, "fp", 1, listed)).unwrap();
    assert!(
        read_cache(
            &target,
            provider,
            &provider.account().parse().unwrap(),
            "fp"
        )
        .is_some()
    );
    let path = cache_path(directory.path(), &provider.account().parse().unwrap(), "fp");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert_eq!(
        read_cache(&path, provider, &provider.account().parse().unwrap(), "fp"),
        None
    );
}

/// Counts key reads: the fingerprint is one read, a fetch would be another.
struct CountingCredentials(Arc<std::sync::atomic::AtomicUsize>);
impl plakous::api_key::ApiKeySource<plakous::anthropic::AnthropicApiKeyRequest>
    for CountingCredentials
{
    fn authority(&self) -> &() {
        &()
    }
    fn generation_grant(
        &self,
        _: &dyn plakous::api_key::ApiKeyRequest,
    ) -> Result<plakous::api_key::ApiKeyGenerationGrant, plakous::api_key::ApiKeyError> {
        Ok(
            plakous::api_key::ApiKey::new("fixture-key-one")?.generation_grant(
                plakous::api_key::CredentialProviderTag::AnthropicPlatform,
                (),
            ),
        )
    }
    fn api_key(
        &self,
        request: &plakous::anthropic::AnthropicApiKeyRequest,
        expected: &plakous::api_key::ApiKeyGenerationGrant,
    ) -> Result<plakous::api_key::ApiKey, plakous::api_key::ApiKeyError> {
        if &self.generation_grant(request)? != expected {
            return Err(plakous::api_key::ApiKeyError::BindingMismatch);
        }
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        plakous::api_key::ApiKey::new("fixture-key-one")
    }
}
impl OpenAiCredentials for CountingCredentials {
    fn openai(
        &self,
        _account: &crate::account::AccountRef,
    ) -> CliResult<Arc<plakous::openai::OpenAiKeySource>> {
        Credentials("fixture-key-one").openai(&"openai".parse().unwrap())
    }
}
impl AnthropicCredentials for CountingCredentials {
    fn anthropic(
        &self,
        _account: &crate::account::AccountRef,
    ) -> CliResult<Arc<plakous::anthropic::AnthropicKeySource>> {
        Ok(Arc::new(CountingCredentials(Arc::clone(&self.0))))
    }
}
impl XaiCredentials for CountingCredentials {
    fn xai(
        &self,
        _account: &crate::account::AccountRef,
    ) -> CliResult<Arc<plakous::xai::XaiPlatformKeySource>> {
        Credentials("fixture-key-one").xai(&"xai-api".parse().unwrap())
    }
}

/// A listing that failed less than five minutes ago is not fetched again: a
/// live lookup uses the stale cache at once instead of waiting on the network.
#[tokio::test]
async fn a_recent_listing_failure_is_not_retried_for_five_minutes() {
    let directory = tempfile::tempdir().unwrap();
    let provider = PlatformProvider::Anthropic;
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let credentials = CountingCredentials(Arc::clone(&reads));
    let fingerprint = plakous::anthropic::anthropic_api_key_fingerprint(
        Credentials("fixture-key-one")
            .anthropic(&"anthropic".parse().unwrap())
            .unwrap()
            .as_ref(),
    )
    .unwrap();
    let path = cache_path(
        directory.path(),
        &provider.account().parse().unwrap(),
        &fingerprint,
    );
    let listed = plakous::anthropic::anthropic_api_bundled_listing().unwrap();
    let now = 2_000_000_000;
    write_cache(&path, &cached_for_test(provider, &fingerprint, 1, listed)).unwrap();
    std::fs::write(path.with_extension("failed"), (now - 60).to_string()).unwrap();
    let catalog = api_model_catalog_observed(
        provider,
        &provider.account().parse().unwrap(),
        &credentials,
        Some(directory.path()),
        ApiModelLists::Live,
        now,
        None,
    )
    .await;
    assert!(
        matches!(catalog.source, ApiCatalogSource::Cached { .. }),
        "{:?}",
        catalog.source
    );
    assert_eq!(
        reads.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the key was read for a fetch inside the failure interval"
    );
}

/// Every (usable row, published effort) the offline catalog admits -- what
/// `plak models <api>` advertises and admission accepts -- constructs on its
/// route, on all three API routes, so the catalog and the route request
/// policy cannot drift apart.
#[tokio::test]
async fn every_admitted_model_effort_constructs_on_its_route() {
    let directory = tempfile::tempdir().unwrap();
    let worker = directory.path().join("uninvoked-worker");
    std::fs::write(&worker, b"offline artifact identity fixture").unwrap();
    let workers = FixtureWorkers(worker);
    let extensions = crate::tools::ProductToolExtensions::default();
    let credentials = Credentials("fixture-key-one");
    let mut constructed = Vec::new();
    let mut failures = Vec::new();
    for provider in [
        PlatformProvider::OpenAi,
        PlatformProvider::Anthropic,
        PlatformProvider::Xai,
    ] {
        for row in platform_models(provider) {
            for effort in row.efforts.clone() {
                let mut spec = spec(directory.path(), &workers, &extensions, false);
                spec.account = provider.account().parse().unwrap();
                spec.wire = provider.protocol();
                spec.literal_model = &row.model;
                spec.effort = Some(effort);
                let built = construct_platform_runtime_with(
                    provider,
                    &credentials,
                    &spec,
                    None,
                    &PlatformAdmission::default(),
                    review_clock_for(provider),
                )
                .await;
                match built {
                    Ok(_) => constructed.push((provider, row.model.clone(), effort)),
                    Err(error) => failures.push(format!(
                        "{} {} {effort:?}: {}",
                        provider.account(),
                        row.model,
                        error.message()
                    )),
                }
            }
        }
    }
    assert!(failures.is_empty(), "admitted but refused: {failures:#?}");
    for (model, effort) in [
        ("grok-4.7", ReasoningEffort::XHigh),
        ("grok-4.3", ReasoningEffort::None),
    ] {
        assert!(
            constructed.contains(&(PlatformProvider::Xai, model.to_owned(), effort)),
            "{model} {effort:?} was not exercised"
        );
    }
}

#[test]
fn unified_api_keys_keep_existing_owner_only_listing_cache_paths() {
    use plakous::api_key::{ApiKey, CredentialProviderTag, StaticApiKeySource};
    use std::path::Path;
    let text = "synthetic-cache-key";
    let anthropic = StaticApiKeySource::for_key(
        ApiKey::new(text).unwrap(),
        CredentialProviderTag::AnthropicPlatform,
        plakous::anthropic::ANTHROPIC_PLATFORM_ORIGIN,
        None,
        (),
    );
    let reference = openai::openai_credential_reference(&"openai".parse().unwrap()).unwrap();
    let openai = plakous::openai::static_openai_api_key_source(
        ApiKey::new(text).unwrap(),
        reference.clone(),
        plakous::openai::CredentialAuthorityId::new("listing-cache-test").unwrap(),
    );
    let xai = StaticApiKeySource::for_key(
        ApiKey::new(text).unwrap(),
        CredentialProviderTag::XaiPlatform,
        "https://api.x.ai",
        None,
        plakous::xai::XaiPlatformApiKeyAuthority::new("account", "generation", 1).unwrap(),
    );
    for (provider, fingerprint, expected) in [
        (
            PlatformProvider::Anthropic,
            plakous::anthropic::anthropic_api_key_fingerprint(&anthropic).unwrap(),
            "anthropic-9cde85e0a177c85a.json",
        ),
        (
            PlatformProvider::OpenAi,
            plakous::openai::openai_api_key_fingerprint(&reference, &openai).unwrap(),
            "openai-56cfe837c01b67b9.json",
        ),
        (
            PlatformProvider::Xai,
            plakous::xai::xai_api_key_fingerprint(&xai).unwrap(),
            "xai-api-7d3c094215a49ad5.json",
        ),
    ] {
        assert_eq!(
            cache_path(
                Path::new("owner"),
                &provider.account().parse().unwrap(),
                &fingerprint
            ),
            Path::new("owner/cache/api-models").join(expected)
        );
    }
}
