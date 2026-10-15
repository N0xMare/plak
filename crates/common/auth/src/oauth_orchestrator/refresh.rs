//! Shared claimed-refresh ladder.

use std::time::{Duration, Instant};

use plakous_http::Client as HttpClient;

use super::hooks::ClaimedRefreshHooks;
use super::{
    CredentialRefreshFailureClass, CredentialRefreshPhase, RESTORE_POLL_INITIAL, RESTORE_POLL_MAX,
    RefreshAttempt, emit_claimed_http_failure, emit_refresh, handle_build_failure,
    http_status_class, quarantine_or_map, restore_or_map,
};
use crate::{PrivateCredentialStore, PrivateStoreError, RefreshClaim};

/// Run the shared claimed-refresh ladder for one dialect.
///
/// Ladder: profile check → needs-refresh → begin-refresh → build request →
/// authorize-dispatch → execute-claimed-http → validate → complete (or
/// quarantine on any post-start failure).
///
/// When the dialect reports no refresh is needed (`fence_needing_refresh` →
/// `None`), returns [`ClaimedRefreshHooks::fresh_output`].
///
/// # Errors
///
/// Propagates dialect-mapped failures. Post-start failures leave the durable
/// generation quarantined Unknown unless complete succeeds.
pub async fn run_claimed_refresh<H>(
    store: &PrivateCredentialStore,
    client: &HttpClient,
    hooks: &mut H,
) -> Result<H::Output, H::Error>
where
    H: ClaimedRefreshHooks,
{
    hooks.check_profile().await?;
    let Some(mut fence) = hooks.fence_needing_refresh().await? else {
        return hooks.fresh_output();
    };
    let started = Instant::now();
    let deadline = started + CLAIM_WAIT;
    let mut backoff = RESTORE_POLL_INITIAL;
    let claim = loop {
        let begun = if hooks.reclaim_unknown_refresh() {
            store.begin_refresh_reclaiming_unknown(&fence)
        } else {
            store.begin_refresh(&fence)
        };
        match begun {
            Ok(claim) => break claim,
            // A peer writer or in-flight claim owns the store and nothing was
            // written. Back off instead of failing: when the peer finishes,
            // the durable fence changes and the dialect re-decides whether a
            // refresh is still needed (a peer's completed refresh is fresh).
            Err(PrivateStoreError::StoreBusy) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(hooks.map_store_error(PrivateStoreError::StoreBusy));
                }
                tokio::time::sleep(backoff.min(deadline.saturating_duration_since(now))).await;
                backoff = backoff.saturating_mul(2).min(RESTORE_POLL_MAX);
                let current = store.load().map_err(|error| hooks.map_store_error(error))?;
                if current.as_ref() != Some(&fence) {
                    match hooks.fence_needing_refresh().await? {
                        Some(next) => fence = next,
                        None => return hooks.fresh_output(),
                    }
                }
            }
            Err(error) => return Err(hooks.map_store_error(error)),
        }
    };
    run_claimed_refresh_from_claim_started(store, client, claim, hooks, started).await
}

/// Longest a claim waits behind a peer's writer or in-flight refresh before
/// reporting the local contention fact.
const CLAIM_WAIT: Duration = Duration::from_secs(30);

/// Continue the claimed-refresh ladder from an already-taken unstarted claim.
///
/// Used when a dialect (or test) must hold the claim before other store
/// activity, then finish the same linear exchange/complete path.
///
/// # Errors
///
/// Same durable failure rules as [`run_claimed_refresh`].
pub async fn run_claimed_refresh_from_claim<H>(
    store: &PrivateCredentialStore,
    client: &HttpClient,
    claim: RefreshClaim,
    hooks: &mut H,
) -> Result<H::Output, H::Error>
where
    H: ClaimedRefreshHooks,
{
    run_claimed_refresh_from_claim_started(store, client, claim, hooks, Instant::now()).await
}

#[allow(
    clippy::too_many_lines,
    reason = "linear claimed-refresh exits stay in one function so telemetry cannot miss a branch"
)]
async fn run_claimed_refresh_from_claim_started<H>(
    store: &PrivateCredentialStore,
    client: &HttpClient,
    claim: RefreshClaim,
    hooks: &mut H,
    started: Instant,
) -> Result<H::Output, H::Error>
where
    H: ClaimedRefreshHooks,
{
    let attempt = RefreshAttempt::new(claim.fence().generation().get(), started);
    emit_refresh(
        hooks.refresh_observer(),
        &attempt,
        CredentialRefreshPhase::Began,
        None,
        None,
        None,
    );

    let (prepared, request) = match hooks.build_refresh_request(&claim) {
        Ok(value) => value,
        Err(failure) => {
            let (error, phase, failure_class) =
                handle_build_failure(store, claim, failure, attempt.deadline, |error| {
                    hooks.map_store_error(error)
                })
                .await;
            emit_refresh(
                hooks.refresh_observer(),
                &attempt,
                phase,
                failure_class,
                None,
                None,
            );
            return Err(error);
        }
    };

    if let Err(error) = hooks.authorize_dispatch().await {
        let (error, restored) = restore_or_map(store, claim, error, attempt.deadline, |e| {
            hooks.map_store_error(e)
        })
        .await;
        emit_refresh(
            hooks.refresh_observer(),
            &attempt,
            if restored {
                CredentialRefreshPhase::Restored
            } else {
                CredentialRefreshPhase::OutcomeUnknown
            },
            Some(if restored {
                CredentialRefreshFailureClass::NotReady
            } else {
                CredentialRefreshFailureClass::Store
            }),
            None,
            None,
        );
        return Err(error);
    }

    let control = hooks.exchange_control();
    let exchange_deadline = control.deadline();
    let mut response = match hooks
        .execute_claimed(store, client, claim, request, control)
        .await
    {
        Ok(response) => response,
        Err(mut error) => {
            emit_claimed_http_failure(
                store,
                hooks.refresh_observer(),
                &attempt,
                &mut error,
                exchange_deadline,
            )
            .await;
            let mapped = hooks.map_claimed_http_error(store, error).await;
            hooks.after_post_start_failure(store, &mapped);
            return Err(mapped);
        }
    };

    let http_status = Some(response.status().as_u16());
    let record = match hooks.validate_tokens(prepared, &mut response).await {
        Ok(record) => record,
        Err(error) => {
            let transport_failure = response.transport_failure();
            let failure_class =
                if let Some(class) = super::transport_failure_class(transport_failure) {
                    class
                } else if response.status().is_success() {
                    CredentialRefreshFailureClass::Validation
                } else {
                    http_status_class(response.status())
                };
            let (error, quarantined) =
                quarantine_or_map(store, response, error, |e| hooks.map_store_error(e));
            emit_refresh(
                hooks.refresh_observer(),
                &attempt,
                if quarantined {
                    CredentialRefreshPhase::Quarantined
                } else {
                    CredentialRefreshPhase::OutcomeUnknown
                },
                Some(if quarantined {
                    failure_class
                } else {
                    CredentialRefreshFailureClass::Store
                }),
                http_status,
                transport_failure.map(plakous_http::Failure::phase),
            );
            hooks.after_post_start_failure(store, &error);
            return Err(error);
        }
    };
    if let Err(error) = hooks.recheck_profile_before_complete().await {
        let (error, quarantined) =
            quarantine_or_map(store, response, error, |e| hooks.map_store_error(e));
        emit_refresh(
            hooks.refresh_observer(),
            &attempt,
            if quarantined {
                CredentialRefreshPhase::Quarantined
            } else {
                CredentialRefreshPhase::OutcomeUnknown
            },
            Some(if quarantined {
                CredentialRefreshFailureClass::Validation
            } else {
                CredentialRefreshFailureClass::Store
            }),
            http_status,
            None,
        );
        hooks.after_post_start_failure(store, &error);
        return Err(error);
    }
    match store.complete_claimed_http(response, record) {
        Ok(fence) => {
            emit_refresh(
                hooks.refresh_observer(),
                &attempt,
                CredentialRefreshPhase::Completed,
                None,
                http_status,
                None,
            );
            hooks.output_from_completed(fence)
        }
        Err(store_error) => {
            emit_refresh(
                hooks.refresh_observer(),
                &attempt,
                CredentialRefreshPhase::OutcomeUnknown,
                Some(CredentialRefreshFailureClass::Store),
                http_status,
                None,
            );
            let mapped = hooks.map_store_error(store_error);
            hooks.after_post_start_failure(store, &mapped);
            Err(mapped)
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use plakous_http::{Client as HttpClient, ExchangeControl, Limits, Method, Origin, Request};
    use tempfile::TempDir;

    use crate::oauth_orchestrator::hooks::{
        BuildRequestFailure, ClaimedRefreshHooks, CredentialRefreshEvent,
        CredentialRefreshFailureClass, CredentialRefreshObserver, CredentialRefreshPhase,
    };
    use crate::{
        AccountBinding, ClaimedHttpError, ClaimedHttpResponse, CredentialFence,
        CredentialGeneration, CredentialRecord, CredentialSlotId, CredentialStatus,
        PrivateCredentialStore, PrivateStoreError, RefreshClaim, SecretBytes,
        SubscriptionIntegrationProfileId,
    };

    struct RecordingObserver {
        events: Mutex<Vec<CredentialRefreshEvent>>,
    }

    impl RecordingObserver {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn phases(&self) -> Vec<CredentialRefreshPhase> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|event| event.phase)
                .collect()
        }
    }

    impl CredentialRefreshObserver for RecordingObserver {
        fn observe(&self, event: CredentialRefreshEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    struct ScriptedHooks<'a> {
        observer: &'a RecordingObserver,
        fence: Option<CredentialFence>,
        build: BuildMode,
        authorize: Result<(), &'static str>,
        validate: ValidateMode,
        request: Option<Request>,
        control: Duration,
    }

    enum BuildMode {
        Restore,
        LeaveQuarantined,
        Request,
    }

    enum ValidateMode {
        Fail,
        Complete,
        BadGeneration,
    }

    impl ClaimedRefreshHooks for ScriptedHooks<'_> {
        type Error = &'static str;
        type Prepared = ();
        type Output = ();

        fn check_profile(&mut self) -> impl Future<Output = Result<(), Self::Error>> {
            std::future::ready(Ok(()))
        }

        fn fence_needing_refresh(
            &mut self,
        ) -> impl Future<Output = Result<Option<CredentialFence>, Self::Error>> {
            std::future::ready(Ok(self.fence.clone()))
        }

        fn fresh_output(&mut self) -> Result<Self::Output, Self::Error> {
            Ok(())
        }

        fn build_refresh_request(
            &mut self,
            _claim: &RefreshClaim,
        ) -> Result<(Self::Prepared, Request), BuildRequestFailure<Self::Error>> {
            match self.build {
                BuildMode::LeaveQuarantined => {
                    Err(BuildRequestFailure::LeaveQuarantined("corrupt"))
                }
                BuildMode::Restore => Err(BuildRequestFailure::Restore("encode")),
                BuildMode::Request => Ok((
                    (),
                    self.request
                        .clone()
                        .unwrap_or_else(|| dummy_request().expect("request")),
                )),
            }
        }

        fn authorize_dispatch(&mut self) -> impl Future<Output = Result<(), Self::Error>> {
            std::future::ready(self.authorize)
        }

        fn exchange_control(&mut self) -> ExchangeControl {
            ExchangeControl::new(Instant::now() + self.control)
        }

        async fn validate_tokens(
            &mut self,
            _prepared: Self::Prepared,
            response: &mut ClaimedHttpResponse,
        ) -> Result<CredentialRecord, Self::Error> {
            match self.validate {
                ValidateMode::Fail => Err("validate"),
                ValidateMode::Complete | ValidateMode::BadGeneration => {
                    crate::oauth_orchestrator::pull_claimed_body(response, 1024)
                        .await
                        .map_err(|_| "validate")?;
                    let generation = match self.validate {
                        ValidateMode::Complete => 3,
                        ValidateMode::BadGeneration | ValidateMode::Fail => 1,
                    };
                    replacement_record(generation).map_err(|_| "validate")
                }
            }
        }

        fn map_claimed_http_error(
            &mut self,
            _store: &PrivateCredentialStore,
            _error: ClaimedHttpError,
        ) -> impl Future<Output = Self::Error> {
            std::future::ready("transport")
        }

        fn map_store_error(&mut self, _error: PrivateStoreError) -> Self::Error {
            "store"
        }

        fn output_from_completed(
            &mut self,
            _fence: CredentialFence,
        ) -> Result<Self::Output, Self::Error> {
            Ok(())
        }

        fn refresh_observer(&self) -> Option<&dyn CredentialRefreshObserver> {
            Some(self.observer)
        }
    }

    fn dummy_request() -> Result<Request, Box<dyn std::error::Error>> {
        let origin = Origin::parse("https://127.0.0.1:1")?;
        Ok(Request::new(
            origin,
            Method::POST,
            "/token",
            plakous_http::HeaderMap::new(),
            plakous_http::Bytes::new(),
            &Limits::default(),
        )?)
    }

    fn fixture_store()
    -> Result<(TempDir, PrivateCredentialStore, CredentialFence), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let path = directory.path().join("credential.json");
        let profile = SubscriptionIntegrationProfileId::new("profile")?;
        let account = AccountBinding::new("opaque-account")?;
        let slot = CredentialSlotId::new("primary")?;
        let store = PrivateCredentialStore::open_slot(&path, profile.clone(), slot)?;
        let record = CredentialRecord::new(
            profile,
            account,
            CredentialGeneration::new(1)?,
            CredentialStatus::Ready,
            SecretBytes::new(b"payload".to_vec()),
        )?;
        store.initialize_selected(&record)?;
        let fence = store.load()?.ok_or("missing record")?;
        Ok((directory, store, fence))
    }

    fn client() -> Result<HttpClient, Box<dyn std::error::Error>> {
        Ok(HttpClient::new_http1(Limits::default())?)
    }

    #[tokio::test]
    async fn no_refresh_needed_emits_nothing() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store, _fence) = fixture_store()?;
        let observer = RecordingObserver::new();
        let mut hooks = ScriptedHooks {
            observer: &observer,
            fence: None,
            build: BuildMode::Request,
            authorize: Ok(()),
            validate: ValidateMode::Fail,
            request: None,
            control: Duration::from_millis(50),
        };
        run_claimed_refresh(&store, &client()?, &mut hooks).await?;
        assert_eq!(
            observer.phases(),
            [] as [crate::oauth_orchestrator::hooks::CredentialRefreshPhase; 0]
        );
        Ok(())
    }

    #[tokio::test]
    async fn leave_quarantined_emits_began_then_quarantined()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store, fence) = fixture_store()?;
        let observer = RecordingObserver::new();
        let mut hooks = ScriptedHooks {
            observer: &observer,
            fence: Some(fence),
            build: BuildMode::LeaveQuarantined,
            authorize: Ok(()),
            validate: ValidateMode::Fail,
            request: None,
            control: Duration::from_millis(50),
        };
        let error = run_claimed_refresh(&store, &client()?, &mut hooks)
            .await
            .err()
            .ok_or("expected failure")?;
        assert_eq!(error, "corrupt");
        assert_eq!(
            observer.phases(),
            [
                CredentialRefreshPhase::Began,
                CredentialRefreshPhase::Quarantined
            ]
        );
        let events = observer.events.lock().unwrap();
        assert_eq!(events[0].generation, 2);
        assert_eq!(
            events[1].failure_class,
            Some(CredentialRefreshFailureClass::Validation)
        );
        Ok(())
    }

    #[tokio::test]
    async fn restore_build_emits_began_then_restored() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store, fence) = fixture_store()?;
        let observer = RecordingObserver::new();
        let mut hooks = ScriptedHooks {
            observer: &observer,
            fence: Some(fence),
            build: BuildMode::Restore,
            authorize: Ok(()),
            validate: ValidateMode::Fail,
            request: None,
            control: Duration::from_millis(50),
        };
        let error = run_claimed_refresh(&store, &client()?, &mut hooks)
            .await
            .err()
            .ok_or("expected failure")?;
        assert_eq!(error, "encode");
        assert_eq!(
            observer.phases(),
            [
                CredentialRefreshPhase::Began,
                CredentialRefreshPhase::Restored
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn authorize_dispatch_failure_emits_restored() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store, fence) = fixture_store()?;
        let observer = RecordingObserver::new();
        let mut hooks = ScriptedHooks {
            observer: &observer,
            fence: Some(fence),
            build: BuildMode::Request,
            authorize: Err("denied"),
            validate: ValidateMode::Fail,
            request: None,
            control: Duration::from_millis(50),
        };
        let error = run_claimed_refresh(&store, &client()?, &mut hooks)
            .await
            .err()
            .ok_or("expected failure")?;
        assert_eq!(error, "denied");
        assert_eq!(
            observer.phases(),
            [
                CredentialRefreshPhase::Began,
                CredentialRefreshPhase::Restored
            ]
        );
        let events = observer.events.lock().unwrap();
        assert_eq!(
            events[1].failure_class,
            Some(CredentialRefreshFailureClass::NotReady)
        );
        Ok(())
    }

    #[tokio::test]
    async fn unstarted_transport_emits_restored() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store, fence) = fixture_store()?;
        let observer = RecordingObserver::new();
        let mut hooks = ScriptedHooks {
            observer: &observer,
            fence: Some(fence),
            build: BuildMode::Request,
            authorize: Ok(()),
            validate: ValidateMode::Fail,
            request: None,
            control: Duration::from_millis(50),
        };
        let error = run_claimed_refresh(&store, &client()?, &mut hooks)
            .await
            .err()
            .ok_or("expected failure")?;
        assert_eq!(error, "transport");
        assert_eq!(
            observer.phases(),
            [
                CredentialRefreshPhase::Began,
                CredentialRefreshPhase::Restored
            ]
        );
        let events = observer.events.lock().unwrap();
        assert!(events[1].transport_phase.is_some());
        assert!(matches!(
            events[1].failure_class,
            Some(
                CredentialRefreshFailureClass::Transport
                    | CredentialRefreshFailureClass::Timeout
                    | CredentialRefreshFailureClass::Store
            )
        ));
        Ok(())
    }

    #[tokio::test]
    async fn completed_refresh_emits_began_then_completed() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_dir, store, fence) = fixture_store()?;
        let endpoint = loopback_endpoint().await?;
        let server = spawn_loopback_ok(endpoint.listener, endpoint.acceptor);
        let observer = RecordingObserver::new();
        let mut hooks = ScriptedHooks {
            observer: &observer,
            fence: Some(fence),
            build: BuildMode::Request,
            authorize: Ok(()),
            validate: ValidateMode::Complete,
            request: Some(loopback_request(endpoint.origin)?),
            control: Duration::from_secs(30),
        };
        run_claimed_refresh(&store, &endpoint.client, &mut hooks).await?;
        server.await??;
        assert_eq!(
            observer.phases(),
            [
                CredentialRefreshPhase::Began,
                CredentialRefreshPhase::Completed
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn credential_refresh_body_failure_keeps_transport_phase()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store, fence) = fixture_store()?;
        let endpoint = loopback_endpoint().await?;
        let server = spawn_loopback_response(
            endpoint.listener,
            endpoint.acceptor,
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nx",
        );
        let observer = RecordingObserver::new();
        let mut hooks = ScriptedHooks {
            observer: &observer,
            fence: Some(fence),
            build: BuildMode::Request,
            authorize: Ok(()),
            validate: ValidateMode::Complete,
            request: Some(loopback_request(endpoint.origin)?),
            control: Duration::from_secs(30),
        };
        assert_eq!(
            run_claimed_refresh(&store, &endpoint.client, &mut hooks).await,
            Err("validate")
        );
        server.await??;
        let events = observer.events.lock().unwrap();
        assert_eq!(events[1].phase, CredentialRefreshPhase::Quarantined);
        assert_eq!(
            events[1].failure_class,
            Some(CredentialRefreshFailureClass::Transport)
        );
        assert_eq!(
            events[1].transport_phase,
            Some(plakous_http::Phase::ResponseBody)
        );
        Ok(())
    }

    #[tokio::test]
    async fn validate_failure_emits_quarantined() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store, fence) = fixture_store()?;
        let endpoint = loopback_endpoint().await?;
        let server = spawn_loopback_ok(endpoint.listener, endpoint.acceptor);
        let observer = RecordingObserver::new();
        let mut hooks = ScriptedHooks {
            observer: &observer,
            fence: Some(fence),
            build: BuildMode::Request,
            authorize: Ok(()),
            validate: ValidateMode::Fail,
            request: Some(loopback_request(endpoint.origin)?),
            control: Duration::from_secs(30),
        };
        let error = run_claimed_refresh(&store, &endpoint.client, &mut hooks)
            .await
            .err()
            .ok_or("expected failure")?;
        assert_eq!(error, "validate");
        server.await??;
        assert_eq!(
            observer.phases(),
            [
                CredentialRefreshPhase::Began,
                CredentialRefreshPhase::Quarantined
            ]
        );
        let events = observer.events.lock().unwrap();
        assert_eq!(
            events[1].failure_class,
            Some(CredentialRefreshFailureClass::Validation)
        );
        Ok(())
    }

    #[tokio::test]
    async fn complete_store_fail_emits_outcome_unknown() -> Result<(), Box<dyn std::error::Error>> {
        let (_dir, store, fence) = fixture_store()?;
        let endpoint = loopback_endpoint().await?;
        let server = spawn_loopback_ok(endpoint.listener, endpoint.acceptor);
        let observer = RecordingObserver::new();
        let mut hooks = ScriptedHooks {
            observer: &observer,
            fence: Some(fence),
            build: BuildMode::Request,
            authorize: Ok(()),
            validate: ValidateMode::BadGeneration,
            request: Some(loopback_request(endpoint.origin)?),
            control: Duration::from_secs(30),
        };
        let error = run_claimed_refresh(&store, &endpoint.client, &mut hooks)
            .await
            .err()
            .ok_or("expected failure")?;
        assert_eq!(error, "store");
        server.await??;
        assert_eq!(
            observer.phases(),
            [
                CredentialRefreshPhase::Began,
                CredentialRefreshPhase::OutcomeUnknown
            ]
        );
        let events = observer.events.lock().unwrap();
        assert_eq!(
            events[1].failure_class,
            Some(CredentialRefreshFailureClass::Store)
        );
        Ok(())
    }

    fn replacement_record(generation: u64) -> Result<CredentialRecord, Box<dyn std::error::Error>> {
        Ok(CredentialRecord::new(
            SubscriptionIntegrationProfileId::new("profile")?,
            AccountBinding::new("opaque-account")?,
            CredentialGeneration::new(generation)?,
            CredentialStatus::Ready,
            SecretBytes::new(b"next".to_vec()),
        )?)
    }

    struct LoopbackEndpoint {
        listener: tokio::net::TcpListener,
        acceptor: tokio_rustls::TlsAcceptor,
        client: HttpClient,
        origin: Origin,
    }

    async fn loopback_endpoint() -> Result<LoopbackEndpoint, Box<dyn std::error::Error>> {
        use rcgen::generate_simple_self_signed;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        use rustls::{ClientConfig, RootCertStore, ServerConfig};
        use std::sync::Arc;

        let certified = generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
        let certificate = certified.cert.der().clone();
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certified.signing_key.serialize_der(),
        ));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut server_config = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)?;
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(certificate.to_vec()))?;
        let client_config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        Ok(LoopbackEndpoint {
            listener,
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(server_config)),
            client: HttpClient::with_tls_config(Limits::default(), client_config)?,
            origin: Origin::parse(&format!("https://{address}"))?,
        })
    }

    fn loopback_request(origin: Origin) -> Result<Request, Box<dyn std::error::Error>> {
        Ok(Request::new(
            origin,
            Method::POST,
            "/token",
            plakous_http::HeaderMap::new(),
            plakous_http::Bytes::new(),
            &Limits::default(),
        )?)
    }

    fn spawn_loopback_ok(
        listener: tokio::net::TcpListener,
        acceptor: tokio_rustls::TlsAcceptor,
    ) -> tokio::task::JoinHandle<Result<(), String>> {
        spawn_loopback_response(
            listener,
            acceptor,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
        )
    }

    fn spawn_loopback_response(
        listener: tokio::net::TcpListener,
        acceptor: tokio_rustls::TlsAcceptor,
        response: &'static [u8],
    ) -> tokio::task::JoinHandle<Result<(), String>> {
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
            let mut stream = acceptor
                .accept(stream)
                .await
                .map_err(|error| error.to_string())?;
            let mut buffer = [0_u8; 1024];
            loop {
                let read = stream
                    .read(&mut buffer)
                    .await
                    .map_err(|error| error.to_string())?;
                if read == 0
                    || buffer[..read]
                        .windows(4)
                        .any(|window| window == b"\r\n\r\n")
                {
                    break;
                }
            }
            stream
                .write_all(response)
                .await
                .map_err(|error| error.to_string())?;
            stream.shutdown().await.map_err(|error| error.to_string())?;
            Ok(())
        })
    }
}
