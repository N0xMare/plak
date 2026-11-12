use super::{decode_changes_answer, encode_changes_answer};
use crate::environment::backend::changes::*;
use plakous_runner_protocol::frame;

fn maximum_status() -> ChangesStatus {
    let root = format!("/{}end", "segment/".repeat(511));
    assert_eq!(root.len(), 4092);
    let root = format!("{root}root");
    ChangesStatus {
        repository: ChangesRepository {
            workspace: root.clone().into(),
            toplevel: root.clone().into(),
            git_dir: root.clone().into(),
            common_dir: root.into(),
            identity: "1".repeat(64),
            git_view: false,
        },
        observation: ChangesObservation {
            head: Some("2".repeat(64)),
            index: "3".repeat(64),
        },
        base_label: "b".repeat(4096),
        entries: (0..MAX_CHANGES_ENTRIES)
            .map(|index| ChangesEntry {
                path: format!("{}{index:08}", "segment/".repeat(511)),
                state: "MM".into(),
                head_object: Some("4".repeat(64)),
                index_object: Some("5".repeat(64)),
                content: ChangesContent::File {
                    size: MAX_CHANGES_INPUT_BYTES,
                    digest: "6".repeat(64),
                },
            })
            .collect(),
    }
}

#[test]
fn changes_two_thousand_maximum_field_entries_fit_local_and_runner_status_budgets() {
    let answer = ChangesAnswer::Status(maximum_status());
    assert_eq!(answer.validate(), Ok(()));
    let value = encode_changes_answer(&answer).unwrap();
    let frame = frame::encode(&serde_json::json!({"result": value})).unwrap();
    assert!(frame.len() > MAX_CHANGES_OUTPUT_BYTES);
    assert!(frame.len() <= frame::MAX_FRAME_BYTES);
    let decoded: serde_json::Value = frame::decode(&frame[4..]).unwrap();
    assert_eq!(
        decode_changes_answer(decoded["result"].clone()).unwrap(),
        answer
    );
}

#[test]
fn changes_status_one_byte_over_internal_budget_is_refused_locally_and_by_codec() {
    let mut status = maximum_status();
    let size = serde_json::to_vec(&status).unwrap().len();
    let extra = MAX_CHANGES_STATUS_BYTES + 1 - size;
    let mut wide = extra / 5;
    let mut narrow = extra % 5;
    for entry in &mut status.entries {
        entry.path = entry
            .path
            .chars()
            .map(|character| {
                if character == '/' {
                    character
                } else if wide != 0 {
                    wide -= 1;
                    '\u{1}'
                } else if narrow != 0 {
                    narrow -= 1;
                    '\n'
                } else {
                    character
                }
            })
            .collect();
    }
    assert_eq!((wide, narrow), (0, 0));
    assert_eq!(
        serde_json::to_vec(&status).unwrap().len(),
        MAX_CHANGES_STATUS_BYTES + 1
    );
    let answer = ChangesAnswer::Status(status);
    assert_eq!(answer.validate(), Err(ChangesRefusal::OutputLimit));
    assert!(encode_changes_answer(&answer).is_err());
    let value = serde_json::to_value(answer).unwrap();
    // The transport can hold this answer, but the narrower status codec cannot.
    assert!(frame::encode(&value).is_ok());
    assert!(decode_changes_answer(value).is_err());
}
