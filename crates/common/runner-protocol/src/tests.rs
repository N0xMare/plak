use serde_json::json;

use super::frame::{FrameError, FrameReader, MAX_FRAME_BYTES, decode, encode};
use super::*;

fn owner(generation: u64, fence: u64) -> LineOwner {
    LineOwner {
        line: "line".into(),
        generation,
        branch: "branch".into(),
        fence,
    }
}

#[test]
fn frames_split_from_any_chunking_of_the_stream() {
    let first = HostFrame::Ping { nonce: 7 };
    let second = HostFrame::Ack {
        request_id: "r1".into(),
    };
    let mut stream = encode(&first).unwrap();
    stream.extend(encode(&second).unwrap());
    for chunk in [1, 3, 5, stream.len()] {
        let mut reader = FrameReader::new();
        let mut frames = Vec::new();
        for bytes in stream.chunks(chunk) {
            reader.push(bytes);
            while let Some(body) = reader.next_frame().unwrap() {
                frames.push(decode::<HostFrame>(&body).unwrap());
            }
        }
        assert_eq!(frames, [first.clone(), second.clone()], "chunk {chunk}");
        assert!(!reader.has_partial_frame());
    }
}

#[test]
fn an_oversized_declared_length_is_refused_before_its_body() {
    let mut reader = FrameReader::new();
    let declared = u32::try_from(MAX_FRAME_BYTES + 1).unwrap();
    reader.push(&declared.to_be_bytes());
    assert_eq!(reader.next_frame(), Err(FrameError::TooLarge));
    let oversized = HostFrame::Stream(StreamFrame {
        stream: 1,
        seq: 0,
        event: StreamEvent::Data {
            payload: json!("x".repeat(MAX_FRAME_BYTES)),
        },
    });
    assert_eq!(encode(&oversized), Err(FrameError::TooLarge));
}

#[test]
fn frames_have_their_documented_wire_shape() {
    let request = HostFrame::Request(Request {
        request_id: "r1".into(),
        class: RequestClass::EffectCall {
            call_id: "c".into(),
            attempt_id: "a".into(),
        },
        owner: Some(owner(2, 5)),
        deadline_ms: Some(1500),
        digest: "d".into(),
        replay: false,
        operation: Operation {
            verb: verbs::READ_WORKSPACE.into(),
            payload: json!({"path": "a.txt"}),
        },
    });
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({
            "kind": "request",
            "request_id": "r1",
            "class": {"class": "effect_call", "call_id": "c", "attempt_id": "a"},
            "owner": {"line": "line", "generation": 2, "branch": "branch", "fence": 5},
            "deadline_ms": 1500,
            "digest": "d",
            "replay": false,
            "operation": {"verb": "workspace.read", "payload": {"path": "a.txt"}}
        })
    );
    let stale = RunnerFrame::StaleOwner {
        request_id: Some("r1".into()),
        control_id: None,
        current: owner(3, 0),
    };
    assert_eq!(
        serde_json::to_value(&stale).unwrap()["kind"],
        json!("stale_owner")
    );
    let refused = RunnerFrame::Refused {
        request_id: None,
        refusal: Refusal::Unsupported {
            what: "process.start".into(),
        },
    };
    assert_eq!(
        serde_json::to_value(&refused).unwrap()["refusal"],
        json!({"refusal": "unsupported", "what": "process.start"})
    );
    let body = serde_json::to_vec(&request).unwrap();
    assert_eq!(decode::<HostFrame>(&body).unwrap(), request);
    assert_eq!(
        serde_json::to_value(RequestClass::ProcessObservation { job: 3, cursor: 9 }).unwrap(),
        json!({"class": "process_observation", "job": 3, "cursor": 9})
    );
    let preempt = HostFrame::Preempt {
        request_id: "r1".into(),
        value: json!("background"),
    };
    assert_eq!(
        serde_json::to_value(&preempt).unwrap(),
        json!({"kind": "preempt", "request_id": "r1", "value": "background"})
    );
}

#[test]
fn unknown_fields_and_frame_kinds_do_not_decode() {
    assert!(decode::<HostFrame>(br#"{"kind":"ping","nonce":1,"extra":true}"#).is_err());
    assert!(decode::<HostFrame>(br#"{"kind":"process_start"}"#).is_err());
    assert!(decode::<RunnerFrame>(br#"{"kind":"pong","nonce":1}"#).is_ok());
}

#[test]
fn owners_of_a_line_are_ordered_by_generation_then_fence() {
    assert!(owner(1, 2).is_newer_than(&owner(1, 1)));
    assert!(owner(2, 0).is_newer_than(&owner(1, 9)));
    assert!(!owner(1, 1).is_newer_than(&owner(1, 1)));
    assert!(owner(1, 1).is_at_least(&owner(1, 1)));
    let mut other_line = owner(9, 9);
    other_line.line = "other".into();
    assert!(!other_line.is_newer_than(&owner(1, 1)));
}
