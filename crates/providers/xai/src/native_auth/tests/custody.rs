//! Local custody of the stored SuperGrok login: facts equal to the route
//! binding, fields under the Ready lease, and the single writer's refresh.

use plakous_auth::custody::{CustodySlot, CustodyStatus};

use super::*;

mod conformance {
    use super::*;
    use plakous_test_support::native_auth::custody as suite;

    fn seed(manager: &XaiNativeAuthManager, account: &str) -> TestResult {
        Ok(install_stored(
            manager,
            account,
            "custody-access",
            "custody-refresh",
            system_unix_seconds()? + 3_600,
        )?)
    }

    #[tokio::test]
    async fn exact_lease_and_moved_fence() -> TestResult {
        let _serial = AUTH_TEST_GATE.lock().await;
        let temp = tempdir()?;
        let peer = mock_peer().await?;
        let manager = manager(&temp, &peer)?;
        let account = "conformance-fields";
        seed(&manager, account)?;
        let slot = CustodySlot::new("grok")?;
        suite::fields_hold_exact_fence(
            &manager,
            &slot,
            &[
                ("authorization", b"Bearer custody-access"),
                ("x-userid", account.as_bytes()),
            ],
        )
        .await?;
        suite::fields_name_the_moved_fence(&manager, &slot, || {
            Ok(manager.refresh_between_custody_reads()?)
        })
        .await?;
        peer.shutdown().await
    }

    #[tokio::test]
    async fn rejection_refresh_returns_busy_while_an_exchange_holds_the_ready_lease() -> TestResult
    {
        let _serial = AUTH_TEST_GATE.lock().await;
        let temp = tempdir()?;
        let peer = mock_peer().await?;
        let manager = manager(&temp, &peer)?;
        seed(&manager, "conformance-busy")?;
        suite::rejected_while_leased_is_busy(&manager, &CustodySlot::new("grok")?).await?;
        peer.shutdown().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_rejections_of_one_generation_claim_exactly_one_grant() -> TestResult {
        let _serial = AUTH_TEST_GATE.lock().await;
        let temp = tempdir()?;
        let peer = mock_peer().await?;
        let manager = Arc::new(manager(&temp, &peer)?);
        let account = "conformance-single-writer";
        seed(&manager, account)?;
        peer.responses
            .send(response(&token_response(
                "renewed-access",
                "renewed-refresh",
                &id_token(account, None)?,
            )))
            .await?;
        peer.responses.send(response(&jwks(TEST_KID))).await?;
        suite::concurrent_rejections_adopt_one_replacement(
            manager.as_ref(),
            &manager,
            &CustodySlot::new("grok")?,
        )
        .await?;
        wait_for_requests(&peer, 2).await?;
        assert_eq!(
            peer.requests
                .lock()
                .await
                .iter()
                .filter(|request| request.starts_with(b"POST /oauth2/token "))
                .count(),
            1,
        );
        peer.shutdown().await
    }
}

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
    assert_eq!(
        facts
            .wire_identity
            .get(SUPERGROK_SELECTION_EPOCH_IDENTITY)
            .map(String::as_str),
        Some(evidence.credential_selection_epoch())
    );
    assert_eq!(
        facts
            .wire_identity
            .get(SUPERGROK_SUBJECT_IDENTITY)
            .map(String::as_str),
        Some("custody-subject")
    );
    auth.shutdown().await?;
    Ok(())
}
