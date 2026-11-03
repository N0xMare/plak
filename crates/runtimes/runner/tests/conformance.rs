//! The conformance subset (spec 01 §8.4): one suite against the direct local
//! backend and through the loopback runner, then the protocol's keying,
//! fencing and restart laws through the loopback and at the frame level.

#![allow(clippy::unwrap_used, reason = "integration tests")]

use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, RunId};
use plakous_runner::{ClientWindows, ConnectError, Loopback, RunnerIdentity, RunnerService};
use plakous_runner_protocol::frame::{FrameReader, MAX_FRAME_BYTES, decode, encode};
use plakous_runner_protocol::{
    Bind, Hello, HostFrame, Operation, Refusal, Request, RequestClass, RunnerFrame, StreamEvent,
    StreamFrame, TransferOwner, VersionTriple,
};
use plakous_tools::internal::runner::{
    RunnerBackend, RunnerCall, RunnerCallFuture, RunnerFailure, RunnerLink, runner_environment,
};
use plakous_tools::{
    ExecutionBackendErrorKind, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentLimits, ExecutionEnvironmentScope, LineOwner, LiteralProcessIntent,
    LiteralProcessRequest, OperationClass, OperationContext, RepositoryMatchCase, RepositoryQuery,
    RepositoryQueryLimits, RunnerId, ServiceProcessRequest, WorkspaceExpectedFileState,
    WorkspaceMutation, WorkspaceMutationTransaction, WorkspaceReadSliceRequest,
    WorkspaceReplacement,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

const LINE: &str = "00000000-0000-0000-0000-0000000000a1";
const BRANCH: &str = "00000000-0000-0000-0000-0000000000b1";

fn scope() -> ExecutionEnvironmentScope {
    ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000041").unwrap())
}

fn config() -> ExecutionEnvironmentConfig {
    ExecutionEnvironmentConfig::new(ExecutionEnvironmentLimits::default(), [], []).unwrap()
}

fn host(root: &Path) -> ExecutionEnvironment {
    ExecutionEnvironment::host(scope(), root, ".", config()).unwrap()
}

fn version() -> VersionTriple {
    VersionTriple::current("conformance", "debug")
}

fn service(root: &Path) -> RunnerService {
    RunnerService::new(
        host(root),
        RunnerIdentity {
            runner_id: RunnerId::new("loopback").unwrap(),
            version: version(),
            boot_id: "boot-1".into(),
            binary_id: None,
        },
    )
    .unwrap()
}

fn bind(host_instance: &str) -> Bind {
    Bind {
        host_id: "host-1".into(),
        host_instance: host_instance.into(),
        version: version(),
    }
}

fn windows() -> ClientWindows {
    ClientWindows {
        read: Duration::from_secs(10),
        effect: Duration::from_secs(10),
    }
}

async fn loopback(root: &Path) -> (Loopback, ExecutionEnvironment) {
    let loopback = Loopback::join(service(root), bind("instance-1"), windows())
        .await
        .unwrap();
    let environment = runner_environment(scope(), root, ".", config(), loopback.backend()).unwrap();
    (loopback, environment)
}

fn unkeyed() -> OperationContext {
    OperationContext::new(CancellationToken::new())
}

fn owner(generation: u64, fence: u64) -> LineOwner {
    LineOwner::new(
        LineId::from_str(LINE).unwrap(),
        generation,
        BranchId::from_str(BRANCH).unwrap(),
        fence,
    )
}

fn effect(owner: LineOwner, call: u8) -> OperationContext {
    OperationContext::new(CancellationToken::new())
        .with_owner(owner)
        .with_class(OperationClass::EffectCall {
            call_id: CallId::from_str(&format!("00000000-0000-0000-0000-0000000000{call:02x}"))
                .unwrap(),
            attempt_id: AttemptId::from_str("00000000-0000-0000-0000-0000000000c1").unwrap(),
        })
}

fn shell(script: &str) -> LiteralProcessRequest {
    LiteralProcessRequest::new(
        "/bin/sh",
        ["-c".to_owned(), script.to_owned()],
        ".",
        Vec::new(),
        LiteralProcessIntent::default(),
    )
    .unwrap()
}

fn appended(root: &Path) -> usize {
    std::fs::read_to_string(root.join("log")).map_or(0, |text| text.lines().count())
}

fn query_limits() -> RepositoryQueryLimits {
    RepositoryQueryLimits::new(100, 100, 1 << 20, 1 << 22, 100, 4096, 1 << 20).unwrap()
}

/// Every operation the subset covers, observed as text; each step also
/// checks its own expectation.
async fn suite(environment: &ExecutionEnvironment, root: &Path) -> Vec<String> {
    std::fs::write(root.join("a.txt"), "alpha\nbeta needle\n").unwrap();
    let mut seen = Vec::new();

    let first = environment
        .read_workspace(
            WorkspaceReadSliceRequest::start("a.txt", 4).unwrap(),
            unkeyed(),
        )
        .await
        .unwrap();
    let rest = environment
        .read_workspace(
            WorkspaceReadSliceRequest::resume(first.next().unwrap().clone(), 64).unwrap(),
            unkeyed(),
        )
        .await
        .unwrap();
    seen.push(format!("{first:?}"));
    seen.push(format!("{rest:?}"));
    let missing = environment
        .read_workspace(
            WorkspaceReadSliceRequest::start("absent.txt", 4).unwrap(),
            unkeyed(),
        )
        .await
        .unwrap_err();
    assert_eq!(missing.kind(), ExecutionBackendErrorKind::NotFound);
    seen.push(format!("{:?} {}", missing.kind(), missing.message()));

    for query in [
        RepositoryQuery::new(
            ".",
            "needle",
            RepositoryMatchCase::Sensitive,
            query_limits(),
        )
        .unwrap(),
        RepositoryQuery::regex(
            ".",
            "^be",
            RepositoryMatchCase::AsciiInsensitive,
            query_limits(),
        )
        .unwrap(),
        RepositoryQuery::list(".", None, 2, query_limits()).unwrap(),
    ] {
        let result = environment
            .query_repository(query, unkeyed())
            .await
            .unwrap();
        seen.push(format!("{result:?}"));
    }

    let process = LiteralProcessRequest::new(
        "/bin/sh",
        [
            "-c".to_owned(),
            "cat; echo out; echo err >&2; exit 3".to_owned(),
        ],
        ".",
        b"in\n".to_vec(),
        LiteralProcessIntent::default(),
    )
    .unwrap();
    let ran = environment
        .execute_literal_process(process, effect(owner(1, 1), 1))
        .await
        .unwrap();
    assert_eq!(ran.exit_code(), Some(3));
    assert_eq!(&ran.stdout()[..], b"in\nout\n");
    assert_eq!(&ran.stderr()[..], b"err\n");
    seen.push(format!("{ran:?} {:?} {:?}", ran.stdout(), ran.stderr()));

    seen.extend(edits(environment, root).await);
    seen
}

/// The suite's mutations, then reads the backend refuses or cannot decode.
async fn edits(environment: &ExecutionEnvironment, root: &Path) -> Vec<String> {
    let mut seen = Vec::new();
    let create = WorkspaceMutationTransaction::new([WorkspaceMutation::new(
        "b.txt",
        WorkspaceExpectedFileState::Missing,
        WorkspaceReplacement::content("bee"),
    )
    .unwrap()])
    .unwrap();
    let created = environment
        .mutate_workspace(create.clone(), effect(owner(1, 1), 2))
        .await
        .unwrap();
    assert_eq!(std::fs::read(root.join("b.txt")).unwrap(), b"bee");
    seen.push(format!("{created:?}"));
    let conflict = environment
        .mutate_workspace(create, effect(owner(1, 1), 3))
        .await
        .unwrap_err();
    assert_eq!(conflict.kind(), ExecutionBackendErrorKind::Conflict);
    assert!(conflict.proven_no_effect());
    seen.push(format!(
        "{:?} {} {:?}",
        conflict.kind(),
        conflict.message(),
        conflict.workspace_conflict_path()
    ));

    let replace = |expected, replacement| {
        WorkspaceMutationTransaction::new([
            WorkspaceMutation::new("b.txt", expected, replacement).unwrap()
        ])
        .unwrap()
    };
    let replaced = environment
        .mutate_workspace(
            replace(
                WorkspaceExpectedFileState::Any,
                WorkspaceReplacement::content("bea"),
            ),
            effect(owner(1, 1), 4),
        )
        .await
        .unwrap();
    seen.push(format!("{replaced:?}"));
    let current = environment
        .read_workspace(
            WorkspaceReadSliceRequest::start("b.txt", 64).unwrap(),
            unkeyed(),
        )
        .await
        .unwrap()
        .content();
    let deleted = environment
        .mutate_workspace(
            replace(
                WorkspaceExpectedFileState::Content(current),
                WorkspaceReplacement::Missing,
            ),
            effect(owner(1, 1), 5),
        )
        .await
        .unwrap();
    seen.push(format!("{deleted:?}"));
    assert!(!root.join("b.txt").exists());

    std::fs::write(root.join("c.bin"), [0xff, 0xfe, b'a', b'\n']).unwrap();
    // A link out of the workspace is the backend's refusal; a path that
    // names an escape is refused before any backend sees it.
    std::os::unix::fs::symlink("/bin/sh", root.join("escape")).unwrap();
    for path in ["c.bin", "escape"] {
        let read = environment
            .read_workspace(
                WorkspaceReadSliceRequest::start(path, 4).unwrap(),
                unkeyed(),
            )
            .await;
        seen.push(match read {
            Ok(slice) => format!("{slice:?}"),
            Err(error) => format!(
                "{:?} {} {}",
                error.kind(),
                error.message(),
                error.proven_no_effect()
            ),
        });
    }
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_suite_observes_the_same_through_the_loopback_as_direct() {
    let direct_root = tempfile::tempdir().unwrap();
    let direct = suite(&host(direct_root.path()), direct_root.path()).await;
    let runner_root = tempfile::tempdir().unwrap();
    let (loopback, environment) = loopback(runner_root.path()).await;
    assert_eq!(
        environment
            .descriptor()
            .backend()
            .runner_id()
            .map(RunnerId::as_str),
        Some("loopback")
    );
    assert_eq!(suite(&environment, runner_root.path()).await, direct);
    drop(loopback);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn services_are_unsupported_through_the_loopback() {
    let root = tempfile::tempdir().unwrap();
    let (_loopback, environment) = loopback(root.path()).await;
    let request = ServiceProcessRequest::new(
        "/bin/cat",
        Vec::<String>::new(),
        Vec::new(),
        Vec::new(),
        1024,
    )
    .unwrap();
    let error = environment
        .spawn_service(request, unkeyed())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ExecutionBackendErrorKind::Unsupported);
    assert!(error.proven_no_effect());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_effect_call_key_runs_at_most_once() {
    let root = tempfile::tempdir().unwrap();
    let (_loopback, environment) = loopback(root.path()).await;
    let append = || shell("echo x >> log");
    environment
        .execute_literal_process(append(), effect(owner(1, 1), 1))
        .await
        .unwrap();
    let again = environment
        .execute_literal_process(append(), effect(owner(1, 1), 1))
        .await
        .unwrap_err();
    // The key is spent: the call ran, so a resend is of unknown outcome.
    assert_eq!(again.kind(), ExecutionBackendErrorKind::RecoveryRequired);
    assert!(!again.proven_no_effect());
    assert_eq!(appended(root.path()), 1);
    // Unkeyed work is not recorded and runs each time.
    for _ in 0..2 {
        environment
            .execute_literal_process(append(), unkeyed())
            .await
            .unwrap();
    }
    assert_eq!(appended(root.path()), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_owner_is_transferred_first_and_older_owners_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let (_loopback, environment) = loopback(root.path()).await;
    let append = || shell("echo x >> log");
    environment
        .execute_literal_process(append(), effect(owner(1, 4), 1))
        .await
        .unwrap();
    // A successor head of the line: a later generation with a lower fence.
    environment
        .execute_literal_process(append(), effect(owner(2, 1), 2))
        .await
        .unwrap();
    for (stale, call) in [(owner(1, 4), 3), (owner(1, 9), 4)] {
        let error = environment
            .execute_literal_process(append(), effect(stale, call))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ExecutionBackendErrorKind::IdentityMismatch);
        assert!(error.proven_no_effect());
    }
    // The newest owner still runs: a refused transfer changed nothing.
    environment
        .execute_literal_process(append(), effect(owner(2, 1), 5))
        .await
        .unwrap();
    assert_eq!(appended(root.path()), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_keyed_call_outlives_a_lost_link_and_answers_once_on_reconnection() {
    let root = tempfile::tempdir().unwrap();
    let (mut loopback, environment) = loopback(root.path()).await;
    let call = tokio::spawn({
        let environment = environment.clone();
        async move {
            environment
                .execute_literal_process(
                    shell("sleep 1; echo x >> log; echo done"),
                    effect(owner(1, 1), 1),
                )
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let hello = loopback.reconnect().await.unwrap();
    assert_eq!(&hello, loopback.service().hello());
    let result = call.await.unwrap().unwrap();
    assert_eq!(&result.stdout()[..], b"done\n");
    assert_eq!(appended(root.path()), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_restart_leaves_outstanding_calls_unknown_and_refences_lines() {
    let root = tempfile::tempdir().unwrap();
    let (mut loopback, environment) = loopback(root.path()).await;
    let before = loopback.service().hello().instance.clone();
    let call = tokio::spawn({
        let environment = environment.clone();
        async move {
            environment
                .execute_literal_process(shell("sleep 1; echo x >> log"), effect(owner(1, 1), 1))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let hello = loopback.restart(service(root.path())).await.unwrap();
    assert_ne!(hello.instance, before);
    let error = call.await.unwrap().unwrap_err();
    assert_eq!(error.kind(), ExecutionBackendErrorKind::RecoveryRequired);
    assert!(!error.proven_no_effect());
    // The new instance accepted no owner: the host transfers again first.
    environment
        .execute_literal_process(shell("echo y >> log"), effect(owner(1, 1), 2))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_without_a_link_is_unavailable_with_no_effect() {
    let root = tempfile::tempdir().unwrap();
    let (loopback, environment) = loopback(root.path()).await;
    loopback.sever();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let error = environment
        .execute_literal_process(shell("echo x >> log"), effect(owner(1, 1), 1))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ExecutionBackendErrorKind::Io);
    assert!(error.proven_no_effect());
    assert_eq!(appended(root.path()), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_of_another_version_is_refused_before_any_request() {
    let root = tempfile::tempdir().unwrap();
    let mut other = bind("instance-1");
    other.version = VersionTriple::current("other-revision", "debug");
    let Err(plakous_runner::LoopbackError::Connect(error)) =
        Loopback::join(service(root.path()), other, windows()).await
    else {
        panic!("another version binds");
    };
    assert!(matches!(error, ConnectError::VersionMismatch { .. }));

    let mut raw = Raw::open(&service(root.path())).await.0;
    raw.send(&HostFrame::Ping { nonce: 7 }).await;
    assert_eq!(
        raw.next().await,
        RunnerFrame::Refused {
            request_id: None,
            refusal: Refusal::Unbound,
        }
    );
}

/// A link that records each call's payload and runs nothing.
#[derive(Default)]
struct Recorder(Mutex<Vec<(String, Value)>>);

impl RunnerLink for Recorder {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        self.0
            .lock()
            .unwrap()
            .push((call.verb().as_str().to_owned(), call.payload().clone()));
        Box::pin(std::future::ready(Err(RunnerFailure::Unavailable(
            "recording".into(),
        ))))
    }
}

/// The operation a runner backend sends for `request`.
async fn operation(
    service: &RunnerService,
    root: &Path,
    request: LiteralProcessRequest,
) -> Operation {
    let recorder = Arc::new(Recorder::default());
    let link: Arc<dyn RunnerLink> = recorder.clone();
    let backend = RunnerBackend::new(service.hello().descriptor.clone(), link).unwrap();
    let environment = runner_environment(scope(), root, ".", config(), Arc::new(backend)).unwrap();
    let _ = environment
        .execute_literal_process(request, unkeyed())
        .await;
    let (verb, payload) = recorder.0.lock().unwrap().pop().unwrap();
    Operation { verb, payload }
}

/// A host speaking frames by hand.
struct Raw {
    reader: ReadHalf<DuplexStream>,
    writer: WriteHalf<DuplexStream>,
    frames: FrameReader,
}

impl Raw {
    async fn open(service: &RunnerService) -> (Self, Hello) {
        let (host, runner) = tokio::io::duplex(1 << 16);
        let service = service.clone();
        tokio::spawn(async move { service.serve_connection(runner).await });
        let (reader, writer) = tokio::io::split(host);
        let mut raw = Self {
            reader,
            writer,
            frames: FrameReader::new(),
        };
        let RunnerFrame::Hello(hello) = raw.next().await else {
            panic!("the runner speaks first");
        };
        (raw, hello)
    }

    async fn bound(service: &RunnerService, host_instance: &str) -> Self {
        let mut raw = Self::open(service).await.0;
        raw.send(&HostFrame::Bind(bind(host_instance))).await;
        raw
    }

    async fn send(&mut self, frame: &HostFrame) {
        self.writer
            .write_all(&encode(frame).unwrap())
            .await
            .unwrap();
    }

    async fn next(&mut self) -> RunnerFrame {
        let mut buffer = [0_u8; 4096];
        loop {
            if let Some(body) = self.frames.next_frame().unwrap() {
                return decode(&body).unwrap();
            }
            let read = self.reader.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0, "the runner closed the connection");
            self.frames.push(&buffer[..read]);
        }
    }

    async fn ask(&mut self, frame: HostFrame) -> RunnerFrame {
        self.send(&frame).await;
        self.next().await
    }
}

fn wire_owner() -> plakous_runner_protocol::LineOwner {
    plakous_runner_protocol::LineOwner {
        line: LINE.into(),
        generation: 1,
        branch: BRANCH.into(),
        fence: 1,
    }
}

fn keyed(request_id: &str, call: &str, operation: &Operation, replay: bool) -> HostFrame {
    HostFrame::Request(Request {
        request_id: request_id.into(),
        class: RequestClass::EffectCall {
            call_id: call.into(),
            attempt_id: "00000000-0000-0000-0000-0000000000c1".into(),
        },
        owner: Some(wire_owner()),
        deadline_ms: None,
        digest: "digest-1".into(),
        replay,
        operation: operation.clone(),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_host_replays_by_key_and_never_runs_twice() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let append = operation(&service, root.path(), shell("echo x >> log; echo ran")).await;
    let first_call = "00000000-0000-0000-0000-000000000001";
    let unseen_call = "00000000-0000-0000-0000-000000000002";

    // The first host process sends a keyed call and dies before its ack.
    let mut before = Raw::bound(&service, "instance-1").await;
    let transfer = HostFrame::TransferOwner(TransferOwner {
        control_id: "t1".into(),
        from: None,
        to: wire_owner(),
    });
    assert_eq!(
        before.ask(transfer).await,
        RunnerFrame::TransferAccepted {
            control_id: "t1".into()
        }
    );
    let RunnerFrame::Result { payload, .. } =
        before.ask(keyed("r1", first_call, &append, false)).await
    else {
        panic!("the call runs");
    };
    drop(before);

    // Its successor replays every request it may have sent.
    let mut after = Raw::bound(&service, "instance-2").await;
    assert_eq!(
        after.ask(keyed("r7", first_call, &append, true)).await,
        RunnerFrame::Result {
            request_id: "r7".into(),
            payload,
        }
    );
    assert_eq!(
        after.ask(keyed("r8", unseen_call, &append, true)).await,
        RunnerFrame::NotSeen {
            request_id: "r8".into()
        }
    );
    // The original of a tombstoned key, arriving late, does not run.
    assert_eq!(
        after.ask(keyed("r9", unseen_call, &append, false)).await,
        RunnerFrame::NotSeen {
            request_id: "r9".into()
        }
    );
    let mut changed = append.clone();
    changed.payload = Value::Null;
    let HostFrame::Request(mut other) = keyed("r10", first_call, &changed, true) else {
        unreachable!()
    };
    other.digest = "digest-2".into();
    assert_eq!(
        after.ask(HostFrame::Request(other)).await,
        RunnerFrame::StaleRequest {
            request_id: "r10".into()
        }
    );
    after
        .send(&HostFrame::Ack {
            request_id: "r7".into(),
        })
        .await;
    assert_eq!(
        after.ask(keyed("r11", first_call, &append, true)).await,
        RunnerFrame::Spent {
            request_id: "r11".into()
        }
    );
    assert_eq!(appended(root.path()), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jobs_and_streams_are_refused_until_the_runner_carries_them() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    assert!(
        !service
            .hello()
            .capabilities
            .iter()
            .any(|capability| capability == plakous_runner_protocol::capabilities::STREAMS)
    );
    let read = operation(&service, root.path(), shell("true")).await;
    let mut raw = Raw::bound(&service, "instance-1").await;
    let HostFrame::Request(mut observation) =
        keyed("r1", "00000000-0000-0000-0000-000000000001", &read, false)
    else {
        unreachable!()
    };
    observation.class = RequestClass::ProcessObservation { job: 1, cursor: 0 };
    assert_eq!(
        raw.ask(HostFrame::Request(observation)).await,
        RunnerFrame::Refused {
            request_id: Some("r1".into()),
            refusal: Refusal::Unsupported {
                what: "request class".into()
            },
        }
    );
    let stream = HostFrame::Stream(StreamFrame {
        stream: 1,
        seq: 0,
        event: StreamEvent::Credit { bytes: 1024 },
    });
    assert_eq!(
        raw.ask(stream).await,
        RunnerFrame::Refused {
            request_id: None,
            refusal: Refusal::Unsupported {
                what: "stream".into()
            },
        }
    );
    raw.send(&HostFrame::Preempt {
        request_id: "r1".into(),
        value: Value::Null,
    })
    .await;
    assert_eq!(
        raw.ask(HostFrame::Ping { nonce: 3 }).await,
        RunnerFrame::Pong { nonce: 3 }
    );
    assert_eq!(appended(root.path()), 0);
}

fn short_config() -> ExecutionEnvironmentConfig {
    let limits = ExecutionEnvironmentLimits::new(
        1 << 20,
        1 << 20,
        1 << 20,
        1 << 22,
        Duration::from_millis(500),
        Duration::from_secs(1),
    )
    .unwrap();
    ExecutionEnvironmentConfig::new(limits, [], []).unwrap()
}

/// The error kinds of a process that outlives its deadline and of one whose
/// caller cancels it.
async fn interrupted(environment: &ExecutionEnvironment) -> Vec<(ExecutionBackendErrorKind, bool)> {
    let mut seen = Vec::new();
    let timed_out = environment
        .execute_literal_process(shell("sleep 5"), effect(owner(1, 1), 1))
        .await
        .unwrap_err();
    seen.push((timed_out.kind(), timed_out.proven_no_effect()));
    let token = CancellationToken::new();
    let control = OperationContext::new(token.clone())
        .with_owner(owner(1, 1))
        .with_class(effect(owner(1, 1), 2).class());
    let cancel = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        token.cancel();
    });
    let cancelled = environment
        .execute_literal_process(shell("sleep 5"), control)
        .await
        .unwrap_err();
    cancel.await.unwrap();
    seen.push((cancelled.kind(), cancelled.proven_no_effect()));
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timeout_and_a_cancellation_answer_through_the_loopback_as_direct() {
    let direct_root = tempfile::tempdir().unwrap();
    let direct =
        ExecutionEnvironment::host(scope(), direct_root.path(), ".", short_config()).unwrap();
    let expected = interrupted(&direct).await;
    assert_eq!(expected[0].0, ExecutionBackendErrorKind::TimedOut);
    assert_eq!(expected[1].0, ExecutionBackendErrorKind::Cancelled);

    let root = tempfile::tempdir().unwrap();
    let runner = ExecutionEnvironment::host(scope(), root.path(), ".", short_config()).unwrap();
    let service = RunnerService::new(
        runner,
        RunnerIdentity {
            runner_id: RunnerId::new("loopback").unwrap(),
            version: version(),
            boot_id: "boot-1".into(),
            binary_id: None,
        },
    )
    .unwrap();
    let loopback = Loopback::join(service, bind("instance-1"), windows())
        .await
        .unwrap();
    let environment = runner_environment(
        scope(),
        root.path(),
        ".",
        short_config(),
        loopback.backend(),
    )
    .unwrap();
    assert_eq!(interrupted(&environment).await, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_frame_ends_the_runner_connection_once() {
    let root = tempfile::tempdir().unwrap();
    let mut raw = Raw::bound(&service(root.path()), "instance-1").await;
    let declared = u32::try_from(MAX_FRAME_BYTES + 1).unwrap();
    raw.writer.write_all(&declared.to_be_bytes()).await.unwrap();
    let refused = tokio::time::timeout(Duration::from_secs(5), raw.next())
        .await
        .unwrap();
    assert!(matches!(
        refused,
        RunnerFrame::Refused {
            request_id: None,
            refusal: Refusal::Malformed { .. },
        }
    ));
    let mut rest = [0_u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(5), raw.reader.read(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read, 0, "the runner closes the connection");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_frame_from_the_runner_loses_the_link() {
    let root = tempfile::tempdir().unwrap();
    let hello = service(root.path()).hello().clone();
    let (host, runner) = tokio::io::duplex(1 << 16);
    let fake = tokio::spawn(async move {
        let (mut reader, mut writer) = tokio::io::split(runner);
        writer
            .write_all(&encode(&RunnerFrame::Hello(hello)).unwrap())
            .await
            .unwrap();
        let mut bind = [0_u8; 4096];
        let _ = reader.read(&mut bind).await.unwrap();
        let declared = u32::try_from(MAX_FRAME_BYTES + 1).unwrap();
        writer.write_all(&declared.to_be_bytes()).await.unwrap();
        // Hold the connection open: only the oversized prefix may end it.
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop((reader, writer));
    });
    let short = ClientWindows {
        read: Duration::from_secs(2),
        effect: Duration::from_secs(2),
    };
    let (client, hello) = plakous_runner::RunnerClient::connect(host, bind("instance-1"), short)
        .await
        .unwrap();
    let link: Arc<dyn RunnerLink> = Arc::new(client);
    let backend = RunnerBackend::new(hello.descriptor, link).unwrap();
    let environment =
        runner_environment(scope(), root.path(), ".", config(), Arc::new(backend)).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let error = environment
        .read_workspace(
            WorkspaceReadSliceRequest::start("a.txt", 4).unwrap(),
            unkeyed(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ExecutionBackendErrorKind::Io);
    assert!(error.proven_no_effect());
    fake.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_spent_under_another_host_instance_is_of_unknown_outcome() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let first = Loopback::join(service.clone(), bind("instance-1"), windows())
        .await
        .unwrap();
    let before = runner_environment(scope(), root.path(), ".", config(), first.backend()).unwrap();
    before
        .execute_literal_process(shell("echo x >> log"), effect(owner(1, 1), 1))
        .await
        .unwrap();
    drop(first);
    // The successor host process sends the same key again after the first
    // acknowledged it: the call ran, so its outcome is unknown, not refused.
    let second = Loopback::join(service, bind("instance-2"), windows())
        .await
        .unwrap();
    let after = runner_environment(scope(), root.path(), ".", config(), second.backend()).unwrap();
    let error = after
        .execute_literal_process(shell("echo x >> log"), effect(owner(1, 1), 1))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ExecutionBackendErrorKind::RecoveryRequired);
    assert!(!error.proven_no_effect());
    assert_eq!(appended(root.path()), 1);
}
