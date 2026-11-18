//! Claude under gateway custody: a session bound under direct custody
//! reopens through an in-process gateway with an equal binding and its
//! Messages POST reaches the upstream byte for byte; a provider 401 is
//! refreshed once by the gateway's single writer before the route's one
//! re-dispatch.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use plakous_http::{Client, Limits, Origin};
use uuid::Uuid;

use super::{
    Respond, TestResult, Upstream, at_review, binding, dispatch_once, fixture_reply, limits,
    revalidate, upstream, upstream_with,
};
use crate::gateway::GatewayCustody;
use crate::gateway::pins::anthropic::{CLAUDE_CREDENTIAL_HEADERS, CLAUDE_UPSTREAM_PINS};
use crate::gateway::server::{InProcessGateway, LocalCustody, new_instance};
use crate::subscriptions::custody::SubscriptionCustodian;
use crate::subscriptions::state::tests::child_catalog::{
    CLAUDE_ACCOUNT, claude_generation, claude_selection, seed_claude,
};
use crate::subscriptions::{NativeProvider, NativeSubscriptions, SubscriptionPaths};

const MESSAGES: &str = "https://api.anthropic.com";
const TOKEN: &str = "https://platform.claude.com";
const MODEL: &str = "claude-sonnet-5";
const PROVIDER: NativeProvider = NativeProvider::Claude;

/// A client dialing the Claude Messages and token origins at `upstream`.
fn dialing(upstream: &Upstream) -> TestResult<Client> {
    let token = Client::new_http1(Limits::default())?.with_test_dial_override(
        Origin::parse(&format!("{TOKEN}/"))?,
        upstream.origin.clone(),
        upstream.client.clone(),
    );
    Ok(
        Client::new_http1(Limits::default())?.with_test_dial_override(
            Origin::parse(&format!("{MESSAGES}/"))?,
            upstream.origin.clone(),
            token,
        ),
    )
}

/// Claude subscriptions over `paths` naming one fixed session.
fn subscriptions(paths: &SubscriptionPaths) -> TestResult<NativeSubscriptions> {
    let mut subscriptions = at_review(paths, PROVIDER)?;
    subscriptions.claude_test_session =
        Some(plakous_model::SessionId::try_from(Uuid::from_u128(9))?);
    Ok(subscriptions)
}

/// Custody through an in-process gateway serving the Claude logins of
/// `paths`: its upstream dials the Messages origin, and its own login
/// refreshes at the token origin, both at `upstream`.
fn gateway_custody(
    paths: &SubscriptionPaths,
    upstream: &Upstream,
) -> TestResult<crate::custody::Custody> {
    let mut local = at_review(paths, PROVIDER)?;
    local.claude_test_transport = Some(dialing(upstream)?);
    let custody = LocalCustody::new(new_instance().ok_or("gateway instance")?).with_kind(
        "claude",
        Arc::new(SubscriptionCustodian::new(local, PROVIDER)),
        CLAUDE_UPSTREAM_PINS,
    );
    let server = InProcessGateway::server(custody, upstream.client.clone())
        .with_dial_override(MESSAGES, upstream.origin.clone());
    let gateway = GatewayCustody::connect(InProcessGateway::endpoint_for(&server), None, |kind| {
        (kind == "claude").then_some(CLAUDE_CREDENTIAL_HEADERS)
    })?;
    Ok(gateway.custody())
}

fn lowercase(request: &[u8]) -> String {
    String::from_utf8_lossy(request).to_ascii_lowercase()
}

#[tokio::test]
async fn a_claude_session_reopens_through_a_gateway_and_sends_identical_messages_bytes()
-> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed_claude(&paths, "access-one", "refresh-one")?;
    let upstream = upstream().await?;

    let mut direct = subscriptions(&paths)?;
    direct.claude_test_transport = Some(dialing(&upstream)?);
    let bound = direct
        .resolve_turn_scoped_provider_for_attach(claude_selection(MODEL)?, limits()?)
        .await?;
    revalidate(&bound).await?;
    dispatch_once(&bound).await?;
    let direct_requests = upstream.take();

    let gateway = subscriptions(&paths)?.with_custody(&gateway_custody(&paths, &upstream)?);
    let reopened = gateway
        .resolve_turn_scoped_provider_for_attach(claude_selection(MODEL)?, limits()?)
        .await?;
    assert_eq!(binding(&reopened)?, binding(&bound)?);
    revalidate(&reopened).await?;
    dispatch_once(&reopened).await?;
    let gateway_requests = upstream.take();

    assert_eq!(direct_requests.len(), 1, "{direct_requests:?}");
    assert!(
        direct_requests[0].starts_with(b"POST /v1/messages?beta=true "),
        "{}",
        String::from_utf8_lossy(&direct_requests[0])
    );
    assert_eq!(gateway_requests.len(), direct_requests.len());
    assert_eq!(
        String::from_utf8_lossy(&gateway_requests[0]),
        String::from_utf8_lossy(&direct_requests[0])
    );
    Ok(())
}

const SSE: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",",
    "\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5\",\"content\":[],",
    "\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,",
    "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,",
    "\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",",
    "\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

/// The first Messages POST is refused 401, the token endpoint grants
/// `access-two`, and every later Messages POST completes.
fn rejecting_once() -> Respond {
    let messages = Arc::new(AtomicUsize::new(0));
    Arc::new(move |request: &[u8]| {
        if request.starts_with(b"POST /v1/oauth/token ") {
            let token = format!(
                r#"{{"access_token":"access-two","refresh_token":"refresh-two","expires_in":3600,"scope":"user:inference","account":{{"uuid":"{CLAUDE_ACCOUNT}"}}}}"#
            );
            return ("200 OK", "application/json", token.into_bytes());
        }
        if request.starts_with(b"POST /v1/messages?beta=true ") {
            if messages.fetch_add(1, Ordering::SeqCst) == 0 {
                return (
                    "401 Unauthorized",
                    "application/json",
                    br#"{"type":"error","error":{"type":"authentication_error","message":"fixture rejection"}}"#
                        .to_vec(),
                );
            }
            return ("200 OK", "text/event-stream", SSE.as_bytes().to_vec());
        }
        let (status, body) = fixture_reply(request);
        (status, "application/json", body)
    })
}

#[tokio::test]
async fn a_relayed_401_is_refreshed_once_by_the_gateway_before_one_redispatch() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed_claude(&paths, "access-one", "refresh-one")?;
    assert_eq!(claude_generation(&paths)?, 1);
    let upstream = upstream_with(rejecting_once()).await?;

    let host = subscriptions(&paths)?.with_custody(&gateway_custody(&paths, &upstream)?);
    let provider = host
        .resolve_turn_scoped_provider_for_attach(claude_selection(MODEL)?, limits()?)
        .await?;
    revalidate(&provider).await?;
    dispatch_once(&provider).await?;
    let requests = upstream.take();

    // Messages (401), the gateway's one refresh, then the route's single
    // re-dispatch on the next generation; the gateway resends nothing.
    let lines: Vec<_> = requests
        .iter()
        .map(|request| {
            let text = String::from_utf8_lossy(request);
            text.lines().next().unwrap_or_default().to_owned()
        })
        .collect();
    assert_eq!(
        lines,
        [
            "POST /v1/messages?beta=true HTTP/1.1",
            "POST /v1/oauth/token HTTP/1.1",
            "POST /v1/messages?beta=true HTTP/1.1",
        ],
        "{requests:?}"
    );
    assert!(lowercase(&requests[0]).contains("\r\nauthorization: bearer access-one\r\n"));
    assert!(lowercase(&requests[2]).contains("\r\nauthorization: bearer access-two\r\n"));
    // The one claimed refresh committed a later generation, the one the
    // re-dispatch carried.
    assert!(claude_generation(&paths)? > 1);
    Ok(())
}
