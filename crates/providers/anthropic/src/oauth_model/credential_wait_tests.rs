#![allow(
    clippy::too_many_lines,
    reason = "Drive a durable Claude root through credential parking, relogin and resumed inference with controlled peers."
)]
use super::auth_binding_tests::configured_builder;
use super::tests::{
    LOCAL_SUCCESS_SSE, local_peer_with_first_response_gate, local_response, manager_with_client,
    private_store, request_gate,
};
use super::*;
use crate::oauth::{CLAUDE_OAUTH_INFERENCE_SCOPE, CLAUDE_OAUTH_TOKEN_PATH};
use plakous_agent::internal::{DurableOperationKind, DurableOperationState};
use plakous_agent::{AgentEventKind, CommandId, RequestSlots, TurnResultOutcome};
use plakous_auth::{
    CredentialStatus,
    subscription_limit::{SubscriptionRequestClass, SubscriptionRequestGate},
};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::test]
async fn denied_refresh_of_an_expiring_ready_credential_never_parks_or_spins() -> TestResult {
    let temp = tempfile::tempdir()?;
    let (client, origin, peer) = tests::local_peer(vec![]).await;
    let gate = request_gate(1, 1);
    let _reserved = gate.acquire(SubscriptionRequestClass::AuthRefresh)?;
    let manager = manager_with_client(private_store(&temp), client.clone())
        .with_test_token_origin(&origin)?
        .with_subscription_request_gate(gate);
    manager.persist_test_grant("access", "refresh", 3600, CLAUDE_OAUTH_INFERENCE_SCOPE)?;
    let (model, authority, descriptor) = manager
        .resolve_oauth_model("claude-sonnet-5", client, SessionLineage::Root)
        .await?;
    let agent = configured_builder(model, authority, descriptor, temp.path())?.build()?;
    // A Ready credential enters the refresh lead after resolution. AuthRefresh
    // is denied before any HTTP request, and logging in is not the remedy.
    manager.persist_test_grant("access", "refresh", 1, CLAUDE_OAUTH_INFERENCE_SCOPE)?;
    let mut turn = agent.prompt("do not spin on a Ready credential").await?;
    let mut events = turn.take_events()?;
    let result = timeout(Duration::from_secs(2), turn.result()).await;
    agent.shutdown().await?;
    assert!(
        manager
            .status()?
            .is_some_and(|state| state.status() == CredentialStatus::Ready)
    );
    let result = result??;
    assert!(!matches!(result.outcome(), TurnResultOutcome::Completed(_)));
    while let Ok(event) = events.recv().await {
        assert!(!matches!(
            event.kind(),
            AgentEventKind::ModelCredentialWait { .. }
        ));
    }
    assert_eq!(peer.await?, Vec::<Vec<u8>>::new());
    Ok(())
}

#[tokio::test]
async fn unchanged_ready_observation_does_not_wake_the_credential_wait() -> TestResult {
    let temp = tempfile::tempdir()?;
    let manager = manager_with_client(
        private_store(&temp),
        Client::new_http1(plakous_http::Limits::default())?,
    );
    manager.persist_test_grant("access", "refresh", 3600, CLAUDE_OAUTH_INFERENCE_SCOPE)?;
    let parked = manager.status()?;
    let mut wait = Box::pin(manager.wait_for_ready(parked));
    assert!(timeout(Duration::from_millis(50), &mut wait).await.is_err());
    manager.persist_test_grant(
        "new-access",
        "new-refresh",
        3600,
        CLAUDE_OAUTH_INFERENCE_SCOPE,
    )?;
    timeout(Duration::from_secs(3), wait).await??;
    Ok(())
}

struct RefreshBeforeSend {
    model: Arc<ClaudeOAuthLanguageModel>,
    prepared: AtomicUsize,
    inject: AtomicBool,
    logout: bool,
}

impl LanguageModel for RefreshBeforeSend {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            if self.prepared.load(Ordering::Acquire) != 0
                && self.inject.swap(false, Ordering::AcqRel)
            {
                if self.logout {
                    self.model
                        .credentials
                        .logout_local()
                        .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
                } else {
                    assert!(
                        self.model
                            .credentials
                            .refresh_after_rejection()
                            .await
                            .is_err()
                    );
                }
            }
            Arc::clone(&self.model)
                .revalidate_runtime_authority(checked_at)
                .await
        })
    }

    fn wait_for_runtime_authority(
        self: Arc<Self>,
    ) -> plakous_model::RuntimeModelAuthorityWaitOperation {
        Arc::clone(&self.model).wait_for_runtime_authority()
    }

    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let prepared = Arc::clone(&self.model).prepare_call(plan, context)?;
        self.prepared.fetch_add(1, Ordering::AcqRel);
        Ok(prepared)
    }
}

#[derive(Clone, Copy)]
enum EndWait {
    Login,
    Cancel,
    DifferentAccount,
    Shutdown,
}

async fn parked_root_with_running_child(end: EndWait) -> TestResult {
    parked_root(end, false).await
}

async fn parked_root(end: EndWait, logout: bool) -> TestResult {
    let temp = tempfile::tempdir()?;
    let observed = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut replies = vec![local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE)];
    if matches!(end, EndWait::Login) {
        replies.push(local_response(200, "text/event-stream", LOCAL_SUCCESS_SSE));
    }
    let (client, origin, peer) = local_peer_with_first_response_gate(
        replies,
        Some((Arc::clone(&observed), Arc::clone(&release))),
    )
    .await;
    // The same parser and injected status used by PLAK_TEST_FAULTS, without
    // mutating process-global environment or contacting a provider endpoint.
    let client = client.with_faults(Some(plakous_http::FaultPlan::parse(&format!(
        "path={CLAUDE_OAUTH_TOKEN_PATH},fault=400,nth=1"
    ))?));
    let gate = request_gate(1, 2);
    let manager = manager_with_client(private_store(&temp), client.clone())
        .with_test_token_origin(&origin)?
        .with_subscription_request_gate(gate.clone());
    manager.persist_test_grant(
        "first-access",
        "first-refresh",
        3600,
        CLAUDE_OAUTH_INFERENCE_SCOPE,
    )?;
    let slots = RequestSlots::new(
        "fixture",
        std::num::NonZeroUsize::new(2).ok_or("nonzero slots")?,
    );
    let (mut child_model, child_authority, child_descriptor) = manager
        .resolve_oauth_model("claude-sonnet-5", client.clone(), SessionLineage::Root)
        .await?;
    Arc::get_mut(&mut child_model)
        .ok_or("child model shared")?
        .test_dial_origin = Some(origin.clone());
    let child = configured_builder(child_model, child_authority, child_descriptor, temp.path())?
        .request_slots(slots.clone())
        .build()?;
    let child_turn = child.prompt("finish the child").await?;
    timeout(Duration::from_secs(10), observed.notified()).await?;
    let (mut root_model, authority, descriptor) = manager
        .resolve_oauth_model("claude-sonnet-5", client, SessionLineage::Root)
        .await?;
    Arc::get_mut(&mut root_model)
        .ok_or("root model shared")?
        .test_dial_origin = Some(origin);
    let model = Arc::new(RefreshBeforeSend {
        model: root_model,
        prepared: AtomicUsize::new(0),
        inject: AtomicBool::new(true),
        logout,
    });
    let root = configured_builder(model.clone(), authority, descriptor, temp.path())?
        .request_slots(slots.clone())
        .build()?;
    let mut turn = root
        .start(CommandId::new("parked-root")?, "finish after login")
        .await?
        .into_turn()
        .ok_or("root not admitted")?;
    let control = turn.control();
    let mut events = turn.take_events()?;
    timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                events.recv().await?.kind(),
                AgentEventKind::ModelCredentialWait { waiting: true }
            ) {
                return Ok::<_, Box<dyn std::error::Error>>(());
            }
        }
    })
    .await??;
    assert_eq!(
        manager
            .status()?
            .ok_or("missing credential status")?
            .status(),
        if logout {
            CredentialStatus::Revoked
        } else {
            CredentialStatus::RefreshOutcomeUnknown
        }
    );
    assert_eq!(
        slots.in_flight(),
        1,
        "root must release its request slot while the child runs"
    );
    let checkpoint = plakous_agent::internal::facade_support::agent_checkpoint(&root);
    let providers: Vec<_> = checkpoint
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        .filter(|row| matches!(row.kind(), DurableOperationKind::Provider { .. }))
        .collect();
    assert_eq!(providers.len(), 1);
    assert!(matches!(
        providers[0].state(),
        DurableOperationState::Prepared
    ));
    let prepared_checkpoint = checkpoint.id();
    release.notify_one();
    let child_result = timeout(Duration::from_secs(10), child_turn.result()).await??;
    assert!(matches!(
        child_result.outcome(),
        TurnResultOutcome::Completed(_)
    ));
    assert!(
        !control.is_settled(),
        "the root attempt must remain live while waiting"
    );
    assert_eq!(slots.in_flight(), 0);
    // This is the same content-free wake the host sends for completed child
    // mail: it must be acknowledged while the Prepared request is parked.
    timeout(Duration::from_secs(5), control.notice()).await??;
    timeout(
        Duration::from_secs(5),
        control.notice_at(prepared_checkpoint)?,
    )
    .await???;
    assert_eq!(
        plakous_agent::internal::facade_support::agent_checkpoint(&root).id(),
        prepared_checkpoint
    );
    match end {
        EndWait::Login => {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(3_601)).await;
            tokio::time::resume();
            manager.persist_test_grant(
                "second-access",
                "second-refresh",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
            )?;
        }
        EndWait::DifferentAccount => {
            manager.persist_test_grant_for_account(
                "second-access",
                "second-refresh",
                3600,
                CLAUDE_OAUTH_INFERENCE_SCOPE,
                "87654321-4321-4321-8321-cba987654321",
            )?;
        }
        EndWait::Cancel => {
            timeout(Duration::from_secs(5), control.request_cancel()).await??;
        }
        EndWait::Shutdown => {
            timeout(Duration::from_secs(5), root.shutdown()).await??;
        }
    }
    let result = timeout(Duration::from_secs(10), turn.result()).await??;
    match (end, result.outcome()) {
        (EndWait::Login, TurnResultOutcome::Completed(_))
        | (EndWait::Cancel | EndWait::Shutdown, TurnResultOutcome::Cancelled(_)) => {}
        (EndWait::DifferentAccount, TurnResultOutcome::Failed(error)) => {
            assert!(error.to_string().contains("account changed"));
        }
        _ => return Err(format!("unexpected root outcome: {:?}", result.outcome()).into()),
    }
    assert_eq!(
        model.prepared.load(Ordering::Acquire),
        1,
        "the captured request must not be prepared again"
    );
    let requests = timeout(Duration::from_secs(5), peer).await??;
    let expected = if matches!(end, EndWait::Login) { 2 } else { 1 };
    assert_eq!(
        requests.len(),
        expected,
        "one child dispatch and at most one resumed root dispatch"
    );
    assert_eq!(
        gate.snapshot()?
            .class(SubscriptionRequestClass::Inference)
            .dispatched(),
        u32::try_from(expected)?
    );
    root.shutdown().await?;
    child.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn refresh_400_parks_prepared_root_while_child_completes_then_login_dispatches_once()
-> TestResult {
    parked_root_with_running_child(EndWait::Login).await
}
#[tokio::test]
async fn cancel_while_waiting_for_login_publishes_clean_cancelled_terminal() -> TestResult {
    parked_root_with_running_child(EndWait::Cancel).await
}
#[tokio::test]
async fn different_account_login_ends_wait_without_root_dispatch() -> TestResult {
    parked_root_with_running_child(EndWait::DifferentAccount).await
}
#[tokio::test]
async fn shutdown_while_waiting_for_login_publishes_clean_cancelled_terminal() -> TestResult {
    parked_root_with_running_child(EndWait::Shutdown).await
}

#[tokio::test]
async fn logged_out_prepared_root_waits_for_login_without_ending_running_child() -> TestResult {
    parked_root(EndWait::Login, true).await
}
