//! The frames of each protocol version, pinned. A home-local runner binds any
//! build of its protocol version (spec 01 §8.4), so a frame that changes
//! without a [`PROTOCOL_VERSION`] bump would meet a peer that reads it
//! differently. Every variant of every frame enum is encoded here once; a
//! change to any of them, to a verb or to a capability fails this test until
//! the version is bumped and the new version's frames are pinned beside the
//! old ones. A pinned file never changes.

use serde_json::{Value, json};

use crate::{
    Bind, Hello, HostFrame, LineOwner, LineStatus, Operation, PROTOCOL_VERSION, Refusal, Request,
    RequestClass, RunnerFrame, StreamEvent, StreamFrame, TransferOwner, VersionTriple, WireError,
    capabilities, verbs,
};

/// Each protocol version's frames, one JSON value per line.
const PINNED: &[(u32, &str)] = &[
    (1, include_str!("pinned/v1.jsonl")),
    (2, include_str!("pinned/v2.jsonl")),
    (3, include_str!("pinned/v3.jsonl")),
];

fn owner() -> LineOwner {
    LineOwner {
        line: "line".into(),
        generation: 2,
        branch: "branch".into(),
        fence: 3,
    }
}

fn version() -> VersionTriple {
    VersionTriple::current("revision", "flavor")
}

/// Names each variant, so a new one fails to compile until it has a sample.
const fn class_variant(class: &RequestClass) -> &'static str {
    match class {
        RequestClass::EffectCall { .. } => "effect_call",
        RequestClass::BuildRead => "build_read",
        RequestClass::ProcessObservation { .. } => "process_observation",
        RequestClass::OwnerControl { .. } => "owner_control",
        RequestClass::TreeOperation { .. } => "tree_operation",
    }
}

const fn event_variant(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::Data { .. } => "data",
        StreamEvent::Credit { .. } => "credit",
        StreamEvent::Close { .. } => "close",
    }
}

const fn refusal_variant(refusal: &Refusal) -> &'static str {
    match refusal {
        Refusal::Unbound => "unbound",
        Refusal::VersionMismatch { .. } => "version_mismatch",
        Refusal::Unsupported { .. } => "unsupported",
        Refusal::OwnerNotTransferred => "owner_not_transferred",
        Refusal::NotPaired { .. } => "not_paired",
        Refusal::InspectOnly => "inspect_only",
        Refusal::Malformed { .. } => "malformed",
    }
}

const fn host_variant(frame: &HostFrame) -> &'static str {
    match frame {
        HostFrame::Bind(_) => "bind",
        HostFrame::Request(_) => "request",
        HostFrame::Cancel { .. } => "cancel",
        HostFrame::Preempt { .. } => "preempt",
        HostFrame::Ack { .. } => "ack",
        HostFrame::TransferOwner(_) => "transfer_owner",
        HostFrame::Lines { .. } => "lines",
        HostFrame::Ping { .. } => "ping",
        HostFrame::Stream(_) => "stream",
    }
}

const fn runner_variant(frame: &RunnerFrame) -> &'static str {
    match frame {
        RunnerFrame::Hello(_) => "hello",
        RunnerFrame::Result { .. } => "result",
        RunnerFrame::Error { .. } => "error",
        RunnerFrame::InProgress { .. } => "in_progress",
        RunnerFrame::NotSeen { .. } => "not_seen",
        RunnerFrame::StaleRequest { .. } => "stale_request",
        RunnerFrame::Spent { .. } => "spent",
        RunnerFrame::StaleOwner { .. } => "stale_owner",
        RunnerFrame::TransferAccepted { .. } => "transfer_accepted",
        RunnerFrame::TransferRefused { .. } => "transfer_refused",
        RunnerFrame::Expired { .. } => "expired",
        RunnerFrame::Lines { .. } => "lines",
        RunnerFrame::Refused { .. } => "refused",
        RunnerFrame::Pong { .. } => "pong",
        RunnerFrame::Stream(_) => "stream",
    }
}

fn request(class: RequestClass) -> Request {
    Request {
        request_id: "request".into(),
        class,
        owner: Some(owner()),
        deadline_ms: Some(1),
        digest: "digest".into(),
        replay: true,
        age_ms: Some(2),
        operation: Operation {
            verb: "verb".into(),
            payload: json!({"payload": true}),
        },
    }
}

fn stream(event: StreamEvent) -> StreamFrame {
    StreamFrame {
        stream: 4,
        seq: 5,
        event,
    }
}

fn classes() -> Vec<RequestClass> {
    vec![
        RequestClass::EffectCall {
            call_id: "call".into(),
            attempt_id: "attempt".into(),
        },
        RequestClass::BuildRead,
        RequestClass::ProcessObservation { job: 6, cursor: 7 },
        RequestClass::OwnerControl {
            control_id: "control".into(),
        },
        RequestClass::TreeOperation {
            tree_id: "tree".into(),
            operation_id: "operation".into(),
        },
    ]
}

fn events() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Data {
            payload: json!("data"),
        },
        StreamEvent::Credit { bytes: 8 },
        StreamEvent::Close {
            reason: Some("reason".into()),
        },
    ]
}

fn refusals() -> Vec<Refusal> {
    vec![
        Refusal::Unbound,
        Refusal::VersionMismatch { runner: version() },
        Refusal::Unsupported {
            what: "what".into(),
        },
        Refusal::OwnerNotTransferred,
        Refusal::NotPaired {
            host_id: "host".into(),
        },
        Refusal::InspectOnly,
        Refusal::Malformed {
            detail: "detail".into(),
        },
    ]
}

fn host_frames() -> Vec<HostFrame> {
    let mut frames = vec![
        HostFrame::Bind(Bind {
            host_id: "host".into(),
            host_instance: "instance".into(),
            version: version(),
            inspect: true,
        }),
        HostFrame::Cancel {
            request_id: "request".into(),
        },
        HostFrame::Preempt {
            request_id: "request".into(),
            value: json!("steer"),
        },
        HostFrame::Ack {
            request_id: "request".into(),
        },
        HostFrame::TransferOwner(TransferOwner {
            control_id: "control".into(),
            from: Some(owner()),
            to: owner(),
        }),
        HostFrame::Lines {
            request_id: "request".into(),
        },
        HostFrame::Ping { nonce: 9 },
    ];
    frames.extend(classes().into_iter().map(request).map(HostFrame::Request));
    frames.extend(events().into_iter().map(stream).map(HostFrame::Stream));
    frames
}

fn runner_frames() -> Vec<RunnerFrame> {
    let mut frames = vec![
        RunnerFrame::Hello(Hello {
            version: version(),
            binary_id: Some("binary".into()),
            runner_id: "runner".into(),
            instance: "instance".into(),
            boot_id: "boot".into(),
            descriptor: json!({"descriptor": true}),
            capabilities: vec!["capability".into()],
            path: Some("/bin".into()),
            home: Some("/home".into()),
            job_bound: 10,
        }),
        RunnerFrame::Result {
            request_id: "request".into(),
            payload: json!({"result": true}),
        },
        RunnerFrame::Error {
            request_id: "request".into(),
            error: WireError {
                error: json!({"error": true}),
                proven_no_effect: true,
            },
        },
        RunnerFrame::InProgress {
            request_id: "request".into(),
        },
        RunnerFrame::NotSeen {
            request_id: "request".into(),
        },
        RunnerFrame::StaleRequest {
            request_id: "request".into(),
        },
        RunnerFrame::Spent {
            request_id: "request".into(),
        },
        RunnerFrame::StaleOwner {
            request_id: Some("request".into()),
            control_id: Some("control".into()),
            current: owner(),
        },
        RunnerFrame::TransferAccepted {
            control_id: "control".into(),
            stopped: vec![crate::StoppedForeground {
                call_key: "call:attempt".into(),
                pid: 42,
                ended: "Terminated".into(),
            }],
        },
        RunnerFrame::TransferRefused {
            control_id: "control".into(),
            detail: "detail".into(),
        },
        RunnerFrame::Expired {
            request_id: "request".into(),
        },
        RunnerFrame::Lines {
            request_id: "request".into(),
            lines: vec![LineStatus {
                host_id: "host".into(),
                line: "line".into(),
                owner: Some(owner()),
                adopted: true,
                foreground: false,
                jobs: json!({"jobs": []}),
            }],
        },
        RunnerFrame::Pong { nonce: 11 },
    ];
    frames.extend(refusals().into_iter().map(|refusal| RunnerFrame::Refused {
        request_id: Some("request".into()),
        refusal,
    }));
    frames.extend(events().into_iter().map(stream).map(RunnerFrame::Stream));
    frames
}

/// Every sample, one JSON value per line, after checking that the samples
/// cover every variant.
fn encoded() -> String {
    let (hosts, runners) = (host_frames(), runner_frames());
    let named = |names: Vec<&'static str>| {
        let mut names = names;
        names.sort_unstable();
        names.dedup();
        names.len()
    };
    assert_eq!(named(hosts.iter().map(host_variant).collect()), 9);
    assert_eq!(named(runners.iter().map(runner_variant).collect()), 15);
    assert_eq!(named(classes().iter().map(class_variant).collect()), 5);
    assert_eq!(named(events().iter().map(event_variant).collect()), 3);
    assert_eq!(named(refusals().iter().map(refusal_variant).collect()), 7);
    let mut lines: Vec<Value> = Vec::new();
    lines.extend(
        hosts
            .iter()
            .map(|frame| serde_json::to_value(frame).unwrap()),
    );
    lines.extend(
        runners
            .iter()
            .map(|frame| serde_json::to_value(frame).unwrap()),
    );
    lines.push(json!({"verbs": [
        verbs::READ_WORKSPACE, verbs::QUERY_REPOSITORY, verbs::MUTATE_WORKSPACE,
        verbs::EXECUTE_LITERAL_PROCESS, verbs::PROCESS_START, verbs::PROCESS_WRITE,
        verbs::PROCESS_POLL, verbs::PROCESS_STOP, verbs::PROCESS_PROMOTE,
        verbs::JOBS_SNAPSHOT, verbs::JOBS_STOP, verbs::JOBS_DISOWN, verbs::JOBS_WATCH,
        verbs::SERVICE_SPAWN, verbs::SERVICE_TERMINATE,
    ]}));
    lines.push(json!({"capabilities": [capabilities::WORKSPACE_VERBS, capabilities::STREAMS]}));
    let mut text = String::new();
    for line in lines {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    text
}

#[test]
fn a_frame_changes_only_with_a_protocol_version_bump() {
    let encoded = encoded();
    let Some((_, pinned)) = PINNED
        .iter()
        .find(|(version, _)| *version == PROTOCOL_VERSION)
    else {
        panic!(
            "protocol {PROTOCOL_VERSION} has no pinned frames: add pinned/v{PROTOCOL_VERSION}.jsonl \
             holding exactly:\n{encoded}"
        );
    };
    assert!(
        encoded == *pinned,
        "a frame, verb or capability changed under protocol {PROTOCOL_VERSION}: bump \
         PROTOCOL_VERSION and pin the new frames beside the old ones. Now:\n{encoded}"
    );
}
