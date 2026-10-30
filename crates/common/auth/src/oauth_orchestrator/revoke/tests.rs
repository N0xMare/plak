use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use plakous_http::{Client as HttpClient, ExchangeControl, Limits, Method, Origin, Request};
use tempfile::TempDir;

use crate::oauth_orchestrator::hooks::{
    BuildRequestFailure, ClaimedRevokeHooks, CredentialRefreshEvent, CredentialRefreshFailureClass,
    CredentialRefreshObserver, CredentialRefreshPhase,
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

struct ScriptedRevokeHooks<'a> {
    observer: &'a RecordingObserver,
    fence: CredentialFence,
    authorize: Result<(), &'static str>,
    build_failure: Option<bool>,
    origin: Option<Origin>,
    accept: Result<(), &'static str>,
}

impl ClaimedRevokeHooks for ScriptedRevokeHooks<'_> {
    type Error = &'static str;
    type Output = ();

    fn check_profile(&mut self) -> impl Future<Output = Result<(), Self::Error>> {
        std::future::ready(Ok(()))
    }

    fn fence_to_revoke(&mut self) -> impl Future<Output = Result<CredentialFence, Self::Error>> {
        std::future::ready(Ok(self.fence.clone()))
    }

    fn build_revoke_request(
        &mut self,
        _claim: &RefreshClaim,
    ) -> Result<Request, BuildRequestFailure<Self::Error>> {
        if let Some(restore) = self.build_failure {
            return Err(if restore {
                BuildRequestFailure::Restore("encode")
            } else {
                BuildRequestFailure::LeaveQuarantined("corrupt")
            });
        }
        let origin = self
            .origin
            .clone()
            .unwrap_or_else(|| Origin::parse("https://127.0.0.1:1").expect("origin"));
        Ok(Request::new(
            origin,
            Method::POST,
            "/revoke",
            plakous_http::HeaderMap::new(),
            plakous_http::Bytes::new(),
            &Limits::default(),
        )
        .expect("request"))
    }

    fn authorize_dispatch(&mut self) -> impl Future<Output = Result<(), Self::Error>> {
        std::future::ready(self.authorize)
    }

    fn exchange_control(&mut self) -> ExchangeControl {
        ExchangeControl::new(Instant::now() + Duration::from_millis(50))
    }

    fn accept_revoke_response(
        &mut self,
        _response: &mut ClaimedHttpResponse,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        std::future::ready(self.accept)
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

    fn output_from_revoked(
        &mut self,
        _generation: CredentialGeneration,
    ) -> Result<Self::Output, Self::Error> {
        Ok(())
    }

    fn observer(&self) -> Option<&dyn CredentialRefreshObserver> {
        Some(self.observer)
    }
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

#[tokio::test]
async fn authorize_dispatch_failure_emits_restored() -> Result<(), Box<dyn std::error::Error>> {
    let (_dir, store, fence) = fixture_store()?;
    let observer = RecordingObserver::new();
    let mut hooks = ScriptedRevokeHooks {
        observer: &observer,
        fence,
        authorize: Err("denied"),
        build_failure: None,
        origin: None,
        accept: Ok(()),
    };
    let error = run_claimed_revoke(
        &store,
        &HttpClient::new_http1(Limits::default())?,
        &mut hooks,
    )
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
async fn revoke_build_failure_reopens_with_the_observed_durable_disposition()
-> Result<(), Box<dyn std::error::Error>> {
    for restore in [true, false] {
        let (directory, store, fence) = fixture_store()?;
        let observer = RecordingObserver::new();
        let mut hooks = ScriptedRevokeHooks {
            observer: &observer,
            fence: fence.clone(),
            authorize: Ok(()),
            build_failure: Some(restore),
            origin: None,
            accept: Ok(()),
        };
        let result = run_claimed_revoke(
            &store,
            &HttpClient::new_http1(Limits::default())?,
            &mut hooks,
        )
        .await;
        assert_eq!(result, Err(if restore { "encode" } else { "corrupt" }));
        drop(store);
        let reopened = PrivateCredentialStore::open_slot(
            directory.path().join("credential.json"),
            SubscriptionIntegrationProfileId::new("profile")?,
            CredentialSlotId::new("primary")?,
        )?;
        let current = reopened.load()?.ok_or("missing credential")?;
        if restore {
            assert_eq!(current.status(), fence.status());
            assert_eq!(current.generation().get(), fence.generation().get() + 2);
        } else {
            assert_eq!(current.status(), CredentialStatus::RefreshOutcomeUnknown);
            assert_eq!(current.generation().get(), fence.generation().get() + 1);
        }
        assert_eq!(
            observer.phases(),
            [
                CredentialRefreshPhase::Began,
                if restore {
                    CredentialRefreshPhase::Restored
                } else {
                    CredentialRefreshPhase::Quarantined
                }
            ]
        );
    }
    Ok(())
}

#[tokio::test]
async fn revoke_accept_failure_reopens_quarantined() -> Result<(), Box<dyn std::error::Error>> {
    let (directory, store, fence) = fixture_store()?;
    let endpoint = loopback_endpoint().await?;
    let server = spawn_loopback_response(
        endpoint.listener,
        endpoint.acceptor,
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
    );
    let observer = RecordingObserver::new();
    let mut hooks = ScriptedRevokeHooks {
        observer: &observer,
        fence: fence.clone(),
        authorize: Ok(()),
        build_failure: None,
        origin: Some(endpoint.origin),
        accept: Err("rejected"),
    };
    let error = run_claimed_revoke(&store, &endpoint.client, &mut hooks)
        .await
        .err()
        .ok_or("accept failure was committed")?;
    assert_eq!(error, "rejected");
    server.await??;
    drop(store);
    let reopened = PrivateCredentialStore::open_slot(
        directory.path().join("credential.json"),
        SubscriptionIntegrationProfileId::new("profile")?,
        CredentialSlotId::new("primary")?,
    )?;
    let current = reopened.load()?.ok_or("missing credential")?;
    assert_eq!(current.status(), CredentialStatus::RefreshOutcomeUnknown);
    assert_eq!(current.generation().get(), fence.generation().get() + 1);
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
