//! A post-Ready rollback re-execs the current host, not the original drainer.

use super::*;
use plakous::runner::{FileOwnerFloors, Floor, OwnerFloors as _};
use plakous_host_protocol::wire::{RequestRefusal, RunnerStatus};
use std::os::unix::fs::MetadataExt as _;

#[test]
fn same_key_image_rollback_readopts_local_and_paired_retained_jobs() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let paired = Paired::new(&fixture);
    let images = Images::new(&fixture, &paired.path);
    let web = web_socket(&fixture);
    source_script(&fixture);
    let mut host = start_env(
        &fixture,
        &["--web", &format!("unix:{}", web.display())],
        &images.environment,
    );
    let mut client = connected(&host, &paired);
    let local = start_root(&fixture, &mut client, "local", None, 10);
    let remote = start_root(&fixture, &mut client, "paired", Some(&paired.id), 20);
    assert_eq!(paired.runner.pid(), Some(remote.background.1));
    assert_ne!(local.background.1, remote.background.1);
    let original = [
        local_runner_root(&fixture.home()).join("owners.json"),
        paired_original_floors(&paired),
    ];
    let roots = [&local, &remote];
    let key = plakd::home::state::state_key();
    let lines = roots.map(|root| identity(&fixture, &key, &root.session).1);
    let original_pid = host.child.as_ref().unwrap().id();
    let listeners = [socket_id(&host.socket), socket_id(&web)];

    fixture.write_script(&json!({
        "version": 1, "process_tools": true,
        "steps": vec![json!({"output":{"text":ANSWER}}); 6]
    }));
    images.first_upgrade(&fixture, &paired);
    drop(client);
    let mut client = connected(&host, &paired);
    for (id, root) in roots.into_iter().enumerate() {
        client.attach(30 + u64::try_from(id).unwrap(), &root.node);
        profiles::wait_idle(&mut client, &root.session);
    }
    let before = serving_identity(&fixture, &host);
    let floors = retained_floors(&original, &lines, &before.instance);
    assert_jobs(&fixture, roots);

    images.reject_after_ready(&fixture, &host, &paired);
    drop(client);
    let mut client = connected(&host, &paired);
    let after = serving_identity(&fixture, &host);
    assert_eq!(after.binary, before.binary);
    assert_ne!(after.generation, before.generation);
    assert_ne!(after.instance, before.instance);
    assert_eq!([socket_id(&host.socket), socket_id(&web)], listeners);
    assert_eq!(host.child.as_ref().unwrap().id(), original_pid);
    assert!(host.child.as_mut().unwrap().try_wait().unwrap().is_none());
    assert_eq!(after.pid, original_pid);
    let adopted = retained_floors(&original, &lines, &after.instance);
    for (old, new) in floors.iter().zip(adopted) {
        assert_eq!(new.owner.branch, old.owner.branch);
        assert_eq!(new.owner.generation, old.owner.generation);
        assert!(new.owner.fence > old.owner.fence, "{old:?} -> {new:?}");
    }
    assert_jobs(&fixture, roots);
    finish_and_check_admission(&fixture, &mut client, roots);
    drop(client);
    stop(&mut host);
    for root in roots {
        notices(&fixture, &root.session, true, &root.name);
        assert_eq!(marked(&fixture, &root.name, "bg"), root.background);
        assert_eq!(marked(&fixture, &root.name, "fg"), root.foreground);
    }
}

struct Images {
    source: PathBuf,
    candidate: PathBuf,
    gate: PathBuf,
    environment: Vec<(&'static str, String)>,
}

impl Images {
    fn new(fixture: &Fixture, path: &str) -> Self {
        let source = replacement_image_named(fixture, "retaining")
            .canonicalize()
            .unwrap();
        let candidate = replacement_image_named(fixture, "qualified")
            .canonicalize()
            .unwrap();
        let gate = fixture.root.join("exec-gate");
        let mut environment = candidate_environment(&source).to_vec();
        environment.extend([
            (
                "PLAK_TEST_HOST_UPGRADE_RUNNER_NEXT_IMAGE",
                candidate.display().to_string(),
            ),
            (
                "PLAK_TEST_HOST_UPGRADE_RUNNER_NEXT_REVISION",
                NEXT_REVISION.into(),
            ),
            (
                "PLAK_TEST_HOST_BEFORE_PAIRED_EXEC",
                gate.display().to_string(),
            ),
            ("PATH", path.into()),
        ]);
        Self {
            source,
            candidate,
            gate,
            environment,
        }
    }

    fn first_upgrade(&self, fixture: &Fixture, paired: &Paired) {
        paired.advance(&self.source);
        let result = candidate_upgrade(fixture, &self.source, &self.environment)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
    }

    fn reject_after_ready(&self, fixture: &Fixture, host: &Host, paired: &Paired) {
        let replacement = replacement_image_named(fixture, "unqualified");
        std::fs::write(&self.gate, []).unwrap();
        paired.advance(&self.candidate);
        let mut requester = TestChild::spawn(
            candidate_upgrade(fixture, &self.candidate, &self.environment)
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        );
        wait_line(host, "waiting at paired exec gate");
        std::fs::rename(replacement, &self.candidate).unwrap();
        // The dedicated runner link is operator-owned; a host transport
        // rollback cannot restore it. Restore the source link independently.
        paired.advance(&self.source);
        std::fs::remove_file(&self.gate).unwrap();
        wait_line(host, "qualified image changed");
        wait_line(host, "serving on");
        let _ = requester.kill();
        let _ = requester.wait();
    }
}

fn wait_line(host: &Host, expected: &str) {
    let deadline = Instant::now() + WAIT;
    while let Ok(line) = host
        .lines
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        if line.contains(expected) {
            return;
        }
    }
    panic!("host did not report {expected}");
}

fn web_socket(fixture: &Fixture) -> PathBuf {
    let config = fixture.home().join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    write_private(
        &config,
        &format!(
            "{text}\n[web]\nallow = [\"operator@example.com\"]\norigin = \"https://web.example.test\"\n"
        ),
    );
    fixture.root.join("web.sock")
}

fn socket_id(path: &Path) -> (u64, u64) {
    let metadata = std::fs::symlink_metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

fn connected(host: &Host, paired: &Paired) -> Client {
    let mut client = Client::connect(host);
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = client.request(8, Op::SessionOptions { workspace: None });
        match &reply {
            Outcome::Ok(Answer::Options { options }) => {
                if ["local", paired.id.as_str()].into_iter().all(|id| {
                    options
                        .runners
                        .iter()
                        .any(|row| row.runner.runner_id == id && row.status == RunnerStatus::Ready)
                }) {
                    return client;
                }
            }
            Outcome::Refused(RequestRefusal::Backpressure) => {}
            _ => panic!("runner options refused: {reply:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "runners did not reconcile: {reply:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct ServingIdentity {
    binary: String,
    instance: String,
    generation: u64,
    pid: u32,
}

fn serving_identity(fixture: &Fixture, host: &Host) -> ServingIdentity {
    let mut stream = UnixStream::connect(&host.socket).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream
        .write_all(
            encode_line(&ClientFrame::<PlakExtension>::Hello(Hello {
                proto: WIRE_VERSION,
                binary_id: "rollback-test".into(),
                caps: Vec::new(),
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    let mut welcome = String::new();
    BufReader::new(stream).read_line(&mut welcome).unwrap();
    let HostFrame::Welcome(welcome) = decode_line::<HostFrame<PlakExtension>>(&welcome).unwrap()
    else {
        panic!("missing Welcome");
    };
    let coordinator = UnixStream::connect(fixture.home().join("host/upgrade.sock")).unwrap();
    coordinator.set_read_timeout(Some(WAIT)).unwrap();
    let mut line = String::new();
    BufReader::new(coordinator).read_line(&mut line).unwrap();
    let identity: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(identity["binary"], welcome.binary_id);
    let lock = std::fs::read_to_string(fixture.home().join("host/host.lock")).unwrap();
    let (pid, binary) = lock.trim().split_once(' ').unwrap();
    assert_eq!(binary, welcome.binary_id);
    ServingIdentity {
        binary: welcome.binary_id,
        instance: identity["instance"].as_str().unwrap().into(),
        generation: welcome.host_generation,
        pid: pid.parse().unwrap(),
    }
}

fn paired_original_floors(paired: &Paired) -> PathBuf {
    let mut builds = std::fs::read_dir(paired.runner.home.join("runner/builds")).unwrap();
    let build = builds.next().unwrap().unwrap().path();
    assert!(builds.next().is_none());
    build.join("owners.json")
}

fn retained_floors(paths: &[PathBuf; 2], lines: &[String; 2], instance: &str) -> [Floor; 2] {
    std::array::from_fn(|i| {
        let (_, floor) = FileOwnerFloors::open(&paths[i])
            .unwrap()
            .load()
            .into_iter()
            .find(|(_, floor)| floor.owner.line == lines[i])
            .unwrap_or_else(|| panic!("original runner lost {}", lines[i]));
        assert!(floor.retention_only, "{floor:?}");
        assert_eq!(floor.holder, instance);
        floor
    })
}

fn assert_jobs(fixture: &Fixture, roots: [&Root; 2]) {
    for root in roots {
        assert_eq!(root.foreground.1, root.background.1);
        assert!(runner::alive(root.background.1), "original daemon was lost");
        assert!(runner::alive(root.background.0) && runner::alive(root.foreground.0));
        assert_eq!(marked(fixture, &root.name, "bg"), root.background);
        assert_eq!(marked(fixture, &root.name, "fg"), root.foreground);
    }
}

fn finish_and_check_admission(fixture: &Fixture, client: &mut Client, roots: [&Root; 2]) {
    let workspace = fixture.root.join("admitted");
    std::fs::create_dir(&workspace).unwrap();
    let request = new_session(&workspace);
    let deadline = Instant::now() + WAIT;
    loop {
        match client.request(50, request.clone()) {
            Outcome::Ok(Answer::Created { .. }) => break,
            Outcome::Refused(RequestRefusal::Backpressure) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            reply => panic!("admission after rollback: {reply:?}"),
        }
    }
    for (id, root) in roots.into_iter().enumerate() {
        for (job, pid) in [("bg", root.background.0), ("fg", root.foreground.0)] {
            std::fs::write(fixture.root.join(format!("{}-{job}-finish", root.name)), []).unwrap();
            wait_exit(pid);
        }
        let id = 60 + 2 * u64::try_from(id).unwrap();
        let attachment = client.attach(id, &root.node);
        client.prompt(id + 1, attachment, "after rollback", ANSWER);
        wait_notices(fixture, &root.session, &root.name);
    }
}
