//! Published answers survive interruption of their later paid workflow.

use super::super::offline_support::Fixture;
use super::summary::{self, Summary};
use rusqlite::{Connection, OpenFlags};

pub(super) fn assert_admitted_abandonment(fixture: &Fixture, before: &Summary, after: &Summary) {
    if before.phase.is_none() || before.workflow_requests.is_empty() {
        return;
    }
    assert!(before.durable_answer);
    assert!(after.durable_answer, "finalized root answer lost");
    assert_eq!(after.original_answers, before.original_answers);
    assert_eq!(after.workflow_requests[0]["state"], "settled");
    let database = Connection::open_with_flags(
        summary::paths(fixture).product_database(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .unwrap();
    let notices: i64 = database.query_row(
        "SELECT count(*) FROM (SELECT anchor,lane,ordinal,row FROM transcript_rows UNION ALL SELECT anchor,1 AS lane,ordinal,row FROM operator_notices) WHERE instr(row, 'interrupted compaction was not applied') > 0",
        [], |row| row.get(0),
    ).unwrap();
    // Spec 02: a summary the ledger holds completed is applied from its
    // record and settles `Compacted` without a notice; any other admitted
    // workflow debt settles `Abandoned` with exactly one.
    let applied = before
        .ledger
        .as_array()
        .unwrap()
        .iter()
        .any(|record| record["kind"] == "compaction" && record["state"] == "completed");
    assert_eq!(
        notices,
        i64::from(!applied),
        "admitted interrupted workflow needs exactly one visible notice unless its summary is applied"
    );
}
