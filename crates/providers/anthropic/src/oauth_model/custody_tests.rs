//! Local custody of the stored Claude login: facts equal to the route
//! binding, the field under the Ready lease, and the single writer's refresh.

use super::tests::{local_peer, local_response, manager_with_client, private_store};
use super::*;
use crate::oauth::CLAUDE_OAUTH_INFERENCE_SCOPE;
use plakous_auth::custody::{CustodySlot, CustodyStatus, ExpectedFence};
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

mod conformance {
    use super::*;
    use plakous_test_support::native_auth::TestResult;
    use plakous_test_support::native_auth::custody as suite;

    #[tokio::test]
    async fn exact_lease_and_moved_fence() -> TestResult {
        let temp = tempfile::tempdir()?;
        let manager = manager_with_client(
            private_store(&temp),
            Client::new_http1(plakous_http::Limits::default())?,
        );
        manager.persist_test_grant("access", "refresh", 3600, CLAUDE_OAUTH_INFERENCE_SCOPE)?;
        let slot = CustodySlot::new("claude")?;
        suite::fields_hold_exact_fence(&manager, &slot, &[("authorization", b"Bearer access")])
            .await?;
        suite::fields_name_the_moved_fence(&manager, &slot, || {
            Ok(manager.refresh_between_custody_reads()?)
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn rejection_refresh_returns_busy_while_an_exchange_holds_the_ready_lease() -> TestResult
    {
        let temp = tempfile::tempdir()?;
        let manager = manager_with_client(
            private_store(&temp),
            Client::new_http1(plakous_http::Limits::default())?,
        );
        manager.persist_test_grant("access", "refresh", 3600, CLAUDE_OAUTH_INFERENCE_SCOPE)?;
        suite::rejected_while_leased_is_busy(&manager, &CustodySlot::new("claude")?).await?;
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_rejections_of_one_generation_claim_exactly_one_grant() -> TestResult {
        let temp = tempfile::tempdir()?;
        // A second scripted grant stays unserved unless a second refresh runs.
        let (client, origin, peer) =
            local_peer(vec![token_reply("rotated-access"), token_reply("second")]).await;
        let manager = Arc::new(
            manager_with_client(private_store(&temp), client).with_test_token_origin(&origin)?,
        );
        manager.persist_test_grant("access", "refresh", 3600, CLAUDE_OAUTH_INFERENCE_SCOPE)?;
        let slot = CustodySlot::new("claude")?;
        suite::concurrent_rejections_adopt_one_replacement(manager.as_ref(), &manager, &slot)
            .await?;
        suite::fields_hold_exact_fence(
            manager.as_ref(),
            &slot,
            &[("authorization", b"Bearer rotated-access")],
        )
        .await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(300), peer)
                .await
                .is_err(),
            "a second grant reached the token endpoint"
        );
        Ok(())
    }
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

/// Local custody of one manager, served as a credential custodian.
struct Served(ClaudeNativeAuthManager);

impl plakous_auth::custody::CredentialCustodian for Served {
    fn facts<'a>(
        &'a self,
        slot: &'a CustodySlot,
    ) -> plakous_auth::custody::CustodyFuture<'a, plakous_auth::custody::AccountFacts> {
        Box::pin(self.0.custody_facts(slot))
    }

    fn refresh<'a>(
        &'a self,
        slot: &'a CustodySlot,
        request: plakous_auth::custody::RefreshRequest,
    ) -> plakous_auth::custody::CustodyFuture<'a, plakous_auth::custody::AccountFacts> {
        Box::pin(async move {
            if let plakous_auth::custody::RefreshRequest::AfterRejection {
                rejected,
                selection_epoch_digest,
            } = request
            {
                self.0
                    .custody_refresh_after_rejection(slot, &rejected, selection_epoch_digest)
                    .await
            } else {
                let _ = self.0.refresh_if_needed().await;
                self.0.custody_facts(slot).await
            }
        })
    }

    fn fields<'a>(
        &'a self,
        slot: &'a CustodySlot,
        expected: &'a ExpectedFence,
    ) -> plakous_auth::custody::CustodyFuture<'a, plakous_auth::custody::CredentialFields> {
        Box::pin(self.0.custody_fields(slot, expected))
    }
}

#[tokio::test]
async fn a_custodied_login_binds_and_sends_exactly_what_the_local_login_does() -> TestResult {
    let temp = tempfile::tempdir()?;
    let client = Client::new_http1(plakous_http::Limits::default())?;
    let local = manager_with_client(private_store(&temp), client.clone())
        .with_credential_source_scope("claude/work".into());
    local.persist_test_grant("access", "refresh", 3600, CLAUDE_OAUTH_INFERENCE_SCOPE)?;
    let (_, bound, _) = local
        .resolve_oauth_model("claude-sonnet-5", client.clone(), SessionLineage::Root)
        .await?;
    let acceptance = crate::ClaudeCompatibilityAcceptance::accept_exact(
        CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
        CLAUDE_OAUTH_SOURCE_REVISION,
        crate::oauth::CLAUDE_OAUTH_NOTICE_REVISION,
        crate::oauth::CLAUDE_OAUTH_KILL_SWITCH_REVISION,
        Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0)?,
    )?;
    let custodied = ClaudeNativeAuthManager::custodied(
        Arc::new(Served(local.clone())),
        CustodySlot::new("claude/work")?,
        client.clone(),
        acceptance,
    )
    .await?
    .with_credential_source_scope("claude/work".into());

    // The same binding, revalidated under its own scope.
    let (model, reopened, _) = custodied
        .resolve_oauth_model("claude-sonnet-5", client, SessionLineage::Root)
        .await?;
    assert_eq!(reopened.account_binding(), bound.account_binding());
    assert_eq!(
        reopened.credential_authority_binding(),
        bound.credential_authority_binding()
    );
    assert_eq!(
        reopened.credential_generation(),
        bound.credential_generation()
    );
    let revalidated = model
        .revalidate_runtime_authority(reopened.observed_at())
        .await?;
    assert_eq!(
        revalidated.credential_authority_binding(),
        bound.credential_authority_binding()
    );

    // The same request identity and `authorization` bytes.
    let ((access, generation, _), identity) = custodied.identity_credentials()?;
    let ((local_access, local_generation, _), local_identity) = local.identity_credentials()?;
    assert_eq!(identity, local_identity);
    assert_eq!(generation, local_generation);
    let crate::native_auth::ClaudeOAuthAccess::Custodied(field) = access else {
        return Err("a custodied login sends the custodian's field".into());
    };
    assert_eq!(
        field.to_vec(),
        format!("Bearer {}", local_access.token().ok_or("local token")?).into_bytes()
    );
    assert_eq!(custodied.status()?, local.status()?);
    assert!(custodied.logout_local().is_err());
    Ok(())
}
