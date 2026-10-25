#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;
use crate::views::transcript::Lane;

fn key(value: u64) -> RowKey {
    RowKey::new(value, Lane::Runtime, 0)
}

fn snapshot(
    scope: TranscriptScope,
    revision: u64,
    rows: &BTreeMap<RowKey, TranscriptRow>,
) -> PresentationSnapshot {
    PresentationSnapshot {
        scope,
        revision,
        items: rows
            .iter()
            .map(|(key, row)| TranscriptItem {
                key: *key,
                row: row.clone(),
            })
            .collect(),
        ..PresentationSnapshot::default()
    }
}

#[test]
fn generated_patch_folds_equal_independent_full_snapshots() {
    for seed in 1_u64..=64 {
        let scope = if seed % 2 == 0 {
            TranscriptScope::Root
        } else {
            TranscriptScope::Agent(format!("c{seed}"))
        };
        let mut random = seed;
        let mut truth = BTreeMap::new();
        let mut folded = TranscriptState::default();
        folded.replace(snapshot(scope.clone(), 0, &truth)).unwrap();
        for revision in 1..=128 {
            let before = truth.clone();
            for _ in 0..4 {
                random = random
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let selected = key((random >> 32) % 64 + 1);
                if random % 4 == 0 {
                    truth.remove(&selected);
                } else {
                    truth.insert(
                        selected,
                        TranscriptRow::assistant(format!("{revision}/{random}")),
                    );
                }
            }
            let patch =
                TranscriptPatch::between(scope.clone(), revision - 1, revision, &before, &truth);
            folded.apply(patch).unwrap();
            let mut restored = TranscriptState::default();
            restored
                .replace(snapshot(scope.clone(), revision, &truth))
                .unwrap();
            assert_eq!(folded, restored, "seed={seed}, revision={revision}");
        }
    }
}

#[test]
fn missed_revision_and_foreign_scope_leave_the_baseline_unchanged() {
    let rows = BTreeMap::from([(key(1), TranscriptRow::assistant("original"))]);
    let mut state = TranscriptState::default();
    state
        .replace(snapshot(TranscriptScope::Root, 7, &rows))
        .unwrap();
    let before = state.clone();
    let replacement = BTreeMap::from([(key(2), TranscriptRow::assistant("new"))]);
    for (scope, revision, expected) in [
        (
            TranscriptScope::Agent("c1".into()),
            7,
            PresentationError::ScopeMismatch,
        ),
        (
            TranscriptScope::Root,
            6,
            PresentationError::RevisionMismatch {
                expected: 7,
                actual: 6,
            },
        ),
        (
            TranscriptScope::Root,
            8,
            PresentationError::RevisionMismatch {
                expected: 7,
                actual: 8,
            },
        ),
    ] {
        let patch = TranscriptPatch::between(scope, revision, 9, &rows, &replacement);
        assert_eq!(state.apply(patch), Err(expected));
        assert_eq!(state, before);
    }
    state
        .replace(snapshot(TranscriptScope::Root, 9, &replacement))
        .unwrap();
    assert_eq!(
        state.items(),
        &replacement,
        "a resync establishes a new baseline"
    );
}

#[test]
fn malformed_and_overflowing_updates_are_atomic() {
    let row = TranscriptRow::assistant("small");
    let rows = BTreeMap::from([(key(1), row.clone())]);
    let mut state = TranscriptState::default();
    state
        .replace(snapshot(TranscriptScope::Root, 0, &rows))
        .unwrap();
    let before = state.clone();
    let patch = TranscriptPatch::between(TranscriptScope::Root, 0, 1, &rows, &BTreeMap::new());
    let item = TranscriptItem {
        key: key(2),
        row: row.clone(),
    };
    for (upserts, deletes, expected) in [
        (
            vec![item.clone(), item.clone()],
            vec![],
            PresentationError::KeyOrder,
        ),
        (
            vec![item.clone()],
            vec![key(2)],
            PresentationError::ConflictingKey,
        ),
        (vec![], vec![key(3), key(2)], PresentationError::KeyOrder),
        (
            vec![TranscriptItem {
                key: key(2),
                row: TranscriptRow::assistant("x".repeat(MAX_PRESENTATION_BYTES)),
            }],
            vec![],
            PresentationError::ByteLimit,
        ),
    ] {
        assert_eq!(
            state.apply(TranscriptPatch {
                upserts,
                deletes,
                ..patch.clone()
            }),
            Err(expected)
        );
        assert_eq!(state, before);
    }
    let full: BTreeMap<_, _> = (1..=MAX_SNAPSHOT_ROWS as u64)
        .map(|value| (key(value), row.clone()))
        .collect();
    state
        .replace(snapshot(TranscriptScope::Root, 1, &full))
        .unwrap();
    let before = state.clone();
    let patch = TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: 1,
        to_revision: 2,
        upserts: vec![TranscriptItem {
            key: key(u64::MAX),
            row,
        }],
        deletes: vec![],
    };
    assert_eq!(state.apply(patch), Err(PresentationError::ItemLimit));
    assert_eq!(state, before);
}
