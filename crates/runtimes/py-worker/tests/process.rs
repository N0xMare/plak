//! The product worker keeps upstream framing and exit behavior at its process boundary.

#![allow(clippy::expect_used, reason = "process contract assertions")]

use std::{
    io::Write,
    process::{Command, Output, Stdio},
};

use monty_proto::{FrameReader, pb, write_frame};

fn worker(input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_monty"))
        .arg("subprocess")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("worker starts");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input)
        .expect("request");
    child.wait_with_output().expect("worker exits")
}

#[test]
fn clean_eof_exits_without_a_frame() {
    let output = worker(&[]);
    assert!(output.status.success());
    assert_eq!(output.stdout, [] as [u8; 0]);
}

#[test]
fn shutdown_replies_then_exits() {
    let mut request = Vec::new();
    write_frame(
        &mut request,
        &pb::ParentRequest {
            kind: Some(pb::parent_request::Kind::Shutdown(pb::Shutdown {})),
            trace_parent: None,
        },
    )
    .expect("frame");
    let output = worker(&request);
    assert!(output.status.success());
    let mut reader = FrameReader::new(output.stdout.as_slice());
    let event = reader
        .read::<pb::ChildEvent>()
        .expect("frame")
        .expect("reply");
    assert!(matches!(event.kind, Some(pb::child_event::Kind::Ok(_))));
    assert!(reader.read::<pb::ChildEvent>().expect("EOF").is_none());
}

#[test]
fn truncated_frame_is_fatal_and_exits_with_protocol_status() {
    let output = worker(&[1]);
    assert_eq!(output.status.code(), Some(76));
    let mut reader = FrameReader::new(output.stdout.as_slice());
    let event = reader
        .read::<pb::ChildEvent>()
        .expect("frame")
        .expect("fatal reply");
    assert!(matches!(
        event.kind,
        Some(pb::child_event::Kind::FatalError(_))
    ));
}

#[test]
fn version_reports_the_interpreter_pin() {
    let output = Command::new(env!("CARGO_BIN_EXE_monty"))
        .arg("--version")
        .output()
        .expect("worker version");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"monty 1.1.0\n");
}

#[test]
fn standalone_python_is_not_an_execution_entrypoint() {
    let output = Command::new(env!("CARGO_BIN_EXE_monty"))
        .args(["-c", "print('unexpected')"])
        .output()
        .expect("worker refusal");
    assert!(!output.status.success());
    assert_eq!(output.stdout, [] as [u8; 0]);
}

#[test]
fn configured_memory_limit_applies_to_the_next_request_frame() {
    let mut input = Vec::new();
    for kind in [
        pb::parent_request::Kind::Configure(pb::Configure {
            script_name: "memory.py".into(),
            limits: Some(pb::ResourceLimits {
                max_memory_bytes: Some(64 * 1024),
                ..Default::default()
            }),
            monty_version: "1.1.0".into(),
            protocol_version: monty_proto::PROTOCOL_VERSION,
            ..Default::default()
        }),
        pb::parent_request::Kind::Feed(pb::Feed {
            code: " ".repeat(8 * 1024 * 1024),
            ..Default::default()
        }),
    ] {
        write_frame(
            &mut input,
            &pb::ParentRequest {
                kind: Some(kind),
                trace_parent: None,
            },
        )
        .expect("frame");
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_monty"))
        .arg("subprocess")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("worker starts");
    // The allocator may exit before stdin finishes writing the oversized frame.
    if let Err(error) = child.stdin.take().expect("stdin").write_all(&input) {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    let output = child.wait_with_output().expect("worker exits");
    // Upstream monty-types 1.1.0 OOM_EXIT_CODE, part of the pool/process contract.
    assert_eq!(
        output.status.code(),
        Some(65),
        "status={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let mut reader = FrameReader::new(output.stdout.as_slice());
    let event = reader
        .read::<pb::ChildEvent>()
        .expect("frame")
        .expect("configured");
    assert!(matches!(event.kind, Some(pb::child_event::Kind::Ok(_))));
    assert!(reader.read::<pb::ChildEvent>().expect("EOF").is_none());
}
