use super::*;

#[tokio::test]
async fn legacy_shared_lock_requires_a_fresh_answer_from_its_distinct_holder() {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let (root, directory, service, _) = fixture();
    let builds = directory.parent().unwrap();
    let old_socket = root.path().join("legacy.sock");
    let native = old_socket.with_extension("lock");
    let published = directory.join("retention-native.lock");
    let serving = {
        let directory = directory.clone();
        tokio::spawn(async move {
            crate::daemon::serve_socket_with_retention(
                service,
                &old_socket,
                &directory,
                Duration::from_secs(300),
            )
            .await
            .unwrap();
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while !published.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    for build in ["v1-before", "v99-after"] {
        stale_socket(builds, build, &native);
    }
    let receiver = private_build(builds, "v9-next");
    let registry = registry::Registry::open(&receiver).unwrap();
    let scope = Bind {
        version: 1,
        host_id: identity("home"),
        host_instance: identity("after"),
    };
    let current = Origin {
        version: 1,
        build: identity("v9-next"),
        instance: identity("next"),
    };
    let lines = registry.discover(&scope, &current).await.unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].owner.line.as_str(), "line");

    // An independently held lock cannot be explained by the live sibling,
    // even if the diagnostic PID (here in the same test process) matches.
    let other = root.path().join("other.lock");
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&other)
        .unwrap();
    file.try_lock().unwrap();
    write!(file, "{}", std::process::id()).unwrap();
    stale_socket(builds, "v88-independent", &other);
    assert_eq!(
        registry
            .discover(&scope, &current)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::ConnectionRefused
    );
    serving.abort();
    let _ = serving.await;
}

fn private_build(builds: &Path, name: &str) -> PathBuf {
    let path = builds.join(name);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap();
    path
}

fn stale_socket(builds: &Path, name: &str, lock: &Path) {
    let directory = private_build(builds, name);
    let path = directory.join("retention-v1.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    link_native_lock(&directory, lock).unwrap();
    drop(listener);
}
