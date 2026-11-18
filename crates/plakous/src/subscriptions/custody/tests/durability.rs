//! Crash cuts on a gateway's claimed Claude refresh (spec 05 §2.9).
//!
//! The gateway's [`SubscriptionCustodian`] runs the operator's forced
//! refresh, and the gateway's runtime is shut down after the claim is
//! written, after the provider answered, or after the store committed. A gateway restarted over the same
//! store then answers the remote side. A durable `RefreshOutcomeUnknown` is
//! quarantined, a relayed request on it is refused `credential_not_ready`
//! without a dial, and Claude never reclaims it. A committed refresh is Ready
//! at the generation after the claim, and a request prepared on the old
//! generation is re-prepared onto it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_auth::CredentialStatus;
use plakous_auth::custody::{
    CredentialCustodian as _, CustodySlot, CustodyStatus, ExpectedFence, RefreshRequest,
};
use plakous_http::{
    Bytes, Client, ExchangeControl, Failure, FaultPlan, HeaderMap, HeaderName, HeaderValue, Limits,
    Method, Origin, Request, StatusCode, TransmissionFact, sensitive_header_value,
};

use super::{Respond, TestResult, Upstream, at_review, fixture_reply, upstream_with};
use crate::custody::Custody;
use crate::gateway::GatewayCustody;
use crate::gateway::pins::anthropic::{CLAUDE_CREDENTIAL_HEADERS, CLAUDE_UPSTREAM_PINS};
use crate::gateway::server::{InProcessGateway, LocalCustody, new_instance};
use crate::subscriptions::custody::SubscriptionCustodian;
use crate::subscriptions::state::tests::child_catalog::{
    CLAUDE_ACCOUNT, claude_stored, seed_claude,
};
use crate::subscriptions::{NativeProvider, SubscriptionPaths};

const MESSAGES: &str = "https://api.anthropic.com";
const TOKEN: &str = "https://platform.claude.com";
const TOKEN_POST: &[u8] = b"POST /v1/oauth/token ";
const MESSAGES_POST: &[u8] = b"POST /v1/messages?beta=true ";
const PROVIDER: NativeProvider = NativeProvider::Claude;

/// The seeded login, the claim's durable Unknown, and the completion.
const SEEDED: u64 = 1;
const CLAIMED: u64 = 2;
const COMMITTED: u64 = 3;

/// The token request waits before it is sent: the claim is written and the
/// provider never sees the grant.
const BEFORE_SEND: &str = "path=/v1/oauth/token,fault=hang,seconds=600";
/// The provider answers and rotates the grant; the body stalls before the
/// gateway reads it.
const AFTER_ANSWER: &str = "path=/v1/oauth/token,fault=stall,after-bytes=0,seconds=600";

/// The token endpoint rotates the grant; Messages answers `200`.
fn granting() -> Respond {
    Arc::new(|request: &[u8]| {
        if request.starts_with(TOKEN_POST) {
            let token = format!(
                r#"{{"access_token":"access-two","refresh_token":"refresh-two","expires_in":3600,"scope":"user:inference","account":{{"uuid":"{CLAUDE_ACCOUNT}"}}}}"#
            );
            return ("200 OK", "application/json", token.into_bytes());
        }
        if request.starts_with(MESSAGES_POST) {
            return ("200 OK", "application/json", b"{}".to_vec());
        }
        let (status, body) = fixture_reply(request);
        (status, "application/json", body)
    })
}

fn slot() -> TestResult<CustodySlot> {
    Ok(CustodySlot::new("claude")?)
}

/// The gateway's custodian over `paths`: its login refreshes at the token
/// origin through `upstream`, under `faults`.
fn custodian(
    paths: &SubscriptionPaths,
    upstream: &Upstream,
    faults: Option<&str>,
) -> TestResult<SubscriptionCustodian> {
    let token = upstream
        .client
        .clone()
        .with_faults(faults.map(FaultPlan::parse).transpose()?);
    let mut local = at_review(paths, PROVIDER)?;
    local.claude_test_transport = Some(
        Client::new_http1(Limits::default())?.with_test_dial_override(
            Origin::parse(&format!("{TOKEN}/"))?,
            upstream.origin.clone(),
            token,
        ),
    );
    Ok(SubscriptionCustodian::new(local, PROVIDER))
}

/// The remote side of a gateway started over `paths`, as a host holds it.
fn gateway(paths: &SubscriptionPaths, upstream: &Upstream) -> TestResult<Custody> {
    let custody = LocalCustody::new(new_instance().ok_or("gateway instance")?).with_kind(
        "claude",
        Arc::new(custodian(paths, upstream, None)?),
        CLAUDE_UPSTREAM_PINS,
    );
    let server = InProcessGateway::server(custody, upstream.client.clone())
        .with_dial_override(MESSAGES, upstream.origin.clone());
    let gateway = GatewayCustody::connect(InProcessGateway::endpoint_for(&server), None, |kind| {
        (kind == "claude").then_some(CLAUDE_CREDENTIAL_HEADERS)
    })?;
    Ok(gateway.custody())
}

/// How many recorded upstream requests start with `line`.
fn seen(upstream: &Upstream, line: &[u8]) -> usize {
    upstream
        .requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|request| request.starts_with(line))
        .count()
}

/// The host's fence on the seeded login, read through a gateway.
async fn seeded(paths: &SubscriptionPaths, upstream: &Upstream) -> TestResult<ExpectedFence> {
    let custody = gateway(paths, upstream)?;
    let facts = custody
        .custodian()
        .ok_or("gateway custodian")?
        .facts(&slot()?)
        .await?;
    assert_eq!(facts.status, CustodyStatus::Ready);
    let expected = facts
        .expected_fence()
        .ok_or("seeded facts carry no fence")?;
    assert_eq!(expected.generation.get(), SEEDED);
    Ok(expected)
}

/// Runs the gateway's forced refresh under `faults` on the gateway's own
/// runtime and shuts that runtime down once `reached` holds: the gateway
/// process dies there. A dropped caller is not a crash, because the claimed
/// refresh runs detached from it; shutting the runtime down drops that task
/// too and releases the claim's execution lock, as the kernel does for a
/// killed process.
async fn cut(
    paths: &SubscriptionPaths,
    upstream: &Upstream,
    faults: Option<&str>,
    reached: impl Fn() -> TestResult<bool>,
) -> TestResult {
    let custodian = custodian(paths, upstream, faults)?;
    let slot = slot()?;
    let process = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let refresh = process.spawn(async move {
        custodian
            .refresh(&slot, RefreshRequest::Operator)
            .await
            .map(|_| ())
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut early = None;
    while !reached()? {
        if refresh.is_finished() {
            early = Some("the refresh settled before the cut");
            break;
        }
        if Instant::now() >= deadline {
            early = Some("the refresh did not reach the cut within 10s");
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::task::spawn_blocking(move || process.shutdown_timeout(Duration::from_secs(5))).await?;
    match early {
        Some(why) => Err(why.into()),
        None => Ok(()),
    }
}

/// One Messages POST from the host through `custody`, prepared on
/// `expected`.
async fn relay_messages(
    custody: &Custody,
    expected: &ExpectedFence,
) -> TestResult<Result<StatusCode, Failure>> {
    let mut fields = custody
        .custodian()
        .ok_or("gateway custodian")?
        .fields(&slot()?, expected)
        .await?;
    let authorization = fields
        .take("authorization")
        .ok_or("no custodied authorization")?;
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("authorization"),
        sensitive_header_value(authorization.into_header_bytes())?,
    );
    let request = Request::new(
        Origin::parse(&format!("{MESSAGES}/"))?,
        Method::POST,
        "/v1/messages?beta=true",
        headers,
        Bytes::from_static(b"{}"),
        &Limits::default(),
    )?;
    let client = custody
        .transport()
        .client(Client::new_http1(Limits::default())?);
    let control = ExchangeControl::new(Instant::now() + Duration::from_secs(5));
    Ok(client
        .execute(request, control)
        .await
        .map(|response| response.status()))
}

/// What the remote side of a restarted gateway sees for the durable Unknown
/// a cut before the commit leaves.
async fn assert_quarantined(
    paths: &SubscriptionPaths,
    upstream: &Upstream,
    expected: &ExpectedFence,
) -> TestResult {
    assert_eq!(
        claude_stored(paths)?,
        (CLAIMED, CredentialStatus::RefreshOutcomeUnknown)
    );
    let custody = gateway(paths, upstream)?;
    let remote = custody.custodian().ok_or("gateway custodian")?;
    let facts = remote
        .facts(&slot()?)
        .await
        .map_err(|error| format!("facts after the cut: {error:?}"))?;
    // A quarantined login is reported from its offline status, which names no
    // usable generation; the claimed generation stays in the store (above).
    assert_eq!(facts.status, CustodyStatus::Quarantined);

    let Err(failure) = relay_messages(&custody, expected).await? else {
        return Err("a quarantined login was relayed".into());
    };
    assert_eq!(failure.cause(), Some("gateway_credential_not_ready"));
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    assert_eq!(seen(upstream, MESSAGES_POST), 0, "the gateway dialed");

    // Claude never reclaims an unconfirmed grant: the operator's forced
    // refresh sends nothing and the record stays quarantined for a login.
    let posts = seen(upstream, TOKEN_POST);
    let _ = remote.refresh(&slot()?, RefreshRequest::Operator).await;
    assert_eq!(seen(upstream, TOKEN_POST), posts);
    assert_eq!(
        claude_stored(paths)?,
        (CLAIMED, CredentialStatus::RefreshOutcomeUnknown)
    );
    assert_eq!(
        remote.facts(&slot()?).await?.status,
        CustodyStatus::Quarantined
    );
    Ok(())
}

#[tokio::test]
async fn a_refresh_killed_after_its_claim_is_quarantined_without_a_grant_sent() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed_claude(&paths, "access-one", "refresh-one")?;
    let upstream = upstream_with(granting()).await?;
    let expected = seeded(&paths, &upstream).await?;

    cut(&paths, &upstream, Some(BEFORE_SEND), || {
        Ok(claude_stored(&paths)? == (CLAIMED, CredentialStatus::RefreshOutcomeUnknown))
    })
    .await?;

    assert_eq!(seen(&upstream, TOKEN_POST), 0, "the grant left the gateway");
    assert_quarantined(&paths, &upstream, &expected).await
}

#[tokio::test]
async fn a_refresh_killed_after_the_provider_answered_is_quarantined() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed_claude(&paths, "access-one", "refresh-one")?;
    let upstream = upstream_with(granting()).await?;
    let expected = seeded(&paths, &upstream).await?;

    cut(&paths, &upstream, Some(AFTER_ANSWER), || {
        Ok(seen(&upstream, TOKEN_POST) == 1)
    })
    .await?;

    // The provider rotated the grant the store never recorded.
    assert_eq!(seen(&upstream, TOKEN_POST), 1);
    assert_quarantined(&paths, &upstream, &expected).await
}

#[tokio::test]
async fn a_refresh_killed_after_the_store_commit_is_ready_at_the_next_generation() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed_claude(&paths, "access-one", "refresh-one")?;
    let upstream = upstream_with(granting()).await?;
    let expected = seeded(&paths, &upstream).await?;

    cut(&paths, &upstream, None, || {
        Ok(claude_stored(&paths)? == (COMMITTED, CredentialStatus::Ready))
    })
    .await?;
    assert_eq!(seen(&upstream, TOKEN_POST), 1);

    let custody = gateway(&paths, &upstream)?;
    let facts = custody
        .custodian()
        .ok_or("gateway custodian")?
        .facts(&slot()?)
        .await?;
    assert_eq!(facts.status, CustodyStatus::Ready);
    assert_eq!(
        facts
            .generation
            .map(plakous_auth::CredentialGeneration::get),
        Some(COMMITTED)
    );

    // A request the host prepared on the seeded generation is re-prepared
    // onto the committed one and carries the rotated token, once.
    assert_eq!(
        relay_messages(&custody, &expected).await?.ok(),
        Some(StatusCode::OK)
    );
    let requests = upstream.take();
    let messages: Vec<_> = requests
        .iter()
        .filter(|request| request.starts_with(MESSAGES_POST))
        .map(|request| String::from_utf8_lossy(request).to_ascii_lowercase())
        .collect();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("\r\nauthorization: bearer access-two\r\n"));
    Ok(())
}
