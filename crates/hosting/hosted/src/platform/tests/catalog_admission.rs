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
        account: PlatformProvider::Anthropic.account(),
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
    let fingerprint = plakous::anthropic::AnthropicApiKey::new("fixture-key-one")
        .unwrap()
        .fingerprint();
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
    let path = cache_path(directory.path(), PlatformProvider::Anthropic, &fingerprint);
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
    let path = cache_path(directory.path(), provider, "fp");
    write_cache(&path, &cached_for_test(provider, "fp", 1, listed)).unwrap();
    assert_eq!(
        read_cache(&path, provider, "fp"),
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
    let path = cache_path(directory.path(), provider, "fp");
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
    assert!(read_cache(&target, provider, "fp").is_some());
    let path = cache_path(directory.path(), provider, "fp");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert_eq!(read_cache(&path, provider, "fp"), None);
}

/// Counts key reads: the fingerprint is one read, a fetch would be another.
struct CountingCredentials(Arc<std::sync::atomic::AtomicUsize>);
impl plakous::anthropic::AnthropicApiKeySource for CountingCredentials {
    fn current_key(
        &self,
    ) -> Result<plakous::anthropic::AnthropicApiKey, plakous::anthropic::AnthropicError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        plakous::anthropic::AnthropicApiKey::new("fixture-key-one")
    }
}
impl PlatformCredentials for CountingCredentials {
    fn openai(&self) -> CliResult<Arc<dyn plakous::openai::PlatformCredentialSource>> {
        Credentials("fixture-key-one").openai()
    }
    fn anthropic(&self) -> CliResult<Arc<dyn plakous::anthropic::AnthropicApiKeySource>> {
        Ok(Arc::new(CountingCredentials(Arc::clone(&self.0))))
    }
    fn xai(&self) -> CliResult<Arc<dyn plakous::xai::XaiPlatformApiKeySource>> {
        Credentials("fixture-key-one").xai()
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
    let fingerprint = plakous::anthropic::AnthropicApiKey::new("fixture-key-one")
        .unwrap()
        .fingerprint();
    let path = cache_path(directory.path(), provider, &fingerprint);
    let listed = plakous::anthropic::anthropic_api_bundled_listing().unwrap();
    let now = 2_000_000_000;
    write_cache(&path, &cached_for_test(provider, &fingerprint, 1, listed)).unwrap();
    std::fs::write(path.with_extension("failed"), (now - 60).to_string()).unwrap();
    let catalog = api_model_catalog_observed(
        provider,
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
                spec.account = provider.account();
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
