use super::*;
use crate::environment::backend::changes::types::Request;
use serde_json::Value;

fn sample_status() -> ChangesStatus {
    ChangesStatus {
        repository: ChangesRepository {
            workspace: "/workspace".into(),
            toplevel: "/workspace".into(),
            git_dir: "/workspace/.git".into(),
            common_dir: "/workspace/.git".into(),
            identity: "1".repeat(64),
            git_view: false,
        },
        observation: ChangesObservation {
            head: Some("2".repeat(40)),
            index: "3".repeat(64),
        },
        base_label: "work tree vs HEAD".into(),
        entries: [
            ChangesContent::File {
                size: 3,
                digest: "4".repeat(64),
            },
            ChangesContent::Missing,
            ChangesContent::TooLarge { size: 1_048_577 },
            ChangesContent::Unsupported,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, content)| ChangesEntry {
            path: format!("file-{index}"),
            state: ".M".into(),
            head_object: Some("5".repeat(40)),
            index_object: Some("5".repeat(40)),
            content,
        })
        .collect(),
    }
}

#[test]
fn protocol_seven_changes_payloads_are_pinned() {
    assert_eq!(plakous_runner_protocol::PROTOCOL_VERSION, 7);
    let status = sample_status();
    let mut samples = vec![
        serde_json::to_value(ChangesRequest::status()).unwrap(),
        serde_json::to_value(ChangesRequest::status_in_repository("/workspace".into()).unwrap())
            .unwrap(),
        serde_json::to_value(
            ChangesRequest::diff(
                status.repository.clone(),
                status.observation.clone(),
                status.entries[0].clone(),
            )
            .unwrap(),
        )
        .unwrap(),
        serde_json::to_value(ChangesAnswer::Status(status)).unwrap(),
        serde_json::to_value(ChangesAnswer::Diff(ChangesDiff::Patch {
            text: "patch\n".into(),
        }))
        .unwrap(),
        serde_json::to_value(ChangesAnswer::Diff(ChangesDiff::Binary {
            numstat: "-\t-\tfile-0\0".into(),
        }))
        .unwrap(),
    ];
    samples.extend(
        [
            ChangesRefusal::NotARepository,
            ChangesRefusal::VcsUnavailable,
            ChangesRefusal::VcsUnsupported,
            ChangesRefusal::ProtectedPath,
            ChangesRefusal::UnsafeRepository,
            ChangesRefusal::InvalidRequest,
            ChangesRefusal::Conflict,
            ChangesRefusal::TooLarge,
            ChangesRefusal::OutputLimit,
            ChangesRefusal::Deadline,
            ChangesRefusal::Cancelled,
            ChangesRefusal::Io,
        ]
        .into_iter()
        .map(|reason| serde_json::to_value(ChangesAnswer::Refused(reason)).unwrap()),
    );
    let pinned = include_str!("../pinned/v7.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        samples, pinned,
        "Changes payload changes require a protocol bump"
    );
}

#[test]
fn decoded_answers_keep_bounds_and_identity_validation_even_when_status_is_empty() {
    let mut status = sample_status();
    status.entries.clear();
    assert!(ChangesAnswer::Status(status.clone()).validate().is_ok());
    status.observation.index = "invalid".into();
    assert!(ChangesAnswer::Status(status).validate().is_err());
    let huge = ChangesAnswer::Diff(ChangesDiff::Patch {
        text: "x".repeat(MAX_CHANGES_OUTPUT_BYTES + 1),
    });
    assert_eq!(huge.validate(), Err(ChangesRefusal::OutputLimit));
    let mut status = sample_status();
    status.entries[0].path = "../outside".into();
    let value = serde_json::to_value(ChangesRequest(Request::Diff {
        repository: status.repository,
        observation: status.observation,
        entry: Box::new(status.entries.remove(0)),
    }))
    .unwrap();
    assert!(serde_json::from_value::<ChangesRequest>(value).is_err());
}
