//! A long turn's persisted bytes grow with what each round adds, not with
//! rounds times history.

use super::*;

const ROUNDS: usize = 200;
const WINDOW: usize = 16;

/// Bytes of one round of the turn, at the last checkpoint holding that many
/// finished provider rounds.
#[derive(Clone, Copy, Default)]
struct RoundBytes {
    /// Everything reachable from that checkpoint.
    head: usize,
    /// The round's own prepared-call record.
    record: usize,
    /// The same record with its wire-input list embedded (the old encoding).
    embedded_record: usize,
    /// Objects first written by the round's commits, except the context state.
    written: usize,
    /// The round's new context-state objects (the superseded request anchor).
    written_context: usize,
}

fn embedded_record_bytes(record: &PreparedCallRecord) -> TestResult<usize> {
    let mut value = serde_json::to_value(record)?;
    let tokens = value["projection"]["token_projection"]
        .as_object_mut()
        .ok_or("token projection object")?;
    tokens.remove("wire_input_identity").ok_or("reference")?;
    tokens.insert(
        "wire_input".to_owned(),
        serde_json::to_value(record.projection().token_projection().wire_input())?,
    );
    Ok(serde_json::to_vec(&value)?.len())
}

/// The finished provider rounds in the checkpoint's ledger and the last
/// round's request bytes.
fn last_provider_request(checkpoint: &RuntimeCheckpoint) -> (usize, Option<Vec<u8>>) {
    let operations = checkpoint.manifest().ledger().operations();
    let mut providers = operations
        .iter()
        .filter(|record| matches!(record.kind(), DurableOperationKind::Provider { .. }));
    let count = providers.clone().count();
    (
        count,
        providers
            .next_back()
            .map(|record| record.request().bytes().to_vec()),
    )
}

fn mean(values: &[usize]) -> usize {
    values.iter().sum::<usize>() / values.len().max(1)
}

/// Bytes per finished round, from every commit the store accepted.
fn round_bytes(store: &TestCreateStore, model: &ArtifactModel) -> TestResult<Vec<RoundBytes>> {
    let live = model
        .prepared_records()
        .into_iter()
        .map(|record| (record.identity().to_string(), record))
        .collect::<BTreeMap<_, _>>();
    let accepted = lock_mutex(&store.accepted_commits).clone();
    let stored = lock_mutex(&store.state).stored_commits.clone();
    let mut seen = BTreeSet::new();
    let mut bytes = vec![RoundBytes::default(); ROUNDS + 2];
    let mut last_commit = vec![None; ROUNDS + 2];
    for (index, (checkpoint, _)) in accepted.iter().enumerate() {
        let (round, request) = last_provider_request(checkpoint);
        let round = round.min(ROUNDS + 1);
        last_commit[round] = Some(index);
        let entry = &mut bytes[round];
        let supplied = stored
            .get(&checkpoint.id())
            .ok_or("accepted commit was not stored")?;
        for object in supplied.bundle().supplied_objects() {
            if seen.insert(object.address()) {
                let size = object.canonical_bytes().len();
                if object.address().kind() == RuntimeObjectKind::ContextState {
                    entry.written_context += size;
                } else {
                    entry.written += size;
                }
            }
        }
        if let Some(request) = request {
            entry.record = request.len();
            let prepared: PreparedCallRecord = serde_json::from_slice(&request)?;
            assert!(
                prepared
                    .projection()
                    .token_projection()
                    .wire_input()
                    .is_none()
            );
            let live = live
                .get(&prepared.identity().to_string())
                .ok_or("ledger record was not prepared live")?;
            assert_eq!(prepared, *live);
            entry.embedded_record = embedded_record_bytes(live)?;
        }
    }
    // Everything reachable from a round's last checkpoint, for the rounds the
    // assertions compare.
    for round in (1..=WINDOW).chain(ROUNDS - WINDOW + 1..=ROUNDS) {
        let index = last_commit[round].ok_or("a round left no checkpoint")?;
        let bundle = RuntimeCheckpointCodec::bundle(&accepted[index].0, None)?;
        bytes[round].head = bundle
            .objects()
            .iter()
            .map(|object| object.canonical_bytes().len())
            .sum();
    }
    Ok(bytes)
}

/// Reports early and late rounds and asserts that neither a round's record
/// nor what it adds to the head grows with the history before it.
fn assert_rounds_track_new_items(bytes: &[RoundBytes]) {
    let per_round = &bytes[1..=ROUNDS];
    let early = &per_round[..WINDOW];
    let late = &per_round[ROUNDS - WINDOW..];
    let growth = |window: &[RoundBytes], pick: fn(&RoundBytes) -> usize| {
        window
            .windows(2)
            .map(|pair| pick(&pair[1]).saturating_sub(pick(&pair[0])))
            .collect::<Vec<_>>()
    };
    let field = |window: &[RoundBytes], pick: fn(&RoundBytes) -> usize| {
        window.iter().map(pick).collect::<Vec<_>>()
    };
    let report = [
        (
            "prepared record",
            field(early, |b| b.record),
            field(late, |b| b.record),
        ),
        (
            "embedded record (old)",
            field(early, |b| b.embedded_record),
            field(late, |b| b.embedded_record),
        ),
        (
            "head growth",
            growth(early, |b| b.head),
            growth(late, |b| b.head),
        ),
        (
            "written",
            field(early, |b| b.written),
            field(late, |b| b.written),
        ),
        (
            "written context state",
            field(early, |b| b.written_context),
            field(late, |b| b.written_context),
        ),
    ];
    for (label, early, late) in &report {
        eprintln!(
            "store-growth {label}: rounds 1-{WINDOW} mean {} B, rounds {}-{ROUNDS} mean {} B",
            mean(early),
            ROUNDS - WINDOW + 1,
            mean(late)
        );
    }
    eprintln!(
        "store-growth head after round {ROUNDS}: {} B; prepared records {} B (old encoding {} B)",
        per_round[ROUNDS - 1].head,
        per_round.iter().map(|b| b.record).sum::<usize>(),
        per_round.iter().map(|b| b.embedded_record).sum::<usize>(),
    );

    // Each round adds the same items. A record that embedded its list grew
    // with the history; the reference does not.
    let (records_early, records_late) = (mean(&report[0].1), mean(&report[0].2));
    let (embedded_early, embedded_late) = (mean(&report[1].1), mean(&report[1].2));
    assert!(
        records_late <= records_early + 64,
        "{records_early} -> {records_late}"
    );
    assert!(
        embedded_late > 4 * embedded_early,
        "{embedded_early} -> {embedded_late}"
    );
    // What the head retains, and every round writes besides the one
    // superseded anchor, tracks the round's new items: the head exactly, the
    // writes up to the ledger map's logarithmic path. The old records alone
    // added about 57 KiB per late round here.
    let (head_early, head_late) = (mean(&report[2].1), mean(&report[2].2));
    assert!(head_late <= head_early + 256, "{head_early} -> {head_late}");
    let (written_early, written_late) = (mean(&report[3].1), mean(&report[3].2));
    assert!(
        written_late <= written_early + written_early / 2,
        "{written_early} -> {written_late}"
    );
}

#[tokio::test]
async fn long_turn_persists_each_round_in_proportion_to_its_new_items() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let rounds = (0..ROUNDS).collect::<Vec<_>>();
    let (builder, model) = placeholder_fixture(
        workspace,
        0x5701_0000,
        500_000_000,
        &[200; ROUNDS],
        &rounds,
        false,
        LocalDurabilityDisposition::DurableRequired,
    )?;
    model.item_context.store(true, Ordering::Release);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let agent = sessions
        .create(
            "create:store-growth",
            builder.config(
                AgentConfig::default()
                    .with_max_model_rounds(ROUNDS + 1)
                    .with_event_capacity(16_384)
                    .with_event_byte_capacity(256 * 1024 * 1024),
            ),
        )
        .await?;
    let result = agent.prompt("probe every round").await?.result().await?;
    assert!(
        matches!(result.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        result.outcome()
    );
    assert_eq!(result.model_rounds(), ROUNDS + 1);

    let bytes = round_bytes(&store, &model)?;
    assert_rounds_track_new_items(&bytes);
    agent.shutdown().await?;
    Ok(())
}
