//! Every operator payload is an exact-version fixture, not a compatibility reader.

use std::collections::BTreeSet;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::*;

fn roundtrip<T: DeserializeOwned + Serialize>(value: &Value) -> T {
    let decoded = serde_json::from_value::<T>(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(&decoded).unwrap(), *value);
    decoded
}

#[test]
fn operator_payloads_are_pinned_to_the_exact_runner_protocol() {
    let pins = [(6, include_str!("../pinned/operator-v6.jsonl"))];
    let (_, text) = pins
        .iter()
        .find(|(version, _)| *version == crate::PROTOCOL_VERSION)
        .expect("pin operator payloads for the new runner protocol");
    let (mut operations, mut events, mut answers, mut statuses, mut refusals) = (
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    );
    for line in text.lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let value = &row["value"];
        match row["kind"].as_str().unwrap() {
            "operation" => {
                operations.insert(match roundtrip::<OperatorOperation>(value) {
                    OperatorOperation::Inventory => "inventory",
                    OperatorOperation::Reclaim { .. } => "reclaim",
                    OperatorOperation::Renew => "renew",
                    OperatorOperation::NamespaceClose => "namespace_close",
                    OperatorOperation::Open { .. } => "open",
                    OperatorOperation::List => "list",
                    OperatorOperation::Attach { .. } => "attach",
                    OperatorOperation::Resize { .. } => "resize",
                    OperatorOperation::Close { .. } => "close",
                });
            }
            "event" => {
                events.insert(match roundtrip::<TerminalEvent>(value) {
                    TerminalEvent::Data { .. } => "data",
                    TerminalEvent::InputAccepted { .. } => "input_accepted",
                    TerminalEvent::OutputGap { .. } => "output_gap",
                    TerminalEvent::Exit { .. } => "exit",
                    TerminalEvent::Refused { .. } => "refused",
                });
            }
            "answer" => {
                answers.insert(match roundtrip::<OperatorAnswer>(value) {
                    OperatorAnswer::Inventory { .. } => "inventory",
                    OperatorAnswer::Reclamation { .. } => "reclamation",
                    OperatorAnswer::Table { .. } => "table",
                    OperatorAnswer::Terminal { .. } => "terminal",
                    OperatorAnswer::Refused { .. } => "refused",
                });
            }
            "status" => {
                statuses.insert(match roundtrip::<TerminalStatus>(value) {
                    TerminalStatus::Starting => "starting",
                    TerminalStatus::Running => "running",
                    TerminalStatus::Closing => "closing",
                    TerminalStatus::Exited => "exited",
                    TerminalStatus::TerminationUnknown => "termination_unknown",
                    TerminalStatus::Lost => "lost",
                });
            }
            "refusal" => {
                refusals.insert(match roundtrip::<OperatorRefusal>(value) {
                    OperatorRefusal::InvalidRequest => "invalid_request",
                    OperatorRefusal::WorkspaceUnavailable => "workspace_unavailable",
                    OperatorRefusal::Quota => "quota",
                    OperatorRefusal::TerminalBusy => "terminal_busy",
                    OperatorRefusal::TerminalLost => "terminal_lost",
                    OperatorRefusal::NamespaceExpired => "namespace_expired",
                    OperatorRefusal::ReconcileRequired => "reconcile_required",
                    OperatorRefusal::Conflict => "conflict",
                    OperatorRefusal::OffsetGap => "offset_gap",
                    OperatorRefusal::Backpressure => "backpressure",
                });
            }
            other => panic!("unknown pin {other}"),
        }
    }
    assert_eq!(
        (
            operations.len(),
            events.len(),
            answers.len(),
            statuses.len(),
            refusals.len()
        ),
        (9, 5, 5, 6, 10)
    );
}
