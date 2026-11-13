use super::*;
use crate::{FileOwnerFloors, Floor, OwnerFloors, RunnerIdentity, RunnerOptions};
use plakous_runner_protocol::LineOwner;
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};
use std::os::unix::fs::DirBuilderExt as _;
use std::sync::Arc;
mod review;

fn fixture() -> (
    tempfile::TempDir,
    PathBuf,
    RunnerService,
    Arc<FileOwnerFloors>,
) {
    let root = tempfile::tempdir().unwrap();
    let version = plakous_runner_protocol::VersionTriple::current("retention-fixture", "test");
    let directory = root
        .path()
        .join("r/b")
        .join(crate::daemon::triple_key(&version));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .unwrap();
    let floors = Arc::new(FileOwnerFloors::open(directory.join("owners.json")).unwrap());
    floors
        .record(
            "home",
            &Floor {
                owner: LineOwner {
                    line: "line".into(),
                    generation: 1,
                    branch: "branch".into(),
                    fence: 1,
                },
                holder: "before".into(),
                retention_only: false,
            },
        )
        .unwrap();
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap()),
        root.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    let service = RunnerService::with_options(
        environment,
        RunnerIdentity {
            runner_id: RunnerId::new("fixture").unwrap(),
            version,
            boot_id: "boot".into(),
            binary_id: None,
        },
        RunnerOptions {
            floors: Some(floors.clone()),
            hosts: Some(Arc::new(|host| host == "home")),
            ..RunnerOptions::default()
        },
    )
    .unwrap();
    (root, directory, service, floors)
}

fn identity(text: &str) -> wire::Id {
    wire::Id::try_from(text.to_owned()).unwrap()
}

async fn connect(
    directory: &Path,
    service: RunnerService,
) -> (UnixStream, Hello, tokio::task::JoinHandle<io::Result<()>>) {
    let endpoint = Endpoint::bind(directory, &service).await.unwrap();
    let path = endpoint.path.clone();
    let serving = tokio::spawn(async move {
        let (stream, _) = endpoint.listener.accept().await?;
        serve(service, stream, endpoint.origin.clone()).await
    });
    let mut stream = UnixStream::connect(path).await.unwrap();
    assert_eq!(
        stream.peer_cred().unwrap().uid(),
        rustix::process::geteuid().as_raw()
    );
    let hello = receive(&mut stream).await.unwrap();
    (stream, hello, serving)
}

#[tokio::test]
async fn retention_socket_inventory_never_adopts_and_owner_cas_persists_before_reply() {
    let (_root, directory, service, floors) = fixture();
    let (mut stream, hello, serving) = connect(&directory, service).await;
    send(
        &mut stream,
        &Bind {
            version: 1,
            host_id: identity("home"),
            host_instance: identity("after"),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        receive::<_, Answer>(&mut stream).await.unwrap(),
        Answer::Ok {}
    );
    let call = Call {
        id: identity("inspect"),
        instance: hello.origin.instance.clone(),
        mutation: None,
        request: wire::Request::Lines { after: 0 },
    };
    send(&mut stream, &call).await.unwrap();
    let Reply {
        answer: Answer::Lines { lines, .. },
        ..
    } = receive(&mut stream).await.unwrap()
    else {
        panic!("inventory")
    };
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].jobs, []);
    assert!(!floors.load()[0].1.retention_only);
    let from = lines[0].owner.clone();
    let to = wire::Owner {
        fence: 2,
        ..from.clone()
    };
    let transfer = Call {
        id: identity("transfer"),
        request: wire::Request::Transfer {
            from: from.clone(),
            to: to.clone(),
        },
        ..call.clone()
    };
    send(&mut stream, &transfer).await.unwrap();
    assert_eq!(
        receive::<_, Reply>(&mut stream).await.unwrap().answer,
        Answer::Transferred {
            next_number: 1,
            has_jobs: false
        }
    );
    let persisted = FileOwnerFloors::open(directory.join("owners.json"))
        .unwrap()
        .load();
    assert!(persisted[0].1.retention_only);
    assert_eq!(persisted[0].1.owner.fence, 2);
    assert_eq!(persisted[0].1.holder, "after");
    // The exact own tuple reconciles a lost transfer reply; it is not a new grant.
    send(&mut stream, &transfer).await.unwrap();
    assert_eq!(
        receive::<_, Reply>(&mut stream).await.unwrap().answer,
        Answer::Transferred {
            next_number: 1,
            has_jobs: false
        }
    );
    drop(stream);
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn retention_socket_refuses_unsupported_version_and_changed_original_instance() {
    let (_root, directory, service, floors) = fixture();
    let (mut stream, _, serving) = connect(&directory, service.clone()).await;
    send(
        &mut stream,
        &Bind {
            version: 0,
            host_id: identity("home"),
            host_instance: identity("after"),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        receive::<_, Answer>(&mut stream).await.unwrap(),
        Answer::Refused {
            reason: wire::Refusal::IncompatibleVersion
        }
    );
    serving.await.unwrap().unwrap();
    let (mut stream, _, serving) = connect(&directory, service).await;
    send(
        &mut stream,
        &Bind {
            version: 1,
            host_id: identity("home"),
            host_instance: identity("after"),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        receive::<_, Answer>(&mut stream).await.unwrap(),
        Answer::Ok {}
    );
    send(
        &mut stream,
        &Call {
            id: identity("old-worker"),
            instance: identity("dead-original-instance"),
            mutation: None,
            request: wire::Request::Lines { after: 0 },
        },
    )
    .await
    .unwrap();
    assert_eq!(
        receive::<_, Reply>(&mut stream).await.unwrap().answer,
        Answer::Refused {
            reason: wire::Refusal::DrainerLost
        }
    );
    assert!(!floors.load()[0].1.retention_only);
    drop(stream);
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn retention_socket_rejects_public_parent_and_symlink_without_unlinking_live_peer() {
    let (_root, directory, service, _) = fixture();
    let endpoint = Endpoint::bind(&directory, &service).await.unwrap();
    let inode = std::fs::symlink_metadata(&endpoint.path).unwrap().ino();
    assert!(Endpoint::bind(&directory, &service).await.is_err());
    assert_eq!(
        std::fs::symlink_metadata(&endpoint.path).unwrap().ino(),
        inode
    );
    drop(endpoint);
    std::fs::set_permissions(
        directory.parent().unwrap(),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(Endpoint::bind(&directory, &service).await.is_err());
    std::fs::set_permissions(
        directory.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let alias = directory.with_file_name("alias");
    std::os::unix::fs::symlink(&directory, &alias).unwrap();
    assert!(Endpoint::bind(&alias, &service).await.is_err());
}

#[tokio::test]
async fn retention_frame_declared_oversize_is_refused_before_reading_any_body() {
    let (mut writer, mut reader) = tokio::io::duplex(16);
    writer
        .write_u32(u32::try_from(wire::MAX_FRAME_BYTES + 1).unwrap())
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(1), receive::<_, Bind>(&mut reader))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}
