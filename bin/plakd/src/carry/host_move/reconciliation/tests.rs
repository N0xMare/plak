use super::*;
use plakous::runner::protocol::{Bind, HostFrame, Refusal, RunnerFrame, VersionTriple, frame};
use plakous::runner::{
    ClientWindows, ConfiguredRunner, RunnerClient, RunnerIdentity, RunnerService, RunnerServices,
    WorkspaceDiscovery, WorkspaceOptions,
};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test]
async fn disposed_witness_cleanup_attempts_later_groups_and_runners_after_connected_refusal() {
    let home = tempfile::tempdir().unwrap();
    let services = Services::for_home(crate::home::paths::PlakPaths::explicit(
        home.path().to_path_buf(),
    ));
    let (first, first_calls, first_peer) =
        refusing_runner(&services, "first-runner", "first-line").await;
    let (second, second_calls, second_peer) =
        refusing_runner(&services, "second-runner", "second-line").await;
    let group = |id: &str, phase: &str, line: &str, runners: &[&str]| {
        serde_json::json!({
            "id":id,"operation":vec![1;32],"trees":[],"products":[],"sessions":[],
            "lines":[line],"runners":runners,"source_digest":null,"source_retired":true,
            "target_digest":null,"phase":phase,"abandon_error":null,
            "abandon_retry_after":null,"transfers":{}
        })
    };
    let mut receipt: Receipt = serde_json::from_value(serde_json::json!({
        "version":1,"id":uuid::Uuid::new_v4(),"sequence":1,"source_key":"source",
        "source_instance":"instance","source_binary":"source","source_executable":"/source/plakd",
        "host_argv":["host"],"destination_key":"destination","destination_binary":"target",
        "destination":"/target/plakd","phase":"acknowledged","source_archival_pending":false,
        "mappings":[],"groups":[
            group("first-group", "ready", "first-line", &["first-runner"]),
            group("second-group", "abandoned", "second-line", &["first-runner", "second-runner"])
        ]
    }))
    .unwrap();
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        release_disposed(&services, &mut receipt),
    )
    .await
    .expect("connected refusals must not wait for a lost reply")
    .unwrap_err();
    assert_eq!(
        *first_calls.lock().unwrap(),
        [
            vec!["first-line".to_owned()],
            vec!["second-line".to_owned()]
        ]
    );
    assert_eq!(
        *second_calls.lock().unwrap(),
        [vec!["second-line".to_owned()]]
    );
    for name in [
        "first-group",
        "second-group",
        "first-runner",
        "second-runner",
    ] {
        assert!(
            error.message().contains(name),
            "missing aggregate failure for {name}: {error:?}"
        );
    }
    assert!(first.is_connected() && second.is_connected());
    for peer in [first_peer, second_peer] {
        peer.abort();
        assert!(peer.await.unwrap_err().is_cancelled());
    }
}

type WitnessCalls = Arc<Mutex<Vec<Vec<String>>>>;

async fn refusing_runner(
    services: &Services,
    id: &str,
    refused_line: &str,
) -> (RunnerClient, WitnessCalls, tokio::task::JoinHandle<()>) {
    let version = VersionTriple::current("review", "test");
    let environment = plakous::tools::ExecutionEnvironment::host(
        plakous::tools::ExecutionEnvironmentScope::Run(
            "00000000-0000-4000-8000-000000000001".parse().unwrap(),
        ),
        services.paths().unwrap().home(),
        ".",
        plakous::tools::ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    let service = RunnerService::new(
        environment,
        RunnerIdentity {
            runner_id: plakous::tools::RunnerId::new(id).unwrap(),
            version: version.clone(),
            boot_id: "boot".into(),
            binary_id: None,
        },
    )
    .unwrap();
    let hello = service.hello().clone();
    let (host, runner) = tokio::io::duplex(65536);
    let calls = WitnessCalls::default();
    let peer = tokio::spawn(witness_peer(
        runner,
        hello,
        refused_line.to_owned(),
        calls.clone(),
    ));
    let (client, hello) = RunnerClient::connect(
        host,
        Bind {
            host_id: "review".into(),
            host_instance: "instance".into(),
            version,
            inspect: false,
        },
        ClientWindows::default(),
    )
    .await
    .unwrap();
    let backend =
        plakous::tools::runner::RunnerBackend::new(hello.descriptor, Arc::new(client.clone()))
            .unwrap();
    services.runners().configure(ConfiguredRunner {
        runner_id: id.into(),
        names: vec![id.into()],
    });
    services
        .runners()
        .ready(
            id,
            client.connection_revision(),
            RunnerServices::new(Arc::new(backend), tokio::runtime::Handle::current()),
            WorkspaceOptions {
                roots: None,
                default_workspace: None,
                copies: false,
            },
            WorkspaceDiscovery::default(),
            1,
        )
        .unwrap();
    (client, calls, peer)
}

async fn witness_peer(
    mut runner: tokio::io::DuplexStream,
    hello: plakous::runner::protocol::Hello,
    refused_line: String,
    calls: WitnessCalls,
) {
    runner
        .write_all(&frame::encode(&RunnerFrame::Hello(hello)).unwrap())
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
            let answer = match frame::decode::<HostFrame>(&body).unwrap() {
                HostFrame::Bind(_) => continue,
                HostFrame::Ping { nonce } => RunnerFrame::Pong { nonce },
                HostFrame::ForgetOwnerWitnesses { request_id, lines } => {
                    let refused = lines.contains(&refused_line);
                    calls.lock().unwrap().push(lines);
                    if refused {
                        RunnerFrame::Refused {
                            request_id: Some(request_id),
                            refusal: Refusal::OwnerNotTransferred,
                        }
                    } else {
                        RunnerFrame::OwnerWitnessesForgotten { request_id }
                    }
                }
                other => panic!("unexpected witness peer request: {other:?}"),
            };
            runner
                .write_all(&frame::encode(&answer).unwrap())
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn disposed_witness_cleanup_attempts_every_group_and_runner_after_missing_peers() {
    let home = tempfile::tempdir().unwrap();
    let services = Services::for_home(crate::home::paths::PlakPaths::explicit(
        home.path().to_path_buf(),
    ));
    let group = |id: &str, phase: &str, runners: &[&str]| {
        serde_json::json!({
            "id":id,"operation":vec![1;32],"trees":[],"products":[],"sessions":[],
            "lines":["line"],"runners":runners,"source_digest":null,"source_retired":true,
            "target_digest":null,"phase":phase,"abandon_error":null,
            "abandon_retry_after":null,"transfers":{}
        })
    };
    let mut receipt: Receipt = serde_json::from_value(serde_json::json!({
        "version":1,"id":uuid::Uuid::new_v4(),"sequence":1,"source_key":"source",
        "source_instance":"instance","source_binary":"source","source_executable":"/source/plakd",
        "host_argv":["host"],"destination_key":"destination","destination_binary":"target",
        "destination":"/target/plakd","phase":"acknowledged","source_archival_pending":false,
        "mappings":[],"groups":[group("first","ready", &["missing-a","missing-b"]),
            group("second","abandoned", &["missing-c"]),
            group("pending","published", &["not-disposed"])]
    }))
    .unwrap();
    let error = release_disposed(&services, &mut receipt).await.unwrap_err();
    for name in ["first", "second", "missing-a", "missing-b", "missing-c"] {
        assert!(
            error.message().contains(name),
            "missing aggregate failure for {name}: {error:?}"
        );
    }
    assert!(!error.message().contains("not-disposed"));
}

fn owner(generation: u64, fence: u64) -> WireOwner {
    WireOwner {
        line: "line".into(),
        branch: "branch".into(),
        generation,
        fence,
    }
}

#[test]
fn move_reconciliation_never_leapfrogs_a_foreign_or_lower_witness() {
    let source = owner(2, 9);
    let intended = OwnerTransfer {
        previous: Vec::new(),
        from: Some(source.clone()),
        to: owner(2, 10),
    };
    assert!(allowed(Some(&source), Some(&source), None));
    assert!(allowed(Some(&intended.to), Some(&source), Some(&intended)));
    assert!(!allowed(Some(&owner(2, 8)), Some(&source), Some(&intended)));
    assert!(!allowed(
        Some(&owner(2, 11)),
        Some(&source),
        Some(&intended)
    ));
    assert!(!allowed(Some(&owner(3, 1)), Some(&source), Some(&intended)));
    assert!(!allowed(None, Some(&source), Some(&intended)));
    assert!(allowed(None, None, None));
    let next = OwnerTransfer {
        previous: Vec::new(),
        from: Some(intended.to.clone()),
        to: owner(2, 12),
    };
    assert!(allowed(Some(&intended.to), Some(&source), Some(&next)));
    assert!(allowed(Some(&next.to), Some(&source), Some(&next)));
    assert!(!allowed(Some(&owner(2, 11)), Some(&source), Some(&next)));
    let pending = OwnerTransfer {
        from: Some(source.clone()),
        to: owner(2, 12),
        previous: vec![owner(2, 10)],
    };
    assert!(
        allowed(Some(&owner(2, 10)), Some(&source), Some(&pending)),
        "a delayed own CAS is not a foreign witness"
    );
    assert!(!allowed(Some(&owner(2, 11)), Some(&source), Some(&pending)));
}

#[test]
fn move_reconciliation_checks_every_original_before_taking_a_higher_lease() {
    use crate::carry::host_move::JobCut;
    let source = owner(2, 9);
    let intended = OwnerTransfer {
        previous: Vec::new(),
        from: Some(source.clone()),
        to: owner(2, 10),
    };
    let floor = |native, retained| plakous::runner::protocol::OwnerFloor {
        line: "line".into(),
        owner: native,
        retained,
    };
    assert_eq!(
        witness_from(
            floor(None, vec![source.clone()]),
            Some(&source),
            None,
            JobCut::Retained
        )
        .unwrap(),
        Some(source.clone())
    );
    assert!(
        witness_from(
            floor(None, vec![source.clone()]),
            Some(&source),
            None,
            JobCut::Settled
        )
        .is_err()
    );
    assert_eq!(
        witness_from(
            floor(None, vec![source.clone(), intended.to.clone()]),
            Some(&source),
            Some(&intended),
            JobCut::Retained
        )
        .unwrap(),
        Some(intended.to.clone())
    );
    for foreign in [owner(2, 8), owner(2, 11), owner(3, 1)] {
        assert!(
            witness_from(
                floor(Some(source.clone()), vec![foreign.clone()]),
                Some(&source),
                Some(&intended),
                JobCut::Retained
            )
            .is_err()
        );
        assert!(
            witness_from(
                floor(Some(foreign), vec![source.clone()]),
                Some(&source),
                Some(&intended),
                JobCut::Retained
            )
            .is_err()
        );
    }
    assert!(witness_from(floor(None, vec![source]), None, None, JobCut::Retained).is_err());
    assert_eq!(
        witness_from(floor(None, vec![]), None, None, JobCut::Settled).unwrap(),
        None
    );
}
