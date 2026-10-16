#![allow(clippy::expect_used, reason = "fixed literal fixtures")]

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::Duration;

use plakous_attempt::Jitter;
use plakous_http::Limits;
use plakous_model::{ModelId, Timestamp};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::{
    OpenRouterApiKey, OpenRouterChatEndpointCandidate, OpenRouterOrigin, OpenRouterToolChoiceModes,
    StaticOpenRouterApiKeySource,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn route(retention: OpenRouterRetentionPolicy) -> TestResult<OpenRouterChatRoute> {
    Ok(OpenRouterChatRoute::new_local_fixture_with_retention(
        Arc::new(StaticOpenRouterApiKeySource::new(
            OpenRouterApiKey::new("offline-retrieval-key")?,
            "offline-account",
            "offline-authority",
        )?),
        OpenRouterOrigin::new("https://127.0.0.1:18443")?,
        retention,
    )?
    .with_plak_attribution()?)
}

fn facts(efforts: &[&str], default_enabled: bool, mandatory: bool) -> OpenRouterReasoningFacts {
    OpenRouterReasoningFacts {
        supported_efforts: Some(efforts.iter().map(|effort| (*effort).to_owned()).collect()),
        default_effort: None,
        default_enabled,
        supports_max_tokens: false,
        mandatory,
    }
}

fn evidence(
    route: &OpenRouterChatRoute,
    reasoning: Option<OpenRouterReasoningFacts>,
) -> TestResult<OpenRouterChatModelEvidence> {
    let parameters = BTreeSet::from(["max_tokens", "reasoning"].map(str::to_owned));
    let now = SystemClock.now();
    Ok(OpenRouterChatModelEvidence::from_authenticated_discovery(
        ModelId::new("vendor/model")?,
        route.identity(),
        vec![OpenRouterChatEndpointCandidate {
            endpoint: "provider/first".to_owned(),
            provider_name: "First".to_owned(),
            supported_parameters: parameters.clone(),
            context_length: Some(131_072),
            max_prompt_tokens: Some(100_000),
            max_completion_tokens: Some(65_536),
            tool_choice: OpenRouterToolChoiceModes::default(),
            supports_implicit_caching: false,
        }],
        parameters,
        reasoning,
        131_072,
        false,
        now,
        Timestamp::new(now.seconds() + 3_600, 0)?,
    )?)
}

fn retrieval(reasoning: Option<OpenRouterReasoningFacts>) -> TestResult<OpenRouterRetrieval> {
    let route = route(OpenRouterRetentionPolicy::strict())?;
    let evidence = evidence(&route, reasoning)?;
    Ok(OpenRouterRetrieval::new(
        route,
        evidence,
        Client::new_http1(Limits::default())?,
    )?)
}

#[test]
fn lowest_reasoning_follows_the_model_facts() {
    assert_eq!(lowest_reasoning(None), ChatRetrievalReasoning::Omit);
    assert_eq!(
        lowest_reasoning(Some(&facts(&["low"], false, false))),
        ChatRetrievalReasoning::Omit
    );
    assert_eq!(
        lowest_reasoning(Some(&facts(&["low"], true, false))),
        ChatRetrievalReasoning::Disabled
    );
    assert_eq!(
        lowest_reasoning(Some(&facts(&["high", "minimal", "medium"], true, true))),
        ChatRetrievalReasoning::Lowest("minimal")
    );
    assert_eq!(
        lowest_reasoning(Some(&facts(&[], true, true))),
        ChatRetrievalReasoning::Omit
    );
}

/// Accepts one request, records its head and body, and answers with `response`.
async fn peer(response: String) -> TestResult<(Origin, Arc<Mutex<Option<String>>>)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let origin = Origin::loopback_http(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        listener.local_addr()?.port(),
    )?;
    let recorded = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&recorded);
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let Ok(read) = stream.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&buffer).into_owned();
            let Some(end) = text.find("\r\n\r\n") else {
                continue;
            };
            let length = text[..end]
                .to_ascii_lowercase()
                .lines()
                .find_map(|line| line.strip_prefix("content-length:").map(str::to_owned))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buffer.len() >= end + 4 + length {
                if let Ok(mut slot) = sink.lock() {
                    *slot = Some(text);
                }
                break;
            }
        }
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    });
    Ok((origin, recorded))
}

fn answered() -> String {
    let body = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Rust 1.99.\",",
        "\"annotations\":[{\"type\":\"url_citation\",\"url_citation\":{\"url\":\"https://blog.rust-lang.org/\",",
        "\"title\":\"Rust Blog\",\"content\":\"snippet\"}}]},\"finish_reason\":\"stop\"}],",
        "\"usage\":{\"prompt_tokens\":40,\"completion_tokens\":5,\"total_tokens\":45}}\n\n",
        "data: [DONE]\n\n",
    );
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn a_side_request_carries_the_route_key_and_attribution() -> TestResult {
    let (origin, recorded) = peer(answered()).await?;
    let retrieval = retrieval(None)?
        .with_dial_origin(origin)
        .with_limits(ChatRetrievalLimits {
            exchange_timeout: Duration::from_secs(10),
            jitter: Some(Jitter::SHORTEST),
        });
    let request = retrieval.request("rust", Vec::new(), Vec::new());
    let outcome = retrieval
        .retrieve(&request, &CancellationToken::new())
        .await?;
    assert_eq!(outcome.text, "Rust 1.99.");
    assert_eq!(outcome.sources[0].url, "https://blog.rust-lang.org/");
    assert_eq!(outcome.usage.input_tokens, 40);
    let sent = recorded
        .lock()
        .map_err(|_| "poisoned")?
        .clone()
        .ok_or("no request")?;
    let head = sent.to_ascii_lowercase();
    assert!(head.starts_with("post /api/v1/chat/completions "));
    assert!(head.contains("authorization: bearer offline-retrieval-key\r\n"));
    assert!(head.contains("x-openrouter-title: plak\r\n"));
    assert!(sent.ends_with(&String::from_utf8(request.body()?)?));
    Ok(())
}

struct At(Timestamp);

impl Clock for At {
    fn now(&self) -> Timestamp {
        self.0
    }
}

#[tokio::test]
async fn admission_is_revalidated_before_every_physical_request() -> TestResult {
    let mut expired = retrieval(None)?;
    expired.clock = Arc::new(At(Timestamp::new(
        expired.evidence.expires_at().seconds(),
        0,
    )?));
    let request = expired.request("rust", Vec::new(), Vec::new());
    let error = expired
        .retrieve(&request, &CancellationToken::new())
        .await
        .err()
        .ok_or("stale evidence dispatched")?;
    assert_eq!(error.kind(), ChatRetrievalErrorKind::Unauthorized);

    let mut rotated = retrieval(None)?;
    rotated.generation = OpenRouterApiKeyGenerationGrant::from_generation_binding(b"rotated")?;
    let error = rotated
        .retrieve(&request, &CancellationToken::new())
        .await
        .err()
        .ok_or("a rotated credential dispatched")?;
    assert_eq!(error.kind(), ChatRetrievalErrorKind::Unauthorized);
    assert!(error.message().contains("credential changed"));
    Ok(())
}

/// Opt-in paid probe, never in a gate tier: one web search side request on
/// OpenRouter with the operator's key. A pass is a non-empty answer with at
/// least one source. Run with `PLAK_LIVE_RETRIEVAL_PROBE=1
/// OPENROUTER_API_KEY=... PLAK_LIVE_OPENROUTER_MODEL=<vendor/model> cargo
/// nextest run -p plakous-openrouter --run-ignored only -E
/// 'test(live_retrieval_probe)'`.
#[tokio::test]
#[ignore = "paid live probe"]
async fn live_retrieval_probe_openrouter() -> TestResult {
    if std::env::var("PLAK_LIVE_RETRIEVAL_PROBE").as_deref() != Ok("1") {
        return Err("set PLAK_LIVE_RETRIEVAL_PROBE=1 to spend one probe".into());
    }
    let key = std::env::var("OPENROUTER_API_KEY")?;
    let model = std::env::var("PLAK_LIVE_OPENROUTER_MODEL")?;
    let route = OpenRouterChatRoute::new(Arc::new(StaticOpenRouterApiKeySource::new(
        OpenRouterApiKey::new(key)?,
        "live-probe",
        "live-probe",
    )?))?
    .with_plak_attribution()?;
    let client = Client::new_http1(Limits::for_streaming_inference())?;
    let evidence = route
        .discover_chat_model(
            &client,
            &model,
            crate::OpenRouterChatDiscoveryConfig::default(),
        )
        .await?;
    let retrieval = OpenRouterRetrieval::new(route, evidence, client)?;
    let request = retrieval.request(
        "Rust programming language latest stable release",
        Vec::new(),
        Vec::new(),
    );
    let outcome = retrieval
        .retrieve(&request, &CancellationToken::new())
        .await?;
    eprintln!(
        "web_search: sources={} usage={:?}",
        outcome.sources.len(),
        outcome.usage
    );
    assert!(
        !outcome.text.trim().is_empty(),
        "web_search answered nothing"
    );
    assert!(!outcome.sources.is_empty(), "web_search reported no source");
    Ok(())
}
