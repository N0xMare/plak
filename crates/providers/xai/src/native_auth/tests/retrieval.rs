//! SuperGrok side requests over loopback TLS: path, OAuth costume, permit
//! and authorization order, and refusal before the socket.

use plakous_responses::{ResponsesRetrievalErrorKind, ResponsesRetrievalQuery};

use super::*;

fn x_search() -> ResponsesRetrievalQuery {
    ResponsesRetrievalQuery::PostSearch {
        query: "Rust programming language release".to_owned(),
        handles: vec!["rustlang".to_owned()],
        from_date: None,
        to_date: None,
    }
}

fn x_search_sse() -> String {
    let completed = json!({
        "type": "response.completed",
        "response": {
            "id": "resp_search",
            "status": "completed",
            "model": XAI_GROK_PRIMARY_CODING_MODEL,
            "output": [
                {"type": "x_search_call", "id": "xs_1", "status": "completed",
                 "action": {"type": "search", "sources": [
                     {"type": "url", "url": "https://x.com/rustlang/status/1"}]}},
                {"type": "message", "role": "assistant", "status": "completed", "content": [
                    {"type": "output_text", "text": "Rust 1.99 is out.", "annotations": []}]},
            ],
            "usage": {"input_tokens": 40, "output_tokens": 8, "total_tokens": 48},
        },
    });
    format!("event: response.completed\ndata: {completed}\n\n")
}

/// Records each inference permit's acquisition and dispatch authorization.
struct Recording {
    inner: Arc<InMemorySubscriptionRequestGate>,
    events: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

struct RecordingPermit {
    inner: Box<dyn SubscriptionRequestPermit>,
    events: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

fn record(events: &std::sync::Mutex<Vec<&'static str>>, event: &'static str) {
    if let Ok(mut events) = events.lock() {
        events.push(event);
    }
}

impl SubscriptionRequestGate for Recording {
    fn acquire(
        &self,
        class: SubscriptionRequestClass,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError> {
        self.inner.acquire(class)
    }

    fn acquire_inference(
        &self,
        input_token_upper_bound: u64,
    ) -> Result<Box<dyn SubscriptionRequestPermit>, SubscriptionLimitError> {
        record(&self.events, "acquire");
        Ok(Box::new(RecordingPermit {
            inner: self.inner.acquire_inference(input_token_upper_bound)?,
            events: Arc::clone(&self.events),
        }))
    }

    fn snapshot(&self) -> Result<SubscriptionLimitSnapshot, SubscriptionLimitError> {
        self.inner.snapshot()
    }
}

impl SubscriptionRequestPermit for RecordingPermit {
    fn refund_not_started(&mut self) -> Result<(), SubscriptionLimitError> {
        self.inner.refund_not_started()
    }

    fn class(&self) -> SubscriptionRequestClass {
        self.inner.class()
    }

    fn authorize_dispatch(&mut self) -> Result<(), SubscriptionLimitError> {
        record(&self.events, "authorize");
        self.inner.authorize_dispatch()
    }
}

struct Session {
    _temp: TempDir,
    auth: MockPeer,
    inference: MockPeer,
    manager: XaiNativeAuthManager,
    model: Arc<XaiLanguageModel>,
    gate: Arc<InMemorySubscriptionRequestGate>,
    events: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

async fn session() -> TestResult<Session> {
    session_with(true).await
}

/// A session whose model either carries the fixture context contract or the
/// release one, which a renewal after a claimed refresh can reproduce.
async fn session_with(fixture_contract: bool) -> TestResult<Session> {
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "search-subject",
        "search-access",
        "search-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    admit_catalog(&manager, &auth).await?;
    let inference = mock_peer().await?;
    let route =
        manager.test_subscription_route(crate::XaiOrigin::new(inference.origin.clone())?)?;
    let resolved = XaiResolvedModel::new(
        route,
        XaiModel::new(ModelId::new(XAI_GROK_PRIMARY_CODING_MODEL)?, None),
        GrokCodingProfile::pinned_compatibility(),
        now_timestamp()?,
    )?;
    let gate = subscription_gate(4)?;
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recording = Arc::new(Recording {
        inner: Arc::clone(&gate),
        events: Arc::clone(&events),
    });
    let model = if fixture_contract {
        subscription_model_with_gate(resolved, inference.client.clone(), recording)?
    } else {
        Arc::new(
            XaiLanguageModel::new(resolved, inference.client.clone())?
                .with_subscription_request_gate(recording)?,
        )
    };
    Ok(Session {
        _temp: temp,
        auth,
        inference,
        manager,
        model,
        gate,
        events,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supergrok_x_search_carries_the_oauth_costume_under_one_authorized_permit() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let session = session().await?;
    session
        .inference
        .responses
        .send(raw_response("200 OK", "text/event-stream", x_search_sse()))
        .await?;
    let outcome = session
        .model
        .retrieval()
        .retrieve(x_search(), &plakous_model::CancellationToken::new())
        .await?;
    assert_eq!(outcome.text, "Rust 1.99 is out.");
    assert_eq!(outcome.sources.len(), 1);
    assert_eq!(outcome.usage.hosted_requests, 1);
    assert_eq!(
        *session.events.lock().map_err(|_| "poisoned")?,
        ["acquire", "authorize"]
    );
    let inference = session
        .gate
        .snapshot()?
        .class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.dispatched(), 1);

    let requests = session.inference.requests.lock().await.clone();
    assert_eq!(requests.len(), 1);
    let head_end = requests[0]
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("missing head")?;
    let head = String::from_utf8_lossy(&requests[0][..head_end]).to_ascii_lowercase();
    assert!(
        head.starts_with(&format!(
            "post {} http/1.1\r\n",
            crate::SUPERGROK_RESPONSES_PATH
        )),
        "{head}"
    );
    for header in [
        "authorization: bearer search-access".to_owned(),
        "accept: text/event-stream".to_owned(),
        format!("x-xai-token-auth: {XAI_TOKEN_AUTH_VALUE}"),
        format!("x-authenticateresponse: {AUTHENTICATE_RESPONSE_VALUE}"),
        format!(
            "x-grok-client-version: {}",
            SUPERGROK_CLIENT_COMPATIBILITY_VERSION.to_ascii_lowercase()
        ),
        format!("user-agent: {}", USER_AGENT.to_ascii_lowercase()),
    ] {
        assert!(head.contains(&header), "{header} missing from {head}");
    }
    let body: serde_json::Value = serde_json::from_slice(&requests[0][head_end + 4..])?;
    assert_eq!(body["model"], XAI_GROK_PRIMARY_CODING_MODEL);
    assert_eq!(
        body["tools"],
        json!([{"type": "x_search", "allowed_x_handles": ["rustlang"]}])
    );
    assert_eq!(body["store"], false);
    assert_eq!(body.get("include"), None);
    session.auth.shutdown().await?;
    session.inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_logged_out_supergrok_login_refuses_the_side_request_before_the_socket() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let session = session().await?;
    session.manager.logout()?;
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        session
            .model
            .retrieval()
            .retrieve(x_search(), &plakous_model::CancellationToken::new()),
    )
    .await?
    .err()
    .ok_or("a logged-out login searched")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Unauthorized);
    assert_eq!(error.physical_requests(), 1);
    assert!(session.events.lock().map_err(|_| "poisoned")?.is_empty());
    assert!(session.inference.requests.lock().await.is_empty());
    session.auth.abort_join().await?;
    session.inference.abort_join().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supergrok_side_request_version_gate_is_final_with_the_newer_build_remedy() -> TestResult
{
    let _serial = AUTH_TEST_GATE.lock().await;
    let session = session().await?;
    let gate = r#"{"error":"Your Grok CLI version (0.2.93) is outdated. Please update to version 1.0.13 or later via `grok update` or the installation documentation."}"#;
    for _ in 0..2 {
        session
            .inference
            .responses
            .send(raw_response(
                "426 Upgrade Required",
                "application/json",
                gate,
            ))
            .await?;
    }
    let search = session
        .model
        .retrieval()
        .retrieve(x_search(), &plakous_model::CancellationToken::new())
        .await
        .err()
        .ok_or("a version-gated search succeeded")?;
    let images = session.model.images();
    let image = images
        .generate(
            &images.request("a red square"),
            &plakous_model::CancellationToken::new(),
        )
        .await
        .err()
        .ok_or("a version-gated image succeeded")?;
    for error in [search, image] {
        assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Rejected);
        assert_eq!(error.status(), Some(426));
        assert_eq!(error.physical_requests(), 1);
        assert_eq!(
            error.message(),
            plakous_auth::CompatibilityDrift::VersionGate.message("Grok")
        );
        assert!(!error.message().contains("grok update"));
        assert!(!error.message().contains("1.0.13"));
    }
    let requests = session.inference.requests.lock().await.clone();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let head = String::from_utf8_lossy(request).to_ascii_lowercase();
        assert!(head.contains(&format!(
            "x-grok-client-version: {}",
            SUPERGROK_CLIENT_COMPATIBILITY_VERSION.to_ascii_lowercase()
        )));
    }
    session.auth.shutdown().await?;
    session.inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unrelated_supergrok_426_keeps_the_generic_side_request_detail() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let session = session().await?;
    session
        .inference
        .responses
        .send(raw_response(
            "426 Upgrade Required",
            "application/json",
            r#"{"error":"unrelated refusal"}"#,
        ))
        .await?;
    let error = session
        .model
        .retrieval()
        .retrieve(x_search(), &plakous_model::CancellationToken::new())
        .await
        .err()
        .ok_or("an unrelated 426 succeeded")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Rejected);
    assert_eq!(error.status(), Some(426));
    assert_eq!(
        error.message(),
        "the provider refused the side request (HTTP 426, error: unrelated refusal)"
    );
    assert_eq!(session.inference.requests.lock().await.len(), 1);
    session.auth.shutdown().await?;
    session.inference.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supergrok_side_request_401_takes_one_claimed_refresh_and_is_reissued() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let session = session_with(false).await?;
    // Even the proven version-gate sentence on a 401 stays a credential refusal.
    let gate = r#"{"error":"Your Grok CLI version (0.2.93) is outdated. Please update to version 1.0.13 or later via `grok update` or the installation documentation."}"#;
    session
        .inference
        .responses
        .send(raw_response("401 Unauthorized", "application/json", gate))
        .await?;
    session
        .inference
        .responses
        .send(raw_response("200 OK", "text/event-stream", x_search_sse()))
        .await?;
    session
        .auth
        .responses
        .send(response(&token_response(
            "search-access-two",
            "search-refresh-two",
            &id_token("search-subject", None)?,
        )))
        .await?;
    session
        .auth
        .responses
        .send(response(&jwks(TEST_KID)))
        .await?;
    enqueue_admitted_catalog(&session.auth).await?;
    let outcome = session
        .model
        .retrieval()
        .retrieve(x_search(), &plakous_model::CancellationToken::new())
        .await?;
    assert_eq!(outcome.text, "Rust 1.99 is out.");
    assert_eq!(token_post_count(&session.auth.requests.lock().await), 1);
    let requests = session.inference.requests.lock().await.clone();
    assert_eq!(requests.len(), 2);
    assert!(
        String::from_utf8_lossy(&requests[1])
            .to_ascii_lowercase()
            .contains("authorization: bearer search-access-two")
    );
    session.auth.abort_join().await?;
    session.inference.shutdown().await?;
    Ok(())
}
