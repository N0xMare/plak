//! Local custody of the stored Claude login: facts equal to the route
//! binding, the field under the Ready lease, and the single writer's refresh.

use super::tests::{local_peer, local_response, manager_with_client, private_store};
use super::*;
use crate::oauth::CLAUDE_OAUTH_INFERENCE_SCOPE;
use plakous_auth::custody::{CustodyError, CustodySlot, CustodyStatus, ExpectedFence};
use std::time::Duration;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn token_reply(access: &str) -> Vec<u8> {
    local_response(
        200,
        "application/json",
        &format!(
            r#"{{"access_token":"{access}","refresh_token":"rotated-refresh","expires_in":3600,"scope":"{CLAUDE_OAUTH_INFERENCE_SCOPE}","account":{{"uuid":"{}"}}}}"#,
            tests::TEST_GRANT_BINDING
        ),
    )
}

#[tokio::test]
async fn custody_facts_carry_the_exact_route_binding() -> TestResult {
    let temp = tempfile::tempdir()?;
    let manager = manager_with_client(
        private_store(&temp),
        Client::new_http1(plakous_http::Limits::default())?,
    );
    manager.persist_test_grant("access", "refresh", 3600, CLAUDE_OAUTH_INFERENCE_SCOPE)?;
    let (_model, authority, _descriptor) = manager
        .resolve_oauth_model(
            "claude-sonnet-5",
            Client::new_http1(plakous_http::Limits::default())?,
            SessionLineage::Root,
        )
        .await?;
    let facts = manager.custody_facts(&CustodySlot::new("claude")?).await?;
    assert_eq!(facts.status, CustodyStatus::Ready);
    assert_eq!(
        facts.account_binding.map(|digest| *digest.as_bytes()),
        Some(*authority.account_binding().bytes())
    );
    assert_eq!(
        facts.credential_authority.map(|digest| *digest.as_bytes()),
        Some(*authority.credential_authority_binding().bytes())
    );
    assert_eq!(
        facts
            .generation
            .map(plakous_auth::CredentialGeneration::get),
        Some(authority.credential_generation())
    );
    assert_eq!(
        facts
            .wire_identity
            .get(crate::CLAUDE_ACCOUNT_UUID_IDENTITY)
            .map(String::as_str),
        Some(tests::TEST_GRANT_BINDING)
    );
    assert!(
        facts
            .wire_identity
            .contains_key(crate::CLAUDE_DEVICE_ID_IDENTITY)
    );
    assert!(facts.selection_epoch_digest.is_some());
    Ok(())
}

#[tokio::test]
async fn custody_fields_come_from_the_exact_fence_under_its_lease() -> TestResult {
    let temp = tempfile::tempdir()?;
    let manager = manager_with_client(
        private_store(&temp),
        Client::new_http1(plakous_http::Limits::default())?,
    );
    manager.persist_test_grant("access", "refresh", 3600, CLAUDE_OAUTH_INFERENCE_SCOPE)?;
    let slot = CustodySlot::new("claude")?;
    let expected = manager
        .custody_facts(&slot)
        .await?
        .expected_fence()
        .ok_or("no fence")?;
    let mut fields = manager.custody_fields(&slot, &expected).await?;
    assert!(fields.take_hold().is_some(), "the Ready lease is held");
    assert_eq!(
        fields
            .take("authorization")
            .ok_or("authorization")?
            .into_header_bytes(),
        b"Bearer access"
    );
    let moved = ExpectedFence {
        generation: expected.generation.next()?,
        ..expected
    };
    assert!(matches!(
        manager.custody_fields(&slot, &moved).await,
        Err(CustodyError::FenceMoved { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn concurrent_rejections_of_one_generation_claim_exactly_one_grant() -> TestResult {
    let temp = tempfile::tempdir()?;
    // A second scripted grant stays unserved unless a second refresh runs.
    let (client, origin, peer) =
        local_peer(vec![token_reply("rotated-access"), token_reply("second")]).await;
    let manager =
        manager_with_client(private_store(&temp), client).with_test_token_origin(&origin)?;
    manager.persist_test_grant("access", "refresh", 3600, CLAUDE_OAUTH_INFERENCE_SCOPE)?;
    let slot = CustodySlot::new("claude")?;
    let facts = manager.custody_facts(&slot).await?;
    let rejected = facts.expected_fence().ok_or("no fence")?;
    let selection = facts.selection_epoch_digest;
    let refreshes: Vec<_> = (0..4)
        .map(|_| {
            let (manager, slot) = (manager.clone(), slot.clone());
            tokio::spawn(async move {
                manager
                    .custody_refresh_after_rejection(&slot, &rejected, selection)
                    .await
            })
        })
        .collect();
    let mut answers = Vec::new();
    for refresh in refreshes {
        answers.push(refresh.await??);
    }
    let current = manager
        .custody_facts(&slot)
        .await?
        .generation
        .ok_or("no generation")?;
    assert!(current > rejected.generation);
    for answer in answers {
        assert_eq!(answer.status, CustodyStatus::Ready);
        assert_eq!(answer.generation, Some(current));
        assert_eq!(answer.account_binding, facts.account_binding);
    }
    let mut fields = manager
        .custody_fields(
            &slot,
            &ExpectedFence {
                generation: current,
                ..rejected
            },
        )
        .await?;
    assert_eq!(
        fields
            .take("authorization")
            .ok_or("authorization")?
            .into_header_bytes(),
        b"Bearer rotated-access"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), peer)
            .await
            .is_err(),
        "a second grant reached the token endpoint"
    );
    Ok(())
}
