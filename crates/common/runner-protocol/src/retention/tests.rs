use super::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[test]
fn retention_one_has_exact_frozen_fixtures_and_no_native_verb_escape() {
    assert_eq!(VERSION, 1);
    let mut verbs = BTreeSet::new();
    for line in include_str!("../pinned/retention-v1.jsonl").lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let value = &row["value"];
        let encoded = match row["kind"].as_str().unwrap() {
            "hello" => {
                serde_json::to_value(serde_json::from_value::<Hello>(value.clone()).unwrap())
                    .unwrap()
            }
            "bind" => serde_json::to_value(serde_json::from_value::<Bind>(value.clone()).unwrap())
                .unwrap(),
            "call" => {
                let call: Call = serde_json::from_value(value.clone()).unwrap();
                verbs.insert(match &call.request {
                    Request::Lines { .. } => "lines",
                    Request::Transfer { .. } => "transfer",
                    Request::JobsSnapshot { .. } => "jobs_snapshot",
                    Request::JobsWatch { .. } => "jobs_watch",
                    Request::Read { .. } => "read",
                    Request::Poll { .. } => "poll",
                    Request::Write { .. } => "write",
                    Request::Stop { .. } => "stop",
                    Request::Disown { .. } => "disown",
                    Request::Ack { .. } => "ack",
                });
                serde_json::to_value(call).unwrap()
            }
            "reply" => {
                serde_json::to_value(serde_json::from_value::<Reply>(value.clone()).unwrap())
                    .unwrap()
            }
            _ => panic!("unknown fixture family"),
        };
        assert_eq!(&encoded, value);
    }
    assert_eq!(
        verbs,
        [
            "ack",
            "disown",
            "jobs_snapshot",
            "jobs_watch",
            "lines",
            "poll",
            "read",
            "stop",
            "transfer",
            "write"
        ]
        .into_iter()
        .collect()
    );
    for verb in [
        "process.start",
        "start",
        "workspace.mutate",
        "effect.lookup",
        "replay",
    ] {
        assert!(serde_json::from_value::<Request>(json!({"verb":verb})).is_err());
    }
    assert!(
        serde_json::from_value::<Request>(json!({"verb":"lines","after":0,"host_id":"override"}))
            .is_err()
    );
    assert!(serde_json::from_value::<Id>(json!("\u{0}")).is_err());
    assert!(serde_json::from_value::<Id>(json!("x".repeat(257))).is_err());
    assert!(admits(1));
    assert!(!admits(0));
    assert!(!admits(2));
}

#[test]
fn retention_unit_outcomes_reject_extra_fields_without_changing_pinned_bytes() {
    for row in include_str!("../pinned/retention-v1-invalid.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        let value = row["value"].clone();
        assert!(match row["kind"].as_str().unwrap() {
            "status" => serde_json::from_value::<Status>(value).is_err(),
            "process_status" => serde_json::from_value::<ProcessStatus>(value).is_err(),
            "answer" => serde_json::from_value::<Answer>(value).is_err(),
            _ => panic!("unknown negative probe family"),
        });
    }
}

#[test]
fn retention_input_and_output_debug_never_print_private_bytes() {
    let owner = Owner {
        line: Id::try_from("line".to_owned()).unwrap(),
        generation: 1,
        branch: Id::try_from("branch".to_owned()).unwrap(),
        fence: 1,
    };
    let request = Request::Write {
        owner,
        session: Id::try_from("session".to_owned()).unwrap(),
        bytes: "credential-fixture".into(),
        close: false,
    };
    assert!(!format!("{request:?}").contains("credential-fixture"));
    let record = Record {
        sequence: 1,
        cursor: 0,
        stream: "stdout".into(),
        bytes: "private-output".into(),
    };
    assert!(!format!("{record:?}").contains("private-output"));
}
