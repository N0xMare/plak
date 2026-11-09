use super::*;
use plakous::runner::WorkspaceDiscovery;
use plakous::runner::protocol::{Hello, Request, RequestClass, UnpromotedProcess};
use plakous::tools::runner::{RunnerLink as _, RunnerSessions};
use plakous::tools::{
    JoinedProcessCommand, JoinedProcessStartRequest, JoinedProcessTerminalMode, LineOwner,
    OperationClass, OperationContext,
};

pub(super) fn line_stream(
    services: &Services,
    close_after_cleanup: bool,
    starts: Arc<AtomicUsize>,
    cleanups: Arc<AtomicUsize>,
) -> (tokio::io::DuplexStream, Hello) {
    let paths = services.paths().unwrap();
    let workspace = paths.workspace().unwrap();
    let catalog =
        WorkspaceCatalog::machine(vec![workspace.to_owned()], paths.home(), false).unwrap();
    let options = catalog.options();
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-4000-8000-000000000001".parse().unwrap()),
        workspace,
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    let service = RunnerService::with_options(
        environment,
        RunnerIdentity {
            runner_id: RunnerId::new("fixture").unwrap(),
            version: VersionTriple::current("fixture", "test"),
            boot_id: "boot".into(),
            binary_id: None,
        },
        RunnerOptions {
            machine_id: Some("fixture-machine".into()),
            workspace_catalog: Some(Arc::new(catalog)),
            ..RunnerOptions::default()
        },
    )
    .unwrap();
    let hello = service.hello().clone();
    let mut stub = Stub {
        instance: hello.instance.clone(),
        options,
        foreground: Vec::new(),
        starts,
        cleanups,
    };
    let (host, mut runner) = tokio::io::duplex(65536);
    let greeting = hello.clone();
    tokio::spawn(async move {
        runner
            .write_all(&frame::encode(&RunnerFrame::Hello(greeting)).unwrap())
            .await
            .unwrap();
        let mut frames = frame::FrameReader::new();
        let mut bytes = [0; 8192];
        loop {
            let count = runner.read(&mut bytes).await.unwrap();
            if count == 0 {
                return;
            }
            frames.push(&bytes[..count]);
            while let Some(body) = frames.next_frame().unwrap() {
                let message = frame::decode::<HostFrame>(&body).unwrap();
                let cleanup = matches!(&message, HostFrame::Request(request) if request.operation.verb == "process.foreground_stop");
                if let Some(response) = stub.answer(message) {
                    if cleanup && close_after_cleanup {
                        tokio::time::pause();
                    }
                    runner
                        .write_all(&frame::encode(&response).unwrap())
                        .await
                        .unwrap();
                    if cleanup && close_after_cleanup {
                        // EOF follows the final answer before the waiting bind
                        // pass can resume on the current-thread runtime.
                        return;
                    }
                }
            }
        }
    });
    (host, hello)
}

struct Stub {
    instance: String,
    options: plakous::runner::WorkspaceOptions,
    foreground: Vec<UnpromotedProcess>,
    starts: Arc<AtomicUsize>,
    cleanups: Arc<AtomicUsize>,
}

impl Stub {
    fn answer(&mut self, message: HostFrame) -> Option<RunnerFrame> {
        match message {
            HostFrame::Lines { request_id } => Some(RunnerFrame::Lines {
                request_id,
                lines: Vec::new(),
            }),
            HostFrame::TransferOwner(transfer) => Some(RunnerFrame::TransferAccepted {
                control_id: transfer.control_id,
                stopped: Vec::new(),
                unpromoted: self.foreground.clone(),
            }),
            HostFrame::Request(request) => Some(self.request(request)),
            HostFrame::Ping { nonce } => Some(RunnerFrame::Pong { nonce }),
            _ => None,
        }
    }

    fn request(&mut self, request: Request) -> RunnerFrame {
        let payload = match request.operation.verb.as_str() {
            "workspace.options" => serde_json::to_value(&self.options).unwrap(),
            "workspace.discover" => serde_json::to_value(WorkspaceDiscovery::default()).unwrap(),
            "process.start" => {
                let RequestClass::EffectCall {
                    call_id,
                    attempt_id,
                } = request.class
                else {
                    panic!("unkeyed start")
                };
                self.foreground.push(UnpromotedProcess {
                    call_key: format!("{call_id}:{attempt_id}"),
                    pid: 1,
                });
                self.starts.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({"session":"00000000000000000000000000000001", "instance":self.instance})
            }
            "process.foreground_stop" => {
                assert_eq!(
                    request.operation.payload["call_key"],
                    self.foreground[0].call_key
                );
                self.foreground.clear();
                self.cleanups.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({"stopped":[]})
            }
            other => panic!("unexpected runner request: {other}"),
        };
        RunnerFrame::Result {
            request_id: request.request_id,
            payload,
        }
    }
}

pub(super) async fn seed_cleanup(services: &Services, client: &RunnerClient, line: LineId) {
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-4000-8000-000000000001".parse().unwrap()),
        services.paths().unwrap().workspace().unwrap(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    let owner = LineOwner::new(
        line,
        1,
        "00000000-0000-4000-8000-000000000002".parse().unwrap(),
        1,
    );
    let call = "00000000-0000-4000-8000-000000000003".parse().unwrap();
    let attempt = "00000000-0000-4000-8000-000000000004".parse().unwrap();
    RunnerSessions::from_link(Arc::new(client.clone()), &environment, owner)
        .start(
            JoinedProcessStartRequest::new(
                JoinedProcessCommand::literal("/bin/false", Vec::new()).unwrap(),
                ".",
                Vec::new(),
                false,
                JoinedProcessTerminalMode::Pipes,
            )
            .unwrap(),
            OperationContext::new(plakous::model::CancellationToken::new()).with_class(
                OperationClass::EffectCall {
                    call_id: call,
                    attempt_id: attempt,
                },
            ),
        )
        .await
        .unwrap();
    let client = client.clone();
    // Mark terminal state without spawning the client's eager cleanup task;
    // the production reconciliation pass must own the final cleanup answer.
    std::thread::spawn(move || client.driver_operation_settled(owner, call, attempt))
        .join()
        .unwrap();
}
