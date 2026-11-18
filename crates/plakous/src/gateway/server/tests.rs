use std::sync::{Arc, Mutex};

use plakous_auth::api_key::{ApiKey, CredentialProviderTag};
use plakous_auth::custody::gateway::GatewayCustodian;
use plakous_auth::custody::{
    CredentialCustodian, CustodyError, CustodySlot, KeyHeaderForm, KeyInjection, KeySource,
    LocalKey, LocalKeyCustodian, LocalKeyStore, RefreshRequest,
};
use plakous_gateway_protocol::envelope::{ExpectedFence, RelayEnvelope, RelayLimits};
use plakous_gateway_protocol::header::{RequestHeader, WireHeader};
use plakous_gateway_protocol::refusal::GatewayRefusal;
use plakous_gateway_protocol::{GatewayAccount, HexDigest};

use super::{InProcessGateway, LocalCustody, new_instance};
use crate::gateway::RelayClient;

const KIND: &str = "openai";

struct Keys(Mutex<&'static str>);

impl LocalKeyStore for Keys {
    fn key(&self, _slot: &CustodySlot) -> Result<Option<LocalKey>, CustodyError> {
        Ok(Some(LocalKey {
            key: ApiKey::new(*self.0.lock().unwrap()).unwrap(),
            source: KeySource::File,
            env_var: None,
        }))
    }
}

fn injection(kind: &str) -> Option<KeyInjection> {
    (kind == KIND).then_some(KeyInjection {
        provider: CredentialProviderTag::OpenAiPlatform,
        header: "authorization",
        form: KeyHeaderForm::Bearer,
    })
}

fn custody(keys: &Arc<Keys>) -> (Arc<LocalKeyCustodian>, LocalCustody) {
    let custodian = Arc::new(LocalKeyCustodian::new(keys.clone(), injection));
    let custody = LocalCustody::new(new_instance().unwrap()).with_kind(
        KIND,
        custodian.clone(),
        crate::gateway::pins::openai::OPENAI_API_UPSTREAM_PINS,
    );
    (custodian, custody)
}

async fn fence(custodian: &LocalKeyCustodian, slot: &CustodySlot) -> ExpectedFence {
    let fence = custodian
        .facts(slot)
        .await
        .unwrap()
        .expected_fence()
        .unwrap();
    ExpectedFence {
        account_binding: HexDigest::from_bytes(fence.account_binding.as_bytes()),
        generation: fence.generation.get(),
    }
}

fn envelope(
    account: &str,
    url: &str,
    expected: ExpectedFence,
    custodied: &[&str],
) -> RelayEnvelope {
    let mut headers = vec![RequestHeader::Value(
        WireHeader::new("content-type", b"application/json".to_vec()).unwrap(),
    )];
    headers.extend(
        custodied
            .iter()
            .map(|name| RequestHeader::custodied(*name).unwrap()),
    );
    RelayEnvelope {
        operation: new_instance().unwrap(),
        attempt: 0,
        account: GatewayAccount::new(account).unwrap(),
        expected,
        method: "POST".into(),
        url: url.into(),
        headers,
        body_len: 0,
        limits: RelayLimits {
            head_timeout_ms: 1,
            body_idle_timeout_ms: 1,
            max_response_bytes: 1,
            max_chunk_bytes: 1,
        },
        deadline_unix_ms: None,
    }
}

#[tokio::test]
async fn injection_checks_the_pin_row_names_and_fence_before_lowering_the_key() {
    let keys = Arc::new(Keys(Mutex::new("synthetic-key-one")));
    let (custodian, custody) = custody(&keys);
    let slot = CustodySlot::new("openai/work").unwrap();
    let expected = fence(&custodian, &slot).await;
    let url = "https://api.openai.com/v1/responses";

    let injected = custody
        .inject(&envelope(
            "openai/work",
            url,
            expected.clone(),
            &["authorization"],
        ))
        .await
        .unwrap();
    assert_eq!(
        format!("{injected:?}"),
        "Injection { values: 1, holding: false }"
    );

    let custody = &custody;
    let refused = |envelope| async move { custody.inject(&envelope).await.unwrap_err() };
    assert_eq!(
        refused(envelope(
            "openai/work",
            "https://api.openai.com/v1/elsewhere",
            expected.clone(),
            &["authorization"]
        ))
        .await,
        GatewayRefusal::UpstreamNotPinned {}
    );
    assert_eq!(
        refused(envelope(
            "openai/work",
            "https://evil.example.test/v1/responses",
            expected.clone(),
            &["authorization"]
        ))
        .await,
        GatewayRefusal::UpstreamNotPinned {}
    );
    assert_eq!(
        refused(envelope(
            "openai/work",
            url,
            expected.clone(),
            &["x-api-key"]
        ))
        .await,
        GatewayRefusal::CustodiedHeadersMismatch {}
    );
    assert_eq!(
        refused(envelope(
            "other/work",
            url,
            expected.clone(),
            &["authorization"]
        ))
        .await,
        GatewayRefusal::AccountUnknown {}
    );
    let other_account = fence(&custodian, &CustodySlot::new("openai/other").unwrap()).await;
    assert_eq!(
        refused(envelope(
            "openai/work",
            url,
            other_account,
            &["authorization"]
        ))
        .await,
        GatewayRefusal::AccountChanged {}
    );

    *keys.0.lock().unwrap() = "synthetic-key-two";
    let GatewayRefusal::FenceMoved { facts } = refused(envelope(
        "openai/work",
        url,
        expected.clone(),
        &["authorization"],
    ))
    .await
    else {
        panic!("a replaced key moves the fence");
    };
    let moved = facts.expected_fence().unwrap();
    assert_eq!(moved.account_binding, expected.account_binding);
    assert_ne!(moved.generation, expected.generation);
}

#[tokio::test]
async fn gateway_custody_reports_the_same_identity_as_local_custody() {
    let keys = Arc::new(Keys(Mutex::new("synthetic-key-one")));
    let (custodian, custody) = custody(&keys);
    let endpoint = InProcessGateway::endpoint(
        custody,
        plakous_http::Client::new_http1(plakous_http::Limits::default()).unwrap(),
    );
    let remote = GatewayCustodian::new(
        Arc::new(RelayClient::new(endpoint, None).unwrap()),
        |kind| {
            (kind == KIND).then_some(crate::gateway::pins::openai::OPENAI_API_CREDENTIAL_HEADERS)
        },
    );
    let slot = CustodySlot::new("openai/work").unwrap();

    let local = custodian.facts(&slot).await.unwrap();
    let relayed = remote.facts(&slot).await.unwrap();
    assert_eq!(relayed, local, "custody location never changes the facts");

    let fence = local.expected_fence().unwrap();
    for request in [
        RefreshRequest::IfDue { expected: fence },
        RefreshRequest::AfterRejection {
            rejected: fence,
            selection_epoch_digest: None,
        },
        RefreshRequest::Operator,
    ] {
        assert_eq!(remote.refresh(&slot, request).await.unwrap(), local);
    }

    let mut fields = remote.fields(&slot, &fence).await.unwrap();
    let placeholder = fields.take("authorization").unwrap().into_header_bytes();
    assert!(placeholder.starts_with(b"\xffplak-custodied openai/work "));
    assert!(
        !placeholder
            .windows(b"synthetic".len())
            .any(|window| window == b"synthetic")
    );

    assert_eq!(
        remote.facts(&CustodySlot::new("other").unwrap()).await,
        Err(CustodyError::UnknownAccount)
    );
}

#[test]
fn the_upstream_hop_never_shortens_a_streaming_callers_silence_bounds() {
    let upstream = super::http1_upstream().unwrap();
    let streaming = plakous_http::Limits::for_streaming_inference();
    assert_eq!(
        upstream.limits().response_head_timeout,
        streaming.response_head_timeout
    );
    assert_eq!(
        upstream.limits().response_body_timeout,
        streaming.response_body_timeout
    );
}
