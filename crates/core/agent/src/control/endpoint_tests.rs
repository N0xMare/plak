use std::error::Error;

use super::*;
use uuid::Uuid;

#[test]
fn storage_pressure_control_error_preserves_the_typed_agent_source() -> Result<(), Box<dyn Error>> {
    let store_error = crate::session::SessionStoreError::admission_capacity(
        crate::session::StoreCapacityBound::Checkpoints,
        57,
        64,
        crate::session::StoreAdmission::Ordinary,
    );
    let crate::session::SessionStoreError::Capacity(capacity) = store_error else {
        return Err("capacity fact".into());
    };
    let error = ControlError::storage_pressure(capacity);
    let source = error
        .source()
        .and_then(|source| source.downcast_ref::<crate::AgentError>())
        .ok_or("typed source")?;
    assert_eq!(source.store_error(), Some(store_error));
    Ok(())
}

#[tokio::test]
async fn lost_command_reply_is_unknown_not_a_definite_refusal() -> Result<(), Box<dyn Error>> {
    let (controls, mut receiver) = mpsc::channel(1);
    let task = tokio::spawn(async move {
        drop(receiver.recv().await);
    });
    let error = submit_actor_command(
        &controls,
        TurnCommand::Start {
            command_id: CommandId::new("lost-reply")?,
            input: "must reconcile".into(),
        },
        || {},
    )
    .await
    .err()
    .ok_or("lost reply must not be accepted")?;
    assert_eq!(error.kind(), ControlErrorKind::OutcomeUnknown);
    task.await?;
    Ok(())
}

#[tokio::test]
async fn compact_endpoint_lost_reply_is_not_unavailable() -> Result<(), Box<dyn Error>> {
    let (controls, mut receiver) = mpsc::channel(1);
    let endpoint = AgentSessionEndpoint::new(controls, Arc::default());
    let task = tokio::spawn(async move {
        drop(receiver.recv().await);
    });
    let error = endpoint
        .compact_session(compact_endpoint_request()?)
        .await
        .err()
        .ok_or("lost reply must fail")?;
    assert_eq!(error, AgentSessionEndpointError::OutcomeUnknown);
    task.await?;
    Ok(())
}

#[tokio::test]
async fn compact_endpoint_full_and_closed_never_ingress() -> Result<(), Box<dyn Error>> {
    let (controls, receiver) = mpsc::channel(1);
    let (reply, _reply_receiver) = oneshot::channel();
    assert!(
        controls
            .try_send(ActorControl::CompactSession {
                request: Box::new(compact_endpoint_request()?),
                reply,
            })
            .is_ok()
    );
    let endpoint = AgentSessionEndpoint::new(controls, Arc::default());
    assert_eq!(
        endpoint.compact_session(compact_endpoint_request()?).await,
        Err(AgentSessionEndpointError::Backpressure)
    );
    drop(receiver);
    assert_eq!(
        endpoint.compact_session(compact_endpoint_request()?).await,
        Err(AgentSessionEndpointError::Unavailable)
    );
    Ok(())
}

fn compact_endpoint_request() -> Result<AgentCompactSessionRequest, Box<dyn Error>> {
    Ok(AgentCompactSessionRequest {
        operation_id: "compact:endpoint".into(),
        session_id: SessionId::try_from(Uuid::from_u128(1))?,
        branch_id: BranchId::try_from(Uuid::from_u128(2))?,
        checkpoint_id: CheckpointId::try_from(Uuid::from_u128(3))?,
        reason: AgentCompactionReason::Manual,
        pending_user_input: None,
        runtime: None,
        fallback_of: None,
    })
}

#[tokio::test]
async fn external_placeholder_suffix_is_refused_before_endpoint_ingress()
-> Result<(), Box<dyn Error>> {
    let (controls, mut receiver) = mpsc::channel(1);
    let endpoint = AgentSessionEndpoint::new(controls, Arc::default());
    let mut request = compact_endpoint_request()?;
    request.operation_id = "x:placeholders".to_owned();
    // A liveness bound only: generous, so load never trips it.
    let result =
        tokio::time::timeout(Duration::from_secs(30), endpoint.compact_session(request)).await?;
    assert!(
        matches!(result, Err(AgentSessionEndpointError::Rejected { reason }) if reason.contains(":placeholders"))
    );
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[tokio::test]
async fn session_endpoint_reports_bounded_ingress_backpressure() -> Result<(), Box<dyn Error>> {
    let session_id = SessionId::try_from(Uuid::from_u128(1))?;
    let branch_id = BranchId::try_from(Uuid::from_u128(2))?;
    let checkpoint_id = CheckpointId::try_from(Uuid::from_u128(3))?;
    let (controls, _receiver) = mpsc::channel(1);
    let (reply, _reply_receiver) = oneshot::channel();
    assert!(
        controls
            .try_send(ActorControl::ContextStatus {
                request: AgentContextStatusRequest {
                    session_id,
                    branch_id,
                    checkpoint_id,
                    pending_user_input: None,
                },
                reply,
            })
            .is_ok()
    );
    let endpoint = AgentSessionEndpoint::new(controls, Arc::default());
    assert_eq!(
        endpoint
            .context_status(session_id, branch_id, checkpoint_id, None)
            .await,
        Err(AgentSessionEndpointError::Backpressure)
    );
    Ok(())
}
