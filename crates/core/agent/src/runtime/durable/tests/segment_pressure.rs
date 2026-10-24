//! Segment-count pressure and the segment refusal below the checkpoint bound.
//!
//! A session of small turns reaches the checkpoint's history-segment bound long
//! before any token trigger. Each test seeds a durable session whose genesis
//! already holds the segment count under test, so no test drives a thousand
//! turns.
use super::fence_compaction::compaction_workflow_count;
use super::*;
use crate::{MAX_RUNTIME_RECORDS, SEGMENT_PRESSURE_SEGMENTS, SEGMENT_REFUSAL_SEGMENTS};

/// A window no seeded history here comes near, so only the segment count acts.
const CEILING: u64 = 4_000_000;

/// `segments` alternating one-line user and assistant groups.
fn small_turns(segments: usize, entry_base: u128) -> TestResult<CanonicalHistory> {
    let mut groups = Vec::with_capacity(segments);
    for index in 0..segments {
        let (actor, provenance, kind) = if index % 2 == 0 {
            (
                Actor::User,
                Provenance::UserInput,
                HistorySegmentKind::RecentIntent,
            )
        } else {
            (
                Actor::Assistant,
                Provenance::ModelOutput,
                HistorySegmentKind::Conversation,
            )
        };
        let entry = EntryId::try_from(uuid::Uuid::from_u128(entry_base + u128::try_from(index)?))?;
        groups.push(HistorySegment::new(
            kind,
            vec![ConversationItem::new(
                entry,
                actor,
                vec![ContentBlock::Text(TextBlock::new(
                    format!("small turn {index}"),
                    None,
                )?)],
                provenance,
            )?],
        )?);
    }
    Ok(CanonicalHistory::new(groups)?)
}

struct Seeded {
    sessions: DurableSessionRuntime,
    agent: Agent,
    model: Arc<ArtifactModel>,
    workspace: Arc<WorkspacePolicy>,
    _directory: tempfile::TempDir,
}

/// A durable session seeded with `segments` small-turn groups.
async fn seeded(
    name: &str,
    segments: usize,
    compaction: bool,
    tool_rounds: &[usize],
) -> TestResult<Seeded> {
    seeded_on(name, segments, compaction, tool_rounds, false).await
}

/// [`seeded`] on a route whose native compaction facet is enrolled when `facet`.
async fn seeded_on(
    name: &str,
    segments: usize,
    compaction: bool,
    tool_rounds: &[usize],
    facet: bool,
) -> TestResult<Seeded> {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let (builder, model) = placeholder_fixture(
        Arc::clone(&workspace),
        0x5e6_0000,
        CEILING,
        &[200; 4],
        tool_rounds,
        facet,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    model.script_tool_rounds_around_compactions();
    let sessions = DurableSessionRuntime::current(
        Arc::new(TestCreateStore::default()),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create_seeded(
            name,
            builder.with_scheduled_compaction_enabled(compaction),
            small_turns(segments, 0x5e6_0000_0000)?,
        )
        .await?;
    assert_eq!(head_segments(&agent.checkpoint()), segments);
    Ok(Seeded {
        sessions,
        agent,
        model,
        workspace,
        _directory: directory,
    })
}

fn head_segments(checkpoint: &RuntimeCheckpoint) -> usize {
    checkpoint.manifest().canonical().history().segments().len()
}

fn assert_quiescent(checkpoint: &RuntimeCheckpoint) {
    assert!(matches!(
        checkpoint.manifest().lifecycle(),
        RuntimeLifecycle::Quiescent
    ));
}

/// The typed refusal: a context-limit failure with the Compact remedy whose
/// message names the remedy for the session's compaction setting.
fn assert_segment_refusal(result: &TurnResult, compaction: bool) {
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        panic!("expected the segment refusal, got {:?}", result.outcome());
    };
    assert_eq!(error.model_kind(), Some(ModelErrorKind::ContextLimit));
    assert!(
        error
            .message()
            .contains(&format!("{SEGMENT_REFUSAL_SEGMENTS}-segment limit")),
        "{error:?}"
    );
    let remedy = if compaction {
        "compact the session or fork it"
    } else {
        "compact the session, or enable scheduled compaction"
    };
    assert!(error.message().contains(remedy), "{error:?}");
    assert_eq!(
        result.context_overflow_remedy(),
        Some(crate::ContextOverflowRemedy::Compact)
    );
    assert_quiescent(result.checkpoint());
}

/// At the pressure threshold the idle-side turn fence pays for one fold even
/// though the history is far below the token trigger; one segment below it
/// pays nothing.
async fn segment_pressure_at(segments: usize) -> TestResult {
    let fold = segments >= SEGMENT_PRESSURE_SEGMENTS;
    let seeded = seeded("segment-pressure:admission", segments, true, &[]).await?;
    let result = seeded.agent.prompt("one more").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{segments}: {:?}",
        result.outcome()
    );
    assert_eq!(
        seeded.model.paid_compaction_calls.load(Ordering::Acquire),
        usize::from(fold),
        "{segments}"
    );
    assert_eq!(
        compaction_workflow_count(result.checkpoint()),
        usize::from(fold)
    );
    if fold {
        assert!(head_segments(result.checkpoint()) < 16, "{segments}");
    } else {
        assert_eq!(head_segments(result.checkpoint()), segments + 2);
    }
    seeded.agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn segment_pressure_folds_below_the_token_trigger() -> TestResult {
    segment_pressure_at(SEGMENT_PRESSURE_SEGMENTS).await
}

#[tokio::test]
async fn no_fold_one_segment_under_the_pressure_threshold() -> TestResult {
    segment_pressure_at(SEGMENT_PRESSURE_SEGMENTS - 1).await
}

/// A native successor keeps every newest user message its token budget
/// admits, so a segment-pressure pass on a native route uses the harness cut.
#[tokio::test]
async fn segment_pressure_uses_the_harness_cut_on_a_native_route() -> TestResult {
    let seeded = seeded_on(
        "segment-pressure:native",
        SEGMENT_PRESSURE_SEGMENTS,
        true,
        &[],
        true,
    )
    .await?;
    let result = seeded.agent.prompt("one more").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(
        seeded.model.paid_compaction_calls.load(Ordering::Acquire),
        1
    );
    let strategy = result
        .checkpoint()
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()
        .ok_or("provenance")?
        .strategy();
    assert_ne!(
        strategy,
        RuntimeCompactionStrategy::NativeResponsesTriggerV1
    );
    assert!(head_segments(result.checkpoint()) < 16);
    seeded.agent.shutdown().await?;
    Ok(())
}

/// A turn that crosses the threshold with a tool group folds at the
/// tool-group fence and continues on the successor.
#[tokio::test]
async fn segment_pressure_folds_at_the_tool_group_fence() -> TestResult {
    let seeded = seeded(
        "segment-pressure:tool-group",
        SEGMENT_PRESSURE_SEGMENTS - 2,
        true,
        &[0],
    )
    .await?;
    let result = seeded.agent.prompt("probe once").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.model_rounds(), 2);
    // Prompt then the tool group reached the threshold; the fold ran between
    // the two ordinary rounds.
    assert_eq!(*lock_mutex(&seeded.model.paid_compaction_rounds), vec![1]);
    assert_eq!(compaction_workflow_count(result.checkpoint()), 1);
    assert!(head_segments(result.checkpoint()) < 16);
    seeded.agent.shutdown().await?;
    Ok(())
}

/// A failed segment fold neither commits a placeholder successor, which would
/// keep every segment, nor pays again in the same turn.
#[tokio::test]
async fn failed_segment_fold_commits_no_placeholders_and_pays_once_per_turn() -> TestResult {
    let seeded = seeded(
        "segment-pressure:failed-fold",
        SEGMENT_PRESSURE_SEGMENTS + 1,
        true,
        &[0],
    )
    .await?;
    seeded.model.fail_compaction_call();
    let result = seeded.agent.prompt("probe once").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.model_rounds(), 2);
    assert_eq!(*lock_mutex(&seeded.model.paid_compaction_rounds), vec![0]);
    let checkpoint = result.checkpoint();
    assert!(
        !checkpoint
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .any(|segment| segment.kind() == HistorySegmentKind::Compaction)
    );
    // Prompt, tool group and answer: nothing replaced or rewritten.
    assert_eq!(head_segments(checkpoint), SEGMENT_PRESSURE_SEGMENTS + 4);
    seeded.agent.shutdown().await?;
    Ok(())
}

/// With compaction disabled, a turn at the refusal limit fails before its
/// prompt appends, so repeated prompts never grow the head; the session stays
/// quiescent, reopens, and an explicit compact relieves it.
#[tokio::test]
async fn segment_refusal_without_compaction_leaves_the_head_reopenable() -> TestResult {
    let seeded = seeded(
        "segment-refusal:disabled",
        SEGMENT_REFUSAL_SEGMENTS,
        false,
        &[],
    )
    .await?;
    for attempt in 0..3 {
        let result = seeded
            .agent
            .prompt(format!("refused {attempt}"))
            .await?
            .result()
            .await?;
        assert_segment_refusal(&result, false);
        assert_eq!(head_segments(result.checkpoint()), SEGMENT_REFUSAL_SEGMENTS);
    }
    assert_eq!(seeded.model.calls.load(Ordering::Acquire), 0);
    let source = seeded.agent.checkpoint();
    seeded.agent.shutdown().await?;
    let topology = source.manifest().topology();
    let (builder, model) = placeholder_fixture(
        Arc::clone(&seeded.workspace),
        0x5e7_0000,
        CEILING,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let DurableRuntimeOpen::Opened(reopened) = seeded
        .sessions
        .open(
            "open:segment-refusal",
            DurableSessionAddress::new(
                topology.session_id,
                topology.branch_id,
                topology.checkpoint_id,
            ),
            builder.with_scheduled_compaction_enabled(false),
        )
        .await?
    else {
        return Err("a refused session did not reopen".into());
    };
    assert_quiescent(&reopened.checkpoint());
    assert_segment_refusal(
        &reopened.prompt("still refused").await?.result().await?,
        false,
    );
    let (compact_builder, compact_model) = placeholder_fixture(
        Arc::clone(&seeded.workspace),
        0x5e8_0000,
        CEILING,
        &[],
        &[],
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    let head = reopened.checkpoint();
    let outcome = reopened
        .session_endpoint()
        .compact_session(AgentCompactSessionRequest::new(
            "compact:segment-refusal".to_owned(),
            reopened.session_id(),
            reopened.branch_id(),
            head.id(),
            AgentCompactionReason::Manual,
            compact_builder,
        ))
        .await
        .map_err(|error| format!("{error:?}"))?;
    assert!(
        matches!(outcome, AgentCompactSessionOutcome::Compacted { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        compact_model.paid_compaction_calls.load(Ordering::Acquire),
        1
    );
    assert!(head_segments(&reopened.checkpoint()) < 16);
    let result = reopened.prompt("relieved").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(model.calls.load(Ordering::Acquire), 1);
    reopened.shutdown().await?;
    Ok(())
}

/// A turn whose tool group reaches the limit is refused before its next
/// round: the terminal appends its note under the bound, and the next turn is
/// refused without growing the head.
#[tokio::test]
async fn segment_refusal_stops_the_next_round_of_a_running_turn() -> TestResult {
    let seeded = seeded(
        "segment-refusal:mid-turn",
        SEGMENT_REFUSAL_SEGMENTS - 1,
        false,
        &[0],
    )
    .await?;
    let result = seeded.agent.prompt("probe once").await?.result().await?;
    assert_segment_refusal(&result, false);
    assert_eq!(seeded.model.calls.load(Ordering::Acquire), 1);
    // Prompt, tool group and the failure note.
    let ended = head_segments(result.checkpoint());
    assert_eq!(ended, SEGMENT_REFUSAL_SEGMENTS + 2);
    assert!(ended < MAX_RUNTIME_RECORDS);
    let again = seeded.agent.prompt("refused").await?.result().await?;
    assert_segment_refusal(&again, false);
    assert_eq!(head_segments(again.checkpoint()), ended);
    seeded.agent.shutdown().await?;
    Ok(())
}

/// Heads at the checkpoint bound still run. One segment under it is the shape
/// an earlier build wedged: the prompt appended at the bound and the answer's
/// publication then exceeded it, leaving the turn parked. Now the turn fence
/// folds such a head when compaction is enabled, and without compaction the
/// turn is refused before its prompt appends and the head stays put.
async fn head_at_the_checkpoint_bound(segments: usize, compaction: bool) -> TestResult {
    let seeded = seeded("segment-bound:full", segments, compaction, &[]).await?;
    let result = seeded.agent.prompt("at the bound").await?.result().await?;
    if compaction {
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{segments}: {:?}",
            result.outcome()
        );
        assert_eq!(
            seeded.model.paid_compaction_calls.load(Ordering::Acquire),
            1
        );
        assert!(head_segments(result.checkpoint()) < 16);
    } else {
        assert_segment_refusal(&result, false);
        assert_eq!(head_segments(result.checkpoint()), segments);
        assert_eq!(seeded.model.calls.load(Ordering::Acquire), 0);
    }
    seeded.agent.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn a_head_one_under_the_checkpoint_bound_compacts() -> TestResult {
    head_at_the_checkpoint_bound(MAX_RUNTIME_RECORDS - 1, true).await
}

#[tokio::test]
async fn a_head_one_under_the_checkpoint_bound_refuses_cleanly() -> TestResult {
    head_at_the_checkpoint_bound(MAX_RUNTIME_RECORDS - 1, false).await
}

#[tokio::test]
async fn a_head_at_the_checkpoint_bound_compacts() -> TestResult {
    head_at_the_checkpoint_bound(MAX_RUNTIME_RECORDS, true).await
}

#[tokio::test]
async fn a_head_at_the_checkpoint_bound_refuses_cleanly() -> TestResult {
    head_at_the_checkpoint_bound(MAX_RUNTIME_RECORDS, false).await
}

/// With compaction enabled but its fold failing, the refusal names the
/// compaction-enabled remedy and the failed fold is not paid for again until
/// a model round has run.
#[tokio::test]
async fn failed_fold_at_the_limit_refuses_without_repeating_it() -> TestResult {
    let seeded = seeded(
        "segment-refusal:failed-fold",
        SEGMENT_REFUSAL_SEGMENTS,
        true,
        &[],
    )
    .await?;
    seeded.model.fail_compaction_call();
    for _ in 0..2 {
        let result = seeded.agent.prompt("refused").await?.result().await?;
        assert_segment_refusal(&result, true);
        assert_eq!(head_segments(result.checkpoint()), SEGMENT_REFUSAL_SEGMENTS);
    }
    assert_eq!(
        seeded.model.paid_compaction_calls.load(Ordering::Acquire),
        1
    );
    assert_eq!(compaction_workflow_count(&seeded.agent.checkpoint()), 1);
    seeded.agent.shutdown().await?;
    Ok(())
}
