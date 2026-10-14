//! Predicate parity over the closure-table fixtures (spec 01 §5.5). The live
//! open, `AgentSessions::inspect_recovery_sqlite` and the rehearsal read the
//! same head: the open from its own store, the read-only callers from one
//! `SqliteSessionStore::inspect_branch` snapshot, which decodes the stored
//! object graph through `RuntimeCheckpointCodec`. Each fixture is published
//! through that codec and read back, and every caller's view of the loaded
//! head must agree with the live verdict on the original.

use super::fold_rows::tests::{decided_round, with_fold};
use super::*;
use crate::runtime::durable::tests::SessionUpdateFixtureMode;
use crate::runtime::durable::tests::landed_round_abandonment::{
    completed_text_round_cut, completed_tool_round_cut, root_without_provider_cut,
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// One closure-table fixture and the verdict its row assigns, when the
/// fixture pins one.
struct Fixture {
    label: &'static str,
    cut: RuntimeCheckpoint,
    expected: Option<CheckpointRecoveryVerdict>,
}

async fn fixtures() -> TestResult<Vec<Fixture>> {
    use CheckpointRecoveryVerdict::{Abandon, Finish};
    let text = completed_text_round_cut().await?;
    let tool =
        completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, false).await?;
    let admission = root_without_provider_cut(false).await?;
    let continued = decided_round(&text, &RoundDecision::Continue)?;
    let overflow = decided_round(&text, &RoundDecision::OverflowFold)?;
    let mut fixtures = vec![
        Fixture {
            label: "finish: P Completed Terminal",
            cut: text.clone(),
            expected: Some(Finish),
        },
        Fixture {
            label: "root Prepared, nothing started",
            cut: root_without_provider_cut(true).await?,
            expected: Some(Abandon),
        },
        Fixture {
            label: "root Dispatched, no provider",
            cut: admission.clone(),
            expected: Some(Abandon),
        },
        Fixture {
            label: "P Finalized ToolBatch",
            cut: tool.clone(),
            expected: Some(Abandon),
        },
        Fixture {
            label: "P Completed ToolBatch, batch records Completed",
            cut: completed_tool_round_cut(SessionUpdateFixtureMode::JoinedForeground, false, true)
                .await?,
            expected: None,
        },
        Fixture {
            label: "P Finalized ToolBatch, live process session",
            cut: completed_tool_round_cut(SessionUpdateFixtureMode::Active, false, false).await?,
            expected: Some(Abandon),
        },
        Fixture {
            label: "P Finalized Continue",
            cut: continued.clone(),
            expected: Some(Abandon),
        },
        Fixture {
            label: "P Finalized OverflowFold, no compaction record",
            cut: overflow.clone(),
            expected: Some(Abandon),
        },
    ];
    for (label, decision) in [
        (
            "P Finalized RetryOutputLimit",
            RoundDecision::RetryOutputLimit,
        ),
        (
            "P Finalized RetryContentFilter",
            RoundDecision::RetryContentFilter,
        ),
    ] {
        fixtures.push(Fixture {
            label,
            cut: decided_round(&text, &decision)?,
            expected: Some(Abandon),
        });
    }
    for (label, source, committed) in [
        ("fold row: open admission", &admission, false),
        ("fold row: committed admission", &admission, true),
        ("fold row: committed overflow", &overflow, true),
        ("fold row: open scheduled after ToolBatch", &tool, false),
        ("fold row: open scheduled after Continue", &continued, false),
        ("fold row: committed scheduled after ToolBatch", &tool, true),
        (
            "fold row: committed scheduled after Continue",
            &continued,
            true,
        ),
    ] {
        fixtures.push(Fixture {
            label,
            cut: with_fold(source, committed)?,
            expected: Some(Abandon),
        });
    }
    Ok(fixtures)
}

/// The head a read-only caller loads: the fixture published through the
/// store's object-graph codec and decoded back.
fn loaded(cut: &RuntimeCheckpoint) -> TestResult<Arc<RuntimeCheckpoint>> {
    Ok(RuntimeCheckpointCodec::bundle(cut, None)?.validated_checkpoint()?)
}

#[tokio::test]
async fn every_closure_fixture_has_one_verdict_for_every_caller() -> TestResult {
    for fixture in fixtures().await? {
        let label = fixture.label;
        let live = checkpoint_recovery_verdict(&fixture.cut);
        if let Some(expected) = fixture.expected {
            assert_eq!(live, expected, "{label}");
        }
        assert!(
            !matches!(
                live,
                CheckpointRecoveryVerdict::Quiescent | CheckpointRecoveryVerdict::Refuse
            ),
            "{label}: {live:?}"
        );
        let head = loaded(&fixture.cut)?;
        // The rehearsal classifies the loaded head through the same verdict.
        assert_eq!(checkpoint_recovery_verdict(&head), live, "{label}");
        // `inspect_recovery_sqlite` reports the loaded head's exact working
        // phase and obligations, which are the original's.
        assert_eq!(
            head.manifest().lifecycle(),
            fixture.cut.manifest().lifecycle(),
            "{label}"
        );
        // The recovery-scope check accepts every verdict but a refusal.
        assert!(require_recoverable(&head).is_ok(), "{label}");
        // The open executes the closure the verdict names on the same bytes.
        let closes = verdict_closure(&head)?.is_some();
        assert_eq!(
            closes,
            matches!(
                live,
                CheckpointRecoveryVerdict::Abandon | CheckpointRecoveryVerdict::Finish
            ),
            "{label}"
        );
        assert_eq!(
            reopen_closes_working_turn(&head),
            reopen_closes_working_turn(&fixture.cut),
            "{label}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn finalized_terminal_without_root_terminal_is_integrity_not_restore() -> TestResult {
    let text = completed_text_round_cut().await?;
    let cut = decided_round(&text, &RoundDecision::Terminal)?;
    assert_eq!(
        checkpoint_recovery_verdict(&cut),
        CheckpointRecoveryVerdict::Refuse
    );
    assert!(require_recoverable(&cut).is_err());
    Ok(())
}
