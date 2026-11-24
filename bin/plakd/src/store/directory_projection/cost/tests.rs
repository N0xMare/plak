#![allow(clippy::unwrap_used, reason = "isolated telemetry fixtures")]

use super::*;
use crate::telemetry_store::TelemetryStore;

fn member(id: u128, turns: u64) -> Member {
    Member {
        locator: Some(SessionLocator::new(
            uuid::Uuid::from_u128(id).try_into().unwrap(),
            uuid::Uuid::from_u128(id + 1).try_into().unwrap(),
            uuid::Uuid::from_u128(id + 2).try_into().unwrap(),
        )),
        started_ms: 100,
        turns,
        remote: false,
    }
}

fn terminal(writer: &Connection, turn: &str, member: Member, reporting: u64, cost: Option<&str>) {
    let loc = member.locator.unwrap();
    writer.execute(
        "INSERT OR IGNORE INTO recorded_terminals VALUES(?1,?2,?3,100,'openai/work','0','0','0','0','0','1',?4,?5)",
        (turn, loc.session_id().to_string(), loc.branch_id().to_string(), reporting.to_string(), cost),
    ).unwrap();
}

#[test]
fn retained_cost_distinguishes_measured_zero_from_partial_unknown_and_replay() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let _writer = TelemetryStore::open(&paths).unwrap();
    let connection = Connection::open(paths.telemetry_database()).unwrap();
    connection
        .execute("UPDATE recording_evidence SET started_at_ms=70", [])
        .unwrap();
    let root = member(11, 1);
    let child = member(21, 1);
    let mut costs = Costs::default();
    assert!(costs.poll(&paths));
    assert_eq!(
        costs.read(root, Some(&[])),
        (
            Cost::Unknown,
            TreeCost {
                cost: Cost::Unknown,
                complete: false
            }
        )
    );
    terminal(&connection, "root", root, 1, Some("0"));
    terminal(&connection, "child", child, 1, Some("7"));
    terminal(&connection, "child", child, 1, Some("700"));
    assert!(costs.poll(&paths));
    assert_eq!(
        costs.read(root, Some(&[child, child])),
        (
            Cost::Measured { micros: 0 },
            TreeCost {
                cost: Cost::Measured { micros: 7 },
                complete: true
            }
        )
    );
    let missing = member(31, 1);
    terminal(&connection, "missing", missing, 0, None);
    assert!(costs.poll(&paths));
    assert_eq!(
        costs.read(root, Some(&[child, missing])).1,
        TreeCost {
            cost: Cost::Measured { micros: 7 },
            complete: false
        }
    );
    let remote = Member {
        remote: true,
        ..child
    };
    assert!(!costs.read(root, Some(&[remote])).1.complete);
    connection.execute_batch(
        "BEGIN IMMEDIATE; UPDATE recording_evidence SET retained_from_ms=101; DELETE FROM recorded_terminals WHERE turn_id='root'; COMMIT;",
    ).unwrap();
    assert!(costs.poll(&paths));
    assert_eq!(
        costs.read(root, Some(&[child])),
        (
            Cost::Unknown,
            TreeCost {
                cost: Cost::Measured { micros: 7 },
                complete: false
            }
        )
    );
}

#[test]
fn indexed_reads_resume_without_resumming_and_checked_overflow_is_unknown() {
    let (_home, paths) = PlakPaths::isolated_test_home();
    let _writer = TelemetryStore::open(&paths).unwrap();
    let mut connection = Connection::open(paths.telemetry_database()).unwrap();
    connection
        .execute("UPDATE recording_evidence SET started_at_ms=70", [])
        .unwrap();
    let limit = u64::try_from(TERMINALS_PER_READ).unwrap();
    let root = member(11, limit + 1);
    let tx = connection.transaction().unwrap();
    for i in 0..=limit {
        terminal(&tx, &format!("turn-{i}"), root, 1, Some("1"));
    }
    tx.commit().unwrap();
    let plan: String = connection
        .query_row(
            &format!("EXPLAIN QUERY PLAN {TERMINALS}"),
            ("fixture", 0, TERMINALS_PER_READ),
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        plan.contains("recorded_terminals_session") && plan.contains("rowid>?"),
        "{plan}"
    );
    let mut costs = Costs::default();
    assert!(costs.poll(&paths));
    assert_eq!(
        costs.read(root, Some(&[])).1,
        TreeCost {
            cost: Cost::Measured { micros: limit },
            complete: false
        }
    );
    assert!(costs.poll(&paths));
    assert_eq!(
        costs.read(root, Some(&[])).0,
        Cost::Measured { micros: limit + 1 }
    );
    assert!(!costs.poll(&paths));
    let child = member(21, 1);
    terminal(
        &connection,
        "overflow",
        child,
        1,
        Some(&u64::MAX.to_string()),
    );
    assert_eq!(
        costs.read(root, Some(&[child])).1,
        TreeCost {
            cost: Cost::Unknown,
            complete: false
        },
    );
}
