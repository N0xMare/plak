//! Local custody of the stored SuperGrok login: facts equal to the route
//! binding, fields under the Ready lease, and the single writer's refresh.

use plakous_auth::custody::{CustodyError, CustodySlot, CustodyStatus, ExpectedFence};

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custody_facts_carry_the_exact_route_binding() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let auth = mock_peer().await?;
    let manager = manager(&temp, &auth)?;
    install_stored(
        &manager,
        "custody-subject",
        "custody-access",
        "custody-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    let catalog_body = json!({
        "object":"list",
        "data":[{
            "id":XAI_GROK_PRIMARY_CODING_MODEL,
            "object":"model",
            "created":1,
            "owned_by":"xai",
            "contextWindow":4096
        }]
    });
    auth.responses.send(response(&catalog_body)).await?;
    let (_model, evidence) = manager
        .resolve_model_with_evidence(
            XAI_GROK_PRIMARY_CODING_MODEL,
            auth.client.clone(),
            StandardXaiHttpConfig::default(),
        )
        .await?;
    let slot = CustodySlot::new("grok")?;
    let facts = manager.custody_facts(&slot).await?;
    assert_eq!(facts.status, CustodyStatus::Ready);
    assert_eq!(
        facts.account_binding.map(|digest| *digest.as_bytes()),
        Some(*evidence.account_fingerprint().bytes())
    );
    assert_eq!(
        facts.credential_authority.map(|digest| *digest.as_bytes()),
        Some(*evidence.credential_authority_binding().bytes())
    );
    assert_eq!(
        facts.generation.map(CredentialGeneration::get),
        Some(evidence.selected_credential_generation())
    );
    let expected = facts.expected_fence().ok_or("no fence")?;
    let mut fields = manager.custody_fields(&slot, &expected).await?;
    assert!(fields.take_hold().is_some(), "the Ready lease is held");
    assert_eq!(
        fields.names().collect::<Vec<_>>(),
        ["authorization", "x-userid"]
    );
    assert_eq!(
        fields
            .take("x-userid")
            .ok_or("subject")?
            .into_header_bytes(),
        b"custody-subject"
    );
    let moved = ExpectedFence {
        generation: expected.generation.next()?,
        ..expected
    };
    assert!(matches!(
        manager.custody_fields(&slot, &moved).await,
        Err(CustodyError::FenceMoved { .. })
    ));
    auth.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_rejections_of_one_generation_claim_exactly_one_grant() -> TestResult {
    let _serial = AUTH_TEST_GATE.lock().await;
    let temp = tempdir()?;
    let peer = mock_peer().await?;
    let manager = Arc::new(manager(&temp, &peer)?);
    install_stored(
        &manager,
        "rejected-subject",
        "rejected-access",
        "rejected-refresh",
        system_unix_seconds()? + 3_600,
    )?;
    peer.responses
        .send(response(&token_response(
            "renewed-access",
            "renewed-refresh",
            &id_token("rejected-subject", None)?,
        )))
        .await?;
    peer.responses.send(response(&jwks(TEST_KID))).await?;
    let slot = CustodySlot::new("grok")?;
    let facts = manager.custody_facts(&slot).await?;
    let rejected = facts.expected_fence().ok_or("no fence")?;
    let selection = facts.selection_epoch_digest;
    let refreshes: Vec<_> = (0..4)
        .map(|_| {
            let (manager, slot) = (Arc::clone(&manager), slot.clone());
            tokio::spawn(async move {
                manager
                    .custody_refresh_after_rejection(&slot, &rejected, selection)
                    .await
            })
        })
        .collect();
    let current = Some(rejected.generation.next()?.next()?);
    for refresh in refreshes {
        let answer = tokio::time::timeout(Duration::from_secs(30), refresh).await???;
        assert_eq!(answer.status, CustodyStatus::Ready);
        assert_eq!(answer.generation, current);
        assert_eq!(answer.selection_epoch_digest, selection);
    }
    wait_for_requests(&peer, 2).await?;
    let requests = peer.requests.lock().await;
    assert_eq!(
        requests
            .iter()
            .filter(|request| String::from_utf8_lossy(request).starts_with("POST /oauth2/token"))
            .count(),
        1
    );
    drop(requests);
    peer.shutdown().await?;
    Ok(())
}
