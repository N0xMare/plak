use serde_json::Value;

pub(super) fn submission_view_cuts(rows: &[Value]) {
    for state in ["pending", "accepted"] {
        assert!(
            rows.iter().any(|row| {
                row["shape"] == "direct_tool_then_final"
                    && row["store"] == "product"
                    && row["before"]["submissions"][0]["state"] == state
            }),
            "missing root submission/view cut: {state}"
        );
    }
    assert!(
        rows.iter().any(|row| {
            row["shape"] == "direct_tool_then_final"
                && row["store"] == "runtime"
                && row["before"]["submissions"][0]["state"] == "pending"
                && row["before"]["ledger"][0]["kind"] == "command"
                && row["before"]["ledger"][0]["state"] == "prepared"
        }),
        "missing retired RuntimeReturned admission cut"
    );
    assert!(
        rows.iter().any(|row| {
            row["shape"] == "direct_tool_then_final"
                && row["before"]["phase"].is_null()
                && row["before"]["durable_answer"] == true
                && row["before"]["original_answers"] == 0
                && row["outcome"] == "COMPLETED_ONCE"
        }),
        "missing retired TerminalReady cut"
    );
}

#[test]
fn missing_submission_view_cut_coverage_is_rejected() {
    assert!(std::panic::catch_unwind(|| submission_view_cuts(&[])).is_err());
}

#[test]
fn each_submission_view_cut_requires_its_own_row() {
    let row = |store: &str, before: Value| {
        serde_json::json!({
            "shape": "direct_tool_then_final", "store": store,
            "before": before, "outcome": "COMPLETED_ONCE"
        })
    };
    let mut rows = ["pending", "accepted"]
        .map(|state| {
            row(
                "product",
                serde_json::json!({
                    "submissions": [{"state": state}]
                }),
            )
        })
        .to_vec();
    rows.push(row(
        "runtime",
        serde_json::json!({
            "submissions": [{"state": "pending"}],
            "ledger": [{"kind": "command", "state": "prepared"}]
        }),
    ));
    rows.push(row(
        "runtime",
        serde_json::json!({
            "phase": null, "durable_answer": true, "original_answers": 0
        }),
    ));
    submission_view_cuts(&rows);
    for index in 0..rows.len() {
        let mut missing = rows.clone();
        missing.remove(index);
        assert!(std::panic::catch_unwind(|| submission_view_cuts(&missing)).is_err());
    }
}
