//! The conformance subset (spec 01 §8.4): one suite against the direct local
//! backend and through the loopback runner, then the protocol's keying,
//! fencing and restart laws through the loopback and at the frame level.

#![allow(clippy::unwrap_used, reason = "integration tests")]

use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, RunId};
use plakous_runner::{
    ClientWindows, ConnectError, Loopback, RunnerIdentity, RunnerLimits, RunnerService,
};
use plakous_runner_protocol::frame::{FrameReader, MAX_FRAME_BYTES, decode, encode};
use plakous_runner_protocol::{
    Bind, Hello, HostFrame, Operation, Refusal, Request, RequestClass, RunnerFrame, StreamEvent,
    StreamFrame, TransferOwner, VersionTriple,
};
use plakous_tools::internal::runner::{
    RunnerBackend, RunnerCall, RunnerCallFuture, RunnerFailure, RunnerLink, RunnerOpenFuture,
    runner_environment,
};
use plakous_tools::{
    ExecutionBackendErrorKind, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentLimits, ExecutionEnvironmentScope, LineOwner, LiteralProcessIntent,
    LiteralProcessRequest, OperationClass, OperationContext, RepositoryMatchCase, RepositoryQuery,
    RepositoryQueryLimits, RunnerId, ServiceProcessRequest, WorkspaceExpectedFileState,
    WorkspaceMutation, WorkspaceMutationTransaction, WorkspaceReadSliceRequest,
    WorkspaceReplacement,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

const LINE: &str = "00000000-0000-0000-0000-0000000000a1";
const BRANCH: &str = "00000000-0000-0000-0000-0000000000b1";

mod model_reads {
    use super::*;
    use plakous_tools::internal::runner::RunnerVerb;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Journal(AtomicUsize);

    impl plakous_runner::CallJournal for Journal {
        fn claim(&self, _: &plakous_runner::JournalEntry) -> std::io::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn settle(&self, _: &str) -> std::io::Result<()> {
            panic!("a model read must never be journaled or settled")
        }

        fn pending(&self) -> std::io::Result<Vec<plakous_runner::JournalEntry>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repository_queries_and_identical_reads_share_one_model_context() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), "alpha").unwrap();
        let (_link, environment) = loopback(root.path()).await;
        for needle in ["alpha", "absent", "alpha"] {
            let query = RepositoryQuery::new(
                ".",
                needle.as_bytes().to_vec(),
                RepositoryMatchCase::Sensitive,
                query_limits(),
            )
            .unwrap();
            let answer = environment
                .query_repository(query, effect(owner(1, 1), 1))
                .await
                .unwrap();
            assert_eq!(answer.matches().len(), usize::from(needle == "alpha"));
        }
        for _ in 0..2 {
            let read = environment
                .read_workspace(
                    WorkspaceReadSliceRequest::start("a", 5).unwrap(),
                    effect(owner(1, 1), 1),
                )
                .await
                .unwrap();
            assert_eq!(read.bytes().as_ref(), b"alpha");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_model_read_verb_is_unrecorded_but_still_owner_fenced() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        let mut raw = Raw::bound(&service, "instance-1").await;
        assert!(matches!(
            raw.ask(transfer("t", None, wire_owner())).await,
            RunnerFrame::TransferAccepted { .. }
        ));
        // Malformed payloads deliberately fail at the backend. Their answers
        // still must not reserve, tombstone or spend a model-call key.
        for verb in RunnerVerb::ALL.into_iter().filter(|verb| verb.is_read()) {
            for (index, replay) in [(0, false), (1, true), (2, false)] {
                let operation = Operation {
                    verb: verb.as_str().into(),
                    payload: json!({"invalid": index}),
                };
                let HostFrame::Request(mut request) = keyed(
                    "read",
                    "00000000-0000-0000-0000-000000000001",
                    &operation,
                    replay,
                ) else {
                    unreachable!()
                };
                request.digest = format!("digest-{index}");
                let answer = raw.ask(HostFrame::Request(request.clone())).await;
                assert!(
                    matches!(
                        answer,
                        RunnerFrame::Result { .. }
                            | RunnerFrame::Error { .. }
                            | RunnerFrame::Refused {
                                refusal: Refusal::Unsupported { .. },
                                ..
                            }
                    ),
                    "{verb:?}: {answer:?}"
                );
                raw.send(&HostFrame::Ack {
                    request_id: "read".into(),
                })
                .await;
                request.owner = Some(wire_owner_of(LINE, 0));
                assert!(
                    matches!(
                        raw.ask(HostFrame::Request(request.clone())).await,
                        RunnerFrame::StaleOwner { .. }
                    ),
                    "{verb:?}: a read, including a replay, stays fenced"
                );
                request.owner = None;
                assert!(matches!(
                    raw.ask(HostFrame::Request(request)).await,
                    RunnerFrame::Refused {
                        refusal: Refusal::Malformed { .. },
                        ..
                    }
                ));
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lost_model_workspace_read_is_held_reissued_and_never_journaled_or_acked() {
        let root = tempfile::tempdir().unwrap();
        let hello = service(root.path()).hello().clone();
        let (link, mut first) = fake_runner(&hello).await;
        let journal = Arc::new(Journal::default());
        let (client, _) = plakous_runner::RunnerClient::connect_journaled(
            link,
            bind("instance-1"),
            placement_windows(),
            Some(journal.clone()),
        )
        .await
        .unwrap();
        let tap = Tap::new(&client);
        let backend = RunnerBackend::new(hello.descriptor.clone(), Arc::new(tap.clone())).unwrap();
        let services = plakous_runner::RunnerServices::new(
            Arc::new(backend),
            tokio::runtime::Handle::current(),
        );
        let environment =
            runner_environment(scope(), root.path(), ".", config(), services.backend()).unwrap();
        let reading = tokio::spawn(environment.read_workspace(
            WorkspaceReadSliceRequest::start("a", 5).unwrap(),
            effect(owner(1, 1), 1),
        ));
        let sent = first.next_request().await;
        assert_eq!(sent.operation.verb, "workspace.read");
        services.set_available(false);
        drop(first);
        client.disconnect();
        client.abandon_lost_effects();
        tokio::time::timeout(Duration::from_secs(10), tap.failed.notified())
            .await
            .unwrap();
        assert!(
            matches!(
                tap.failures().as_slice(),
                [RunnerFailure::ReadInterrupted(_)]
            ),
            "{:?}",
            tap.failures()
        );
        assert!(
            !reading.is_finished(),
            "the placement holds the interrupted read"
        );
        assert_eq!(journal.0.load(Ordering::SeqCst), 0);
        let (link, mut second) = fake_runner(&hello).await;
        client.reconnect(link).await.unwrap();
        services.set_available(true);
        let reissued = second.next_request().await;
        assert_eq!(reissued.operation, sent.operation);
        assert_eq!(reissued.owner, sent.owner);
        assert_eq!(reissued.class, sent.class);
        assert!(!reissued.replay);
        assert_ne!(reissued.request_id, sent.request_id);
        second.fail(&reissued).await;
        assert_eq!(
            reading.await.unwrap().unwrap_err().kind(),
            ExecutionBackendErrorKind::Io
        );
        // A later request is a transport barrier: an erroneous Ack would
        // precede it in this connection's FIFO.
        let barrier =
            tokio::spawn(client.call(RunnerCall::unkeyed(RunnerVerb::ProbeWorkspace, json!({}))));
        loop {
            match second.next_host().await {
                HostFrame::Ack { .. } => panic!("a model read must not be acknowledged"),
                HostFrame::Request(request) => {
                    second.fail(&request).await;
                    break;
                }
                _ => {}
            }
        }
        assert!(barrier.await.unwrap().is_err());
        assert_eq!(journal.0.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restarted_runner_interrupts_model_reads_without_unknown_effects() {
        let root = tempfile::tempdir().unwrap();
        let mut hello = service(root.path()).hello().clone();
        let (link, mut first) = fake_runner(&hello).await;
        let (client, _) =
            plakous_runner::RunnerClient::connect(link, bind("instance-1"), windows())
                .await
                .unwrap();
        let tap = Tap::new(&client);
        let backend =
            Arc::new(RunnerBackend::new(hello.descriptor.clone(), Arc::new(tap.clone())).unwrap());
        let environment = runner_environment(scope(), root.path(), ".", config(), backend).unwrap();
        let reading = tokio::spawn(environment.read_workspace(
            WorkspaceReadSliceRequest::start("a", 5).unwrap(),
            effect(owner(1, 1), 1),
        ));
        let _sent = first.next_request().await;
        hello.instance = "restarted".into();
        let (link, _second) = fake_runner(&hello).await;
        client.reconnect(link).await.unwrap();
        reading.await.unwrap().unwrap_err();
        assert!(
            matches!(
                tap.failures().as_slice(),
                [RunnerFailure::ReadInterrupted(_)]
            ),
            "{:?}",
            tap.failures()
        );
    }
}

mod review_call_keys {
    use super::*;
    use plakous_tools::internal::runner::{RunnerExtension, RunnerVerb, ServeFuture};

    struct ReadLink {
        _root: tempfile::TempDir,
        client: plakous_runner::RunnerClient,
        tap: Tap,
        environment: ExecutionEnvironment,
        runner: Raw,
    }

    impl ReadLink {
        async fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let hello = service(root.path()).hello().clone();
            let (link, runner) = fake_runner(&hello).await;
            let (client, _) = plakous_runner::RunnerClient::connect(
                link,
                bind("instance-1"),
                ClientWindows::default(),
            )
            .await
            .unwrap();
            let tap = Tap::new(&client);
            let backend =
                Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(tap.clone())).unwrap());
            let environment =
                runner_environment(scope(), root.path(), ".", config(), backend).unwrap();
            Self {
                _root: root,
                client,
                tap,
                environment,
                runner,
            }
        }

        fn read(
            &self,
        ) -> tokio::task::JoinHandle<
            Result<plakous_tools::WorkspaceReadSlice, plakous_tools::ExecutionBackendError>,
        > {
            tokio::spawn(self.environment.read_workspace(
                WorkspaceReadSliceRequest::start("a", 5).unwrap(),
                effect(owner(1, 1), 1),
            ))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_healthy_model_query_keeps_the_effect_window_past_thirty_seconds() {
        let mut link = ReadLink::new().await;
        let query = RepositoryQuery::new(
            ".",
            b"needle".to_vec(),
            RepositoryMatchCase::Sensitive,
            query_limits(),
        )
        .unwrap();
        let querying = tokio::spawn(
            link.environment
                .query_repository(query, effect(owner(1, 1), 1)),
        );
        let sent = link.runner.next_request().await;
        assert_eq!(sent.operation.verb, "repository.query");
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        assert!(
            !querying.is_finished(),
            "a healthy model read still has its 120 s window"
        );
        link.runner
            .answer(&RunnerFrame::Result {
                request_id: sent.request_id,
                payload: json!({"matches":[], "files_considered":0, "files_scanned":0,
                "bytes_scanned":0, "skipped_binary":0, "skipped_oversize":0,
                "skipped_changed":0, "entries_skipped":0, "lossy_lines":0, "stop_reason":null}),
            })
            .await;
        assert_eq!(querying.await.unwrap().unwrap().matches().len(), 0);
        assert_eq!(link.tap.failures(), []);
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_build_read_survives_a_same_instance_reconnect_inside_its_window() {
        let root = tempfile::tempdir().unwrap();
        let hello = service(root.path()).hello().clone();
        let (link, mut first) = fake_runner(&hello).await;
        let (client, _) = plakous_runner::RunnerClient::connect(
            link,
            bind("instance-1"),
            ClientWindows::default(),
        )
        .await
        .unwrap();
        let reading =
            tokio::spawn(client.call(RunnerCall::unkeyed(RunnerVerb::ProbeWorkspace, json!({}))));
        let sent = first.next_request().await;
        assert_eq!(sent.class, RequestClass::BuildRead);
        drop(first);
        let _ = client.lines().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert!(
            !reading.is_finished(),
            "a host read keeps its 30 s replay window"
        );
        let (link, mut second) = fake_runner(&hello).await;
        client.reconnect(link).await.unwrap();
        let replay = tokio::time::timeout(Duration::from_secs(5), second.next_request())
            .await
            .unwrap();
        assert!(replay.replay);
        assert_eq!(replay.request_id, sent.request_id);
        assert_eq!(replay.operation, sent.operation);
        second
            .answer(&RunnerFrame::Result {
                request_id: replay.request_id,
                payload: json!({"ok":true}),
            })
            .await;
        assert_eq!(reading.await.unwrap(), Ok(json!({"ok":true})));
    }

    #[tokio::test(start_paused = true)]
    async fn an_observation_survives_a_same_instance_reconnect_inside_its_window() {
        let root = tempfile::tempdir().unwrap();
        let hello = service(root.path()).hello().clone();
        let (link, mut first) = fake_runner(&hello).await;
        let (client, _) = plakous_runner::RunnerClient::connect(
            link,
            bind("instance-1"),
            ClientWindows::default(),
        )
        .await
        .unwrap();
        let tap = Tap::new(&client);
        let sessions = Arc::new(plakous_tools::internal::runner::RunnerSessions::from_link(
            Arc::new(tap.clone()),
            &host(root.path()),
            owner(1, 1),
        ));
        let id = started(&sessions, &mut first, &hello).await;
        let polling = poll_at(&sessions, id);
        let sent = first.next_request().await;
        assert!(matches!(
            sent.class,
            RequestClass::ProcessObservation { cursor: 5, .. }
        ));
        drop(first);
        let _ = client.lines().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert!(
            !polling.is_finished(),
            "an observation keeps its 30 s replay window"
        );
        let (link, mut second) = fake_runner(&hello).await;
        client.reconnect(link).await.unwrap();
        let replay = tokio::time::timeout(Duration::from_secs(5), second.next_request())
            .await
            .unwrap();
        assert!(replay.replay);
        assert_eq!(replay.request_id, sent.request_id);
        assert_eq!(replay.class, sent.class);
        assert_eq!(replay.operation, sent.operation);
        second.fail(&replay).await;
        assert_eq!(
            polling.await.unwrap().unwrap_err().kind(),
            ExecutionBackendErrorKind::Io
        );
        assert!(matches!(
            tap.failures().as_slice(),
            [RunnerFailure::Backend(_)]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_model_read_interrupts_within_the_link_recheck_not_the_effect_window() {
        let mut link = ReadLink::new().await;
        let reading = link.read();
        let _sent = link.runner.next_request().await;
        link.client.disconnect();
        let _ = link.client.lines().await;
        tokio::time::timeout(Duration::from_secs(2), reading)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            link.tap.failures().as_slice(),
            [RunnerFailure::ReadInterrupted(_)]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn abandoning_a_lost_placement_interrupts_its_model_read_at_once() {
        let mut link = ReadLink::new().await;
        let reading = link.read();
        let _sent = link.runner.next_request().await;
        link.client.disconnect();
        let now = tokio::time::Instant::now();
        link.client.abandon_lost_effects();
        tokio::time::timeout(Duration::from_secs(1), reading)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(
            tokio::time::Instant::now(),
            now,
            "no answer window or recheck wait"
        );
        assert!(matches!(
            link.tap.failures().as_slice(),
            [RunnerFailure::ReadInterrupted(_)]
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_model_read_from_the_current_owners_sibling_runs_without_taking_the_line() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), "alpha").unwrap();
        let service = service(root.path());
        let recorder = Arc::new(Recorder::default());
        let backend = Arc::new(
            RunnerBackend::new(service.hello().descriptor.clone(), recorder.clone()).unwrap(),
        );
        let environment = runner_environment(scope(), root.path(), ".", config(), backend).unwrap();
        let _ = environment
            .read_workspace(WorkspaceReadSliceRequest::start("a", 5).unwrap(), unkeyed())
            .await;
        let (verb, payload) = recorder.0.lock().unwrap().pop().unwrap();
        let operation = Operation { verb, payload };
        let mut holder = Raw::bound(&service, "instance-1").await;
        assert!(matches!(
            holder.ask(transfer("t", None, wire_owner())).await,
            RunnerFrame::TransferAccepted { .. }
        ));
        let mut sibling = Raw::bound(&service, "instance-2").await;
        let call = "00000000-0000-0000-0000-000000000001";
        for replay in [false, true] {
            assert!(matches!(
                sibling.ask(keyed("read", call, &operation, replay)).await,
                RunnerFrame::Result { .. }
            ));
        }
        let append = super::operation(&service, root.path(), shell("echo x >> log")).await;
        assert_eq!(
            sibling.ask(keyed("effect", call, &append, false)).await,
            not_transferred("effect")
        );
        assert_eq!(appended(root.path()), 0);
        assert!(matches!(
            holder.ask(keyed("effect", call, &append, false)).await,
            RunnerFrame::Result { .. }
        ));
        assert_eq!(appended(root.path()), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_runner_restart_keeps_a_pending_process_observation_unknown() {
        let root = tempfile::tempdir().unwrap();
        let mut hello = service(root.path()).hello().clone();
        let (link, mut first) = fake_runner(&hello).await;
        let (client, _) =
            plakous_runner::RunnerClient::connect(link, bind("instance-1"), windows())
                .await
                .unwrap();
        let tap = Tap::new(&client);
        let sessions = Arc::new(plakous_tools::internal::runner::RunnerSessions::from_link(
            Arc::new(tap.clone()),
            &host(root.path()),
            owner(1, 1),
        ));
        let id = started(&sessions, &mut first, &hello).await;
        let polling = poll_at(&sessions, id);
        let sent = first.next_request().await;
        assert!(matches!(
            sent.class,
            RequestClass::ProcessObservation { .. }
        ));
        hello.instance = "restarted".into();
        let (link, _second) = fake_runner(&hello).await;
        client.reconnect(link).await.unwrap();
        polling.await.unwrap().unwrap_err();
        assert!(
            matches!(
                tap.failures().as_slice(),
                [RunnerFailure::OutcomeUnknown(_)]
            ),
            "{:?}",
            tap.failures()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_model_effect_answer_is_acked_after_abandon_or_unreachable_cancel() {
        for abandon in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let hello = service(root.path()).hello().clone();
            let (link, mut first) = fake_runner(&hello).await;
            let (client, _) =
                plakous_runner::RunnerClient::connect(link, bind("instance-1"), windows())
                    .await
                    .unwrap();
            let sessions = plakous_tools::internal::runner::RunnerSessions::from_link(
                Arc::new(client.clone()),
                &host(root.path()),
                owner(1, 1),
            );
            let control = effect(owner(1, 1), 1);
            let cancellation = control.cancellation().clone();
            let starting =
                tokio::spawn(async move { sessions.start(start_request(), control).await });
            let sent = first.next_request().await;
            drop(first);
            let _ = client.lines().await;
            if abandon {
                client.abandon_lost_effects();
            } else {
                cancellation.cancel();
            }
            starting.await.unwrap().unwrap_err();
            let (link, mut second) = fake_runner(&hello).await;
            client.reconnect(link).await.unwrap();
            second
                .answer(&RunnerFrame::Result {
                    request_id: sent.request_id.clone(),
                    payload: json!({"session": SESSION, "instance":hello.instance}),
                })
                .await;
            let ack = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if let HostFrame::Ack { request_id } = second.next_host().await {
                        break request_id;
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(ack, sent.request_id);
        }
    }

    #[derive(Default)]
    struct DelayedEffect {
        started: tokio::sync::Notify,
        release: Arc<tokio::sync::Notify>,
    }

    impl RunnerExtension for DelayedEffect {
        fn serve(&self, _: RunnerVerb, _: Value) -> ServeFuture {
            self.started.notify_one();
            let release = Arc::clone(&self.release);
            Box::pin(async move {
                release.notified().await;
                Ok(json!({"ran":true}))
            })
        }
    }

    // Observe the real host's Ack while forwarding it to the real runner.
    async fn ack_relay(
        host: DuplexStream,
        runner: DuplexStream,
        ack: tokio::sync::oneshot::Sender<String>,
    ) {
        let (mut host_read, mut host_write) = tokio::io::split(host);
        let (mut runner_read, mut runner_write) = tokio::io::split(runner);
        let replies = tokio::io::copy(&mut runner_read, &mut host_write);
        let requests = async move {
            let mut ack = Some(ack);
            let mut frames = FrameReader::new();
            let mut buffer = [0; 4096];
            loop {
                let read = host_read.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                runner_write.write_all(&buffer[..read]).await.unwrap();
                frames.push(&buffer[..read]);
                while let Some(frame) = frames.next_frame().unwrap() {
                    if let HostFrame::Ack { request_id } = decode::<HostFrame>(&frame).unwrap()
                        && let Some(ack) = ack.take()
                    {
                        let _ = ack.send(request_id);
                    }
                }
            }
        };
        tokio::select! { _ = replies => {}, () = requests => {} }
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_keyed_answer_after_window_expiry_is_acked_and_no_longer_pins_idle() {
        let root = tempfile::tempdir().unwrap();
        let delayed = Arc::new(DelayedEffect::default());
        let service = RunnerService::with_options(
            host(root.path()),
            RunnerIdentity {
                runner_id: RunnerId::new("loopback").unwrap(),
                version: version(),
                boot_id: "boot".into(),
                binary_id: None,
            },
            plakous_runner::RunnerOptions {
                extension: Some(delayed.clone()),
                limits: RunnerLimits {
                    dead: Duration::from_secs(3600),
                    ..RunnerLimits::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let (host, relay_host) = tokio::io::duplex(1 << 16);
        let (relay_runner, runner) = tokio::io::duplex(1 << 16);
        let serving = tokio::spawn({
            let service = service.clone();
            async move { service.serve_connection(runner).await }
        });
        let (ack, acked) = tokio::sync::oneshot::channel();
        let relay = tokio::spawn(ack_relay(relay_host, relay_runner, ack));
        let (client, _) = plakous_runner::RunnerClient::connect(
            host,
            bind("instance-1"),
            ClientWindows::default(),
        )
        .await
        .unwrap();
        let call = tokio::spawn(client.call(RunnerCall::tree(
            RunnerVerb::VcsEffect,
            json!({}),
            "tree",
            "op",
        )));
        delayed.started.notified().await;
        tokio::time::advance(Duration::from_secs(121)).await;
        assert!(matches!(
            call.await.unwrap(),
            Err(RunnerFailure::OutcomeUnknown(_))
        ));
        delayed.release.notify_one();
        assert_ne!(
            tokio::time::timeout(Duration::from_secs(1), acked)
                .await
                .unwrap()
                .unwrap(),
            ""
        );
        client.lines().await.unwrap(); // FIFO barrier after the forwarded Ack.
        client.disconnect();
        relay.await.unwrap();
        serving.await.unwrap();
        assert!(
            service.is_idle(),
            "holds_unacknowledged must clear for the late keyed answer"
        );
    }
}

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
        inspect: false,
    }
}

fn windows() -> ClientWindows {
    ClientWindows {
        read: Duration::from_secs(10),
        effect: Duration::from_secs(10),
        ..ClientWindows::default()
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
        if call.verb() == plakous_tools::internal::runner::RunnerVerb::ProbeWorkspace {
            let served =
                plakous_tools::internal::runner::serve_probe(call.payload().clone(), &|_| true);
            return Box::pin(async move { served.await.map_err(|_| RunnerFailure::Unsupported) });
        }
        self.0
            .lock()
            .unwrap()
            .push((call.verb().as_str().to_owned(), call.payload().clone()));
        Box::pin(std::future::ready(Err(RunnerFailure::Unavailable(
            "recording".into(),
        ))))
    }

    fn open(&self, call: RunnerCall) -> RunnerOpenFuture {
        self.0
            .lock()
            .unwrap()
            .push((call.verb().as_str().to_owned(), call.payload().clone()));
        Box::pin(std::future::ready(Err(RunnerFailure::Unavailable(
            "recording".into(),
        ))))
    }
}

/// The operation a runner backend sends to spawn `program`.
async fn spawn_operation(
    service: &RunnerService,
    root: &Path,
    program: &str,
    args: &[&str],
) -> Operation {
    let recorder = Arc::new(Recorder::default());
    let link: Arc<dyn RunnerLink> = recorder.clone();
    let backend = RunnerBackend::new(service.hello().descriptor.clone(), link).unwrap();
    let environment = runner_environment(scope(), root, ".", config(), Arc::new(backend)).unwrap();
    let request = ServiceProcessRequest::new(
        program,
        args.iter().map(|arg| (*arg).to_owned()),
        Vec::new(),
        Vec::new(),
        1024,
    )
    .unwrap();
    let _ = environment.spawn_service(request, unkeyed()).await;
    let (verb, payload) = recorder.0.lock().unwrap().pop().unwrap();
    Operation { verb, payload }
}

fn unkeyed_request(request_id: &str, operation: &Operation) -> HostFrame {
    HostFrame::Request(Request {
        request_id: request_id.into(),
        class: RequestClass::BuildRead,
        owner: None,
        deadline_ms: None,
        digest: "digest".into(),
        replay: false,
        age_ms: None,
        operation: operation.clone(),
    })
}

/// Reads frames until a stream frame arrives, skipping others.
async fn stream_frame(raw: &mut Raw) -> StreamFrame {
    loop {
        if let RunnerFrame::Stream(frame) = raw.next().await {
            return frame;
        }
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

    /// As a fake runner: the next frame the host sent.
    async fn next_host(&mut self) -> HostFrame {
        let mut buffer = [0_u8; 4096];
        loop {
            if let Some(body) = self.frames.next_frame().unwrap() {
                return decode(&body).unwrap();
            }
            let read = self.reader.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0, "the host closed the connection");
            self.frames.push(&buffer[..read]);
        }
    }

    /// As a fake runner: sends one frame to the host.
    async fn send_runner(&mut self, frame: &RunnerFrame) {
        self.writer
            .write_all(&encode(frame).unwrap())
            .await
            .unwrap();
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
        age_ms: None,
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
        accepted("t1", &service.hello().instance)
    );
    let RunnerFrame::Result { payload, .. } =
        before.ask(keyed("r1", first_call, &append, false)).await
    else {
        panic!("the call runs");
    };
    drop(before);

    // Its successor takes the line with a newer owner, then replays every
    // request its predecessor may have sent.
    let mut after = Raw::bound(&service, "instance-2").await;
    let take_over = HostFrame::TransferOwner(TransferOwner {
        control_id: "t2".into(),
        from: Some(wire_owner()),
        to: wire_owner_of(LINE, 2),
    });
    assert_eq!(
        after.ask(take_over).await,
        accepted("t2", &service.hello().instance)
    );
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
async fn observations_need_an_owner_and_stray_stream_frames_change_nothing() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    assert!(
        service
            .hello()
            .capabilities
            .iter()
            .any(|capability| capability == plakous_runner_protocol::capabilities::STREAMS)
    );
    assert_eq!(service.hello().job_bound, 128);
    let read = operation(&service, root.path(), shell("true")).await;
    let mut raw = Raw::bound(&service, "instance-1").await;
    let HostFrame::Request(mut observation) =
        keyed("r1", "00000000-0000-0000-0000-000000000001", &read, false)
    else {
        unreachable!()
    };
    observation.class = RequestClass::ProcessObservation { job: 1, cursor: 0 };
    observation.owner = None;
    assert!(matches!(
        raw.ask(HostFrame::Request(observation)).await,
        RunnerFrame::Refused {
            refusal: Refusal::Malformed { .. },
            ..
        }
    ));
    raw.send(&HostFrame::Stream(StreamFrame {
        stream: 99,
        seq: 0,
        event: StreamEvent::Credit { bytes: 1024 },
    }))
    .await;
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
    // The runner outlives the connection, so only the connection's own
    // close can end the stream.
    let runner = service(root.path());
    let mut raw = Raw::bound(&runner, "instance-1").await;
    let declared = u32::try_from(MAX_FRAME_BYTES + 1).unwrap();
    raw.writer.write_all(&declared.to_be_bytes()).await.unwrap();
    // A liveness bound only: generous, so load never trips it.
    let refused = tokio::time::timeout(Duration::from_secs(30), raw.next())
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
    let read = tokio::time::timeout(Duration::from_secs(30), raw.reader.read(&mut rest))
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
        ..ClientWindows::default()
    };
    let (client, _) = plakous_runner::RunnerClient::connect(host, bind("instance-1"), short)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!client.is_connected());
    let error = client
        .call(RunnerCall::unkeyed(
            plakous_tools::internal::runner::RunnerVerb::ReadWorkspace,
            json!({}),
        ))
        .await
        .unwrap_err();
    assert!(matches!(error, RunnerFailure::NotSent(_)), "{error:?}");
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
    // The host acknowledges the answer as it takes it; one more round trip
    // on the connection has the runner see that acknowledgement first.
    let _ = before
        .execute_literal_process(shell("true"), unkeyed())
        .await;
    drop(first);
    // The successor host process, under the newer owner its own lease
    // mints, sends the same key again after the first acknowledged it: the
    // call ran, so its outcome is unknown, not refused.
    let second = Loopback::join(service, bind("instance-2"), windows())
        .await
        .unwrap();
    let after = runner_environment(scope(), root.path(), ".", config(), second.backend()).unwrap();
    let error = after
        .execute_literal_process(shell("echo x >> log"), effect(owner(1, 2), 1))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ExecutionBackendErrorKind::RecoveryRequired);
    assert!(!error.proven_no_effect());
    assert_eq!(appended(root.path()), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rebind_of_the_same_host_instance_drains_its_older_connection() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let mut older = Raw::bound(&service, "instance-1").await;
    assert_eq!(
        older.ask(HostFrame::Ping { nonce: 1 }).await,
        RunnerFrame::Pong { nonce: 1 }
    );
    let mut newer = Raw::bound(&service, "instance-1").await;
    assert_eq!(
        newer.ask(HostFrame::Ping { nonce: 2 }).await,
        RunnerFrame::Pong { nonce: 2 }
    );
    let mut rest = [0_u8; 16];
    // A liveness bound only: generous, so load never trips it.
    let read = tokio::time::timeout(Duration::from_secs(30), older.reader.read(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read, 0, "the older connection is closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_link_is_lost_at_both_ends_and_pings_keep_it() {
    let root = tempfile::tempdir().unwrap();
    let limits = RunnerLimits {
        dead: Duration::from_millis(400),
        ..RunnerLimits::default()
    };
    let runner = || {
        RunnerService::with_limits(
            host(root.path()),
            RunnerIdentity {
                runner_id: RunnerId::new("loopback").unwrap(),
                version: version(),
                boot_id: "boot-1".into(),
                binary_id: None,
            },
            limits,
        )
        .unwrap()
    };
    // A host that sends nothing is closed by the runner.
    let kept = runner();
    let mut silent = Raw::bound(&kept, "instance-1").await;
    let mut rest = [0_u8; 16];
    // A liveness bound only: generous, so load never trips it.
    let read = tokio::time::timeout(Duration::from_secs(30), silent.reader.read(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read, 0, "the runner closes a silent connection");

    // A pinging host keeps its link across many dead windows.
    let pinging = ClientWindows {
        ping: Duration::from_millis(100),
        dead: Duration::from_millis(400),
        ..windows()
    };
    let loopback = Loopback::join(runner(), bind("instance-1"), pinging)
        .await
        .unwrap();
    let environment =
        runner_environment(scope(), root.path(), ".", config(), loopback.backend()).unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    std::fs::write(root.path().join("a.txt"), "alpha\n").unwrap();
    environment
        .read_workspace(
            WorkspaceReadSliceRequest::start("a.txt", 4).unwrap(),
            unkeyed(),
        )
        .await
        .unwrap();

    // A runner that never answers is lost by the host.
    let hello = runner().hello().clone();
    let (host_end, runner_end) = tokio::io::duplex(1 << 16);
    let mute = tokio::spawn(async move {
        let (mut reader, mut writer) = tokio::io::split(runner_end);
        writer
            .write_all(&encode(&RunnerFrame::Hello(hello)).unwrap())
            .await
            .unwrap();
        let mut sink = [0_u8; 4096];
        while reader.read(&mut sink).await.is_ok_and(|read| read > 0) {}
        drop(writer);
    });
    let (client, _) = plakous_runner::RunnerClient::connect(host_end, bind("instance-2"), pinging)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!client.is_connected());
    let error = client
        .call(RunnerCall::unkeyed(
            plakous_tools::internal::runner::RunnerVerb::ReadWorkspace,
            json!({}),
        ))
        .await
        .unwrap_err();
    assert!(matches!(error, RunnerFailure::NotSent(_)), "{error:?}");
    mute.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_stream_gap_or_overrun_aborts_the_stream_and_ends_the_service() {
    let root = tempfile::tempdir().unwrap();
    let runner = service(root.path());
    let sleeper = spawn_operation(&runner, root.path(), "/bin/sleep", &["30"]).await;
    let mut raw = Raw::bound(&runner, "instance-1").await;

    // A gap.
    let RunnerFrame::Result { payload, .. } = raw.ask(unkeyed_request("r1", &sleeper)).await else {
        panic!("the service spawns");
    };
    let stream = payload["stream"].as_u64().unwrap();
    raw.send(&HostFrame::Stream(StreamFrame {
        stream,
        seq: 3,
        event: StreamEvent::Data {
            payload: json!("00"),
        },
    }))
    .await;
    let close = stream_frame(&mut raw).await;
    assert_eq!(close.stream, stream);
    assert!(matches!(
        close.event,
        StreamEvent::Close { reason: Some(_) }
    ));
    let terminate = Operation {
        verb: "service.terminate".into(),
        payload: json!({ "stream": stream, "grace_ms": 0 }),
    };
    assert!(matches!(
        raw.ask(unkeyed_request("r2", &terminate)).await,
        RunnerFrame::Error { .. }
    ));

    // Data beyond the credit: the service does not read its stdin.
    let RunnerFrame::Result { payload, .. } = raw.ask(unkeyed_request("r3", &sleeper)).await else {
        panic!("the service spawns");
    };
    let stream = payload["stream"].as_u64().unwrap();
    let chunk = "ab".repeat(64 * 1024);
    for seq in 0..8 {
        raw.send(&HostFrame::Stream(StreamFrame {
            stream,
            seq,
            event: StreamEvent::Data {
                payload: json!(chunk),
            },
        }))
        .await;
    }
    let close = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = stream_frame(&mut raw).await;
            if matches!(frame.event, StreamEvent::Close { .. }) {
                return frame;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(close.stream, stream);
    let StreamEvent::Close {
        reason: Some(reason),
    } = close.event
    else {
        panic!("an overrun aborts the stream");
    };
    assert!(reason.contains("credit"), "{reason}");
    // The aborted stream grants nothing more, even for data its service
    // took before the abort.
    let after = tokio::time::timeout(Duration::from_millis(500), stream_frame(&mut raw)).await;
    assert!(after.is_err(), "a frame after the abort: {after:?}");
    let terminate = Operation {
        verb: "service.terminate".into(),
        payload: json!({ "stream": stream, "grace_ms": 0 }),
    };
    assert!(matches!(
        raw.ask(unkeyed_request("r4", &terminate)).await,
        RunnerFrame::Error { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_stream_gap_aborts_the_stream_on_the_host() {
    let root = tempfile::tempdir().unwrap();
    let hello = service(root.path()).hello().clone();
    let (host_end, runner_end) = tokio::io::duplex(1 << 16);
    let fake = tokio::spawn(async move {
        let (reader, writer) = tokio::io::split(runner_end);
        let mut raw = Raw {
            reader,
            writer,
            frames: FrameReader::new(),
        };
        raw.writer
            .write_all(&encode(&RunnerFrame::Hello(hello)).unwrap())
            .await
            .unwrap();
        // Bind, then the spawn request: answer it with stream 7.
        let mut request_id = None;
        while request_id.is_none() {
            if let HostFrame::Request(request) = raw.next_host().await {
                request_id = Some(request.request_id);
            }
        }
        raw.send_runner(&RunnerFrame::Result {
            request_id: request_id.unwrap(),
            payload: json!({ "stream": 7 }),
        })
        .await;
        // Wait for the host's first grant, then skip a sequence number.
        loop {
            if let HostFrame::Stream(frame) = raw.next_host().await
                && matches!(frame.event, StreamEvent::Credit { .. })
            {
                break;
            }
        }
        raw.send_runner(&RunnerFrame::Stream(StreamFrame {
            stream: 7,
            seq: 2,
            event: StreamEvent::Data {
                payload: json!("6869"),
            },
        }))
        .await;
        loop {
            if let HostFrame::Stream(frame) = raw.next_host().await
                && let StreamEvent::Close {
                    reason: Some(reason),
                } = frame.event
            {
                return (frame.stream, reason);
            }
        }
    });
    let (client, _) =
        plakous_runner::RunnerClient::connect(host_end, bind("instance-1"), windows())
            .await
            .unwrap();
    let spawn = spawn_operation(&service(root.path()), root.path(), "/bin/cat", &[]).await;
    let (_, streams) = client
        .open(RunnerCall::unkeyed(
            plakous_tools::internal::runner::RunnerVerb::ServiceSpawn,
            spawn.payload,
        ))
        .await
        .unwrap();
    let mut stdout = streams.output;
    let mut read = Vec::new();
    // A liveness bound only: generous, so load never trips it.
    tokio::time::timeout(Duration::from_secs(30), stdout.read_to_end(&mut read))
        .await
        .unwrap()
        .unwrap();
    assert!(read.is_empty(), "nothing after the gap reaches the host");
    let (stream, reason) = tokio::time::timeout(Duration::from_secs(30), fake)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stream, 7);
    assert!(reason.contains("skipped"), "{reason}");
}

// Ownership across host processes of one home (spec 01 §8.4): every process
// binds the home's `host_id` with its own `host_instance`. A line is held by
// the instance whose transfer the runner last accepted.

const OTHER_LINE: &str = "00000000-0000-0000-0000-0000000000a2";

fn wire_owner_of(line: &str, fence: u64) -> plakous_runner_protocol::LineOwner {
    plakous_runner_protocol::LineOwner {
        line: line.into(),
        fence,
        ..wire_owner()
    }
}

fn transfer(
    control: &str,
    from: Option<plakous_runner_protocol::LineOwner>,
    to: plakous_runner_protocol::LineOwner,
) -> HostFrame {
    HostFrame::TransferOwner(TransferOwner {
        control_id: control.into(),
        from,
        to,
    })
}

fn accepted(control: &str, _instance: &str) -> RunnerFrame {
    RunnerFrame::TransferAccepted {
        control_id: control.into(),
        stopped: Vec::new(),
        unpromoted: Vec::new(),
    }
}

fn keyed_for(
    request_id: &str,
    call: &str,
    owner: plakous_runner_protocol::LineOwner,
    operation: &Operation,
) -> HostFrame {
    let HostFrame::Request(mut request) = keyed(request_id, call, operation, false) else {
        unreachable!()
    };
    request.owner = Some(owner);
    HostFrame::Request(request)
}

fn not_transferred(request_id: &str) -> RunnerFrame {
    RunnerFrame::Refused {
        request_id: Some(request_id.into()),
        refusal: Refusal::OwnerNotTransferred,
    }
}

fn floored(root: &Path, floors: &Path) -> RunnerService {
    RunnerService::with_options(
        host(root),
        RunnerIdentity {
            runner_id: RunnerId::new("loopback").unwrap(),
            version: version(),
            boot_id: "boot-1".into(),
            binary_id: None,
        },
        plakous_runner::RunnerOptions {
            floors: Some(Arc::new(
                plakous_runner::FileOwnerFloors::open(floors).unwrap(),
            )),
            ..plakous_runner::RunnerOptions::default()
        },
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_same_owner_transfer_from_a_non_holder_is_stale() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let mut holder = Raw::bound(&service, "instance-1").await;
    assert_eq!(
        holder.ask(transfer("t1", None, wire_owner())).await,
        accepted("t1", &service.hello().instance)
    );
    let mut sibling = Raw::bound(&service, "instance-2").await;
    assert_eq!(
        sibling
            .ask(transfer("t2", Some(wire_owner()), wire_owner()))
            .await,
        RunnerFrame::StaleOwner {
            request_id: None,
            control_id: Some("t2".into()),
            current: wire_owner(),
        }
    );
    // A newer owner, which only the line's lease holder can mint, takes it.
    let newer = wire_owner_of(LINE, 2);
    assert_eq!(
        sibling
            .ask(transfer("t3", Some(wire_owner()), newer.clone()))
            .await,
        accepted("t3", &service.hello().instance)
    );
    // The former holder's retry of its own owner is now stale too.
    assert!(matches!(
        holder
            .ask(transfer("t4", Some(wire_owner()), wire_owner()))
            .await,
        RunnerFrame::StaleOwner { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_owned_requests_from_a_non_holder_are_not_transferred() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let append = operation(&service, root.path(), shell("echo x >> log")).await;
    let mut holder = Raw::bound(&service, "instance-1").await;
    assert_eq!(
        holder.ask(transfer("t1", None, wire_owner())).await,
        accepted("t1", &service.hello().instance)
    );
    let mut sibling = Raw::bound(&service, "instance-2").await;
    let call = "00000000-0000-0000-0000-000000000003";
    assert_eq!(
        sibling
            .ask(keyed_for("s1", call, wire_owner(), &append))
            .await,
        not_transferred("s1")
    );
    let stop = HostFrame::Request(Request {
        request_id: "s2".into(),
        class: RequestClass::OwnerControl {
            control_id: "c1".into(),
        },
        owner: Some(wire_owner()),
        deadline_ms: None,
        digest: "digest-c".into(),
        replay: false,
        age_ms: None,
        operation: Operation {
            verb: plakous_runner_protocol::verbs::JOBS_STOP.into(),
            payload: json!({ "grace_ms": 0 }),
        },
    });
    assert_eq!(sibling.ask(stop).await, not_transferred("s2"));
    assert_eq!(appended(root.path()), 0, "nothing ran for the sibling");
    // The holder's own call under the same key runs.
    assert!(matches!(
        holder
            .ask(keyed_for("h1", call, wire_owner(), &append))
            .await,
        RunnerFrame::Result { .. }
    ));
    assert_eq!(appended(root.path()), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_expiry_orphans_only_the_holder_lines() {
    let root = tempfile::tempdir().unwrap();
    let service = RunnerService::with_limits(
        host(root.path()),
        RunnerIdentity {
            runner_id: RunnerId::new("loopback").unwrap(),
            version: version(),
            boot_id: "boot-1".into(),
            binary_id: None,
        },
        RunnerLimits {
            owner_lease: Duration::from_millis(100),
            ..RunnerLimits::default()
        },
    )
    .unwrap();
    let append = operation(&service, root.path(), shell("echo x >> log")).await;
    let mut holder = Raw::bound(&service, "instance-1").await;
    assert_eq!(
        holder.ask(transfer("t1", None, wire_owner())).await,
        accepted("t1", &service.hello().instance)
    );
    let mut sibling = Raw::bound(&service, "instance-2").await;
    let other = wire_owner_of(OTHER_LINE, 1);
    assert_eq!(
        sibling.ask(transfer("t2", None, other.clone())).await,
        accepted("t2", &service.hello().instance)
    );
    drop(holder);
    // Past the holder's lease, though its sibling stayed connected.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let _ = service.is_idle();
    let mut returned = Raw::bound(&service, "instance-1").await;
    let call = |n: u8| format!("00000000-0000-0000-0000-0000000000{n:02x}");
    assert_eq!(
        returned
            .ask(keyed_for("h1", &call(1), wire_owner(), &append))
            .await,
        not_transferred("h1"),
        "the holder's line is orphaned"
    );
    assert!(matches!(
        sibling.ask(keyed_for("s1", &call(2), other, &append)).await,
        RunnerFrame::Result { .. }
    ));
    // The holder re-adopts its orphan with its own owner.
    assert_eq!(
        returned.ask(transfer("t3", None, wire_owner())).await,
        accepted("t3", &service.hello().instance)
    );
    assert!(matches!(
        returned
            .ask(keyed_for("h2", &call(3), wire_owner(), &append))
            .await,
        RunnerFrame::Result { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_after_restart_floor_owner_is_refused_from_a_sibling() {
    let root = tempfile::tempdir().unwrap();
    let floors = root.path().join("owners.json");
    let first = floored(root.path(), &floors);
    let mut holder = Raw::bound(&first, "instance-1").await;
    assert_eq!(
        holder.ask(transfer("t1", None, wire_owner())).await,
        accepted("t1", &first.hello().instance)
    );
    drop(holder);
    drop(first);

    // A restarted runner: a sibling binding first cannot take the line with
    // the floor owner; the floor's holder can, and any newer owner can.
    let restarted = floored(root.path(), &floors);
    let mut sibling = Raw::bound(&restarted, "instance-2").await;
    assert!(matches!(
        sibling.ask(transfer("t2", None, wire_owner())).await,
        RunnerFrame::StaleOwner { .. }
    ));
    let mut holder = Raw::bound(&restarted, "instance-1").await;
    assert_eq!(
        holder.ask(transfer("t3", None, wire_owner())).await,
        accepted("t3", &restarted.hello().instance)
    );
    assert_eq!(
        sibling
            .ask(transfer("t4", Some(wire_owner()), wire_owner_of(LINE, 2)))
            .await,
        accepted("t4", &restarted.hello().instance)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_a_sibling_cannot_take_or_tombstone_the_holder_keys() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let slow = operation(
        &service,
        root.path(),
        shell("sleep 1; echo x >> log; echo ran"),
    )
    .await;
    let mut holder = Raw::bound(&service, "instance-1").await;
    assert_eq!(
        holder.ask(transfer("t1", None, wire_owner())).await,
        accepted("t1", &service.hello().instance)
    );
    let running = "00000000-0000-0000-0000-000000000004";
    holder
        .send(&keyed_for("r1", running, wire_owner(), &slow))
        .await;
    let mut sibling = Raw::bound(&service, "instance-2").await;
    // The running key is neither readdressed by a resend nor by a replay.
    assert_eq!(
        sibling
            .ask(keyed_for("s1", running, wire_owner(), &slow))
            .await,
        not_transferred("s1")
    );
    assert_eq!(
        sibling.ask(keyed("s2", running, &slow, true)).await,
        not_transferred("s2")
    );
    // Its answer reaches the holder.
    assert!(
        matches!(holder.next().await, RunnerFrame::Result { request_id, .. } if request_id == "r1")
    );
    // An unseen key a sibling replays is not tombstoned: the holder runs it.
    let unseen = "00000000-0000-0000-0000-000000000005";
    assert_eq!(
        sibling.ask(keyed("s3", unseen, &slow, true)).await,
        not_transferred("s3")
    );
    assert!(matches!(
        holder
            .ask(keyed_for("r2", unseen, wire_owner(), &slow))
            .await,
        RunnerFrame::Result { .. }
    ));
    assert_eq!(appended(root.path()), 2);
}

/// A runner end a placement-hold test plays by hand over one connection.
async fn fake_runner(hello: &Hello) -> (DuplexStream, Raw) {
    let (host, runner) = tokio::io::duplex(1 << 16);
    let (reader, writer) = tokio::io::split(runner);
    let mut raw = Raw {
        reader,
        writer,
        frames: FrameReader::new(),
    };
    raw.answer(&RunnerFrame::Hello(hello.clone())).await;
    (host, raw)
}

impl Raw {
    async fn answer(&mut self, frame: &RunnerFrame) {
        self.writer
            .write_all(&encode(frame).unwrap())
            .await
            .unwrap();
    }

    /// The host's next request, accepting every owner transfer before it.
    async fn next_request(&mut self) -> Request {
        loop {
            // A liveness bound only: generous, so load never trips it.
            match tokio::time::timeout(Duration::from_secs(30), self.next_host())
                .await
                .unwrap()
            {
                HostFrame::Request(request) => return request,
                HostFrame::TransferOwner(transfer) => {
                    self.answer(&RunnerFrame::TransferAccepted {
                        control_id: transfer.control_id,
                        stopped: Vec::new(),
                        unpromoted: Vec::new(),
                    })
                    .await;
                }
                _ => {}
            }
        }
    }

    async fn fail(&mut self, request: &Request) {
        self.answer(&RunnerFrame::Error {
            request_id: request.request_id.clone(),
            error: plakous_runner_protocol::WireError {
                error: json!({
                    "kind": "io",
                    "message": "answered",
                    "proven_no_effect": true,
                    "conflict_path": null,
                }),
                proven_no_effect: true,
            },
        })
        .await;
    }
}

/// Records every link failure the client answers.
#[derive(Clone)]
struct Tap {
    client: plakous_runner::RunnerClient,
    failures: Arc<Mutex<Vec<RunnerFailure>>>,
    failed: Arc<tokio::sync::Notify>,
}

impl Tap {
    fn new(client: &plakous_runner::RunnerClient) -> Self {
        Self {
            client: client.clone(),
            failures: Arc::default(),
            failed: Arc::default(),
        }
    }

    fn failures(&self) -> Vec<RunnerFailure> {
        self.failures.lock().unwrap().clone()
    }
}

impl RunnerLink for Tap {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        if call.verb() == plakous_tools::internal::runner::RunnerVerb::ProbeWorkspace {
            let served =
                plakous_tools::internal::runner::serve_probe(call.payload().clone(), &|_| true);
            return Box::pin(async move { served.await.map_err(|_| RunnerFailure::Unsupported) });
        }
        let answer = self.client.call(call);
        let failures = Arc::clone(&self.failures);
        let failed = Arc::clone(&self.failed);
        Box::pin(async move {
            let answer = answer.await;
            if let Err(failure) = &answer {
                failures.lock().unwrap().push(failure.clone());
                failed.notify_one();
            }
            answer
        })
    }
}

const SESSION: &str = "0000000000000000000000000000002a";

fn placement_windows() -> ClientWindows {
    ClientWindows {
        read: Duration::from_millis(300),
        effect: Duration::from_millis(300),
        ..ClientWindows::default()
    }
}

fn start_request() -> plakous_tools::JoinedProcessStartRequest {
    plakous_tools::JoinedProcessStartRequest::new(
        plakous_tools::JoinedProcessCommand::literal(
            "/bin/sh",
            ["-c".to_owned(), "true".to_owned()],
        )
        .unwrap(),
        ".",
        Vec::new(),
        false,
        plakous_tools::JoinedProcessTerminalMode::Pipes,
    )
    .unwrap()
}

type Sessions = Arc<plakous_tools::internal::runner::RunnerSessions>;
type Started = tokio::task::JoinHandle<
    Result<plakous_tools::JoinedProcessSessionId, plakous_tools::ExecutionBackendError>,
>;
type Polled = tokio::task::JoinHandle<
    Result<plakous_tools::JoinedProcessCursorPollResult, plakous_tools::ExecutionBackendError>,
>;

/// A model tool call's start under call key `call`.
fn start_as(sessions: &Sessions, call: u8) -> Started {
    let sessions = Arc::clone(sessions);
    tokio::spawn(async move {
        sessions
            .start(start_request(), effect(owner(1, 1), call))
            .await
    })
}

/// A model's foreground poll of `id` at cursor 5.
fn poll_at(sessions: &Sessions, id: plakous_tools::JoinedProcessSessionId) -> Polled {
    let sessions = Arc::clone(sessions);
    tokio::spawn(async move {
        sessions
            .poll_from(
                id,
                plakous_tools::JoinedProcessOutputCursor::new(5),
                1024,
                16,
                Duration::ZERO,
                effect(owner(1, 1), 3),
            )
            .await
    })
}

/// The reread poll and the held start on a released connection, in either
/// order, each fresh and each exactly once.
async fn reread_and_sent(second: &mut Raw) -> (Request, Request) {
    let mut reread = None;
    let mut sent = None;
    while reread.is_none() || sent.is_none() {
        let request = second.next_request().await;
        assert!(!request.replay, "a fresh send, not a replay: {request:?}");
        match request.class {
            RequestClass::ProcessObservation { .. } => {
                assert!(reread.replace(request).is_none(), "one reread");
            }
            RequestClass::EffectCall { .. } => {
                assert!(sent.replace(request).is_none(), "one send");
            }
            _ => panic!("unexpected request {request:?}"),
        }
    }
    (reread.unwrap(), sent.unwrap())
}

/// A started session over `runner`, its start answered at once.
async fn started(
    sessions: &Sessions,
    runner: &mut Raw,
    hello: &Hello,
) -> plakous_tools::JoinedProcessSessionId {
    let starting = start_as(sessions, 1);
    let request = runner.next_request().await;
    runner
        .answer(&RunnerFrame::Result {
            request_id: request.request_id,
            payload: json!({"session": SESSION, "instance": hello.instance}),
        })
        .await;
    starting.await.unwrap().unwrap()
}

/// A model effect sent before its link was lost keeps its key past the
/// effect window while the link stays lost, and takes the answer the same
/// runner instance gives its replay: no unknown outcome merely at the
/// window (spec 01 §8.4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_effect_is_observed_past_its_window_while_its_link_is_lost() {
    let root = tempfile::tempdir().unwrap();
    let hello = service(root.path()).hello().clone();
    let (link, mut first) = fake_runner(&hello).await;
    let (client, _) =
        plakous_runner::RunnerClient::connect(link, bind("instance-1"), placement_windows())
            .await
            .unwrap();
    let tap = Tap::new(&client);
    let sessions = plakous_tools::internal::runner::RunnerSessions::from_link(
        Arc::new(tap.clone()),
        &host(root.path()),
        owner(1, 1),
    );
    let starting = tokio::spawn(async move {
        sessions
            .start(start_request(), effect(owner(1, 1), 7))
            .await
    });
    let sent = first.next_request().await;
    assert!(matches!(sent.class, RequestClass::EffectCall { .. }));
    drop(first);
    // Three effect windows with the link lost.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(!starting.is_finished(), "{:?}", tap.failures());
    let (link, mut second) = fake_runner(&hello).await;
    client.reconnect(link).await.unwrap();
    let replayed = second.next_request().await;
    assert!(replayed.replay);
    assert_eq!(
        (&replayed.class, &replayed.digest),
        (&sent.class, &sent.digest)
    );
    second
        .answer(&RunnerFrame::Result {
            request_id: replayed.request_id,
            payload: json!({"session": SESSION, "instance": hello.instance}),
        })
        .await;
    tokio::time::timeout(Duration::from_secs(30), starting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(tap.failures(), []);
}

/// On a healthy link the windows stay the ordinary failure: an unanswered
/// effect is of unknown outcome, and an unanswered observation too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthy_link_expiry_still_fixes_unknown_outcomes() {
    let root = tempfile::tempdir().unwrap();
    let hello = service(root.path()).hello().clone();
    let (link, mut runner) = fake_runner(&hello).await;
    let (client, _) =
        plakous_runner::RunnerClient::connect(link, bind("instance-1"), placement_windows())
            .await
            .unwrap();
    let tap = Tap::new(&client);
    let sessions = Arc::new(plakous_tools::internal::runner::RunnerSessions::from_link(
        Arc::new(tap.clone()),
        &host(root.path()),
        owner(1, 1),
    ));
    let id = started(&sessions, &mut runner, &hello).await;
    let starting = start_as(&sessions, 2);
    let _unanswered = runner.next_request().await;
    let error = tokio::time::timeout(Duration::from_secs(30), starting)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), ExecutionBackendErrorKind::RecoveryRequired);
    let polling = poll_at(&sessions, id);
    let _unanswered = runner.next_request().await;
    tokio::time::timeout(Duration::from_secs(30), polling)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    let failures = tap.failures();
    assert!(
        matches!(
            failures.as_slice(),
            [
                RunnerFailure::OutcomeUnknown(_),
                RunnerFailure::OutcomeUnknown(_)
            ]
        ),
        "{failures:?}"
    );
    drop(runner);
}

/// Through the placement's admitted link, a model's foreground poll whose
/// link was lost is read again at the same cursor once the placement is
/// released, and a model effect made while held is sent once, under its
/// own key, only after release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_placement_rereads_a_lost_poll_and_sends_a_held_effect_once() {
    let root = tempfile::tempdir().unwrap();
    let hello = service(root.path()).hello().clone();
    let (link, mut first) = fake_runner(&hello).await;
    let (client, _) =
        plakous_runner::RunnerClient::connect(link, bind("instance-1"), placement_windows())
            .await
            .unwrap();
    let tap = Tap::new(&client);
    let backend = RunnerBackend::new(hello.descriptor.clone(), Arc::new(tap.clone())).unwrap();
    let services =
        plakous_runner::RunnerServices::new(Arc::new(backend), tokio::runtime::Handle::current());
    let sessions = Arc::new(plakous_tools::internal::runner::RunnerSessions::from_link(
        services.backend().link(),
        &host(root.path()),
        owner(1, 1),
    ));
    let id = started(&sessions, &mut first, &hello).await;
    let polling = poll_at(&sessions, id);
    let polled = first.next_request().await;
    assert!(matches!(
        polled.class,
        RequestClass::ProcessObservation { cursor: 5, .. }
    ));
    // The link is lost; its owner holds the placement, as the watcher does.
    drop(first);
    services.set_available(false);
    let starting = start_as(&sessions, 9);
    // Past the read window: the lost poll was interrupted, not failed.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(!polling.is_finished() && !starting.is_finished());
    assert!(
        matches!(
            tap.failures().as_slice(),
            [RunnerFailure::ReadInterrupted(_)]
        ),
        "{:?}",
        tap.failures()
    );
    let (link, mut second) = fake_runner(&hello).await;
    client.reconnect(link).await.unwrap();
    services.set_available(true);
    let (reread, sent) = reread_and_sent(&mut second).await;
    assert_eq!(
        reread.operation, polled.operation,
        "the same cursor and arguments"
    );
    assert_eq!(reread.class, polled.class);
    assert_eq!(sent.operation.verb, "process.start");
    second.fail(&reread).await;
    second
        .answer(&RunnerFrame::Result {
            request_id: sent.request_id.clone(),
            payload: json!({"session": "0000000000000000000000000000002b", "instance": hello.instance}),
        })
        .await;
    let polled = tokio::time::timeout(Duration::from_secs(30), polling)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(polled.kind(), ExecutionBackendErrorKind::Io, "{polled:?}");
    tokio::time::timeout(Duration::from_secs(30), starting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // Nothing more is sent: the held effect went exactly once.
    let quiet = tokio::time::Instant::now() + Duration::from_millis(300);
    while let Ok(frame) = tokio::time::timeout_at(quiet, second.next_host()).await {
        assert!(!matches!(frame, HostFrame::Request(_)), "{frame:?}");
    }
}

/// A model effect sent before its link was lost waits for a rebinding; a
/// refused placement (here the runner was upgraded while away) ends that
/// wait at once as of unknown outcome, rather than leaving it forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_placement_ends_a_lost_links_effect_wait() {
    let root = tempfile::tempdir().unwrap();
    let hello = service(root.path()).hello().clone();
    let (link, mut first) = fake_runner(&hello).await;
    let (client, _) =
        plakous_runner::RunnerClient::connect(link, bind("instance-1"), placement_windows())
            .await
            .unwrap();
    let tap = Tap::new(&client);
    let backend = RunnerBackend::new(hello.descriptor.clone(), Arc::new(tap.clone())).unwrap();
    let services =
        plakous_runner::RunnerServices::new(Arc::new(backend), tokio::runtime::Handle::current());
    let registry = plakous_runner::RunnerRegistry::default();
    registry.configure(plakous_runner::ConfiguredRunner {
        runner_id: "loopback".into(),
        names: vec!["loopback".into()],
    });
    registry
        .ready(
            "loopback",
            client.connection_revision(),
            services.clone(),
            plakous_runner::WorkspaceOptions {
                roots: None,
                default_workspace: None,
                copies: false,
            },
            plakous_runner::WorkspaceDiscovery::default(),
            1,
        )
        .unwrap();
    let sessions: Sessions = Arc::new(plakous_tools::internal::runner::RunnerSessions::from_link(
        services.backend().link(),
        &host(root.path()),
        owner(1, 1),
    ));
    let starting = start_as(&sessions, 4);
    let _sent = first.next_request().await;
    drop(first);
    // Two windows with the link lost: still waiting for a rebinding.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!starting.is_finished(), "{:?}", tap.failures());
    registry.blocked(
        "loopback",
        plakous_runner::PlacementFailure::RunnerVersionMismatch {
            expected: version(),
            actual: VersionTriple::current("upgraded", "debug"),
        },
        2,
    );
    let error = tokio::time::timeout(Duration::from_secs(10), starting)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), ExecutionBackendErrorKind::RecoveryRequired);
    assert!(
        matches!(
            tap.failures().as_slice(),
            [RunnerFailure::OutcomeUnknown(_)]
        ),
        "{:?}",
        tap.failures()
    );
}

/// A runner that refuses the host on a rebinding proves only the requests
/// first sent on that connection never ran: a model effect replayed from
/// the lost link is of unknown outcome, while one first sent on the
/// refused connection is unavailable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_after_rebinding_leaves_replayed_effects_unknown() {
    let root = tempfile::tempdir().unwrap();
    let hello = service(root.path()).hello().clone();
    let (link, mut first) = fake_runner(&hello).await;
    let (client, _) =
        plakous_runner::RunnerClient::connect(link, bind("instance-1"), placement_windows())
            .await
            .unwrap();
    let tap = Tap::new(&client);
    let sessions: Sessions = Arc::new(plakous_tools::internal::runner::RunnerSessions::from_link(
        Arc::new(tap.clone()),
        &host(root.path()),
        owner(1, 1),
    ));
    let earlier = start_as(&sessions, 5);
    let _sent = first.next_request().await;
    drop(first);
    let (link, mut second) = fake_runner(&hello).await;
    client.reconnect(link).await.unwrap();
    let replayed = second.next_request().await;
    assert!(replayed.replay);
    let fresh = start_as(&sessions, 6);
    let sent = second.next_request().await;
    assert!(!sent.replay);
    second
        .answer(&RunnerFrame::Refused {
            request_id: None,
            refusal: Refusal::NotPaired {
                host_id: "host-1".into(),
            },
        })
        .await;
    let earlier = tokio::time::timeout(Duration::from_secs(10), earlier)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(earlier.kind(), ExecutionBackendErrorKind::RecoveryRequired);
    let fresh = tokio::time::timeout(Duration::from_secs(10), fresh)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(fresh.proven_no_effect(), "{fresh:?}");
    assert_ne!(fresh.kind(), ExecutionBackendErrorKind::RecoveryRequired);
}
