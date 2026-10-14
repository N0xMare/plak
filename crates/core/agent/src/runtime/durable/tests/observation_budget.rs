//! Observation allowances on rounds without provider usage, and across the
//! durable effect-decision and server-pause frames.
use super::*;

/// A round whose request was admitted on an unanchored estimate and whose
/// response reports no usage still grants half of the estimated headroom, so
/// a modest tool result is kept whole and no pressure fold runs.
#[tokio::test]
async fn a_usage_less_unanchored_round_keeps_half_its_estimated_headroom() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        workspace,
        0x0b5e_0001,
        1_000_000,
        &[25_000],
        &[0],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    // The wire's one-token-per-byte bound overflows the window while its
    // bytes/4 estimate fits, so admission delegates to the provider.
    model.item_context.store(true, Ordering::Release);
    model.item_system_bytes.store(1_200_000, Ordering::Release);
    model
        .usage_suppressed_until
        .store(usize::MAX, Ordering::Release);
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions.create("create:usage-less-round", builder).await?;
    let result = agent.prompt("read the file").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert!(
        result
            .checkpoint()
            .manifest()
            .canonical()
            .context()
            .request_anchor()
            .is_none(),
        "no round reported usage"
    );
    assert!(
        committed_tool_result_texts(result.checkpoint())
            .iter()
            .any(|text| *text == "x".repeat(25_000)),
        "half the estimated headroom keeps a 25 KB result whole"
    );
    assert_eq!(compaction_workflow_count(result.checkpoint()), 0);
    assert!(model.observed_compaction_contexts().is_empty());
    agent.shutdown().await?;
    Ok(())
}
