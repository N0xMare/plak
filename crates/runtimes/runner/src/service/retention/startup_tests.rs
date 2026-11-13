//! A replacement may be constructed before the incumbent records its last floor.

#![allow(clippy::unwrap_used, reason = "isolated daemon startup fixtures")]

use super::*;
use crate::{FileOwnerFloors, Floor, OwnerFloors, RunnerIdentity, RunnerOptions};
use plakous_tools::ExecutionEnvironment;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::sync::Arc;

fn private_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    root
}

#[tokio::test]
async fn retention_startup_same_triple_reloads_late_owner_floors_before_serving() {
    late_floor(false).await;
}

#[tokio::test]
async fn retention_startup_home_local_reloads_late_owner_floors_before_serving() {
    late_floor(true).await;
}

fn replacement(
    root: &Path,
    path: &Path,
    version: plakous_runner_protocol::VersionTriple,
    home_local: bool,
) -> (RunnerService, Arc<FileOwnerFloors>) {
    let floors = Arc::new(FileOwnerFloors::open(path).unwrap());
    let environment = ExecutionEnvironment::host(
        plakous_tools::ExecutionEnvironmentScope::Run(
            "00000000-0000-0000-0000-000000000001".parse().unwrap(),
        ),
        root,
        ".",
        plakous_tools::ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    let service = RunnerService::with_options(
        environment,
        RunnerIdentity {
            runner_id: plakous_tools::RunnerId::new("startup-fixture").unwrap(),
            version,
            boot_id: "boot".into(),
            binary_id: None,
        },
        RunnerOptions {
            floors: Some(floors.clone()),
            home_local: home_local.then(|| "home".into()),
            ..RunnerOptions::default()
        },
    )
    .unwrap();
    (service, floors)
}

async fn late_floor(home_local: bool) {
    let root = private_root();
    let version = plakous_runner_protocol::VersionTriple::current("test", "debug");
    let build = root
        .path()
        .join("builds")
        .join(crate::daemon::triple_key(&version));
    let native = if home_local {
        root.path()
            .join("host/runner")
            .join(format!("v{}", version.protocol))
    } else {
        build.clone()
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&build)
        .unwrap();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(native.join("run"))
        .unwrap();
    let path = native.join("owners.json");
    let socket = native.join("run/runner.sock");
    let incumbent_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(socket.with_extension("lock"))
        .unwrap();
    incumbent_lock.lock().unwrap();
    let incumbent = FileOwnerFloors::open(&path).unwrap();
    let owner = LineOwner {
        line: "line".into(),
        branch: "branch".into(),
        generation: 1,
        fence: 1,
    };
    incumbent
        .record(
            "home",
            &Floor {
                owner: owner.clone(),
                holder: "source".into(),
                retention_only: false,
            },
        )
        .unwrap();
    let (service, floors) = replacement(root.path(), &path, version, home_local);
    let latest = Floor {
        owner: LineOwner { fence: 3, ..owner },
        holder: "destination".into(),
        retention_only: true,
    };
    let serving = crate::daemon::serve_socket_with_retention(
        service.clone(),
        &socket,
        &build,
        Duration::from_secs(300),
    );
    tokio::pin!(serving);
    tokio::select! {
        biased;
        result = &mut serving => panic!("replacement must wait for its incumbent: {result:?}"),
        () = tokio::task::yield_now() => {}
    }
    assert!(!socket.exists());
    // The incumbent's final CAS is later than both replacement caches and
    // the replacement's first attempt to acquire the native lock.
    incumbent.record("home", &latest).unwrap();
    incumbent_lock.unlock().unwrap();
    tokio::select! {
        result = &mut serving => panic!("startup ended before binding: {result:?}"),
        () = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !socket.exists() { tokio::task::yield_now().await; }
            }).await.unwrap();
        } => {}
    }
    assert_reloaded(&service, &floors, &path, &latest);
    std::fs::remove_file(&socket).unwrap();
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .unwrap()
        .unwrap();
}

fn assert_reloaded(service: &RunnerService, floors: &FileOwnerFloors, path: &Path, latest: &Floor) {
    assert_eq!(floors.load(), [("home".into(), latest.clone())]);
    {
        let mut tables = lock(&service.inner.tables);
        assert_eq!(
            tables.owner("home", "line"),
            Some((latest.owner.clone(), false))
        );
        assert!(matches!(
            tables.transfer(
                "home",
                "destination",
                "native".into(),
                None,
                latest.owner.clone()
            ),
            RunnerFrame::TransferRefused { .. }
        ));
        assert!(matches!(
            tables.transfer_retained(
                "home",
                "foreign",
                "stale".into(),
                &LineOwner {
                    fence: 1,
                    ..latest.owner.clone()
                },
                LineOwner {
                    fence: 2,
                    ..latest.owner.clone()
                },
            ),
            RunnerFrame::StaleOwner { .. }
        ));
    }
    assert_eq!(
        FileOwnerFloors::open(path).unwrap().load(),
        [("home".into(), latest.clone())]
    );
}

#[tokio::test]
async fn retention_startup_unreadable_late_floors_refuse_both_endpoints() {
    for missing in [false, true] {
        let root = private_root();
        let path = root.path().join("owners.json");
        let incumbent = FileOwnerFloors::open(&path).unwrap();
        let floor = Floor {
            owner: LineOwner {
                line: "line".into(),
                branch: "branch".into(),
                generation: 1,
                fence: 3,
            },
            holder: "source".into(),
            retention_only: true,
        };
        incumbent.record("home", &floor).unwrap();
        let (service, floors) = replacement(
            root.path(),
            &path,
            plakous_runner_protocol::VersionTriple::current("test", "debug"),
            false,
        );
        if missing {
            std::fs::remove_file(&path).unwrap();
        } else {
            std::fs::write(&path, b"not an owner floor document").unwrap();
        }
        let socket = root.path().join("run/runner.sock");
        let build = root
            .path()
            .join("builds")
            .join(crate::daemon::triple_key(&service.hello().version));
        assert!(matches!(
            crate::daemon::serve_socket_with_retention(service, &socket, &build, Duration::ZERO)
                .await,
            Err(crate::daemon::DaemonError::Io(_))
        ));
        assert!(!socket.exists());
        assert!(!build.join("retention-v1.sock").exists());
        assert_eq!(floors.load(), [("home".into(), floor)]);
    }
}
