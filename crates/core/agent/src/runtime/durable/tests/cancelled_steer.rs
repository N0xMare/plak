//! A live cancelled turn settles its queued steer as unapplied.
use super::*;
use crate::feed::FeedFact;

#[derive(Default)]
struct HeldTool {
    started: AtomicBool,
    release: AtomicBool,
}

impl plakous_tools::ToolExecutor for HeldTool {
    fn execute(
        self: Arc<Self>,
        _: ToolInvocation,
        _: plakous_tools::ToolContext,
    ) -> plakous_tools::ToolFuture {
        Box::pin(async move {
            self.started.store(true, Ordering::Release);
            wait_for_scheduler_fact(&self.release).await;
            ToolOutcome::success_text("held result")
        })
    }
}

/// A steer queued behind a running tool never reaches the model when the turn
/// is cancelled: the result and the one terminal fact both say so.
#[tokio::test]
async fn live_cancel_settles_a_queued_steer_as_unapplied() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let descriptor = durable_test_descriptor("test.route")?;
    let model = Arc::new(
        ArtifactModel::new(
            descriptor.clone(),
            artifact_authority(&descriptor, 1)?,
            0,
            46_900,
            false,
        )
        .with_tool_first(),
    );
    let tool = Arc::new(HeldTool::default());
    let mut registry = ToolRegistryBuilder::default();
    registry.register(ToolRegistration::new(
        RegisteredToolId::new("probe.registration")?,
        ToolExecutorId::new("probe.executor")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"probe.executor"])?,
        ToolDefinition::new(
            ToolName::new("probe")?,
            "1.0.0",
            "Held probe.",
            ToolInputContract::structured(json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }))?,
            ToolOutputContract::model_content(128, 1_048_576)?,
            ToolConcurrency::Serial,
            ToolEffect::ReadOnly,
        )?,
        tool.clone(),
    ))?;
    let registry = registry.freeze();
    let store = Arc::new(TestCreateStore::default());
    let runtime = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = runtime
        .create(
            "create:cancel-steer",
            session_update_builder(workspace, model.clone(), registry)?,
        )
        .await?;
    let turn = agent.prompt("read").await?;
    wait_for_scheduler_fact(&tool.started).await;
    turn.control().steer("queued instruction").await?;
    turn.control().request_cancel().await?;
    tool.release.store(true, Ordering::Release);
    let result = turn.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Cancelled(_)),
        "{:?}",
        result.outcome()
    );
    assert!(result.has_unapplied_steering());
    assert_eq!(
        model.calls.load(Ordering::Acquire),
        1,
        "the steer was never sent"
    );
    let settlements = lock_mutex(&store.feed_facts)
        .iter()
        .flatten()
        .filter_map(|fact| match fact {
            FeedFact::TurnTerminal {
                turn_id,
                steering_settled_unapplied,
                ..
            } if *turn_id == result.turn_id() => Some(*steering_settled_unapplied),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(settlements, vec![true]);
    agent.shutdown().await?;
    Ok(())
}
