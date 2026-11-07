use super::*;
use crate::runtime::checkpoint::candidate::{SuccessorFacts, successor_candidate};

fn rebuilt_without(
    source: &RuntimeOperationLedger,
    removed: u64,
) -> TestResult<RuntimeOperationLedger> {
    Ok(RuntimeOperationLedger::new(
        source.commands().into_iter().cloned().collect(),
        source
            .operations()
            .into_iter()
            .filter(|record| record.operation_id() != removed)
            .cloned()
            .collect(),
    )?)
}

fn refuses(
    parent: RuntimeOperationLedger,
    successor: RuntimeOperationLedger,
    turn: TurnId,
    message: &str,
) -> TestResult {
    let lifecycle = crate::runtime::operation_record::working_lifecycle(turn, &parent)?;
    let prior = crate::checkpoint_bundle::tests::fixture_with_history_and_ledger(
        plakous_model::CanonicalHistory::default(),
        3,
        true,
        Some(parent),
        Some(lifecycle.clone()),
    )?;
    let bundle = crate::RuntimeCheckpointCodec::bundle(&prior, None)?;
    let manifest = prior.manifest();
    let result = successor_candidate(
        &prior,
        &bundle,
        SuccessorFacts {
            checkpoint_id: checkpoint(0x902)?,
            lifecycle,
            history: manifest.canonical().history().clone(),
            ledger: successor,
            jobs: manifest.jobs().clone(),
            named_state: manifest.named_state().clone(),
            context: manifest.canonical().context().retained_for_exact_resume(),
            family: manifest.family_artifact().cloned(),
        },
        1,
        None,
    );
    let error = result.err().ok_or("illegal successor published")?;
    assert_eq!(error.kind(), AgentErrorKind::Internal);
    assert_eq!(error.message(), message);
    Ok(())
}

#[test]
fn precas_refuses_hand_built_missing_pinned_round_harness_and_failed_fold() -> TestResult {
    let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let mut records = ledger
        .operations()
        .into_iter()
        .filter(|record| !(2..=6).contains(&record.operation_id()))
        .cloned()
        .collect::<Vec<_>>();
    records.push(retirement_round(2, turn, RoundDecision::Continue)?);
    records.extend(crate::runtime::compaction::instruction::retirement_fold_pair(3, true)?);
    records.extend(crate::runtime::compaction::instruction::retirement_fold_pair(5, false)?);
    let source =
        RuntimeOperationLedger::new(ledger.commands().into_iter().cloned().collect(), records)?;
    for id in [2, 3, 4, 5, 6] {
        let forged =
            rebuilt_without(&source, id)?.with_retirement_source(&source, &BTreeSet::new());
        refuses(
            source.clone(),
            forged,
            turn,
            "operation ledger retirement violates its frontier or accounting",
        )?;
    }
    Ok(())
}

#[test]
fn precas_refuses_working_removal_without_retirement_source() -> TestResult {
    let (source, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    refuses(
        source.clone(),
        rebuilt_without(&source, 2)?,
        turn,
        "working ledger removal lacks its staged retirement evidence",
    )
}

#[test]
fn precas_refuses_retirement_source_missing_a_parent_record() -> TestResult {
    let (parent, turn) = retirement_fixture(100, RoundDecision::Continue)?;
    let source = rebuilt_without(&parent, 2)?;
    let forged = source
        .clone()
        .with_retirement_source(&source, &BTreeSet::new());
    refuses(
        parent,
        forged,
        turn,
        "retirement source disagrees with its parent",
    )
}

#[test]
fn precas_refuses_removed_nonfinalized_and_post_p_records() -> TestResult {
    for post_p in [false, true] {
        let (ledger, turn) = retirement_fixture(100, RoundDecision::Continue)?;
        let id = if post_p { 103 } else { 2 };
        let mut records = ledger
            .operations()
            .into_iter()
            .filter(|record| record.operation_id() != id)
            .cloned()
            .collect::<Vec<_>>();
        records.push(leaf(id, turn, post_p)?);
        let source =
            RuntimeOperationLedger::new(ledger.commands().into_iter().cloned().collect(), records)?;
        let mut forged = rebuilt_without(&source, id)?.with_retired_attempts(Some((turn, 1)))?;
        // Preserve the high-water independently of P's semantic prohibition.
        if post_p {
            forged = forged.append_operation(leaf(104, turn, true)?)?;
        }
        forged = forged.with_retirement_source(&source, &BTreeSet::new());
        refuses(
            source,
            forged,
            turn,
            "operation ledger retirement violates its frontier or accounting",
        )?;
    }
    Ok(())
}
