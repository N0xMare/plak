//! The current protocol's frames, pinned. A home-local runner binds any
//! build of its protocol version (spec 01 §8.4), so a frame that changes
//! without a [`PROTOCOL_VERSION`] bump would meet a peer that reads it
//! differently. Every shared frame variant, verb and capability is pinned;
//! an exact-triple-only extension gets a separate pin.

use serde_json::{Value, json};

use crate::{
    Bind, Hello, HostFrame, LineOwner, LineStatus, Operation, PROTOCOL_VERSION, Refusal, Request,
    RequestClass, RunnerFrame, StreamEvent, StreamFrame, TransferOwner, VersionTriple, WireError,
    capabilities, verbs,
};

const PINNED_VERSION: u32 = 9;
const PINNED: &str = include_str!("pinned/v9.jsonl");

#[test]
fn obsolete_runner_frames_still_decode_for_inspection() {
    let mut frames = include_str!("pinned/obsolete-inspection.jsonl").lines();
    let RunnerFrame::Hello(hello) =
        crate::frame::decode(frames.next().unwrap().as_bytes()).unwrap()
    else {
        panic!("expected obsolete Hello");
    };
    assert_eq!(hello.version.protocol, 1);
    assert_eq!(hello.machine_id, None);
    assert!(
        frames.clone().count() >= 2,
        "pin both obsolete Lines shapes"
    );
    for frame in frames {
        let pinned: Value = serde_json::from_str(frame).unwrap();
        let RunnerFrame::Lines { request_id, lines } =
            crate::frame::decode(frame.as_bytes()).unwrap()
        else {
            panic!("expected obsolete Lines answer");
        };
        assert_eq!(pinned["request_id"], request_id);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].foreground,
            pinned["lines"][0]["foreground"].as_bool().unwrap_or(false)
        );
        assert_eq!(pinned["lines"][0]["line"], lines[0].line);
    }
}

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
        RequestClass::OperatorControl { .. } => "operator_control",
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
        HostFrame::Qualify { .. } => "qualify",
        HostFrame::Request(_) => "request",
        HostFrame::Cancel { .. } => "cancel",
        HostFrame::Preempt { .. } => "preempt",
        HostFrame::Ack { .. } => "ack",
        HostFrame::TransferOwner(_) => "transfer_owner",
        HostFrame::Lines { .. } => "lines",
        HostFrame::CarryLines { .. } => "carry_lines",
        HostFrame::OwnerFloors { .. } => "owner_floors",
        HostFrame::JobTerminals { .. } => "job_terminals",
        HostFrame::ForgetOwnerWitnesses { .. } => "forget_owner_witnesses",
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
        RunnerFrame::OwnerFloors { .. } => "owner_floors",
        RunnerFrame::JobTerminals { .. } => "job_terminals",
        RunnerFrame::OwnerWitnessesForgotten { .. } => "owner_witnesses_forgotten",
        RunnerFrame::Refused { .. } => "refused",
        RunnerFrame::Pong { .. } => "pong",
        RunnerFrame::Stream(_) => "stream",
    }
}

fn request(class: RequestClass) -> Request {
    Request {
        request_id: "request".into(),
        owner: (!matches!(class, RequestClass::OperatorControl { .. })).then(owner),
        class,
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
        RequestClass::OperatorControl {
            namespace: crate::operator::OperatorNamespace {
                state_key: crate::operator::OperatorId::try_from("state".to_owned()).unwrap(),
                principal: crate::operator::OperatorId::try_from("principal".to_owned()).unwrap(),
            },
        },
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
        HostFrame::Qualify {
            host_id: "host".into(),
            version: version(),
        },
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
        HostFrame::OwnerFloors {
            request_id: "request".into(),
            lines: vec!["line".into(), "unknown".into()],
        },
        HostFrame::JobTerminals {
            request_id: "request".into(),
            owner: owner(),
            after: 0,
        },
        HostFrame::ForgetOwnerWitnesses {
            request_id: "request".into(),
            lines: vec!["line".into(), "unknown".into()],
        },
        HostFrame::Ping { nonce: 9 },
    ];
    frames.extend(classes().into_iter().map(request).map(HostFrame::Request));
    frames.extend(events().into_iter().map(stream).map(HostFrame::Stream));
    frames
}

fn runner_hello() -> RunnerFrame {
    RunnerFrame::Hello(Hello {
        version: version(),
        binary_id: Some("binary".into()),
        runner_id: "runner".into(),
        instance: "instance".into(),
        boot_id: "boot".into(),
        machine_id: Some("machine".into()),
        descriptor: json!({"descriptor": true}),
        capabilities: vec!["capability".into()],
        path: Some("/bin".into()),
        home: Some("/home".into()),
        job_bound: 10,
    })
}

fn runner_frames() -> Vec<RunnerFrame> {
    let mut frames = vec![
        runner_hello(),
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
            unpromoted: vec![crate::UnpromotedProcess {
                call_key: "live:attempt".into(),
                pid: 43,
            }],
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
        RunnerFrame::OwnerFloors {
            request_id: "request".into(),
            floors: vec![
                crate::OwnerFloor {
                    line: "line".into(),
                    retained: vec![owner()],
                    owner: Some(owner()),
                },
                crate::OwnerFloor {
                    line: "unknown".into(),
                    retained: Vec::new(),
                    owner: None,
                },
            ],
        },
        RunnerFrame::JobTerminals {
            request_id: "request".into(),
            owner: owner(),
            jobs: vec![],
            next: None,
        },
        RunnerFrame::OwnerWitnessesForgotten {
            request_id: "request".into(),
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
    assert_eq!(named(hosts.iter().map(host_variant).collect()), 13);
    assert_eq!(named(runners.iter().map(runner_variant).collect()), 18);
    assert_eq!(named(classes().iter().map(class_variant).collect()), 6);
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
        verbs::FOREGROUND_STOP,
        verbs::WORKSPACE_OPTIONS, verbs::WORKSPACE_DISCOVER, verbs::WORKSPACE_VALIDATE, verbs::WORKSPACE_CHANGES, verbs::READ_WORKSPACE, verbs::QUERY_REPOSITORY, verbs::MUTATE_WORKSPACE,
        verbs::EXECUTE_LITERAL_PROCESS, verbs::PROCESS_START, verbs::PROCESS_WRITE,
        verbs::PROCESS_POLL, verbs::PROCESS_STOP, verbs::PROCESS_PROMOTE,
        verbs::JOBS_SNAPSHOT, verbs::JOBS_STOP, verbs::JOBS_DISOWN, verbs::JOBS_WATCH,
        verbs::JOBS_ACK,
        verbs::SERVICE_SPAWN, verbs::SERVICE_TERMINATE,
    ]}));
    lines.push(json!({"capabilities": [capabilities::WORKSPACE_VERBS, capabilities::STREAMS, capabilities::OPERATOR_PTY]}));
    lines.push(json!({"operator_verbs": crate::operator::VERBS}));
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
    assert_eq!(
        PINNED_VERSION, PROTOCOL_VERSION,
        "protocol {PROTOCOL_VERSION} has no pinned frames: replace the fixture with \
         pinned/v{PROTOCOL_VERSION}.jsonl holding exactly:\n{encoded}"
    );
    assert!(
        encoded == PINNED,
        "a frame, verb or capability changed under protocol {PROTOCOL_VERSION}: bump \
         PROTOCOL_VERSION and pin the new frames in place of the old ones. Now:\n{encoded}"
    );
}

#[test]
fn exact_triple_carry_inventory_extension_is_pinned_separately() {
    let frame = HostFrame::CarryLines {
        request_id: "request".into(),
    };
    assert_eq!(
        serde_json::to_value(frame).unwrap(),
        json!({"kind":"carry_lines","request_id":"request"})
    );
    assert_eq!(capabilities::CARRY_LINES, "carry-lines");
}
