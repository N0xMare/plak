//! Product consent and byte streams over a real runner PTY, without an agent runtime.

use super::*;
use plakous::runner::protocol::{Bind, VersionTriple};
use plakous::runner::{ClientWindows, RunnerClient, RunnerIdentity, RunnerOptions, RunnerService};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};
use plakous_hosted::host_service::AttachmentCount;
use plakous_recursive_subagents::tree::{FakeVcs, ProductAgentTree, TreeCustody, TreeStore};

struct Linked {
    _home: tempfile::TempDir,
    work: tempfile::TempDir,
    paths: crate::home::paths::PlakPaths,
    registry: Arc<TerminalRegistry>,
    console: Arc<Console>,
    record: crate::store::sessions::SessionRecord,
    count: AttachmentCount,
}

impl Linked {
    fn new() -> Self {
        let (home, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(work.path()).unwrap();
        let mut record = crate::store::sessions::SessionRecord::new(cwd.clone());
        record.node = plakous_hosted::sessions::SessionNode {
            tree_id: "terminal-tree".into(),
            agent_id: "root".into(),
        };
        crate::store::sessions::save_session(&paths, &mut record).unwrap();
        let tree = ProductAgentTree::bootstrap_root_for_test_with(
            Arc::new(FakeVcs::new()),
            "terminal-tree",
            "root",
            "owner",
            cwd,
            "tuple",
            plakous_host_protocol::topology::WriterMode::SingleLease,
        )
        .unwrap();
        let store = crate::store::tree::open_tree_store(&paths).unwrap();
        let claim = store
            .take_custody(
                "terminal-tree",
                plakous_recursive_subagents::tree::custody_host(),
                None,
            )
            .unwrap();
        assert!(store.save(&tree, &claim).unwrap().value() > 0);
        let registry = TerminalRegistry::new(paths.clone(), context().host_generation).unwrap();
        let count = AttachmentCount::default();
        let held = count.clone();
        registry
            .bind_residency(Arc::new(move |_| {
                held.hold().ok_or_else(|| "releasing".into())
            }))
            .unwrap();
        let console = Console::new(
            paths.clone(),
            paths.home().join("state"),
            Some(tokio::runtime::Handle::current()),
            dto::ConfiguredSwitches {
                web_accounts_read: false,
                web_config_read: false,
                web_credentials_write: false,
                web_config_write: false,
                web_files_read: false,
                web_terminal: false,
            },
        );
        Self {
            _home: home,
            work,
            paths,
            registry,
            console,
            record,
            count,
        }
    }

    fn service(&self) -> RunnerService {
        let cwd = std::fs::canonicalize(self.work.path()).unwrap();
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap()),
            &cwd,
            &cwd,
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap();
        RunnerService::with_options(
            environment,
            RunnerIdentity {
                runner_id: RunnerId::new("terminal-fixture").unwrap(),
                version: VersionTriple::current("revision", "debug"),
                boot_id: "boot".into(),
                binary_id: None,
            },
            RunnerOptions {
                operator_workspace: Some(Arc::new(|cwd| {
                    let mut launch = plakous::runner::operator::account_launch(
                        cwd.into(),
                        "/usr/bin:/bin".into(),
                        None,
                    )?;
                    launch.shell = "/bin/sh".into();
                    launch.home = cwd.into();
                    Ok(launch)
                })),
                ..RunnerOptions::default()
            },
        )
        .unwrap()
    }

    async fn connect(
        &self,
        service: &RunnerService,
    ) -> (RunnerClient, tokio::task::JoinHandle<()>) {
        let (host, remote) = tokio::io::duplex(64 * 1024);
        let service = service.clone();
        let serving = tokio::spawn(async move {
            service.serve_connection(remote).await;
        });
        let (client, _) = RunnerClient::connect(
            host,
            Bind {
                host_id: crate::commands::runner::host_id(&self.paths).unwrap(),
                host_instance: plakous_recursive_subagents::tree::process_instance().to_string(),
                version: VersionTriple::current("revision", "debug"),
                inspect: false,
            },
            ClientWindows::default(),
        )
        .await
        .unwrap();
        self.registry.reconcile("local", &client).await.unwrap();
        (client, serving)
    }

    fn grant(&self, context: &HostRequestContext, action: dto::OperatorAction) -> String {
        let HostResponse::OperatorPrepared { consent } =
            self.console.prepare(context.clone(), action).unwrap()
        else {
            panic!("prepared")
        };
        let HostResponse::OperatorConfirmed { confirmation, .. } =
            self.console.confirm(context, &consent.nonce).unwrap()
        else {
            panic!("confirmed")
        };
        confirmation
    }

    async fn call(&self, context: &HostRequestContext, request: HostRequest) -> HostResponse {
        self.console
            .terminal_request(context.clone(), request, Some(self.registry.clone()))
            .await
            .unwrap()
    }

    async fn attach(
        &self,
        context: &HostRequestContext,
        terminal: &dto::TerminalView,
        takeover: bool,
    ) -> u64 {
        let action = dto::OperatorAction::TerminalAttach {
            terminal: terminal.terminal.clone(),
            cursor: terminal.oldest.clone(),
            takeover,
        };
        let confirmation = self.grant(context, action);
        let HostResponse::TerminalAttached { stream, .. } = self
            .call(
                context,
                HostRequest::TerminalAttach {
                    terminal: terminal.terminal.clone(),
                    cursor: terminal.oldest.clone(),
                    takeover,
                    confirmation,
                },
            )
            .await
        else {
            panic!("attached")
        };
        let response = HostResponse::TerminalAttached {
            terminal: terminal.clone(),
            stream,
            input_credit: WINDOW,
        };
        self.console.terminal_response_sent(context, &response);
        stream
    }

    async fn event(&self, context: &HostRequestContext) -> dto::TerminalEvent {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(HostFrame::TerminalStream { event, .. }) =
                self.console.terminal_output(context)
            {
                return event;
            }
            assert!(Instant::now() < deadline, "terminal event did not arrive");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}

async fn opened(linked: &Linked) -> (dto::TerminalView, u64) {
    let size = dto::TerminalDimensions {
        rows: 24,
        columns: 80,
    };
    let request = |confirmation| HostRequest::TerminalOpen {
        session: linked.record.id.clone(),
        node: "root".into(),
        terminal_id: "terminal".into(),
        dimensions: size,
        confirmation,
    };
    assert!(matches!(
        linked.call(&context(), request("missing".into())).await,
        HostResponse::Refused {
            reason: dto::ReadRefusal::ConfirmationRequired {}
        }
    ));
    let confirmation = linked.grant(
        &context(),
        dto::OperatorAction::TerminalOpen {
            session: linked.record.id.clone(),
            node: "root".into(),
            terminal_id: "terminal".into(),
            dimensions: size,
        },
    );
    let HostResponse::Terminal { terminal } = linked.call(&context(), request(confirmation)).await
    else {
        panic!("open")
    };
    let first = linked.attach(&context(), &terminal, false).await;
    let HostResponse::Terminal {
        terminal: recovered,
    } = linked.call(&context(), request("spent-grant".into())).await
    else {
        panic!("lookup-only retry")
    };
    assert_eq!(recovered.terminal, terminal.terminal);
    assert_eq!(linked.count.get(), 1);
    (terminal, first)
}

#[tokio::test]
async fn operator_console_pty_wire_small_chunk_stall_keeps_stream_live_and_closes_admission() {
    let linked = Linked::new();
    let service = linked.service();
    let (client, serving) = linked.connect(&service).await;
    let (terminal, stream) = opened(&linked).await;
    let command = b"n=0; while [ \"$n\" -lt 60 ]; do printf x; n=$((n+1)); sleep 0.03; done\n";
    assert!(matches!(
        linked
            .call(
                &context(),
                HostRequest::TerminalInput {
                    stream,
                    offset: 0,
                    bytes_b64: STANDARD.encode(command),
                }
            )
            .await,
        HostResponse::TerminalAccepted {}
    ));
    tokio::time::sleep(std::time::Duration::from_millis(2300)).await;
    assert!(!linked.console.terminals.lock().unwrap().streams[&stream].ended);
    assert!(matches!(
        linked
            .call(
                &context(),
                HostRequest::TerminalInput {
                    stream,
                    offset: command.len() as u64,
                    bytes_b64: STANDARD.encode(b"printf 'SMALL''-DONE\\n'\n"),
                }
            )
            .await,
        HostResponse::TerminalAccepted {}
    ));
    let mut output = Vec::new();
    while !output.windows(10).any(|bytes| bytes == b"SMALL-DONE") {
        match linked.event(&context()).await {
            dto::TerminalEvent::Data { bytes_b64, .. } => {
                output.extend(STANDARD.decode(bytes_b64).unwrap());
            }
            dto::TerminalEvent::InputAccepted { .. } => {}
            _ => panic!("slow reader lost the controller"),
        }
    }
    assert!(matches!(
        linked
            .call(
                &context(),
                HostRequest::TerminalClose {
                    terminal: terminal.terminal,
                    acknowledge_loss: false,
                }
            )
            .await,
        HostResponse::Terminal { .. }
    ));
    closed_admission(&linked, stream).await;
    linked.registry.reconcile("local", &client).await.unwrap();
    client.disconnect();
    serving.await.unwrap();
    let confirmation = linked.grant(
        &context(),
        dto::OperatorAction::TerminalRelease {
            session: linked.record.id.clone(),
        },
    );
    assert!(
        matches!(
            linked
                .call(
                    &context(),
                    HostRequest::TerminalReleaseSession {
                        session: linked.record.id.clone(),
                        confirmation,
                    }
                )
                .await,
            HostResponse::TerminalAccepted {}
        ),
        "completed unpinned rows need no live runner"
    );
}

async fn closed_admission(linked: &Linked, stream: u64) {
    let until = Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let closed = {
            let state = linked.console.terminals.lock().unwrap();
            let stream = &state.streams[&stream];
            stream.ended && stream.commands.is_closed()
        };
        if closed {
            break;
        }
        assert!(Instant::now() < until);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(matches!(
        linked
            .call(
                &context(),
                HostRequest::TerminalInput {
                    stream,
                    offset: 0,
                    bytes_b64: STANDARD.encode(b"not-admitted"),
                }
            )
            .await,
        HostResponse::Refused {
            reason: dto::ReadRefusal::TerminalLost {}
        }
    ));
}

async fn executed(linked: &Linked, terminal: &dto::TerminalView, first: u64) -> HostRequestContext {
    let next = HostRequestContext {
        connection_id: 8,
        ..context()
    };
    let busy_grant = linked.grant(
        &next,
        dto::OperatorAction::TerminalAttach {
            terminal: terminal.terminal.clone(),
            cursor: terminal.oldest.clone(),
            takeover: false,
        },
    );
    for _ in 0..2 {
        assert!(matches!(
            linked
                .call(
                    &next,
                    HostRequest::TerminalAttach {
                        terminal: terminal.terminal.clone(),
                        cursor: terminal.oldest.clone(),
                        takeover: false,
                        confirmation: busy_grant.clone()
                    }
                )
                .await,
            HostResponse::Refused {
                reason: dto::ReadRefusal::TerminalBusy {}
            }
        ));
    }
    let second = linked.attach(&next, terminal, true).await;
    assert_ne!(first, second);
    assert!(matches!(
        linked
            .call(
                &context(),
                HostRequest::TerminalInput {
                    stream: first,
                    offset: 0,
                    bytes_b64: STANDARD.encode(b"not accepted")
                }
            )
            .await,
        HostResponse::Refused {
            reason: dto::ReadRefusal::TerminalLost {}
        }
    ));
    let bytes = b"printf x >> marker; printf '\\127\\111\\122\\105\\055\\117\\113\\n'\n";
    assert!(matches!(
        linked
            .call(
                &next,
                HostRequest::TerminalInput {
                    stream: second,
                    offset: 0,
                    bytes_b64: STANDARD.encode(bytes)
                }
            )
            .await,
        HostResponse::TerminalAccepted {}
    ));
    let mut output = Vec::new();
    while !output.windows(7).any(|w| w == b"WIRE-OK") {
        if let dto::TerminalEvent::Data { bytes_b64, .. } = linked.event(&next).await {
            output.extend(STANDARD.decode(bytes_b64).unwrap());
        }
    }
    next
}

#[tokio::test]
async fn operator_console_pty_wire_consent_takeover_disconnect_and_loss() {
    let linked = Linked::new();
    let service = linked.service();
    let (client, serving) = linked.connect(&service).await;
    let (terminal, first) = opened(&linked).await;
    let next = executed(&linked, &terminal, first).await;
    linked
        .console
        .disconnect(&next.principal, next.connection_id, next.host_generation);
    linked
        .registry
        .disconnect(&next.principal, next.connection_id, next.host_generation);
    let third_owner = HostRequestContext {
        connection_id: 9,
        ..context()
    };
    let _third = linked.attach(&third_owner, &terminal, true).await;
    assert_eq!(
        std::fs::read(linked.work.path().join("marker")).unwrap(),
        b"x"
    );
    assert_eq!(client.lines().await.unwrap(), Vec::new());
    serving.abort();
    assert!(serving.await.unwrap_err().is_cancelled());
    while !matches!(
        linked.event(&third_owner).await,
        dto::TerminalEvent::Suspended {}
    ) {}
    let replacement = linked.service();
    let (replacement_client, replacement_serving) = linked.connect(&replacement).await;
    let HostResponse::TerminalList { terminals } = linked
        .call(&third_owner, HostRequest::TerminalList {})
        .await
    else {
        panic!("list")
    };
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].status, dto::TerminalStatus::Lost);
    assert_eq!(linked.count.get(), 1);
    assert!(!linked.paths.runtime_database().exists());
    assert_eq!(
        crate::store::sessions::load_session(&linked.paths, &linked.record.id)
            .unwrap()
            .store_revision,
        linked.record.store_revision
    );
    linked
        .call(
            &third_owner,
            HostRequest::TerminalClose {
                terminal: terminals[0].terminal.clone(),
                acknowledge_loss: true,
            },
        )
        .await;
    assert_eq!(linked.count.get(), 0);
    assert!(service.operator_terminals().unwrap().has_live_terminals());
    assert!(
        !replacement
            .operator_terminals()
            .unwrap()
            .has_live_terminals()
    );
    service.operator_terminals().unwrap().close_all();
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while service.operator_terminals().unwrap().has_live_terminals() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    replacement_client.disconnect();
    replacement_serving.await.unwrap();
}
