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
        let origin = Origin::parse("https://127.0.0.1:1").expect("origin");
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
        std::future::ready(Ok(()))
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
