use super::*;
use base64::Engine as _;
use plakous_runner_protocol::{StreamEvent, StreamFrame};

#[derive(Default)]
struct Fake {
    spawned: usize,
    input: Vec<Vec<u8>>,
    sizes: Vec<TerminalDimensions>,
    closed: usize,
    proof: bool,
    outputs: Vec<Arc<Mutex<Output>>>,
}

struct FakeBackend(Arc<Mutex<Fake>>);
struct FakeProcess(Arc<Mutex<Fake>>, bool, bool);

impl Backend for FakeBackend {
    fn spawn(
        &self,
        _: OperatorLaunch,
        _: TerminalDimensions,
        output: Arc<Mutex<Output>>,
    ) -> Result<Box<dyn Process>, OperatorRefusal> {
        let mut state = lock(&self.0);
        state.spawned += 1;
        state.outputs.push(output);
        Ok(Box::new(FakeProcess(Arc::clone(&self.0), false, false)))
    }
}

impl Process for FakeProcess {
    fn input(&mut self, bytes: Vec<u8>) -> Result<(), OperatorRefusal> {
        lock(&self.0).input.push(bytes);
        Ok(())
    }
    fn resize(&mut self, dimensions: TerminalDimensions) -> Result<(), OperatorRefusal> {
        lock(&self.0).sizes.push(dimensions);
        Ok(())
    }
    fn poll(&mut self) -> Option<(TerminalStatus, Option<u32>)> {
        self.1.then(|| {
            (
                if self.2 {
                    TerminalStatus::Lost
                } else if lock(&self.0).proof {
                    TerminalStatus::Exited
                } else {
                    TerminalStatus::TerminationUnknown
                },
                Some(0),
            )
        })
    }
    fn close(&mut self) {
        self.1 = true;
        lock(&self.0).closed += 1;
    }
    fn reprove(&mut self, acknowledge_loss: bool) {
        if acknowledge_loss {
            self.2 = true;
        }
    }
}

struct Fixture {
    service: OperatorTerminalService,
    clock: Arc<Mutex<Instant>>,
    fake: Arc<Mutex<Fake>>,
}

fn id(text: &str) -> OperatorId {
    OperatorId::try_from(text.to_owned()).unwrap()
}
fn namespace() -> OperatorNamespace {
    OperatorNamespace {
        state_key: id("state"),
        principal: id("operator"),
    }
}
fn key(n: usize) -> TerminalKey {
    TerminalKey {
        session: id("session"),
        node: id("node"),
        terminal_id: id(&format!("terminal-{n}")),
    }
}
fn dimensions() -> TerminalDimensions {
    TerminalDimensions::new(24, 80).unwrap()
}

impl Fixture {
    fn new() -> Self {
        let clock = Arc::new(Mutex::new(Instant::now()));
        let time = Arc::clone(&clock);
        let fake = Arc::new(Mutex::new(Fake::default()));
        let service = OperatorTerminalService {
            inner: Arc::new(Inner {
                instance: id("instance"),
                table: Mutex::default(),
                clock: Arc::new(move || *lock(&time)),
                backend: Arc::new(FakeBackend(Arc::clone(&fake))),
                workspace: Arc::new(|cwd| {
                    Ok(OperatorLaunch {
                        cwd: cwd.to_path_buf(),
                        path: "/usr/bin:/bin".into(),
                        home: "/home".into(),
                        user: "operator".into(),
                        shell: "/bin/sh".into(),
                        discovery_ceiling: None,
                    })
                }),
            }),
        };
        let fixture = Self {
            service,
            clock,
            fake,
        };
        fixture.call(1, OperatorOperation::Renew);
        fixture
    }

    fn call(&self, connection: u64, operation: OperatorOperation) -> OperatorAnswer {
        self.service.execute(
            "paired",
            "generation",
            connection,
            namespace(),
            "digest",
            operation,
        )
    }
    fn advance(&self, seconds: u64) {
        *lock(&self.clock) += Duration::from_secs(seconds);
    }
    fn open(&self, n: usize) -> TerminalRow {
        let OperatorAnswer::Terminal { terminal, .. } = self.call(
            1,
            OperatorOperation::Open {
                key: key(n),
                workspace: "/workspace".into(),
                dimensions: dimensions(),
            },
        ) else {
            panic!("open failed");
        };
        terminal
    }
    fn attach(&self, n: usize, connection: u64) -> u64 {
        let OperatorAnswer::Terminal {
            stream: Some(stream),
            ..
        } = self.call(
            connection,
            OperatorOperation::Attach {
                key: key(n),
                cursor: TerminalCursor {
                    instance: id("instance"),
                    offset: 0,
                },
            },
        )
        else {
            panic!("attach failed");
        };
        stream
    }
    fn input(&self, stream: u64, seq: u64, offset: u64, bytes: &[u8]) -> Vec<StreamFrame> {
        self.service
            .receive(
                "paired",
                "generation",
                1,
                StreamFrame {
                    stream,
                    seq,
                    event: StreamEvent::Data {
                        payload: serde_json::to_value(TerminalEvent::Data {
                            offset,
                            bytes_b64: STANDARD.encode(bytes),
                        })
                        .unwrap(),
                    },
                },
            )
            .unwrap()
    }
}

fn refused(answer: OperatorAnswer, expected: OperatorRefusal) {
    match answer {
        OperatorAnswer::Inventory { .. } | OperatorAnswer::Reclamation { .. } => {
            panic!("expected refusal, got control authority")
        }
        OperatorAnswer::Refused { refusal } => assert_eq!(refusal, expected),
        OperatorAnswer::Table { terminals } => panic!(
            "expected refusal, got a terminal: {}",
            terminals.into_iter().next().is_some()
        ),
        OperatorAnswer::Terminal { terminal, .. } => panic!(
            "expected refusal for {}",
            String::from(terminal.key.terminal_id)
        ),
    }
}

#[test]
fn operator_inventory_recovers_only_its_paired_host_and_state_without_output() {
    let fixture = Fixture::new();
    fixture.open(0);
    let other = OperatorNamespace {
        principal: id("other"),
        ..namespace()
    };
    assert!(matches!(
        fixture.service.execute(
            "paired",
            "generation",
            1,
            other.clone(),
            "renew",
            OperatorOperation::Renew,
        ),
        OperatorAnswer::Table { .. }
    ));
    assert!(matches!(
        fixture.service.execute(
            "paired",
            "generation",
            1,
            other,
            "open",
            OperatorOperation::Open {
                key: key(1),
                workspace: "/workspace/child".into(),
                dimensions: dimensions(),
            },
        ),
        OperatorAnswer::Terminal { .. }
    ));
    let OperatorAnswer::Inventory { instance, pins } =
        fixture.call(1, OperatorOperation::Inventory)
    else {
        panic!("inventory failed");
    };
    assert_eq!(instance, id("instance"));
    assert_eq!(pins.len(), 2);
    assert!(
        pins.iter()
            .any(|pin| pin.namespace.principal == id("other"))
    );
    assert!(
        pins.iter().all(
            |pin| pin.generation == id("generation") && pin.workspace.starts_with("/workspace")
        )
    );
    for (host, state) in [("foreign", "state"), ("paired", "foreign")] {
        assert!(matches!(fixture.service.execute(host, "generation", 1,
            OperatorNamespace { state_key: id(state), ..namespace() }, "inventory", OperatorOperation::Inventory),
            OperatorAnswer::Inventory { pins, .. } if pins.is_empty()));
    }
    assert_eq!(lock(&fixture.fake).spawned, 2);
    assert_eq!(lock(&fixture.fake).input, Vec::<Vec<u8>>::new());
}

#[test]
fn operator_scope_reconciliation_has_no_terminal_lifetime_or_namespace_quota() {
    let fixture = Fixture::new();
    fixture.advance(91);
    fixture.service.tick();
    for _ in 0..129 {
        assert!(
            matches!(fixture.call(1, OperatorOperation::ReconcileScope), OperatorAnswer::Inventory { pins, .. } if pins.is_empty())
        );
        fixture.advance(91);
    }
    refused(
        fixture.call(1, OperatorOperation::Renew),
        OperatorRefusal::NamespaceExpired,
    );
    assert!(matches!(
        fixture.call(1, OperatorOperation::ReconcileScope),
        OperatorAnswer::Inventory { pins, .. } if pins.is_empty()
    ));
    for n in 0..128 {
        assert!(matches!(
            fixture.service.execute(
                "paired",
                "generation",
                1,
                OperatorNamespace {
                    principal: id(&format!("principal-{n}")),
                    ..namespace()
                },
                "renew",
                OperatorOperation::Renew
            ),
            OperatorAnswer::Table { .. }
        ));
    }
    assert_eq!(lock(&fixture.fake).spawned, 0);
}

#[test]
fn operator_reclamation_stream_holds_until_owned_close_or_disconnect() {
    let fixture = Fixture::new();
    let OperatorAnswer::Reclamation {
        stream: Some(stream),
    } = fixture.call(
        1,
        OperatorOperation::Reclaim {
            workspace: "/workspace".into(),
        },
    )
    else {
        panic!("reclaim failed");
    };
    let open = |n| OperatorOperation::Open {
        key: key(n),
        workspace: "/workspace/child".into(),
        dimensions: dimensions(),
    };
    refused(fixture.call(1, open(0)), OperatorRefusal::TerminalBusy);
    assert!(
        fixture
            .service
            .receive(
                "foreign",
                "generation",
                1,
                StreamFrame {
                    stream,
                    seq: 0,
                    event: StreamEvent::Close { reason: None }
                }
            )
            .is_none()
    );
    refused(fixture.call(1, open(1)), OperatorRefusal::TerminalBusy);
    assert!(
        fixture
            .service
            .receive(
                "paired",
                "generation",
                1,
                StreamFrame {
                    stream,
                    seq: 0,
                    event: StreamEvent::Close { reason: None }
                }
            )
            .is_some()
    );
    assert!(matches!(
        fixture.call(1, open(2)),
        OperatorAnswer::Terminal { .. }
    ));
    let second = Fixture::new();
    assert!(matches!(
        second.call(
            1,
            OperatorOperation::Reclaim {
                workspace: "/workspace".into()
            }
        ),
        OperatorAnswer::Reclamation { .. }
    ));
    assert!(
        second.service.has_live_terminals(),
        "a live reclamation lease prevents idle exit"
    );
    second.service.disconnect("paired", 1);
    assert!(!second.service.has_live_terminals());
    assert!(matches!(
        second.call(1, open(0)),
        OperatorAnswer::Terminal { .. }
    ));
}

#[test]
fn operator_reserves_concurrent_open_quota_and_recovers_lost_open_answer() {
    let fixture = Fixture::new();
    let mut threads = Vec::new();
    for n in 0..16 {
        let service = fixture.service.clone();
        threads.push(std::thread::spawn(move || {
            service.execute(
                "paired",
                "generation",
                1,
                namespace(),
                "digest",
                OperatorOperation::Open {
                    key: key(n),
                    workspace: "/workspace".into(),
                    dimensions: dimensions(),
                },
            )
        }));
    }
    let rows: Vec<_> = threads
        .into_iter()
        .map(|t| t.join().unwrap())
        .filter_map(|a| {
            if let OperatorAnswer::Terminal { terminal, .. } = a {
                Some(terminal)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(lock(&fixture.fake).spawned, 2);
    let operation = OperatorOperation::Open {
        key: rows[0].key.clone(),
        workspace: "/workspace".into(),
        dimensions: dimensions(),
    };
    assert!(matches!(
        fixture.call(1, operation.clone()),
        OperatorAnswer::Terminal { .. }
    ));
    refused(
        fixture.service.execute(
            "paired",
            "generation",
            1,
            namespace(),
            "other-digest",
            operation,
        ),
        OperatorRefusal::Conflict,
    );
    assert_eq!(lock(&fixture.fake).spawned, 2);
}

#[test]
fn operator_input_ack_loss_offsets_binary_and_single_controller() {
    let fixture = Fixture::new();
    fixture.open(0);
    let stream = fixture.attach(0, 1);
    let lost_ack = fixture.input(stream, 0, 0, &[0, 255, 1]);
    assert!(matches!(&lost_ack[0].event,StreamEvent::Data {payload} if payload["next"]==3));
    fixture.input(stream, 1, 0, &[0, 255, 1]);
    assert_eq!(lock(&fixture.fake).input, vec![vec![0, 255, 1]]);
    let gap = fixture.input(stream, 2, 4, b"wrong");
    assert!(
        matches!(&gap[0].event,StreamEvent::Data {payload} if payload["refusal"]=="offset_gap")
    );
    refused(
        fixture.call(
            1,
            OperatorOperation::Attach {
                key: key(0),
                cursor: TerminalCursor {
                    instance: id("instance"),
                    offset: 0,
                },
            },
        ),
        OperatorRefusal::TerminalBusy,
    );
    fixture.service.disconnect("paired", 1);
    refused(
        fixture.call(
            2,
            OperatorOperation::Attach {
                key: key(0),
                cursor: TerminalCursor {
                    instance: id("instance"),
                    offset: 0,
                },
            },
        ),
        OperatorRefusal::ReconcileRequired,
    );
    fixture.call(2, OperatorOperation::List);
    fixture.attach(0, 2);
    assert_eq!(
        lock(&fixture.fake).input.len(),
        1,
        "reattach must not replay ambiguous keyboard bytes"
    );
}

#[test]
fn operator_stream_sequence_corruption_detaches_without_killing_shell() {
    let fixture = Fixture::new();
    fixture.open(0);
    let stream = fixture.attach(0, 1);
    fixture.input(stream, 0, 0, b"one");
    let frames = fixture.input(stream, 0, 3, b"two");
    assert!(matches!(frames[0].event, StreamEvent::Close { .. }));
    assert_eq!(lock(&fixture.fake).closed, 0);
    assert_eq!(lock(&fixture.fake).input.len(), 1);
}

#[test]
fn operator_idle_is_awake_input_time_not_output_resize_or_credit() {
    let fixture = Fixture::new();
    fixture.open(0);
    fixture.attach(0, 1);
    for _ in 0..30 {
        fixture.advance(30);
        lock(&lock(&fixture.fake).outputs[0]).push(b"busy output");
        fixture.call(1, OperatorOperation::Renew);
    }
    fixture.service.tick();
    assert_eq!(lock(&fixture.fake).closed, 1);
    assert!(
        fixture.service.has_live_terminals(),
        "unknown cleanup keeps daemon alive"
    );
    assert!(
        fixture
            .service
            .workspace_pinned(Path::new("/workspace/child"))
    );
}

#[test]
fn operator_detach_and_namespace_deadlines_are_independent_of_clock_suspension() {
    let fixture = Fixture::new();
    fixture.open(0);
    fixture.service.tick();
    assert_eq!(
        lock(&fixture.fake).closed,
        0,
        "unchanged awake clock never expires"
    );
    for _ in 0..10 {
        fixture.advance(30);
        fixture.call(1, OperatorOperation::Renew);
    }
    assert_eq!(
        lock(&fixture.fake).closed,
        1,
        "detached shell expires despite namespace renewal"
    );
    let second = Fixture::new();
    second.open(0);
    second.attach(0, 1);
    second.advance(90);
    refused(
        second.call(1, OperatorOperation::Renew),
        OperatorRefusal::NamespaceExpired,
    );
    assert_eq!(lock(&second.fake).closed, 1);
}

#[test]
fn operator_close_requires_proof_before_releasing_pin_or_quota() {
    let fixture = Fixture::new();
    fixture.open(0);
    fixture.call(
        1,
        OperatorOperation::Close {
            generation: None,
            key: key(0),
            acknowledge_loss: false,
        },
    );
    fixture.service.tick();
    assert!(fixture.service.workspace_pinned(Path::new("/workspace")));
    fixture.call(
        1,
        OperatorOperation::Close {
            generation: None,
            key: key(0),
            acknowledge_loss: false,
        },
    );
    assert_eq!(lock(&fixture.fake).closed, 1, "close is idempotent");
    lock(&fixture.fake).proof = true;
    fixture.service.tick();
    assert!(!fixture.service.workspace_pinned(Path::new("/workspace")));
    assert!(!fixture.service.has_live_terminals());
}

#[test]
fn operator_expired_unknown_close_reports_acknowledged_loss_before_collection() {
    let fixture = Fixture::new();
    fixture.open(0);
    fixture.advance(90);
    assert!(fixture.service.workspace_pinned(Path::new("/workspace")));
    fixture.call(1, OperatorOperation::List);
    fixture.call(
        1,
        OperatorOperation::Close {
            generation: None,
            key: key(0),
            acknowledge_loss: true,
        },
    );
    fixture.service.tick();
    assert!(!fixture.service.has_live_terminals());
    assert!(!fixture.service.workspace_pinned(Path::new("/workspace")));
    assert!(
        matches!(fixture.call(1, OperatorOperation::List), OperatorAnswer::Table { terminals } if terminals[0].status == TerminalStatus::Lost)
    );
    fixture.service.tick();
    assert!(lock(&fixture.service.inner.table).namespaces.is_empty());
}

#[test]
fn operator_exited_receipts_collect_without_ever_respawning_evicted_ids() {
    let fixture = Fixture::new();
    lock(&fixture.fake).proof = true;
    for n in 0..100 {
        fixture.open(n);
        fixture.call(
            1,
            OperatorOperation::Close {
                generation: None,
                key: key(n),
                acknowledge_loss: false,
            },
        );
        fixture.service.tick();
        assert!(
            lock(&fixture.service.inner.table)
                .namespaces
                .values()
                .all(|ns| ns.terminals.len() <= 8)
        );
    }
    refused(
        fixture.call(
            1,
            OperatorOperation::Open {
                key: key(0),
                workspace: "/workspace".into(),
                dimensions: dimensions(),
            },
        ),
        OperatorRefusal::TerminalLost,
    );
    assert_eq!(lock(&fixture.fake).spawned, 100);
}

#[test]
fn operator_collected_namespaces_fence_129_superseded_generations() {
    let fixture = Fixture::new();
    lock(&fixture.fake).proof = true;
    for n in 0..140 {
        let generation = format!("generation-{n}");
        assert!(matches!(
            fixture.service.execute(
                "paired",
                &generation,
                1,
                namespace(),
                "renew",
                OperatorOperation::Renew
            ),
            OperatorAnswer::Table { .. }
        ));
        let _ = fixture.service.execute(
            "paired",
            &generation,
            1,
            namespace(),
            "close",
            OperatorOperation::NamespaceClose,
        );
        fixture.service.tick();
        assert!(lock(&fixture.service.inner.table).namespaces.is_empty());
    }
    refused(
        fixture.service.execute(
            "paired",
            "generation-0",
            1,
            namespace(),
            "renew",
            OperatorOperation::Renew,
        ),
        OperatorRefusal::NamespaceExpired,
    );
}

#[test]
fn operator_output_capacity_never_consumes_unsent_gap_data_or_sequence() {
    use plakous_runner_protocol::{StreamEvent, StreamFrame};
    let fixture = Fixture::new();
    fixture.open(0);
    let stream = fixture.attach(0, 1);
    lock(&lock(&fixture.fake).outputs[0]).push(&vec![7; RING_BYTES + 1]);
    let _ = fixture.service.receive(
        "paired",
        "generation",
        1,
        StreamFrame {
            stream,
            seq: 0,
            event: StreamEvent::Credit {
                bytes: WINDOW_BYTES,
            },
        },
    );
    assert_eq!(
        fixture.service.output_round("paired", "generation", 1, 0),
        []
    );
    let gap = fixture.service.output_round("paired", "generation", 1, 1);
    assert_eq!(gap.len(), 1);
    assert_eq!(gap[0].seq, 0);
    assert!(
        matches!(&gap[0].event, StreamEvent::Data { payload } if payload["terminal_event"] == "output_gap")
    );
    let data = fixture.service.output_round("paired", "generation", 1, 1);
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].seq, 1);
    assert!(matches!(&data[0].event, StreamEvent::Data { payload } if payload["offset"] == 1));
}

#[test]
fn operator_ring_overflow_reports_gap_before_binary_replay_and_exit() {
    let fixture = Fixture::new();
    fixture.open(0);
    let stream = fixture.attach(0, 1);
    lock(&lock(&fixture.fake).outputs[0]).push(&vec![255; RING_BYTES + 9]);
    fixture
        .service
        .receive(
            "paired",
            "generation",
            1,
            StreamFrame {
                stream,
                seq: 0,
                event: StreamEvent::Credit {
                    bytes: WINDOW_BYTES,
                },
            },
        )
        .unwrap();
    let frames = fixture.service.output_round("paired", "generation", 1, 64);
    assert!(
        matches!(&frames[0].event,StreamEvent::Data {payload} if payload["terminal_event"]=="output_gap" && payload["oldest"]["offset"]==9)
    );
    assert!(matches!(&frames[1].event,StreamEvent::Data {payload} if payload["offset"]==9));
    assert_eq!(frames[0].seq, 0);
    assert_eq!(frames[1].seq, 1);
}

#[test]
fn operator_namespace_visibility_generation_loss_and_restart_never_adopt() {
    let fixture = Fixture::new();
    fixture.open(0);
    let other = OperatorNamespace {
        principal: id("other"),
        ..namespace()
    };
    assert_eq!(
        fixture.service.execute(
            "paired",
            "generation",
            2,
            other,
            "digest",
            OperatorOperation::Renew
        ),
        OperatorAnswer::Table { terminals: vec![] }
    );
    let rows = fixture.service.execute(
        "paired",
        "successor",
        3,
        namespace(),
        "digest",
        OperatorOperation::Renew,
    );
    refused(rows, OperatorRefusal::TerminalBusy);
    refused(
        fixture.service.execute(
            "paired",
            "successor",
            3,
            namespace(),
            "digest",
            OperatorOperation::Open {
                key: key(0),
                workspace: "/workspace".into(),
                dimensions: dimensions(),
            },
        ),
        OperatorRefusal::TerminalBusy,
    );
    assert!(fixture.service.workspace_pinned(Path::new("/workspace")));
    let restarted = Fixture::new();
    refused(
        restarted.call(
            1,
            OperatorOperation::Attach {
                key: key(0),
                cursor: TerminalCursor {
                    instance: id("old-instance"),
                    offset: 0,
                },
            },
        ),
        OperatorRefusal::TerminalLost,
    );
    assert_eq!(lock(&restarted.fake).spawned, 0);
}

#[test]
fn operator_expired_generation_renew_cannot_close_its_successors_terminals() {
    let fixture = Fixture::new();
    fixture.open(0);
    lock(&fixture.fake).proof = true;
    let renewed = fixture.service.execute(
        "paired",
        "successor",
        2,
        namespace(),
        "renew",
        OperatorOperation::Renew,
    );
    refused(renewed, OperatorRefusal::TerminalBusy);
    fixture.service.tick();
    assert!(matches!(
        fixture.service.execute(
            "paired",
            "successor",
            2,
            namespace(),
            "renew",
            OperatorOperation::Renew
        ),
        OperatorAnswer::Table { .. }
    ));
    let opened = fixture.service.execute(
        "paired",
        "successor",
        2,
        namespace(),
        "open",
        OperatorOperation::Open {
            key: key(0),
            workspace: "/workspace".into(),
            dimensions: dimensions(),
        },
    );
    assert!(
        matches!(opened,OperatorAnswer::Terminal {terminal,..} if terminal.status==TerminalStatus::Running)
    );
    refused(
        fixture.call(1, OperatorOperation::Renew),
        OperatorRefusal::NamespaceExpired,
    );
    refused(
        fixture.call(
            1,
            OperatorOperation::Close {
                key: key(0),
                generation: Some(id("successor")),
                acknowledge_loss: true,
            },
        ),
        OperatorRefusal::NamespaceExpired,
    );
    fixture.service.tick();
    assert_eq!(
        lock(&fixture.fake).closed,
        1,
        "a stale heartbeat must not signal the new generation"
    );
    let table = fixture.service.execute(
        "paired",
        "successor",
        2,
        namespace(),
        "list",
        OperatorOperation::List,
    );
    assert!(
        matches!(table,OperatorAnswer::Table {terminals} if terminals.len()==1 && terminals[0].status==TerminalStatus::Running)
    );
}

#[test]
fn operator_successor_close_is_cleanup_only_and_principal_scoped() {
    let fixture = Fixture::new();
    fixture.open(0);
    refused(
        fixture.service.execute(
            "paired",
            "successor",
            2,
            namespace(),
            "renew",
            OperatorOperation::Renew,
        ),
        OperatorRefusal::TerminalBusy,
    );
    let close = OperatorOperation::Close {
        key: key(0),
        generation: Some(id("generation")),
        acknowledge_loss: false,
    };
    refused(
        fixture.service.execute(
            "paired",
            "successor",
            2,
            OperatorNamespace {
                principal: id("other"),
                ..namespace()
            },
            "close",
            close.clone(),
        ),
        OperatorRefusal::TerminalLost,
    );
    assert_eq!(lock(&fixture.fake).closed, 1);
    let answer = fixture
        .service
        .execute("paired", "successor", 2, namespace(), "close", close);
    assert!(matches!(answer, OperatorAnswer::Terminal { terminal, .. }
        if terminal.status == TerminalStatus::TerminationUnknown));
    assert_eq!(lock(&fixture.fake).closed, 1);
    assert_eq!(lock(&fixture.fake).spawned, 1);
    assert!(fixture.service.workspace_pinned(Path::new("/workspace")));
    lock(&fixture.fake).proof = true;
    fixture.service.tick();
    assert!(!fixture.service.workspace_pinned(Path::new("/workspace")));
    assert!(matches!(fixture.service.execute(
        "paired", "successor", 2, namespace(), "renew", OperatorOperation::Renew,
    ), OperatorAnswer::Table { terminals } if terminals.is_empty()));
}

#[test]
fn operator_namespace_quota_refusal_has_no_cross_generation_effect() {
    let fixture = Fixture::new();
    fixture.open(0);
    for n in 1..128 {
        let namespace = OperatorNamespace {
            state_key: id(&format!("state-{n}")),
            ..namespace()
        };
        let answer = fixture.service.execute(
            "paired",
            "generation",
            1,
            namespace,
            "renew",
            OperatorOperation::Renew,
        );
        assert!(matches!(answer, OperatorAnswer::Table { .. }));
    }
    refused(
        fixture.service.execute(
            "paired",
            "successor",
            2,
            namespace(),
            "renew",
            OperatorOperation::Renew,
        ),
        OperatorRefusal::Quota,
    );
    assert_eq!(
        lock(&fixture.fake).closed,
        0,
        "a refused reservation must not close another namespace"
    );
    assert!(fixture.service.workspace_pinned(Path::new("/workspace")));
}

#[test]
fn operator_ring_is_raw_bounded_and_reports_exact_cursor() {
    let mut output = Output::default();
    output.push(&[0, 255, 1]);
    assert_eq!(
        output.bytes.iter().copied().collect::<Vec<_>>(),
        [0, 255, 1]
    );
    output.push(&vec![7; RING_BYTES + 3]);
    assert_eq!(output.bytes.len(), RING_BYTES);
    assert_eq!(output.oldest(), 6);
    assert_eq!(output.next, RING_BYTES as u64 + 6);
}

#[test]
fn operator_reclamation_and_open_are_mutually_exclusive_in_both_orders() {
    let fixture = Fixture::new();
    let admission = fixture.service.reclaim(Path::new("/workspace")).unwrap();
    refused(
        fixture.call(
            1,
            OperatorOperation::Open {
                key: key(0),
                workspace: "/workspace/child".into(),
                dimensions: dimensions(),
            },
        ),
        OperatorRefusal::TerminalBusy,
    );
    drop(admission);
    fixture.open(1);
    assert!(
        fixture
            .service
            .reclaim(Path::new("/workspace/child"))
            .is_err()
    );
    refused(
        fixture
            .service
            .replay_open("paired", "generation", namespace(), "digest", &key(2)),
        OperatorRefusal::TerminalLost,
    );
    assert_eq!(lock(&fixture.fake).spawned, 1);
}

#[test]
fn operator_workspace_pin_cannot_be_bypassed_by_a_symlink_alias() {
    let fixture = Fixture::new();
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    let answer = fixture.call(
        1,
        OperatorOperation::Open {
            key: key(0),
            workspace: workspace.to_string_lossy().into_owned(),
            dimensions: dimensions(),
        },
    );
    assert!(matches!(answer, OperatorAnswer::Terminal { .. }));
    assert!(fixture.service.workspace_pinned(&alias));
    assert!(fixture.service.reclaim(&alias).is_err());
}

#[test]
fn operator_four_streams_share_decoded_output_budget_without_starvation() {
    let fixture = Fixture::new();
    let mut streams = Vec::new();
    for n in 0..4 {
        let key = TerminalKey {
            session: id(&format!("session-{n}")),
            ..key(n)
        };
        let OperatorAnswer::Terminal { terminal, .. } = fixture.call(
            1,
            OperatorOperation::Open {
                key: key.clone(),
                workspace: "/workspace".into(),
                dimensions: dimensions(),
            },
        ) else {
            panic!("open failed");
        };
        let OperatorAnswer::Terminal {
            stream: Some(stream),
            ..
        } = fixture.call(
            1,
            OperatorOperation::Attach {
                key,
                cursor: terminal.oldest,
            },
        )
        else {
            panic!("attach failed");
        };
        streams.push(stream);
        fixture
            .service
            .receive(
                "paired",
                "generation",
                1,
                StreamFrame {
                    stream,
                    seq: 0,
                    event: StreamEvent::Credit {
                        bytes: WINDOW_BYTES,
                    },
                },
            )
            .unwrap();
    }
    for output in &lock(&fixture.fake).outputs {
        lock(output).push(&vec![255; 64 * 1024]);
    }
    let frames = fixture.service.output_round("paired", "generation", 1, 64);
    assert_eq!(frames.len(), 4);
    let mut bytes = 0;
    for frame in frames {
        assert!(streams.contains(&frame.stream));
        let StreamEvent::Data { payload } = frame.event else {
            panic!("expected data");
        };
        let TerminalEvent::Data { bytes_b64, .. } = serde_json::from_value(payload).unwrap() else {
            panic!("expected terminal data");
        };
        bytes += STANDARD.decode(bytes_b64).unwrap().len();
    }
    assert_eq!(bytes, 32 * 1024);
    assert!(
        fixture
            .service
            .output_round("paired", "generation", 1, 64)
            .is_empty(),
        "caller cannot bypass the awake rate budget"
    );
    fixture.advance(1);
    assert_eq!(
        fixture
            .service
            .output_round("paired", "generation", 1, 64)
            .len(),
        4
    );
}

#[test]
fn operator_input_bounds_refuse_before_acceptance_and_never_refresh_idle() {
    let fixture = Fixture::new();
    fixture.open(0);
    let stream = fixture.attach(0, 1);
    for (seq, text) in [(0, "%%%"), (1, "AB=="), (2, "YQ")] {
        let frames=fixture.service.receive("paired","generation",1,StreamFrame {stream,seq,
            event:StreamEvent::Data {payload:serde_json::json!({"terminal_event":"data","offset":0,"bytes_b64":text})}}).unwrap();
        assert!(
            matches!(&frames[0].event,StreamEvent::Data {payload} if payload["refusal"]=="invalid_request")
        );
    }
    assert_eq!(lock(&fixture.fake).input, Vec::<Vec<u8>>::new());
    let first = fixture.input(stream, 3, 0, &vec![0; CHUNK_BYTES]);
    assert!(matches!(&first[0].event,StreamEvent::Data {payload} if payload["next"]==CHUNK_BYTES));
    for n in 1..4 {
        fixture.input(stream, n + 3, n * CHUNK_BYTES as u64, &vec![0; CHUNK_BYTES]);
    }
    let full = fixture.input(stream, 7, WINDOW_BYTES, b"extra");
    assert!(
        matches!(&full[0].event,StreamEvent::Data {payload} if payload["refusal"]=="backpressure")
    );
    assert_eq!(lock(&fixture.fake).input.len(), 4);
}
