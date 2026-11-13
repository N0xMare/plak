use super::*;

#[tokio::test]
async fn retention_review_version_two_discovers_and_speaks_one_refusing_zero_and_three() {
    let (_root, directory, service, _) = fixture();
    let endpoint = Endpoint::bind(&directory, &service).await.unwrap();
    let origin = endpoint.origin.clone();
    let parent = directory.parent().unwrap();
    let own = parent.join("v8-next");
    std::fs::create_dir(&own).unwrap();
    std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o700)).unwrap();
    let registry = registry::Registry::open(&own).unwrap();
    let scope = Bind {
        version: 2,
        host_id: identity("home"),
        host_instance: identity("after"),
    };
    let current = Origin {
        version: 2,
        build: identity("v8-next"),
        instance: identity("next"),
    };
    let serving = tokio::spawn(async move {
        let mut clients = tokio::task::JoinSet::new();
        loop {
            let (stream, _) = endpoint.listener.accept().await.unwrap();
            let service = service.clone();
            let origin = origin.clone();
            clients.spawn(async move {
                serve_current(service, stream, origin, 2).await.unwrap();
            });
        }
    });
    let lines = registry
        .discover_current(&scope, &current, 2)
        .await
        .unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].origin.version, 1);
    for version in [0, 3] {
        let path = directory.join(format!("retention-v{version}.sock"));
        let unsupported = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = registry
            .discover_current(&scope, &current, 2)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("finish/consume/disown"));
        drop(unsupported);
        std::fs::remove_file(path).unwrap();
    }
    serving.abort();
    let _ = serving.await;
}

#[tokio::test]
async fn retention_review_peer_uid_and_world_writable_socket_are_refused() {
    let owner = rustix::process::geteuid().as_raw();
    assert!(check_peer_uid(owner, owner).is_ok());
    assert_eq!(
        check_peer_uid(owner.wrapping_add(1), owner)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let (_root, directory, service, _) = fixture();
    let endpoint = Endpoint::bind(&directory, &service).await.unwrap();
    std::fs::set_permissions(&endpoint.path, std::fs::Permissions::from_mode(0o666)).unwrap();
    let error = Client::inspect(
        directory.parent().unwrap(),
        &endpoint.origin.build,
        &Bind {
            version: 1,
            host_id: identity("home"),
            host_instance: identity("after"),
        },
        1,
        1,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[tokio::test]
async fn retention_review_connect_errors_need_instance_inode_or_unlocked_absence_proof() {
    use std::os::unix::fs::OpenOptionsExt as _;
    let (_root, directory, service, _) = fixture();
    let endpoint = Endpoint::bind(&directory, &service).await.unwrap();
    let origin = endpoint.origin.clone();
    let old = loss::SocketIdentity::observe(directory.parent().unwrap(), &origin).unwrap();
    let native = directory.join("fixture-native.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&native)
        .unwrap();
    lock.try_lock().unwrap();
    link_native_lock(&directory, &native).unwrap();
    let path = endpoint.path.clone();
    assert!(!loss::positive(
        directory.parent().unwrap(),
        &origin,
        Some(old)
    ));
    std::fs::remove_file(&path).unwrap();
    assert!(
        !loss::positive(directory.parent().unwrap(), &origin, Some(old)),
        "missing socket with the native lock held is transient"
    );
    let replacement = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        loss::positive(directory.parent().unwrap(), &origin, Some(old)),
        "a different socket inode proves replacement"
    );
    drop(replacement);
    std::fs::remove_file(&path).unwrap();
    drop(lock);
    assert!(
        loss::positive(directory.parent().unwrap(), &origin, Some(old)),
        "absence plus an unlocked native guard proves loss"
    );
    drop(endpoint);
    let refused = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let unchanged = loss::SocketIdentity::observe(directory.parent().unwrap(), &origin).unwrap();
    drop(refused);
    let error = Client::connect(
        directory.parent().unwrap(),
        &origin,
        &Bind {
            version: 1,
            host_id: identity("home"),
            host_instance: identity("after"),
        },
    )
    .await
    .err()
    .unwrap();
    assert!(!confirmed_loss(&error));
    assert!(
        !loss::positive(directory.parent().unwrap(), &origin, Some(unchanged)),
        "a refused connect with an unchanged inode is not loss"
    );
    assert!(!confirmed_loss(&io::Error::from(
        io::ErrorKind::ConnectionRefused
    )));
    assert!(!confirmed_loss(&io::Error::from(io::ErrorKind::NotFound)));
}
