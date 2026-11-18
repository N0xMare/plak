//! API-key crash rows for the credential gateway: a killed gateway, a
//! killed host, a dropped caller and a restart, over the shared relay core.
//!
//! Subscription refresh crash cuts are in `plakous`
//! (`subscriptions::custody::tests::durability`), where the custodian is.

use std::time::Duration;

use plakous_http::{Phase, StatusCode, TransmissionFact};

use super::tests::{self};
use super::{FixtureError, GatewayScript};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn gateway_killed_mid_stream_is_may_have_transmitted_without_replay() -> TestResult {
    let gateway = hung_gateway()?;
    let client = gateway.relayed().await?;
    let response = client
        .execute(
            tests::api_request(logical()?, tests::placeholder()?)?,
            tests::control(),
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(gateway.dials(), 1);
    gateway.kill();
    let mut body = response.into_body();
    let outcome = tokio::time::timeout(Duration::from_secs(2), body.next_chunk())
        .await
        .map_err(|_| "the body did not break within 2s of the gateway being killed")?;
    let Some(result) = outcome else {
        return Err("the body ended cleanly after the gateway was killed".into());
    };
    let Err(failure) = result else {
        return Err("the body continued after the gateway was killed".into());
    };
    assert_eq!(failure.phase(), Phase::ResponseBody);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(gateway.dials(), 1, "a killed gateway must not be replayed");
    Ok(())
}

#[tokio::test]
async fn host_killed_mid_stream_cancels_the_upstream() -> TestResult {
    let gateway = hung_gateway()?;
    let running = gateway.clone();
    let task = tokio::spawn(async move {
        let client = running.relayed().await?;
        let response = client
            .execute(
                tests::api_request(logical()?, tests::placeholder()?)?,
                tests::control(),
            )
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let _ = body.next_chunk().await;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    wait_until(|| gateway.dials() == 1).await?;
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(1), gateway.wait_for_upstream_close())
        .await
        .map_err(|_| "the upstream was still open 1s after the host was killed")?;
    assert_eq!(gateway.dials(), 1);
    Ok(())
}

#[tokio::test]
async fn dropping_the_caller_cancels_the_upstream_within_a_bound() -> TestResult {
    let gateway = hung_gateway()?;
    let client = gateway.relayed().await?;
    let response = client
        .execute(
            tests::api_request(logical()?, tests::placeholder()?)?,
            tests::control(),
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(gateway.dials(), 1);
    drop(response);
    tokio::time::timeout(Duration::from_secs(1), gateway.wait_for_upstream_close())
        .await
        .map_err(|_| "dropping the caller did not cancel the upstream within 1s")?;
    assert_eq!(gateway.dials(), 1);
    Ok(())
}

#[tokio::test]
async fn a_restarted_gateway_serves_the_next_request() -> TestResult {
    let gateway = tests::open_gateway()?;
    let first = gateway.instance();
    let client = gateway.relayed().await?;
    let response = client
        .execute(
            tests::api_request(logical()?, tests::placeholder()?)?,
            tests::control(),
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let restarted = gateway.restart()?;
    assert_ne!(restarted, first);
    assert_eq!(gateway.instance(), restarted);
    let client = gateway.relayed().await?;
    let response = client
        .execute(
            tests::api_request(logical()?, tests::placeholder()?)?,
            tests::control(),
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(gateway.dials(), 2);
    Ok(())
}

fn hung_gateway() -> Result<super::FixtureGateway, FixtureError> {
    let gateway = tests::open_gateway()?;
    gateway.push(GatewayScript::DropAfterHead);
    Ok(gateway)
}

fn logical() -> TestResult<plakous_http::Origin> {
    Ok(plakous_http::Origin::parse(&format!(
        "{}/",
        super::service::LOGICAL
    ))?)
}

async fn wait_until(mut ready: impl FnMut() -> bool) -> TestResult {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        if ready() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err("the upstream was not dialed".into())
}
