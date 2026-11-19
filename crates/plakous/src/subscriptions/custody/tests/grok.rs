//! SuperGrok continuity under gateway custody: a session bound under direct
//! custody reopens through an in-process gateway over the same store with an
//! equal binding, and its catalog, Responses and billing exchanges reach the
//! upstream byte for byte.

use std::sync::Arc;

use super::{
    GROK_MODEL, TestResult, Upstream, at_review, binding, dispatch_once, limits, revalidate,
    upstream,
};
use crate::gateway::GatewayCustody;
use crate::gateway::pins::xai::{SUPERGROK_CREDENTIAL_HEADERS, SUPERGROK_UPSTREAM_PINS};
use crate::gateway::server::{InProcessGateway, LocalCustody, new_instance};
use crate::subscriptions::custody::SubscriptionCustodian;
use crate::subscriptions::state::tests::child_catalog::{grok_selection, seed_grok};
use crate::subscriptions::{NativeProvider, NativeSubscriptionError, SubscriptionPaths};

const SUPERGROK: &str = "https://cli-chat-proxy.grok.com";

#[tokio::test]
async fn a_path_bound_supergrok_binding_revalidates_through_gateway_v1() -> TestResult {
    use plakous_auth::custody::gateway::GatewayCustodian;
    use plakous_http::relay::RelayClient;
    use plakous_model::{Digest, DigestDomain, sha256_hex};

    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed_grok(&paths, "fixture-grok-subject")?;
    let upstream = upstream().await?;
    let provider = NativeProvider::Xai;
    let subscriptions = at_review(&paths, provider)?;
    let path = subscriptions.state.provider_record(provider);
    let legacy_authority = format!(
        "plak-native-xai-route:{}",
        sha256_hex(&[
            path.as_os_str().as_encoded_bytes(),
            b"\0xai-route-credential-authority-v2\0",
            b"primary",
        ]),
    );
    let recorded = Digest::sha256(
        DigestDomain::ModelBinding,
        1,
        &[
            b"plakous-xai-credential-authority-binding-v1",
            b"xai.grok_subscription",
            SUPERGROK.as_bytes(),
            legacy_authority.as_bytes(),
        ],
    )?;

    let current = SubscriptionCustodian::new(subscriptions.clone(), provider);
    let legacy = current.for_gateway_v1();
    let custody = LocalCustody::new(new_instance().ok_or("gateway instance")?).with_kind_versions(
        "grok",
        Arc::new(current),
        Some(Arc::new(legacy)),
        SUPERGROK_UPSTREAM_PINS,
    );
    let server = InProcessGateway::server(custody, upstream.client.clone())
        .with_dial_override(SUPERGROK, upstream.origin.clone());
    let relay = Arc::new(RelayClient::new(
        super::previous_gateway_endpoint(server)?,
        None,
    )?);
    assert_eq!(relay.version(&RelayClient::call_control()).await?, 1);
    let legacy = Arc::new(GatewayCustodian::new(Arc::clone(&relay), |_| {
        Some(SUPERGROK_CREDENTIAL_HEADERS)
    }));
    let host = subscriptions.with_custody(&crate::custody::Custody::gateway(
        plakous_http::RouteTransport::relayed(relay),
        legacy,
    ));
    let reopened = host
        .resolve_turn_scoped_provider_for_attach(grok_selection(GROK_MODEL)?, limits()?)
        .await?;
    assert_eq!(
        reopened.initial_authority().credential_authority_binding(),
        recorded
    );
    let fresh = reopened
        .model()
        .revalidate_runtime_authority(reopened.initial_authority().observed_at())
        .await?;
    assert_eq!(fresh.credential_authority_binding(), recorded);
    Ok(())
}

/// Custody through an in-process gateway that serves the SuperGrok logins
/// of `paths` and dials the production origin at `upstream`.
fn gateway_custody(
    paths: &SubscriptionPaths,
    upstream: &Upstream,
) -> TestResult<crate::custody::Custody> {
    let custodian =
        SubscriptionCustodian::new(at_review(paths, NativeProvider::Xai)?, NativeProvider::Xai);
    let custody = LocalCustody::new(new_instance().ok_or("gateway instance")?).with_kind(
        "grok",
        Arc::new(custodian),
        SUPERGROK_UPSTREAM_PINS,
    );
    let server = InProcessGateway::server(custody, upstream.client.clone())
        .with_dial_override(SUPERGROK, upstream.origin.clone());
    let gateway = GatewayCustody::connect(InProcessGateway::endpoint_for(&server), None, |kind| {
        (kind == "grok").then_some(SUPERGROK_CREDENTIAL_HEADERS)
    })?;
    Ok(gateway.custody())
}

#[tokio::test]
async fn a_supergrok_session_reopens_through_a_gateway_with_an_equal_binding_and_identical_bytes()
-> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed_grok(&paths, "fixture-grok-subject")?;
    let upstream = upstream().await?;
    let provider = NativeProvider::Xai;

    // Bound under direct custody: the route keeps its production origin and
    // every client dials the fixture, exactly as the gateway's upstream does.
    let mut direct = at_review(&paths, NativeProvider::Xai)?;
    direct.xai_test_transport = Some(
        plakous_http::Client::new_http1(plakous_http::Limits::default())?.with_test_dial_override(
            plakous_http::Origin::parse(&format!("{SUPERGROK}/"))?,
            upstream.origin.clone(),
            upstream.client.clone(),
        ),
    );
    let bound = direct
        .resolve_turn_scoped_provider_for_attach(grok_selection(GROK_MODEL)?, limits()?)
        .await?;
    revalidate(&bound).await?;
    dispatch_once(&bound).await?;
    let usage_direct = direct.subscription_usage(provider).await;
    let direct_requests = upstream.take();

    // Reopened through a gateway over the same store.
    let gateway =
        at_review(&paths, NativeProvider::Xai)?.with_custody(&gateway_custody(&paths, &upstream)?);
    assert_eq!(
        gateway.status(provider).await?,
        direct.status(provider).await?
    );
    let reopened = gateway
        .resolve_turn_scoped_provider_for_attach(grok_selection(GROK_MODEL)?, limits()?)
        .await?;
    assert_eq!(binding(&reopened)?, binding(&bound)?);
    revalidate(&reopened).await?;
    dispatch_once(&reopened).await?;
    let usage_gateway = gateway.subscription_usage(provider).await;
    let gateway_requests = upstream.take();

    // Catalog GET, Responses POST and billing GET, in order, byte for byte
    // with the same header order.
    assert_eq!(direct_requests.len(), 3, "{direct_requests:?}");
    assert_eq!(gateway_requests.len(), direct_requests.len());
    for (direct, relayed) in direct_requests.iter().zip(&gateway_requests) {
        assert_eq!(
            String::from_utf8_lossy(relayed),
            String::from_utf8_lossy(direct)
        );
    }
    for (request, line) in direct_requests.iter().zip([
        b"GET /v1/models ".as_slice(),
        b"POST /v1/responses ",
        b"GET /v1/billing?format=credits ",
    ]) {
        assert!(
            request.starts_with(line),
            "{}",
            String::from_utf8_lossy(request)
        );
    }
    for usage in [usage_direct, usage_gateway] {
        assert!(
            matches!(usage, plakous_model::SubscriptionUsageRead::Snapshot(_)),
            "{usage:?}"
        );
    }

    // Store operations refuse on the gateway host.
    assert_eq!(
        gateway.named_accounts(provider).await.err(),
        Some(NativeSubscriptionError::GatewayCustody)
    );
    assert_eq!(
        gateway.logout_local(provider).await.err(),
        Some(NativeSubscriptionError::GatewayCustody)
    );
    Ok(())
}

#[tokio::test]
async fn a_named_supergrok_session_reopens_through_a_gateway_with_its_scoped_authority_id()
-> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    let provider = NativeProvider::Xai;
    seed_grok(
        &crate::subscriptions::state::tests::child_catalog::named_account(
            &paths, provider, "work",
        )?,
        "fixture-grok-work-subject",
    )?;
    let upstream = upstream().await?;

    let mut direct = at_review(&paths, provider)?.for_named_account(provider, "work")?;
    direct.xai_test_transport = Some(
        plakous_http::Client::new_http1(plakous_http::Limits::default())?.with_test_dial_override(
            plakous_http::Origin::parse(&format!("{SUPERGROK}/"))?,
            upstream.origin.clone(),
            upstream.client.clone(),
        ),
    );
    let bound = direct
        .resolve_turn_scoped_provider_for_attach(grok_selection(GROK_MODEL)?, limits()?)
        .await?;
    dispatch_once(&bound).await?;
    let direct_requests = upstream.take();

    // The account is `grok/work`: the gateway reports the authority id it
    // computes over that named slot, which the session bound.
    let gateway = at_review(&paths, provider)?
        .for_named_account(provider, "work")?
        .with_custody(&gateway_custody(&paths, &upstream)?);
    assert_eq!(
        gateway.status(provider).await?,
        direct.status(provider).await?
    );
    let reopened = gateway
        .resolve_turn_scoped_provider_for_attach(grok_selection(GROK_MODEL)?, limits()?)
        .await?;
    assert_eq!(binding(&reopened)?, binding(&bound)?);
    revalidate(&reopened).await?;
    dispatch_once(&reopened).await?;
    let gateway_requests = upstream.take();
    assert_eq!(direct_requests.len(), 2, "{direct_requests:?}");
    assert_eq!(gateway_requests.len(), direct_requests.len());
    for (direct, relayed) in direct_requests.iter().zip(&gateway_requests) {
        assert_eq!(
            String::from_utf8_lossy(relayed),
            String::from_utf8_lossy(direct)
        );
    }
    Ok(())
}
