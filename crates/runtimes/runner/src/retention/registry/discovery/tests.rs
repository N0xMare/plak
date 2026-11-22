use super::*;
use plakous_runner_protocol::retention::{Answer, Hello, Reply};
use std::fs::{DirBuilder, File, OpenOptions, Permissions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use tokio::net::UnixListener;

struct SharedLock {
    _home: tempfile::TempDir,
    builds: PathBuf,
    lock: File,
    sibling: Origin,
    listener: UnixListener,
}

impl SharedLock {
    fn new(recorded_pid: u32) -> Self {
        let home = tempfile::tempdir().unwrap();
        let builds = home.path().join("b");
        let sibling = Origin {
            version: 1,
            build: id("v9-sibling"),
            instance: id("sibling"),
        };
        let dir = builds.join(sibling.build.as_str());
        private_dir(&dir);
        let mut lock = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(dir.join("retention-native.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        write!(lock, "{recorded_pid}").unwrap();
        let listener = listen(&dir.join("retention-v1.sock"));
        Self {
            _home: home,
            builds,
            lock,
            sibling,
            listener,
        }
    }

    fn holder(&self) -> NativeHolder {
        native_holder(&self.builds, self.sibling.build.as_str()).unwrap()
    }

    fn refused(&self, build: Id) -> Refused {
        let dir = self.builds.join(build.as_str());
        if build != self.sibling.build {
            private_dir(&dir);
            std::fs::hard_link(
                self.builds
                    .join(self.sibling.build.as_str())
                    .join("retention-native.lock"),
                dir.join("retention-native.lock"),
            )
            .unwrap();
        }
        let path = dir.join("retention-v2.sock");
        drop(listen(&path));
        Refused {
            socket: Some(SocketIdentity::at(&path).unwrap()),
            endpoint: Endpoint {
                build,
                version: 2,
                socket: path,
            },
            error: io::Error::from(io::ErrorKind::ConnectionRefused),
        }
    }
}

fn id(value: &str) -> Id {
    Id::try_from(value.to_owned()).unwrap()
}

fn scope() -> Bind {
    Bind {
        version: 1,
        host_id: id("host"),
        host_instance: id("host-instance"),
    }
}

fn private_dir(path: &Path) {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap();
}

fn listen(path: &Path) -> UnixListener {
    let listener = UnixListener::bind(path).unwrap();
    std::fs::set_permissions(path, Permissions::from_mode(0o600)).unwrap();
    listener
}

async fn answer_inventory(
    listener: UnixListener,
    origin: Origin,
    before_reply: impl FnOnce(),
) -> bool {
    let (mut stream, _) = listener.accept().await.unwrap();
    retention::send(&mut stream, &Hello { origin })
        .await
        .unwrap();
    let _: Bind = retention::receive(&mut stream).await.unwrap();
    retention::send(&mut stream, &Answer::Ok {}).await.unwrap();
    let codec = Codec::select(1, 1).unwrap();
    let call = match codec.read_call(&mut stream).await {
        Ok(call) => call,
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return false,
        Err(error) => panic!("{error}"),
    };
    before_reply();
    codec
        .write_reply(
            &mut stream,
            &Reply {
                id: call.id,
                answer: Answer::Lines {
                    lines: Vec::new(),
                    next: None,
                },
            },
        )
        .await
        .unwrap();
    true
}

#[tokio::test]
async fn same_build_is_not_a_distinct_witness() {
    let fixture = SharedLock::new(std::process::id());
    let refused = fixture.refused(fixture.sibling.build.clone());
    let answers = BTreeMap::from([(fixture.holder(), fixture.sibling.clone())]);
    let probe = answer_inventory(fixture.listener, fixture.sibling, || {});
    let bind = scope();
    let verification = verify_refusals(&fixture.builds, &bind, 1, &answers, vec![refused]);
    tokio::select! {
        result = verification => assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionRefused),
        _ = probe => panic!("discovery probed another version of the same build as a witness"),
    }
}

#[tokio::test]
async fn replaced_refused_socket_is_not_a_stale_alias() {
    let fixture = SharedLock::new(std::process::id());
    let refused = fixture.refused(id("v9-refused"));
    let socket = &refused.endpoint.socket;
    std::fs::rename(socket, socket.with_extension("old")).unwrap();
    drop(listen(socket));
    let answers = BTreeMap::from([(fixture.holder(), fixture.sibling.clone())]);
    let bind = scope();
    let (result, answered) = tokio::join!(
        verify_refusals(&fixture.builds, &bind, 1, &answers, vec![refused]),
        answer_inventory(fixture.listener, fixture.sibling, || {}),
    );
    assert!(answered);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionRefused);
}

#[tokio::test]
async fn changed_sibling_instance_is_not_a_witness() {
    let fixture = SharedLock::new(std::process::id());
    let holder = fixture.holder();
    let actual = Origin {
        instance: id("different"),
        ..fixture.sibling.clone()
    };
    let bind = scope();
    let (proven, answered) = tokio::join!(
        proves_sibling(&fixture.builds, &bind, 1, &fixture.sibling, holder),
        answer_inventory(fixture.listener, actual, || {}),
    );
    assert!(!proven && !answered);
}

#[tokio::test]
async fn peer_pid_must_match_the_native_holder() {
    let fixture = SharedLock::new(std::process::id() + 1);
    let holder = fixture.holder();
    let bind = scope();
    let (proven, answered) = tokio::join!(
        proves_sibling(&fixture.builds, &bind, 1, &fixture.sibling, holder),
        answer_inventory(fixture.listener, fixture.sibling.clone(), || {}),
    );
    assert!(!proven && !answered);
}

#[tokio::test]
async fn disappearing_holder_during_inventory_cannot_prove_an_alias() {
    let fixture = SharedLock::new(std::process::id());
    let holder = fixture.holder();
    let bind = scope();
    let (proven, answered) = tokio::join!(
        proves_sibling(&fixture.builds, &bind, 1, &fixture.sibling, holder),
        answer_inventory(fixture.listener, fixture.sibling.clone(), || drop(
            fixture.lock
        )),
    );
    assert!(answered);
    assert!(!proven);
}
