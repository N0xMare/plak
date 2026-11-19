use super::*;
use plakous_auth::custody::{
    AccountFacts, CredentialCustodian, CredentialFields, CustodyError, CustodyFuture, CustodySlot,
    CustodyStatus, ExpectedFence, IdentityDigest, RefreshRequest,
};
use plakous_http::relay::RelayClient;

struct ReadySlot;

impl CredentialCustodian for ReadySlot {
    fn facts<'a>(&'a self, slot: &'a CustodySlot) -> CustodyFuture<'a, AccountFacts> {
        Box::pin(async move {
            let mut facts = AccountFacts::signed_out(slot.clone());
            facts.status = CustodyStatus::Ready;
            facts.generation = Some(
                plakous_auth::CredentialGeneration::new(1).map_err(|_| CustodyError::Internal)?,
            );
            facts.account_binding = Some(IdentityDigest::from_bytes([3; 32]));
            facts.credential_authority = Some(IdentityDigest::from_bytes([4; 32]));
            Ok(facts)
        })
    }

    fn refresh<'a>(
        &'a self,
        slot: &'a CustodySlot,
        request: RefreshRequest,
    ) -> CustodyFuture<'a, AccountFacts> {
        Box::pin(async move {
            let facts = self.facts(slot).await?;
            let RefreshRequest::IfDue { expected } = request else {
                return Err(CustodyError::Internal);
            };
            facts.check_fence(&expected)?;
            Ok(facts)
        })
    }

    fn fields<'a>(
        &'a self,
        _: &'a CustodySlot,
        _: &'a ExpectedFence,
    ) -> CustodyFuture<'a, CredentialFields> {
        Box::pin(async { Err(CustodyError::Internal) })
    }
}

#[tokio::test]
async fn current_gateway_custody_refuses_legacy_native_authority_but_keeps_other_v1_kinds()
-> TestResult {
    let slot = ReadySlot;
    let allowed = ["claude", "openai", "anthropic", "openrouter", "xai-api"];
    let refused = ["codex", "codex/work", "grok"];
    for served in [vec![1], vec![1, 7]] {
        let mut local = LocalCustody::new(new_instance().ok_or("gateway instance")?);
        for kind in allowed.into_iter().chain(["codex", "grok"]) {
            local = local.with_kind(kind, Arc::new(ReadySlot), &[]);
        }
        let server = InProcessGateway::server(local, crate::gateway::server::http1_upstream()?);
        let endpoint = if served == [1] {
            previous_gateway_endpoint(server)?
        } else {
            advertised_gateway_endpoint(server, served.clone())?
        };
        let gateway = GatewayCustody::connect(endpoint, None, |_| None)?;
        let remote = gateway.custodian();
        for name in allowed.into_iter().chain(refused) {
            let name_slot = CustodySlot::new(name)?;
            let wanted = slot.facts(&name_slot).await?;
            let request = RefreshRequest::IfDue {
                expected: wanted.expected_fence().ok_or("fixture fence")?,
            };
            for answer in [
                remote.facts(&name_slot).await,
                remote.refresh(&name_slot, request).await,
            ] {
                if refused.contains(&name) {
                    assert!(
                        matches!(answer, Err(CustodyError::VersionMismatch { supported }) if supported == served),
                        "{name}"
                    );
                } else {
                    assert_eq!(answer?, wanted);
                }
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn gateway_rejection_refresh_refuses_selection_from_the_other_version() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed(&paths, super::super::super::unix_now()?)?;
    let upstream = upstream().await?;
    let mut subscriptions = NativeSubscriptions::new(paths);
    subscriptions.codex_test_transport = Some((
        plakous_http::Client::new_http1(plakous_http::Limits::default())?.with_test_dial_override(
            plakous_http::Origin::parse("https://auth.openai.com/")?,
            upstream.origin.clone(),
            upstream.client.clone(),
        ),
        plakous_openai::EndpointOrigin::parse(CHATGPT)?,
    ));
    let current = SubscriptionCustodian::new(subscriptions, NativeProvider::Codex);
    let legacy = current.for_gateway_v1();
    let local = LocalCustody::new(new_instance().ok_or("gateway instance")?).with_kind_versions(
        "codex",
        Arc::new(current),
        Some(Arc::new(legacy)),
        CODEX_UPSTREAM_PINS,
    );
    let server = InProcessGateway::server(local, upstream.client.clone());
    let current = remote_for(InProcessGateway::endpoint_for(&server))?;
    let legacy = remote_for(previous_gateway_endpoint(server)?)?;
    let slot = CustodySlot::new("codex")?;
    let v1 = legacy.facts(&slot).await?;
    let v2 = current.facts(&slot).await?;
    assert!(v1.selection_epoch_digest.is_some());
    assert!(v2.selection_epoch_digest.is_some());
    assert_ne!(v1.selection_epoch_digest, v2.selection_epoch_digest);
    assert_eq!(v1.expected_fence(), v2.expected_fence());
    for (remote, own, other) in [(&legacy, &v1, &v2), (&current, &v2, &v1)] {
        let result = remote
            .refresh(
                &slot,
                RefreshRequest::AfterRejection {
                    rejected: own.expected_fence().ok_or("fixture fence")?,
                    selection_epoch_digest: other.selection_epoch_digest,
                },
            )
            .await;
        let Err(CustodyError::FenceMoved { facts }) = result else {
            return Err(format!("expected fence_moved, got {result:?}").into());
        };
        assert_eq!(*facts, *own);
        assert_eq!(remote.facts(&slot).await?, *own);
    }
    assert!(upstream.take().is_empty(), "no token exchange");
    Ok(())
}

fn remote_for(
    endpoint: crate::gateway::GatewayEndpoint,
) -> TestResult<plakous_auth::custody::gateway::GatewayCustodian> {
    Ok(plakous_auth::custody::gateway::GatewayCustodian::new(
        Arc::new(RelayClient::new(endpoint, None)?),
        |_| Some(CODEX_CREDENTIAL_HEADERS),
    ))
}
