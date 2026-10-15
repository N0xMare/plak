//! Turn-admission compaction at the anchored occupancy trigger.
use super::*;

/// A child session has no host idle fence, so its turn admission is the only
/// fence before a new prompt. Once a completed round anchors occupancy at or
/// over the trigger, the next admitted turn folds first even though the
/// committed head still fits; a turn without an anchor never pays for it.
#[tokio::test]
async fn child_turn_admission_folds_at_the_anchored_occupancy_trigger() -> TestResult {
    for anchored in [true, false] {
        let directory = tempfile::tempdir()?;
        let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
        let (builder, model) = placeholder_fixture(
            workspace,
            0x0ad3_0000 + u128::from(anchored),
            40_000,
            &[],
            &[],
            false,
            LocalDurabilityDisposition::Ephemeral,
        )?;
        if !anchored {
            model
                .usage_suppressed_until
                .store(usize::MAX, Ordering::Release);
        }
        let agent = builder.build()?;
        assert!(matches!(
            agent
                .prompt("older intent ".repeat(200))
                .await?
                .result()
                .await?
                .outcome(),
            TurnResultOutcome::Completed(_)
        ));
        // The provider reports most of the window in use: the committed head
        // still fits, but occupancy now meets the trigger.
        model
            .item_provider_input_tokens
            .store(38_000, Ordering::Release);
        assert!(matches!(
            agent
                .prompt("second intent ".repeat(200))
                .await?
                .result()
                .await?
                .outcome(),
            TurnResultOutcome::Completed(_)
        ));
        assert_eq!(model.paid_compaction_calls.load(Ordering::Acquire), 0);
        let result = agent.prompt("short follow-up").await?.result().await?;
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(result.model_rounds(), 1);
        assert_eq!(
            model.paid_compaction_calls.load(Ordering::Acquire),
            usize::from(anchored),
            "anchored={anchored}"
        );
        assert_eq!(
            compaction_workflow_count(result.checkpoint()),
            usize::from(anchored),
            "anchored={anchored}"
        );
        agent.shutdown().await?;
    }
    Ok(())
}
