use super::*;
use crate::service::lock;
use crate::{FileOwnerFloors, OwnerFloors, RunnerIdentity, RunnerOptions, WorkspaceCatalog};
use plakous_runner_protocol::{Bind, LineOwner, Operation, StreamFrame, TransferOwner, frame};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt as _, DuplexStream, ReadHalf, WriteHalf};

struct Fixture {
    root: tempfile::TempDir,
    service: RunnerService,
    floors: Arc<FileOwnerFloors>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(workspace.join("marker"), "unchanged").unwrap();
        let floors = Arc::new(FileOwnerFloors::open(home.join("floors.json")).unwrap());
        let catalog =
            Arc::new(WorkspaceCatalog::machine(vec![workspace.clone()], &home, false).unwrap());
        let probe_root = workspace.clone();
        let service = RunnerService::with_options(
            ExecutionEnvironment::host(
                ExecutionEnvironmentScope::Run(
                    "00000000-0000-4000-8000-000000000001".parse().unwrap(),
                ),
                &workspace,
                ".",
                ExecutionEnvironmentConfig::default(),
            )
            .unwrap(),
            RunnerIdentity {
                runner_id: RunnerId::new("qualification-fixture").unwrap(),
                version: VersionTriple::current("review", "test"),
                boot_id: "boot".into(),
                binary_id: None,
            },
            RunnerOptions {
                hosts: Some(Arc::new(|host| host == "paired")),
                probe_paths: Some(Arc::new(move |path| path.starts_with(&probe_root))),
                workspace_catalog: Some(catalog),
                floors: Some(floors.clone()),
                ..RunnerOptions::default()
            },
        )
        .unwrap();
        Self {
            root,
            service,
            floors,
        }
    }

    fn first(&self, host: &str) -> HostFrame {
        HostFrame::Qualify {
            host_id: host.into(),
            version: self.service.hello().version.clone(),
        }
    }

    fn request(&self, verb: RunnerVerb) -> Request {
        let operation = Operation {
            verb: verb.as_str().into(),
            payload: json!({"op":"canonicalize", "path":self.root.path().join("workspace")}),
        };
        Request {
            request_id: "read".into(),
            class: RequestClass::BuildRead,
            owner: None,
            deadline_ms: Some(1000),
            digest: crate::client::digest(&operation),
            replay: false,
            age_ms: None,
            operation,
        }
    }

    fn untouched(&self) {
        let inner = &self.service.inner;
        assert!(lock(&inner.hosts).is_empty());
        assert!(lock(&inner.leases).is_empty());
        assert!(lock(&inner.registered).is_empty());
        assert!(lock(&inner.running).is_empty());
        assert!(lock(&inner.lines).is_empty());
        assert!(lock(&inner.services).is_empty());
        assert_eq!(self.floors.load(), Vec::new());
        assert_eq!(
            std::fs::read_dir(self.root.path().join("home"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            std::fs::read(self.root.path().join("workspace/marker")).unwrap(),
            b"unchanged"
        );
    }
}

struct Peer {
    reader: FrameStream<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    task: tokio::task::JoinHandle<()>,
}

impl Peer {
    async fn connect(fixture: &Fixture, first: HostFrame) -> Self {
        let (host, runner) = tokio::io::duplex(64 * 1024);
        let service = fixture.service.clone();
        let task = tokio::spawn(async move {
            service.serve_connection(runner).await;
        });
        let (reader, writer) = tokio::io::split(host);
        let mut peer = Self {
            reader: FrameStream::new(reader),
            writer,
            task,
        };
        assert!(matches!(peer.next().await, RunnerFrame::Hello(_)));
        peer.send(first).await;
        peer
    }

    async fn send(&mut self, frame: HostFrame) {
        self.writer
            .write_all(&frame::encode(&frame).unwrap())
            .await
            .unwrap();
    }

    async fn next(&mut self) -> RunnerFrame {
        tokio::time::timeout(Duration::from_secs(3), self.reader.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    async fn close(mut self) {
        self.writer.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), self.task)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn paired_qualification_reads_under_root_admission_without_persisting_any_authority() {
    let fixture = Fixture::new();
    let mut peer = Peer::connect(&fixture, fixture.first("paired")).await;
    peer.send(HostFrame::Request(
        fixture.request(RunnerVerb::ProbeWorkspace),
    ))
    .await;
    assert!(
        matches!(peer.next().await, RunnerFrame::Result { payload, .. } if payload["path"] == fixture.root.path().join("workspace").to_str().unwrap())
    );
    let mut outside = fixture.request(RunnerVerb::ProbeWorkspace);
    outside.operation.payload["path"] = json!(fixture.root.path().join("home"));
    outside.digest = crate::client::digest(&outside.operation);
    peer.send(HostFrame::Request(outside)).await;
    assert!(matches!(peer.next().await, RunnerFrame::Error { .. }));
    peer.send(HostFrame::Ping { nonce: 7 }).await;
    assert_eq!(peer.next().await, RunnerFrame::Pong { nonce: 7 });
    fixture.untouched();
    peer.close().await;
    fixture.untouched();
}

#[tokio::test]
async fn qualification_requires_pairing_and_the_exact_revision_flavor_and_protocol() {
    let fixture = Fixture::new();
    let mut peer = Peer::connect(&fixture, fixture.first("unpaired")).await;
    assert!(matches!(
        peer.next().await,
        RunnerFrame::Refused {
            refusal: Refusal::NotPaired { .. },
            ..
        }
    ));
    peer.close().await;
    for part in ["protocol", "revision", "flavor"] {
        let mut version = fixture.service.hello().version.clone();
        match part {
            "protocol" => version.protocol += 1,
            "revision" => version.source_revision.push_str("-other"),
            _ => version.flavor.push_str("-other"),
        }
        let mut peer = Peer::connect(
            &fixture,
            HostFrame::Qualify {
                host_id: "paired".into(),
                version,
            },
        )
        .await;
        assert!(matches!(
            peer.next().await,
            RunnerFrame::Refused {
                refusal: Refusal::VersionMismatch { .. },
                ..
            }
        ));
        peer.close().await;
    }
    fixture.untouched();
}

#[tokio::test]
async fn qualification_client_uses_only_workspace_reads_and_never_registers_a_host() {
    let fixture = Fixture::new();
    let (host, runner) = tokio::io::duplex(64 * 1024);
    let service = fixture.service.clone();
    let task = tokio::spawn(async move {
        service.serve_connection(runner).await;
    });
    let (client, _) = crate::RunnerClient::connect_qualification(
        host,
        "paired".into(),
        fixture.service.hello().version.clone(),
        crate::ClientWindows::default(),
    )
    .await
    .unwrap();
    let options = client.workspace_options().await.unwrap();
    assert_eq!(
        options.roots,
        Some(vec![
            fixture
                .root
                .path()
                .join("workspace")
                .to_str()
                .unwrap()
                .to_owned()
        ])
    );
    assert!(client.lines().await.is_err());
    let workspace = fixture.root.path().join("workspace");
    assert!(
        client
            .workspace_validate(workspace.to_str().unwrap(), true)
            .await
            .unwrap()
            .is_ok()
    );
    assert!(client.is_connected());
    fixture.untouched();
    client.disconnect();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    fixture.untouched();
}

#[tokio::test]
async fn qualification_refuses_every_non_build_request_class_without_recording_it() {
    let fixture = Fixture::new();
    for class in [
        RequestClass::OperatorControl {
            namespace: plakous_runner_protocol::operator::OperatorNamespace {
                state_key: "state".to_owned().try_into().unwrap(),
                principal: "principal".to_owned().try_into().unwrap(),
            },
        },
        RequestClass::EffectCall {
            call_id: "call".into(),
            attempt_id: "attempt".into(),
        },
        RequestClass::ProcessObservation { job: 1, cursor: 0 },
        RequestClass::OwnerControl {
            control_id: "control".into(),
        },
        RequestClass::TreeOperation {
            tree_id: "tree".into(),
            operation_id: "operation".into(),
        },
    ] {
        let mut peer = Peer::connect(&fixture, fixture.first("paired")).await;
        let mut request = fixture.request(RunnerVerb::ProbeWorkspace);
        request.class = class;
        peer.send(HostFrame::Request(request)).await;
        assert!(matches!(
            peer.next().await,
            RunnerFrame::Refused {
                refusal: Refusal::InspectOnly,
                ..
            }
        ));
        peer.close().await;
        fixture.untouched();
    }
}

#[tokio::test]
async fn qualification_refuses_disallowed_verbs_and_malformed_read_envelopes() {
    let fixture = Fixture::new();
    for verb in RunnerVerb::ALL {
        if matches!(
            verb,
            RunnerVerb::WorkspaceOptions
                | RunnerVerb::WorkspaceValidate
                | RunnerVerb::ProbeWorkspace
                | RunnerVerb::ReadWorkspace
                | RunnerVerb::ReadInstructions
        ) {
            continue;
        }
        let mut peer = Peer::connect(&fixture, fixture.first("paired")).await;
        peer.send(HostFrame::Request(fixture.request(verb))).await;
        assert!(matches!(
            peer.next().await,
            RunnerFrame::Refused {
                refusal: Refusal::InspectOnly,
                ..
            }
        ));
        peer.close().await;
    }
    for malformed in ["owner", "replay", "age", "digest"] {
        let mut request = fixture.request(RunnerVerb::ProbeWorkspace);
        match malformed {
            "owner" => {
                request.owner = Some(LineOwner {
                    line: "line".into(),
                    generation: 1,
                    branch: "branch".into(),
                    fence: 1,
                });
            }
            "replay" => request.replay = true,
            "age" => request.age_ms = Some(0),
            _ => request.digest = "wrong".into(),
        }
        let mut peer = Peer::connect(&fixture, fixture.first("paired")).await;
        peer.send(HostFrame::Request(request)).await;
        assert!(matches!(
            peer.next().await,
            RunnerFrame::Refused {
                refusal: Refusal::InspectOnly,
                ..
            }
        ));
        peer.close().await;
    }
    fixture.untouched();
}

#[tokio::test]
async fn qualification_refuses_transfer_and_all_non_read_frames_and_leaves_unpaired_inspection_unchanged()
 {
    let fixture = Fixture::new();
    for frame in [
        HostFrame::TransferOwner(TransferOwner {
            control_id: "control".into(),
            from: None,
            to: LineOwner {
                line: "line".into(),
                generation: 1,
                branch: "branch".into(),
                fence: 1,
            },
        }),
        HostFrame::Ack {
            request_id: "read".into(),
        },
        HostFrame::Lines {
            request_id: "lines".into(),
        },
        HostFrame::Cancel {
            request_id: "read".into(),
        },
        HostFrame::Preempt {
            request_id: "read".into(),
            value: json!(null),
        },
        fixture.first("paired"),
        HostFrame::Bind(Bind {
            host_id: "paired".into(),
            host_instance: "must-not-bind".into(),
            version: fixture.service.hello().version.clone(),
            inspect: false,
        }),
        HostFrame::Stream(StreamFrame {
            stream: 1,
            seq: 0,
            event: plakous_runner_protocol::StreamEvent::Credit { bytes: 1024 },
        }),
    ] {
        let mut peer = Peer::connect(&fixture, fixture.first("paired")).await;
        peer.send(frame).await;
        assert!(matches!(
            peer.next().await,
            RunnerFrame::Refused {
                refusal: Refusal::InspectOnly,
                ..
            }
        ));
        peer.close().await;
    }
    let mut peer = Peer::connect(
        &fixture,
        HostFrame::Bind(Bind {
            host_id: "unpaired".into(),
            host_instance: "inspection".into(),
            version: fixture.service.hello().version.clone(),
            inspect: true,
        }),
    )
    .await;
    peer.send(HostFrame::Lines {
        request_id: "lines".into(),
    })
    .await;
    assert!(matches!(peer.next().await, RunnerFrame::Lines { lines, .. } if lines.is_empty()));
    peer.send(HostFrame::Request(
        fixture.request(RunnerVerb::ProbeWorkspace),
    ))
    .await;
    assert!(matches!(
        peer.next().await,
        RunnerFrame::Refused {
            refusal: Refusal::InspectOnly,
            ..
        }
    ));
    peer.close().await;
    fixture.untouched();
}
