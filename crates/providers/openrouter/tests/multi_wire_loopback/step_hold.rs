//! Host holds compose with real provider-owned retry and semantic watchdogs.
use super::*;
use plakous_agent::internal::{BoundaryPhase, BoundaryProgress, facade_support};

fn fixed_virtual_time() -> std::sync::mpsc::Sender<()> {
    let (keep, held) = std::sync::mpsc::channel();
    tokio::task::spawn_blocking(move || {
        let _ = held.recv();
    });
    keep
}

fn agent(
    model: plakous_model::ConfiguredLanguageModel,
    root: &std::path::Path,
) -> TestResult<plakous_agent::Agent> {
    Ok(AgentBuilder::new(
        model,
        ToolRegistryBuilder::default().freeze(),
        Arc::new(WorkspacePolicy::host(root, ".")?),
    )
    .config(
        plakous_agent::AgentConfig::default()
            .with_model_operation_timeout(Duration::from_secs(1200)),
    )
    .request_policy(
        AgentRequestPolicy::default()
            .with_generation_limits(GenerationLimits::new(NonZeroU32::new(64), Vec::new())?),
    )
    .identity_sources(
        Arc::new(review_clock()),
        Arc::new(SequenceIdGenerator::new(
            (100_u128..=400).map(Uuid::from_u128),
        )?),
    )
    .build()?)
}

async fn held_turn(agent: &plakous_agent::Agent) -> TestResult<plakous_agent::Turn> {
    let mut hold = facade_support::hold_step_boundary(agent)?;
    while !matches!(hold.progress()?, BoundaryProgress::Held { turn: None, .. }) {
        hold.changed().await?;
    }
    let turn = agent.prompt("hello").await?;
    while !matches!(
        hold.progress()?,
        BoundaryProgress::Held { turn: Some(_), .. }
    ) {
        hold.changed().await?;
    }
    tokio::time::advance(Duration::from_secs(600)).await;
    assert!(matches!(
        hold.progress()?,
        BoundaryProgress::Held { turn: Some(_), .. }
    ));
    hold.release().await?;
    Ok(turn)
}

#[tokio::test(start_paused = true)]
async fn boundary_hold_excludes_real_retry_after_wait_budget() -> TestResult {
    let _clock = fixed_virtual_time();
    let mut replies = status_replies();
    replies.push((
        "429 Too Many Requests",
        "application/json",
        "{\"error\":{\"message\":\"wait\",\"code\":429}}".into(),
        "retry-after: 60\r\nx-request-id: req-held-refusal\r\n",
    ));
    replies.push((
        "200 OK",
        "text/event-stream",
        chat_sse(),
        "x-request-id: req-held-retry\r\n",
    ));
    let (client, origin, peer) = loopback_with_status(b"held-retry-budget", replies).await?;
    let model = discovered_chat_model(client, origin).await?;
    let root = tempfile::tempdir()?;
    let agent = agent(model, root.path())?;
    let mut turn = held_turn(&agent).await?;
    let mut events = turn.take_events()?;
    let mut result = Box::pin(turn.result());
    loop {
        let event = events.recv().await?;
        if let Some(ModelEvent::Retry(retry)) = event.kind().model_event() {
            assert_eq!(retry.delay(), Duration::from_secs(60));
            let hold = facade_support::hold_step_boundary(&agent)?;
            assert!(matches!(
                hold.progress()?,
                BoundaryProgress::Draining {
                    phase: BoundaryPhase::Model,
                    ..
                }
            ));
            tokio::time::advance(Duration::from_secs(59)).await;
            assert!(
                std::future::poll_fn(|context| {
                    std::task::Poll::Ready(result.as_mut().poll(context))
                })
                .await
                .is_pending()
            );
            tokio::time::advance(Duration::from_secs(2)).await;
            drop(hold);
            break;
        }
    }
    let result = result.await?;
    assert!(
        result.error().is_none(),
        "{:?}",
        result.error().map(plakous_agent::AgentError::message)
    );
    assert_eq!(result.response().ok_or("response")?.output_text(), "chat");
    agent.shutdown().await?;
    assert_eq!(peer.await??.len(), 4);
    Ok(())
}

async fn responses_model(
    client: Client,
    origin: String,
) -> TestResult<plakous_model::ConfiguredLanguageModel> {
    let route = OpenRouterChatRoute::new_local_fixture_with_retention(
        Arc::new(plakous_openrouter::static_openrouter_api_key_source(
            ApiKey::new("offline-held-key")?,
            "offline-account",
            "offline-authority",
        )?),
        OpenRouterOrigin::new(origin)?,
        plakous_openrouter::OpenRouterRetentionPolicy::strict(),
    )?
    .with_clock(Arc::new(review_clock()));
    let evidence = route
        .discover_chat_model(
            &client,
            MODEL,
            OpenRouterChatDiscoveryConfig {
                operation_timeout: Duration::from_secs(5),
                maximum_body_bytes: 64 * 1024,
                evidence_ttl: Duration::from_secs(60),
            },
        )
        .await?;
    OpenRouterResponsesLanguageModel::configured_gateway_managed_behavior(
        route.clone(),
        evidence,
        context(
            &route,
            "responses-family",
            "responses-wire-bytes-openrouter-v1",
            "responses-canonical-openrouter-v1",
        )?,
        behavior(
            "openrouter.responses.dynamic-custom-v1",
            "openrouter-responses-integration-v1",
        )?,
        client,
    )
    .map_err(Into::into)
}

fn text_prefix() -> String {
    let events = [
        serde_json::json!({"type":"response.created","sequence_number":0,"response":{"id":"resp-held","object":"response","status":"in_progress","model":MODEL}}),
        serde_json::json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"msg-held","type":"message","role":"assistant","content":[]}}),
        serde_json::json!({"type":"response.content_part.added","sequence_number":2,"item_id":"msg-held","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        serde_json::json!({"type":"response.output_text.delta","sequence_number":3,"item_id":"msg-held","output_index":0,"content_index":0,"delta":"partial"}),
    ];
    let mut output = String::new();
    for event in events {
        output.push_str("data: ");
        output.push_str(&event.to_string());
        output.push_str("\n\n");
    }
    output
}

fn text_reply() -> String {
    let part = serde_json::json!({"type":"output_text","text":"resumed","annotations":[]});
    let item = serde_json::json!({"id":"msg-held","type":"message","role":"assistant","status":"completed","content":[part.clone()]});
    let events = [
        serde_json::json!({"type":"response.output_text.done","sequence_number":4,"item_id":"msg-held","output_index":0,"content_index":0,"text":"resumed"}),
        serde_json::json!({"type":"response.content_part.done","sequence_number":5,"item_id":"msg-held","output_index":0,"content_index":0,"part":part}),
        serde_json::json!({"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":item.clone()}),
        serde_json::json!({"type":"response.completed","sequence_number":7,
        "response":{"id":"resp-held","object":"response","status":"completed","model":MODEL,
            "output":[item],
            "usage":{"input_tokens":4,"output_tokens":1,"total_tokens":5},"incomplete_details":null,"error":null}}),
    ];
    let mut output = text_prefix().replace("partial", "resumed");
    for event in events {
        output.push_str("data: ");
        output.push_str(&event.to_string());
        output.push_str("\n\n");
    }
    output.push_str("data: [DONE]\n\n");
    output
}

#[tokio::test(start_paused = true)]
async fn boundary_hold_excludes_real_semantic_stall_budget_but_running_generation_keeps_it()
-> TestResult {
    let _clock = fixed_virtual_time();
    let prefix = text_prefix();
    let mut replies = status_replies();
    replies.push((
        "200 OK",
        "text/event-stream",
        format!("{prefix}{}", responses_sse()),
        "",
    ));
    replies.push(("200 OK", "text/event-stream", text_reply(), ""));
    let release = Arc::new(Notify::new());
    let (client, origin, peer) = loopback_with_status_holding(
        b"held-semantic-budget",
        replies,
        Some(HoldOneReply {
            index: 2,
            release: Arc::clone(&release),
            body_prefix: Some(prefix.len()),
            close_without_tail: true,
        }),
        None,
    )
    .await?;
    let mut limits = client.limits().clone();
    limits.response_body_timeout = Duration::from_secs(600);
    let model = responses_model(client.with_limits(limits)?, origin).await?;
    let root = tempfile::tempdir()?;
    let agent = agent(model, root.path())?;
    let mut turn = held_turn(&agent).await?;
    let mut events = turn.take_events()?;
    while !matches!(
        events.recv().await?.kind().model_event(),
        Some(ModelEvent::TextDelta(_))
    ) {}
    let hold = facade_support::hold_step_boundary(&agent)?;
    assert!(matches!(
        hold.progress()?,
        BoundaryProgress::Draining {
            phase: BoundaryPhase::Model,
            ..
        }
    ));
    tokio::time::advance(Duration::from_secs(299)).await;
    loop {
        let mut next = Box::pin(events.recv());
        match std::future::poll_fn(|context| std::task::Poll::Ready(next.as_mut().poll(context)))
            .await
        {
            std::task::Poll::Pending => break,
            std::task::Poll::Ready(event) => assert!(!matches!(
                event?.kind().model_event(),
                Some(ModelEvent::Retry(_)),
            )),
        }
    }
    tokio::time::advance(Duration::from_secs(2)).await;
    loop {
        let event = events.recv().await?;
        if let Some(ModelEvent::Retry(retry)) = event.kind().model_event() {
            assert!(retry.loss().elapsed_ms >= 300_000);
            assert_eq!(
                retry.cause(),
                plakous_model::ModelRetryCause::StreamInterrupted {
                    fault: plakous_model::ModelTransportFault::Timeout
                }
            );
            release.notify_one();
            tokio::time::advance(retry.delay() + Duration::from_millis(2)).await;
            break;
        }
    }
    drop(hold);
    let result = turn.result().await?;
    assert!(
        result.error().is_none(),
        "{:?}",
        result.error().map(plakous_agent::AgentError::message)
    );
    assert_eq!(
        result.response().ok_or("replayed response")?.output_text(),
        "resumed"
    );
    agent.shutdown().await?;
    assert_eq!(peer.await??.len(), 4);
    Ok(())
}
