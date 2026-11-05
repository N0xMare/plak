//! The process-session payloads of each runner protocol version, pinned
//! beside the protocol crate's frames: a home-local runner binds any build of
//! its protocol version (spec 01 §8.4), so a payload that changes without a
//! `PROTOCOL_VERSION` bump would meet a peer that reads it differently. Every
//! session request and answer this module encodes is sampled once, the start
//! with and without its host's process context; a change fails this test
//! until the version is bumped and the new payloads are pinned beside the old
//! ones. A pinned file never changes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use plakous_model::Timestamp;
use plakous_runner_protocol::PROTOCOL_VERSION;
use serde_json::Value;

use super::super::super::{
    JoinedProcessCommand, JoinedProcessCursorPollResult, JoinedProcessOutputCursor,
    JoinedProcessOutputRecord, JoinedProcessOutputStream, JoinedProcessSessionId,
    JoinedProcessShellProfile, JoinedProcessStartRequest, JoinedProcessStatus,
    JoinedProcessTerminalMode, PrefixFact, ProcessFacts,
};
use super::{
    PollWire, SessionWire, StartedWire, StopWire, WriteWire, encode_polled, encode_reason,
    encode_session, encode_snapshot, encode_start, encode_status, to_value,
};
use crate::{JobStatus, JobStopReason, JobView};

/// Each protocol version's session payloads, one JSON value per line.
const PINNED: &[(u32, &str)] = &[(1, include_str!("pinned/v1.jsonl"))];

fn session() -> JoinedProcessSessionId {
    JoinedProcessSessionId([7; 16])
}

fn facts() -> ProcessFacts {
    ProcessFacts {
        path: "/usr/bin:/bin".into(),
        prefix: Some(PrefixFact {
            path: PathBuf::from("/workspace/tools"),
            device: 1,
            inode: 2,
        }),
        home: Some(PathBuf::from("/home/user")),
        ceiling: Some(PathBuf::from("/")),
    }
}

fn starts() -> Vec<Value> {
    let shell = JoinedProcessStartRequest::new(
        JoinedProcessCommand::shell(
            JoinedProcessShellProfile::new("/bin/sh", ["-c".to_owned()]).unwrap(),
            "echo shell",
        )
        .unwrap(),
        "sub",
        b"in".to_vec(),
        true,
        JoinedProcessTerminalMode::UnixPty,
    )
    .unwrap();
    let mut literal = JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal("/bin/echo", ["literal".to_owned()]).unwrap(),
        ".",
        Vec::new(),
        false,
        JoinedProcessTerminalMode::Pipes,
    )
    .unwrap();
    literal.output_spool_threshold = Some(4096);
    vec![
        encode_start(&shell, 3, Some(&facts())).unwrap(),
        encode_start(&literal, 4, None).unwrap(),
    ]
}

/// Every status each enum names, so a new variant fails to compile until it
/// has a sample.
const fn process_status(status: JoinedProcessStatus) -> JoinedProcessStatus {
    match status {
        JoinedProcessStatus::Running
        | JoinedProcessStatus::Exited(_)
        | JoinedProcessStatus::OutcomeUnknown
        | JoinedProcessStatus::Released
        | JoinedProcessStatus::Terminated
        | JoinedProcessStatus::Cancelled
        | JoinedProcessStatus::TimedOut
        | JoinedProcessStatus::OutputLimit
        | JoinedProcessStatus::Failed
        | JoinedProcessStatus::CleanupUnresolved => status,
    }
}

const fn stop_reason(reason: JobStopReason) -> JobStopReason {
    match reason {
        JobStopReason::Model
        | JobStopReason::User
        | JobStopReason::SessionExit
        | JobStopReason::AttemptEnded
        | JobStopReason::HardCap => reason,
    }
}

const fn job_status(status: JobStatus) -> JobStatus {
    match status {
        JobStatus::Running
        | JobStatus::Exited(_)
        | JobStatus::Stopped(_)
        | JobStatus::Released
        | JobStatus::OutcomeUnknown => status,
    }
}

const fn output_stream(stream: JoinedProcessOutputStream) -> JoinedProcessOutputStream {
    match stream {
        JoinedProcessOutputStream::Stdout
        | JoinedProcessOutputStream::Stderr
        | JoinedProcessOutputStream::Terminal => stream,
    }
}

fn statuses() -> Vec<JoinedProcessStatus> {
    [
        JoinedProcessStatus::Running,
        JoinedProcessStatus::Exited(Some(3)),
        JoinedProcessStatus::Exited(None),
        JoinedProcessStatus::OutcomeUnknown,
        JoinedProcessStatus::Released,
        JoinedProcessStatus::Terminated,
        JoinedProcessStatus::Cancelled,
        JoinedProcessStatus::TimedOut,
        JoinedProcessStatus::OutputLimit,
        JoinedProcessStatus::Failed,
        JoinedProcessStatus::CleanupUnresolved,
    ]
    .into_iter()
    .map(process_status)
    .collect()
}

fn reasons() -> Vec<JobStopReason> {
    [
        JobStopReason::Model,
        JobStopReason::User,
        JobStopReason::SessionExit,
        JobStopReason::AttemptEnded,
        JobStopReason::HardCap,
    ]
    .into_iter()
    .map(stop_reason)
    .collect()
}

fn job(number: u32, status: JobStatus) -> JobView {
    JobView {
        number,
        command: "sleep 30".into(),
        started_at: Timestamp::new(1_800_000_000, 5).unwrap(),
        owner: "agent".into(),
        elapsed: Duration::from_millis(1500),
        status: job_status(status),
        output: "tail".into(),
        session_id: [9; 16],
        owner_generation: 6,
    }
}

fn encoded() -> String {
    let mut lines = starts();
    lines.push(
        to_value(&StartedWire {
            session: encode_session(session()),
            instance: "instance".into(),
        })
        .unwrap(),
    );
    lines.push(
        to_value(&WriteWire {
            session: encode_session(session()),
            bytes: "6869".into(),
            close: true,
        })
        .unwrap(),
    );
    lines.push(
        to_value(&PollWire {
            session: encode_session(session()),
            cursor: 10,
            max_bytes: 65_536,
            max_records: 64,
            wait_ms: 1000,
        })
        .unwrap(),
    );
    let records: Vec<JoinedProcessOutputRecord> = [
        JoinedProcessOutputStream::Stdout,
        JoinedProcessOutputStream::Stderr,
        JoinedProcessOutputStream::Terminal,
    ]
    .into_iter()
    .map(output_stream)
    .zip(1_u64..)
    .map(|(stream, sequence)| JoinedProcessOutputRecord {
        sequence,
        cursor: sequence * 10,
        stream,
        bytes: Arc::from(&b"out"[..]),
    })
    .collect();
    lines.push(
        encode_polled(&JoinedProcessCursorPollResult {
            records: records.into(),
            status: JoinedProcessStatus::Running,
            next_cursor: JoinedProcessOutputCursor(40),
            more_output: true,
            skipped_bytes: 2,
        })
        .unwrap(),
    );
    lines.extend(statuses().into_iter().map(encode_status));
    for (reason, session) in
        reasons()
            .into_iter()
            .zip([Some(encode_session(session())), None, None, None, None])
    {
        lines.push(
            to_value(&StopWire {
                session,
                reason: encode_reason(reason).to_owned(),
                grace_ms: 200,
            })
            .unwrap(),
        );
    }
    lines.push(
        to_value(&SessionWire {
            session: encode_session(session()),
        })
        .unwrap(),
    );
    let mut jobs = vec![
        job(1, JobStatus::Running),
        job(2, JobStatus::Exited(Some(0))),
        job(3, JobStatus::Exited(None)),
        job(5, JobStatus::Released),
        job(6, JobStatus::OutcomeUnknown),
    ];
    jobs.extend(
        reasons()
            .into_iter()
            .zip(10_u32..)
            .map(|(reason, number)| job(number, JobStatus::Stopped(reason))),
    );
    lines.push(encode_snapshot("instance", &jobs).unwrap());
    let mut text = String::new();
    for line in lines {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    text
}

#[test]
fn a_session_payload_changes_only_with_a_protocol_version_bump() {
    let encoded = encoded();
    let Some((_, pinned)) = PINNED
        .iter()
        .find(|(version, _)| *version == PROTOCOL_VERSION)
    else {
        panic!(
            "protocol {PROTOCOL_VERSION} has no pinned session payloads: add \
             pinned/v{PROTOCOL_VERSION}.jsonl holding exactly:\n{encoded}"
        );
    };
    assert!(
        encoded == *pinned,
        "a session payload changed under protocol {PROTOCOL_VERSION}: bump PROTOCOL_VERSION \
         and pin the new payloads beside the old ones. Now:\n{encoded}"
    );
}
