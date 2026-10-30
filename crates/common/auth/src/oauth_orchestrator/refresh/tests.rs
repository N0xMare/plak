use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use plakous_http::{Client as HttpClient, ExchangeControl, Limits, Method, Origin, Request};
use tempfile::TempDir;

use crate::oauth_orchestrator::hooks::{
    BuildRequestFailure, ClaimedRefreshHooks, CredentialRefreshDetail, CredentialRefreshEvent,
    CredentialRefreshFailureClass, CredentialRefreshFailureReason, CredentialRefreshObserver,
    CredentialRefreshPhase,
};
use crate::{
    AccountBinding, ClaimedHttpError, ClaimedHttpResponse, CredentialFence, CredentialGeneration,
    CredentialRecord, CredentialSlotId, CredentialStatus, PrivateCredentialStore,
    PrivateStoreError, RefreshClaim, SecretBytes, SubscriptionIntegrationProfileId,
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
            BuildMode::LeaveQuarantined => Err(BuildRequestFailure::LeaveQuarantined("corrupt")),
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

    fn failure_detail(&mut self, error: &Self::Error) -> CredentialRefreshDetail {
        match *error {
            "corrupt" => CredentialRefreshDetail {
                reason: Some(CredentialRefreshFailureReason::StoredCredential),
                ..CredentialRefreshDetail::default()
            },
            "validate" => CredentialRefreshDetail {
                reason: Some(CredentialRefreshFailureReason::IdTokenExp),
                expires_in_secs: Some(21_600),
                id_token_iat_age_secs: Some(3_700),
                id_token_exp_in_secs: Some(-100),
                ..Default::default()
            },
            _ => CredentialRefreshDetail::default(),
        }
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
    assert_eq!(observer.phases(), [] as [CredentialRefreshPhase; 0]);
    Ok(())
}

#[tokio::test]
async fn leave_quarantined_emits_began_then_quarantined() -> Result<(), Box<dyn std::error::Error>>
{
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
    assert_eq!(events[0].detail, CredentialRefreshDetail::default());
    assert_eq!(
        events[1].detail.reason,
        Some(CredentialRefreshFailureReason::StoredCredential)
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
async fn completed_refresh_emits_began_then_completed() -> Result<(), Box<dyn std::error::Error>> {
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
    assert_eq!(
        events[1].detail,
        CredentialRefreshDetail {
            reason: Some(CredentialRefreshFailureReason::IdTokenExp),
            expires_in_secs: Some(21_600),
            id_token_iat_age_secs: Some(3_700),
            id_token_exp_in_secs: Some(-100),
            ..Default::default()
        }
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
