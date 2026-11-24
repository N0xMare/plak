//! Replay the real client's recorded control traffic without a terminal.

use std::collections::{BTreeMap, VecDeque};
use std::io;

use serde_json::Value;

use super::super::runner::wait_local_runner_ready;
use super::client_control::*;
use super::*;

mod cuts;
mod directory_tests;
mod legal_reads;
pub(super) mod transcript;

#[derive(Clone, Copy)]
enum ReplayMode<'a> {
    Journey,
    Pilot(&'a Path),
    Cut(&'a Path),
}

impl<'a> ReplayMode<'a> {
    fn trace(self) -> Option<&'a Path> {
        match self {
            Self::Journey => None,
            Self::Pilot(trace) | Self::Cut(trace) => Some(trace),
        }
    }

    fn cut(self) -> bool {
        matches!(self, Self::Cut(_))
    }
}

fn fixture_trace(child: bool, switching: bool) -> Value {
    let source = match (child, switching) {
        (false, false) => {
            include_str!("../../../fixtures/client-traces/host-control-stop-root.json")
        }
        (true, false) => {
            include_str!("../../../fixtures/client-traces/host-control-stop-child.json")
        }
        (false, true) => {
            include_str!("../../../fixtures/client-traces/host-control-switch-root.json")
        }
        (true, true) => {
            include_str!("../../../fixtures/client-traces/host-control-switch-child.json")
        }
    };
    serde_json::from_str(source).unwrap()
}

struct Bindings(BTreeMap<String, Value>);

fn directory_sort_key(row: &Value) -> (std::cmp::Reverse<u64>, &str) {
    (
        std::cmp::Reverse(row["updated_at"].as_u64().expect("directory timestamp")),
        row["session"].as_str().expect("directory session identity"),
    )
}

impl Bindings {
    fn new(session: &str, workspace: &Path) -> Self {
        Self(BTreeMap::from([
            ("@uuid:0".into(), json!(session)),
            ("@workspace".into(), json!(workspace)),
        ]))
    }

    fn outgoing(&mut self, value: &Value, key: &str) -> Value {
        match value {
            Value::Object(fields) => fields
                .iter()
                .map(|(key, value)| (key.clone(), self.outgoing(value, key)))
                .collect(),
            Value::Array(values) => values
                .iter()
                .map(|value| self.outgoing(value, key))
                .collect(),
            Value::String(token) if generated_alias(token, key) => {
                if !self.0.contains_key(token) {
                    assert!(
                        matches!(key, "client_id" | "request_id" | "binary_id"),
                        "unbound input {key}: {token}"
                    );
                    let value = if key == "binary_id" {
                        json!(token)
                    } else {
                        json!(uuid::Uuid::new_v4())
                    };
                    self.0.insert(token.clone(), value);
                }
                self.0[token].clone()
            }
            _ => value.clone(),
        }
    }

    fn compare(&mut self, expected: &Value, actual: &Value) {
        self.compare_at(expected, actual, "");
    }

    fn compare_after_reconcile(&mut self, expected: &Value, actual: &Value) {
        let answer = &expected["outcome"]["ok"];
        if answer["answer"] != "sessions" {
            self.compare(expected, actual);
            return;
        }
        // The capture listed while the restarted host was still reconciling.
        // Replay waits for Ready, so only that transient status is replaced;
        // every other captured field is compared unchanged.
        let reconciling = json!({
            "status": "blocked",
            "reason": {"reason": "runner_unreachable", "phase": "reconcile", "timed_out": false},
        });
        let mut expected = expected.clone();
        for row in expected["outcome"]["ok"]["rows"].as_array_mut().unwrap() {
            assert_eq!(row["runner"]["runner_id"], "local");
            assert_eq!(row["runner_status"], reconciling, "captured status");
            row["runner_status"] = json!({"status": "ready"});
        }
        self.compare(&expected, actual);
    }

    fn compare_directory_rows(&mut self, expected: &[Value], actual: &[Value]) {
        assert_eq!(expected.len(), actual.len(), "directory row count");
        let order = actual.iter().map(directory_sort_key).collect::<Vec<_>>();
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "directory rows violate updated_at DESC, session ASC: {actual:?}"
        );
        // @time deliberately discards capture-time ordering facts. Check the
        // live sort above, then compare every complete row by bound identity.
        let mut expected = expected
            .iter()
            .map(|row| {
                let identity = self.outgoing(&row["session"], "session");
                (identity.as_str().unwrap().to_owned(), row)
            })
            .collect::<Vec<_>>();
        expected.sort_by(|left, right| left.0.cmp(&right.0));
        let mut actual = actual.iter().collect::<Vec<_>>();
        actual.sort_by_key(|row| row["session"].as_str().unwrap().to_owned());
        for ((_, expected), actual) in expected.into_iter().zip(actual) {
            self.compare_at(expected, actual, "rows");
        }
    }

    fn compare_at(&mut self, expected: &Value, actual: &Value, key: &str) {
        if matches!(key, "created_at" | "updated_at") && expected == "@time" {
            assert!(actual.is_u64(), "timestamp: {actual}");
            return;
        }
        if let Some(token) = expected
            .as_str()
            .filter(|value| generated_alias(value, key))
        {
            if let Some(bound) = self.0.get(token) {
                assert_eq!(bound, actual, "generated identity {token}");
            } else {
                assert!(
                    !self.0.iter().any(|(key, value)| key
                        .starts_with(token.split(':').next().unwrap())
                        && value == actual),
                    "different generated identities collapsed: {token}: {actual}"
                );
                self.0.insert(token.into(), actual.clone());
            }
            return;
        }
        match (expected, actual) {
            (Value::Object(left), Value::Object(right)) => {
                assert_eq!(
                    left.keys().collect::<Vec<_>>(),
                    right.keys().collect::<Vec<_>>(),
                    "field {key}: expected {expected}; actual {actual}"
                );
                for (key, value) in left {
                    self.compare_at(value, &right[key], key);
                }
            }
            (Value::Array(left), Value::Array(right)) => {
                if key == "rows" {
                    self.compare_directory_rows(left, right);
                    return;
                }
                assert_eq!(left.len(), right.len(), "{left:?} != {right:?}");
                for (left, right) in left.iter().zip(right) {
                    self.compare_at(left, right, key);
                }
            }
            _ => assert_eq!(expected, actual),
        }
    }
}

fn generated_alias(value: &str, key: &str) -> bool {
    (value.starts_with('@') && generated_field(key))
        || (key == "binary_id"
            && (value.starts_with("<client-binary:") || value.starts_with("<host-binary:")))
}

fn generated_field(key: &str) -> bool {
    matches!(
        key,
        "session"
            | "tree_id"
            | "agent_id"
            | "client_id"
            | "request_id"
            | "host_generation"
            | "workspace"
    )
}

struct Connection {
    writer: Option<std::process::ChildStdin>,
    reader: mpsc::Receiver<io::Result<String>>,
    child: Child,
    controls: VecDeque<Value>,
    answered: bool,
    transcript: transcript::FaceTranscript,
}

impl Connection {
    fn open(fixture: &Fixture) -> io::Result<Self> {
        let mut child = Child::spawn(
            fixture
                .command()
                .args(["host", "connect", "--stdio"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        );
        let output = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?;
        let (send, reader) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            reader,
            writer: child.stdin.take(),
            child,
            controls: VecDeque::new(),
            answered: false,
            transcript: transcript::FaceTranscript::default(),
        })
    }

    fn read(&mut self) -> io::Result<Value> {
        let line = self.reader.recv_timeout(WAIT).map_err(|error| {
            io::Error::from(match error {
                mpsc::RecvTimeoutError::Timeout => io::ErrorKind::TimedOut,
                mpsc::RecvTimeoutError::Disconnected => io::ErrorKind::UnexpectedEof,
            })
        })??;
        let frame: Value = serde_json::from_str(&line)?;
        self.transcript.observe(&frame)?;
        if frame["frame"] == "event" && frame.to_string().contains(ANSWER) {
            self.answered = true;
        }
        Ok(frame)
    }

    fn send(&mut self, frame: &Value) -> io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?;
        writeln!(writer, "{frame}")
    }

    fn closed(&mut self, mode: ReplayMode<'_>) -> io::Result<()> {
        drop(self.writer.take());
        let until = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                assert!(
                    mode.cut() || status.success(),
                    "bridge did not close cleanly: {status}"
                );
                self.child.wait()?;
                return Ok(());
            }
            assert!(Instant::now() < until, "bridge child was not reaped");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn eof(&mut self, mode: ReplayMode<'_>) -> io::Result<()> {
        loop {
            match self.read() {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    return self.closed(mode);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn control(&mut self) -> io::Result<Value> {
        if let Some(frame) = self.controls.pop_front() {
            return Ok(frame);
        }
        loop {
            let frame = self.read()?;
            if matches!(
                frame["frame"].as_str(),
                Some("welcome" | "response" | "refused")
            ) {
                return Ok(frame);
            }
        }
    }

    fn answer_barrier(&mut self) -> io::Result<()> {
        while !self.answered {
            let frame = self.read()?;
            if matches!(
                frame["frame"].as_str(),
                Some("welcome" | "response" | "refused")
            ) {
                self.controls.push_back(frame);
            }
        }
        Ok(())
    }
}

fn replay(
    fixture: &Fixture,
    host: &mut Host,
    session: &str,
    trace: &Value,
    child: bool,
    mode: ReplayMode<'_>,
) -> io::Result<()> {
    let mut bindings = Bindings::new(session, &fixture.workspace());
    let mut connections: BTreeMap<u64, Connection> = BTreeMap::new();
    let mut before = None;
    let mut notices_before_stop = None;
    for entry in trace["entries"].as_array().unwrap() {
        if let Some(barrier) = entry["barrier"].as_str() {
            if barrier == "host-restarted" {
                if !matches!(mode, ReplayMode::Journey) {
                    break;
                }
                before = Some((dispatches(fixture), child_dispatches(fixture)));
                connections.clear();
                host.child.as_mut().unwrap().kill().unwrap();
                host.child.take().unwrap().wait().unwrap();
                *host = start(fixture);
                wait_local_runner_ready(host);
            } else if barrier == "new-session-time-boundary" {
                separate_session_timestamp(fixture, session);
            } else if barrier == "stop-time-boundary" {
                separate_stop_timestamp(fixture);
            } else if barrier == "answered" {
                connections.get_mut(&1).unwrap().answer_barrier()?;
                let until = Instant::now() + WAIT;
                while child && child_dispatches(fixture) == 0 {
                    if mode.cut() && host.child.as_mut().unwrap().try_wait()?.is_some() {
                        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                    }
                    assert!(Instant::now() < until, "child never dispatched");
                    std::thread::sleep(Duration::from_millis(5));
                }
            } else if barrier == "stopped" {
                wait_stopped_notice(fixture, host, session, mode, notices_before_stop)?;
                if !mode.cut() {
                    wait_stopped_tree(fixture, host, session, child);
                }
            } else if barrier == "reconnected-without-replay" {
                connections
                    .get_mut(&2)
                    .unwrap()
                    .assert_transcript(fixture, session)?;
                assert_eq!(
                    before.unwrap(),
                    (dispatches(fixture), child_dispatches(fixture))
                );
                let (interrupts, decisions) = root_record(fixture, session);
                assert!(!interrupts.contains(&RootInterruptCause::Handoff));
                assert_eq!(decisions, 0);
            }
            continue;
        }
        let id = entry["connection"].as_u64().unwrap();
        if entry["opened"] == true {
            connections.insert(id, Connection::open(fixture)?);
        } else if entry["direction"] == "client" {
            if entry["frame"]["frame"] == "bye" {
                connections
                    .get_mut(&id)
                    .unwrap()
                    .assert_transcript(fixture, session)?;
            }
            let value = bindings.outgoing(&entry["frame"], "");
            let frame: ClientFrame<PlakExtension> = serde_json::from_value(value)?;
            if matches!(
                &frame,
                ClientFrame::Request {
                    op: Op::StopSession { .. },
                    ..
                }
            ) {
                notices_before_stop = mode.trace().map(notice_trace_ordinal).transpose()?;
            }
            connections
                .get_mut(&id)
                .unwrap()
                .send(&serde_json::to_value(frame)?)?;
        } else if entry["direction"] == "host" {
            let actual = connections.get_mut(&id).unwrap().control()?;
            if before.is_some() {
                bindings.compare_after_reconcile(&entry["frame"], &actual);
            } else {
                bindings.compare(&entry["frame"], &actual);
            }
        } else if entry["eof"] == "client" {
            drop(connections.get_mut(&id).unwrap().writer.take());
        } else if entry["eof"] == "host" {
            let connection = connections.get_mut(&id).unwrap();
            connection.eof(mode)?;
        }
    }
    Ok(())
}

fn wait_stopped_notice(
    fixture: &Fixture,
    host: &mut Host,
    session: &str,
    mode: ReplayMode<'_>,
    notices_before_stop: Option<u64>,
) -> io::Result<()> {
    let until = Instant::now() + WAIT;
    if let Some(trace) = mode.trace() {
        loop {
            let next = host
                .lines
                .recv_timeout(until.saturating_duration_since(Instant::now()));
            match next {
                Ok(line) if line == "commit-trace: stop_notice" => {
                    let before = notices_before_stop.expect("recorded stop request");
                    assert!(
                        notice_trace_ordinal(trace)? > before,
                        "missing stop trace receipt"
                    );
                    assert!(
                        stopped_notice_committed(fixture, session),
                        "missing durable notice"
                    );
                    return Ok(());
                }
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) if mode.cut() => {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                Err(error) => panic!("the stop notice was not committed and traced: {error}"),
            }
        }
    }
    loop {
        if stopped_notice_committed(fixture, session) {
            return Ok(());
        }
        assert!(Instant::now() < until, "the stop notice was not committed");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn notice_trace_ordinal(trace: &Path) -> io::Result<u64> {
    Ok(indexed_trace_rows(&std::fs::read_to_string(trace)?)
        .filter(|row| {
            row["store"] == "product"
                && row["label"] == "stop_notice"
                && row["tables"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|table| table == "operator_notices")
        })
        .last()
        .map_or(0, |row| row["store_ordinal"].as_u64().unwrap()))
}

#[test]
fn the_stop_trace_requires_a_complete_indexed_product_notice() {
    let directory = tempfile::tempdir().unwrap();
    let trace = directory.path().join("trace.jsonl");
    let mut file = std::fs::File::create(&trace).unwrap();
    for (store, index, indexed, label, table) in [
        ("product", 1, true, "stop_notice", "operator_notices"),
        ("runtime", 2, true, "stop_notice", "operator_notices"),
        ("product", 3, false, "stop_notice", "operator_notices"),
        ("product", 4, true, "stop_notice", "sessions"),
        ("product", 5, true, "unlabelled", "operator_notices"),
    ] {
        writeln!(
            file,
            "{}",
            json!({"store": store, "store_ordinal": index,
            "indexed": indexed, "label": label, "tables": [table]})
        )
        .unwrap();
    }
    write!(
        file,
        "{}",
        json!({"store": "product", "store_ordinal": 6,
        "indexed": true, "label": "stop_notice", "tables": ["sessions", "operator_notices"]})
    )
    .unwrap();
    assert_eq!(notice_trace_ordinal(&trace).unwrap(), 1);
    writeln!(file).unwrap();
    assert_eq!(notice_trace_ordinal(&trace).unwrap(), 6);
}

#[test]
fn real_client_control_traces_replay_root_child_switch_stop_and_reconnect() {
    for child in [false, true] {
        for switching in [false, true] {
            let fixture = fixture().unwrap();
            cold_script(&fixture, child);
            let mut host = start(&fixture);
            let session = create(&host, &fixture).unwrap();
            eprintln!("replaying {}", fixture_trace(child, switching)["fixture"]);
            replay(
                &fixture,
                &mut host,
                &session,
                &fixture_trace(child, switching),
                child,
                ReplayMode::Journey,
            )
            .unwrap();
            stop(&mut host);
        }
    }
}
