//! The Codex search side request over loopback TLS: path, headers, permit
//! and authorization order, and refusal before the socket.

use plakous_attempt::ReplaySafety;
use plakous_auth::subscription_limit::SubscriptionRequestClass;
use plakous_responses::{ResponsesRetrievalErrorKind, ResponsesRetrievalQuery};

use super::*;

const MODEL: &str = "gpt-5.6-luna";

fn search(query: &str) -> ResponsesRetrievalQuery {
    ResponsesRetrievalQuery::WebSearch {
        query: query.to_owned(),
        allowed_domains: Vec::new(),
        blocked_domains: Vec::new(),
    }
}

fn split(request: &[u8]) -> TestResult<(String, serde_json::Value)> {
    let head_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("missing head")?
        + 4;
    Ok((
        std::str::from_utf8(&request[..head_end])?.to_owned(),
        serde_json::from_slice(&request[head_end..])?,
    ))
}

#[tokio::test]
async fn codex_search_posts_the_search_body_under_the_codex_costume_and_one_permit() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_private, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "account-search",
        "subject-search",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(1, 4)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let server = spawn_sequence_then_probe(
        endpoint,
        vec![
            response("200 OK", "application/json", &catalog_body(&[MODEL])?, ""),
            response(
                "200 OK",
                "application/json",
                br#"{"output":"Rust 1.99 shipped.","encrypted_output":"gAAA","results":[{"url":"https://blog.rust-lang.org/a","title":"Rust 1.99"}]}"#,
                "",
            ),
        ],
    );
    let resolver =
        CodexModelResolver::new_for_test(manager, client, StandardOpenAiHttpConfig::default())?;
    resolver.refresh_catalog().await?;
    let (_, _, retrieval, _) = resolver.resolve_with_retrieval(MODEL)?;
    let outcome = retrieval
        .retrieve(
            search("rust 1.99"),
            &plakous_model::CancellationToken::new(),
        )
        .await?;
    assert_eq!(outcome.text, "Rust 1.99 shipped.");
    assert_eq!(outcome.sources.len(), 1);
    assert_eq!(outcome.usage.hosted_requests, 1);

    let (requests, unexpected) = server.await??;
    assert!(!unexpected, "the side request opened a second connection");
    assert_eq!(requests.len(), 2);
    let (head, body) = split(&requests[1])?;
    assert!(
        head.starts_with("POST /backend-api/codex/alpha/search HTTP/1.1\r\n"),
        "{head}"
    );
    assert!(head.contains("authorization: Bearer opaque-seeded-access\r\n"));
    assert!(head.contains("chatgpt-account-id: account-search\r\n"));
    assert!(head.contains("originator: plak\r\n"));
    assert!(head.contains(&format!("user-agent: {USER_AGENT}\r\n")));
    assert!(head.contains("accept: application/json\r\n"));
    assert!(!head.contains("x-openai-internal-codex-responses-lite"));
    assert_eq!(body["model"], MODEL);
    assert_eq!(
        body["commands"],
        serde_json::json!({"search_query": [{"q": "rust 1.99"}]})
    );
    assert!(body["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert_eq!(
        crate::classify_codex_search_body(&serde_json::to_vec(&body)?),
        ReplaySafety::CostOnly
    );
    let inference = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 1);
    assert_eq!(inference.dispatched(), 1);
    Ok(())
}

#[tokio::test]
async fn a_changed_login_refuses_the_codex_search_before_the_socket() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_private, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "account-search",
        "subject-search",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(2, 4)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let server = spawn_sequence_then_probe(
        endpoint,
        vec![response(
            "200 OK",
            "application/json",
            &catalog_body(&[MODEL])?,
            "",
        )],
    );
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        StandardOpenAiHttpConfig::default(),
    )?;
    resolver.refresh_catalog().await?;
    let (_, _, retrieval, _) = resolver.resolve_with_retrieval(MODEL)?;
    manager.install_selected_account(AccountBinding::new("changed-after-catalog")?)?;
    let error = retrieval
        .retrieve(
            search("rust 1.99"),
            &plakous_model::CancellationToken::new(),
        )
        .await
        .err()
        .ok_or("a changed login searched")?;
    assert_eq!(error.kind(), ResponsesRetrievalErrorKind::Unauthorized);
    assert_eq!(error.physical_requests(), 1);
    let (requests, unexpected) = server.await??;
    assert!(!unexpected, "the refused side request reached the socket");
    assert_eq!(requests.len(), 1);
    let inference = gate.snapshot()?.class(SubscriptionRequestClass::Inference);
    assert_eq!(inference.reserved(), 0);
    Ok(())
}

#[test]
fn codex_search_refuses_post_search_and_excluded_domains() {
    for query in [
        ResponsesRetrievalQuery::PostSearch {
            query: "rust".to_owned(),
            handles: Vec::new(),
            from_date: None,
            to_date: None,
        },
        ResponsesRetrievalQuery::WebSearch {
            query: "rust".to_owned(),
            allowed_domains: Vec::new(),
            blocked_domains: vec!["example.com".to_owned()],
        },
    ] {
        assert_eq!(
            crate::codex_search::refuse_unexpressible(&query).map(|error| error.kind()),
            Some(ResponsesRetrievalErrorKind::InvalidRequest)
        );
    }
    assert!(crate::codex_search::refuse_unexpressible(&search("rust")).is_none());
    let allowed = ResponsesRetrievalQuery::WebSearch {
        query: "rust".to_owned(),
        allowed_domains: vec!["rust-lang.org".to_owned()],
        blocked_domains: Vec::new(),
    };
    assert!(crate::codex_search::refuse_unexpressible(&allowed).is_none());
}

const WRAPPED_OVERLOAD_BODY: &[u8] = br#"{"error":{"message":"authentication failed (status=503): {\"error\":\"Too many concurrent requests\"}","type":"authentication_error","param":null,"code":null}}"#;

fn wrapped_then(success: &'static [u8]) -> TestResult<Vec<Vec<u8>>> {
    Ok(vec![
        response("200 OK", "application/json", &catalog_body(&[MODEL])?, ""),
        response(
            "401 Unauthorized",
            "application/json",
            WRAPPED_OVERLOAD_BODY,
            "Retry-After: 0\r\n",
        ),
        response("200 OK", "application/json", success, ""),
    ])
}

fn side_paths(requests: &[Vec<u8>]) -> Vec<String> {
    requests
        .iter()
        .map(|request| {
            let line = request
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default();
            String::from_utf8_lossy(line).trim().to_owned()
        })
        .collect()
}

/// The real Codex search transport opts into the wrapped-503 retry. Returning
/// false from `retry_wrapped_overload` sends the 401 to the token endpoint.
#[tokio::test]
async fn a_wrapped_overload_401_on_codex_search_is_retried_without_a_refresh() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_private, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "account-search-overload",
        "subject-search-overload",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(4, 4)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let server = spawn_sequence_then_probe(
        endpoint,
        wrapped_then(
            br#"{"output":"Rust 1.99 shipped.","results":[{"url":"https://blog.rust-lang.org/a","title":"Rust 1.99"}]}"#,
        )?,
    );
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        StandardOpenAiHttpConfig::default(),
    )?;
    resolver.refresh_catalog().await?;
    let generation = manager.load_fence().await?.generation();
    let (_, _, retrieval, _) = resolver.resolve_with_retrieval(MODEL)?;
    let outcome = retrieval
        .retrieve(
            search("rust 1.99"),
            &plakous_model::CancellationToken::new(),
        )
        .await?;
    assert_eq!(outcome.text, "Rust 1.99 shipped.");
    let (requests, unexpected) = server.await??;
    assert!(
        !unexpected,
        "the overload opened a connection past the retry"
    );
    let paths = side_paths(&requests);
    assert_eq!(paths.len(), 3, "{paths:?}");
    assert!(paths[0].contains(OPENAI_CODEX_MODELS_PATH), "{paths:?}");
    assert!(
        paths[1].starts_with("POST /backend-api/codex/alpha/search "),
        "{paths:?}"
    );
    assert!(
        paths[2].starts_with("POST /backend-api/codex/alpha/search "),
        "{paths:?}"
    );
    assert!(
        paths.iter().all(|path| !path.contains(TOKEN_PATH)),
        "token endpoint was called: {paths:?}"
    );
    assert_eq!(manager.load_fence().await?.generation(), generation);
    assert_eq!(manager.status()?, CodexCredentialStatus::Ready);
    assert_eq!(
        gate.snapshot()?
            .class(SubscriptionRequestClass::Inference)
            .dispatched(),
        2
    );
    Ok(())
}

/// The real Codex image transport opts into the same retry.
#[tokio::test]
async fn a_wrapped_overload_401_on_codex_images_is_retried_without_a_refresh() -> TestResult {
    let endpoint = test_endpoint().await?;
    let client = endpoint.client.clone();
    let (_private, slot) = private_slot()?;
    seed_ready_credential(
        &slot,
        "account-image-overload",
        "subject-image-overload",
        now_unix_secs() + 3_600,
    )?;
    let gate = subscription_gate(4, 4)?;
    let manager = manager_for_endpoint_with_gate(&slot, &endpoint, gate.clone())?;
    let server = spawn_sequence_then_probe(
        endpoint,
        wrapped_then(br#"{"data":[{"b64_json":"YWI="}]}"#)?,
    );
    let resolver = CodexModelResolver::new_for_test(
        Arc::clone(&manager),
        client,
        StandardOpenAiHttpConfig::default(),
    )?;
    resolver.refresh_catalog().await?;
    let generation = manager.load_fence().await?.generation();
    let (_, _, _, images) = resolver.resolve_with_retrieval(MODEL)?;
    let request = plakous_responses::ResponsesImageRequest::new(
        crate::CODEX_IMAGE_MODEL,
        plakous_responses::ResponsesImageFlavor::Codex,
        "a red fox",
    );
    let image = images
        .generate(&request, &plakous_model::CancellationToken::new())
        .await?;
    assert_eq!(image, b"ab");
    let (requests, unexpected) = server.await??;
    assert!(
        !unexpected,
        "the overload opened a connection past the retry"
    );
    let paths = side_paths(&requests);
    assert_eq!(paths.len(), 3, "{paths:?}");
    assert!(paths[0].contains(OPENAI_CODEX_MODELS_PATH), "{paths:?}");
    assert!(
        paths[1].starts_with(&format!("POST {} ", crate::CODEX_IMAGE_GENERATIONS_PATH)),
        "{paths:?}"
    );
    assert!(
        paths[2].starts_with(&format!("POST {} ", crate::CODEX_IMAGE_GENERATIONS_PATH)),
        "{paths:?}"
    );
    assert!(
        paths.iter().all(|path| !path.contains(TOKEN_PATH)),
        "token endpoint was called: {paths:?}"
    );
    assert_eq!(manager.load_fence().await?.generation(), generation);
    assert_eq!(manager.status()?, CodexCredentialStatus::Ready);
    assert_eq!(
        gate.snapshot()?
            .class(SubscriptionRequestClass::Inference)
            .dispatched(),
        2
    );
    Ok(())
}
