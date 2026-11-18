//! Local custody of the stored Codex login: facts equal to the route
//! binding, fields under the Ready lease, and the single writer's refresh.

use plakous_auth::custody::{CustodyError, CustodySlot, CustodyStatus, ExpectedFence};

use super::*;

fn slot() -> TestResult<CustodySlot> {
    Ok(CustodySlot::new("codex")?)
}

#[tokio::test]
async fn custody_facts_carry_the_exact_route_binding() -> TestResult {
    let account = "custody-facts-account";
    let subject = "custody-facts-subject";
    let (fixture, _peers, server) =
        renew_fixture(account, subject, now_unix_secs() + 3_600, 0, Vec::new()).await?;
    let facts = fixture.manager.custody_facts(&slot()?).await?;
    assert_eq!(facts.status, CustodyStatus::Ready);
    assert_eq!(
        facts.account_binding.map(|digest| *digest.as_bytes()),
        Some(*fixture.evidence.account_fingerprint().bytes())
    );
    assert_eq!(
        facts.credential_authority.map(|digest| *digest.as_bytes()),
        Some(*fixture.evidence.credential_authority_binding().bytes())
    );
    assert_eq!(
        facts
            .selection_epoch_digest
            .map(|digest| *digest.as_bytes()),
        Some(*fixture.evidence.credential_selection_epoch().bytes())
    );
    assert_eq!(
        facts.generation.map(CredentialGeneration::get),
        Some(fixture.evidence.selected_credential_generation())
    );
    assert_eq!(
        facts.access_expires_at_unix_ms,
        Some((now_unix_secs() + 3_600) * 1000)
    );
    let (requests, unexpected) = server.await??;
    assert_eq!(request_kinds(&requests), ["catalog"]);
    assert!(!unexpected, "reading custody facts reached the wire");
    Ok(())
}

#[tokio::test]
async fn custody_fields_come_from_the_exact_fence_under_its_lease() -> TestResult {
    let account = "custody-fields-account";
    let (fixture, _peers, _server) = renew_fixture(
        account,
        "custody-fields-subject",
        now_unix_secs() + 3_600,
        0,
        Vec::new(),
    )
    .await?;
    let slot = slot()?;
    let facts = fixture.manager.custody_facts(&slot).await?;
    let expected = facts.expected_fence().ok_or("no fence")?;
    let mut fields = fixture.manager.custody_fields(&slot, &expected).await?;
    assert_eq!(
        fields.names().collect::<Vec<_>>(),
        ["authorization", "chatgpt-account-id"]
    );
    assert!(fields.take_hold().is_some(), "the Ready lease is held");
    assert_eq!(
        fields
            .take("authorization")
            .ok_or("authorization")?
            .into_header_bytes(),
        b"Bearer opaque-seeded-access"
    );
    assert_eq!(
        fields
            .take("chatgpt-account-id")
            .ok_or("account")?
            .into_header_bytes(),
        account.as_bytes()
    );
    let moved = ExpectedFence {
        generation: expected.generation.next()?,
        ..expected
    };
    assert!(matches!(
        fixture.manager.custody_fields(&slot, &moved).await,
        Err(CustodyError::FenceMoved { .. })
    ));
    let foreign = ExpectedFence {
        account_binding: plakous_auth::custody::IdentityDigest::from_bytes([7; 32]),
        ..expected
    };
    assert!(matches!(
        fixture.manager.custody_fields(&slot, &foreign).await,
        Err(CustodyError::AccountChanged)
    ));
    Ok(())
}

#[tokio::test]
async fn concurrent_rejections_of_one_generation_claim_exactly_one_grant() -> TestResult {
    let account = "custody-single-writer-account";
    let subject = "custody-single-writer-subject";
    let (fixture, peers, server) = renew_fixture(
        account,
        subject,
        now_unix_secs() + 3_600,
        1,
        refresh_exchange(subject)?,
    )
    .await?;
    let slot = slot()?;
    let facts = fixture.manager.custody_facts(&slot).await?;
    let rejected = facts.expected_fence().ok_or("no fence")?;
    let selection = facts.selection_epoch_digest;
    // One manager's callers serialize on its lifecycle lock; separate
    // processes serialize on the store's refresh claim.
    let peer = peers.first().ok_or("no peer")?;
    let refreshes: Vec<_> = (0..4)
        .map(|_| {
            let (peer, slot) = (Arc::clone(peer), slot.clone());
            tokio::spawn(async move {
                peer.custody_refresh_after_rejection(&slot, &rejected, selection)
                    .await
            })
        })
        .collect();
    let mut answers = Vec::new();
    for refresh in refreshes {
        answers.push(refresh.await?);
    }
    let current = fixture.manager.custody_facts(&slot).await?;
    assert_eq!(current.status, CustodyStatus::Ready);
    assert_eq!(current.account_binding, facts.account_binding);
    assert_eq!(current.selection_epoch_digest, facts.selection_epoch_digest);
    assert!(current.generation > Some(rejected.generation));
    for answer in answers {
        assert_eq!(answer?.generation, current.generation);
    }
    let (requests, unexpected) = server.await??;
    assert_eq!(request_kinds(&requests), ["catalog", "refresh", "userinfo"]);
    assert!(!unexpected, "a second grant reached the token endpoint");
    Ok(())
}
