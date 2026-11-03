//! The line head each agent's tool environment names for its operations.

use super::*;

fn owner_of(agent: &Agent, fence: u64) -> plakous_tools::LineOwner {
    let topology = agent.checkpoint().manifest().topology();
    plakous_tools::LineOwner::new(
        topology.line,
        topology.line_generation,
        agent.branch_id(),
        fence,
    )
}

#[tokio::test]
async fn an_ephemeral_agent_owns_its_own_line_at_generation_one_without_a_fence() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, _model) = placeholder_fixture(
        workspace,
        0x0a11_0000,
        40_000,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::Ephemeral,
    )?;
    let agent = builder.build()?;
    assert_eq!(
        agent.line_owner_for_test(),
        Some(plakous_tools::LineOwner::new(
            plakous_model::LineId::started_by(agent.branch_id()),
            1,
            agent.branch_id(),
            0,
        ))
    );
    agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn a_durable_agent_owns_its_line_head_under_the_lease_it_holds() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let semantic_store: Arc<dyn SessionStore> = store.clone();
    let sessions = DurableSessionRuntime::current(
        semantic_store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("nonzero lease")?,
    )?;
    let builder = || {
        durable_test_builder(
            Arc::clone(&workspace),
            one_tool_registry()?,
            "test.route",
            b"alpha",
            false,
        )
    };
    let created = sessions.create("create:line-owner", builder()?).await?;
    let created_fence = store
        .active_lease()
        .ok_or("create holds a lease")?
        .fence()
        .get();
    assert_eq!(
        created.line_owner_for_test(),
        Some(owner_of(&created, created_fence))
    );
    created.shutdown().await?;
    let address = DurableSessionAddress::new(
        created.session_id(),
        created.branch_id(),
        created.checkpoint().id(),
    );

    let DurableRuntimeOpen::Opened(reopened) = sessions
        .open("open:line-owner", address, builder()?)
        .await?
    else {
        return Err("quiescent session unexpectedly required recovery".into());
    };
    let reopened_fence = store
        .active_lease()
        .ok_or("open holds a lease")?
        .fence()
        .get();
    assert!(reopened_fence > created_fence, "a reopen holds a new lease");
    assert_eq!(
        reopened.line_owner_for_test(),
        Some(owner_of(&reopened, reopened_fence))
    );
    assert_eq!(
        reopened.checkpoint().manifest().topology().line,
        created.checkpoint().manifest().topology().line
    );
    reopened.shutdown().await?;
    Ok(())
}
