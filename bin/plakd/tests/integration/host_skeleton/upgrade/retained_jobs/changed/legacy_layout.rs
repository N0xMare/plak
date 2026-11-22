//! A forward hop must not mistake stale legacy aliases for unavailable jobs.

use super::*;
use plakous::runner::protocol::{PROTOCOL_VERSION, VersionTriple};
use plakous::runner::{RunnerIdentity, RunnerOptions, RunnerService, daemon};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};
use plakous_host_protocol::wire::{RequestRefusal, RunnerStatus};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::os::unix::net::UnixListener;

#[test]
fn forward_hop_with_a_busy_legacy_shared_lock_reconciles_admission_and_wake() {
    let Some((fixture, wake_script)) = root_wake::child_fixture(false) else {
        return;
    };
    let legacy = LegacyLayout::new(&fixture);
    if daemon::check_socket_path(&legacy.stale_socket()).is_err() {
        return;
    }
    let mut source_script = wake_script.clone();
    source_script["steps"] = json!([{"output": {"text": ANSWER}}]);
    fixture.write_script(&source_script);
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let gate = fixture.root.join("exec-gate");
    let mut environment = candidate_environment(&image).to_vec();
    environment.push((
        "PLAK_TEST_HOST_BEFORE_PAIRED_EXEC",
        gate.display().to_string(),
    ));
    let mut host = start_env(&fixture, &[], &environment);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    let (before, old_node) = prompt(&fixture, &mut client);
    root_wake::idle_turns(&fixture, &before, 1);
    drop(client);
    fixture.write_script(&wake_script);

    std::fs::write(&gate, []).unwrap();
    let mut request = TestChild::spawn(
        candidate_upgrade(&fixture, &image, &environment)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()),
    );
    rollback::wait_line(&host, "waiting at paired exec gate");
    // Inject the old protocol-only lock once the source has finished its
    // sweep. The candidate must discover the same layout on its first bind.
    let _legacy_daemon = legacy.serve(&fixture);
    std::fs::remove_file(&gate).unwrap();
    assert!(request.wait().unwrap().success());

    let mut client = ready_picker(&host);
    client.attach(40, &old_node);
    let (fresh, node) = prompt(&fixture, &mut client);
    assert_ne!(fresh, before);
    root_wake::idle_turns(&fixture, &fresh, 1);
    std::fs::write(fixture.root.join("release-child"), "continue").unwrap();
    root_wake::submitted_result(&fixture, &node);
    root_wake::idle_turns(&fixture, &fresh, 2);
    assert!(legacy.stale_socket().exists(), "discovery is read-only");
    drop(client);
    stop(&mut host);
}

fn prompt(fixture: &Fixture, client: &mut Client) -> (String, NodeAddress) {
    let request = uuid::Uuid::new_v4().to_string();
    let outcome = client.request(
        41,
        new_session_request(&fixture.workspace(), None, &request),
    );
    let Outcome::Ok(Answer::Created { session, node, .. }) = outcome else {
        panic!("workspace admission failed: {outcome:?}");
    };
    let attachment = client.attach(42, &node);
    client.prompt(43, attachment, "run the offline fixture", ANSWER);
    (session, node)
}

fn ready_picker(host: &Host) -> Client {
    let mut client = Client::connect(host);
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = client.request(50, Op::SessionOptions { workspace: None });
        match &reply {
            Outcome::Ok(Answer::Options { options }) => {
                if options
                    .runners
                    .iter()
                    .any(|row| row.runner.runner_id == "local" && row.status == RunnerStatus::Ready)
                {
                    return client;
                }
            }
            Outcome::Refused(RequestRefusal::Backpressure) => {}
            _ => panic!("workspace picker refused: {reply:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "picker did not reconcile: {reply:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct LegacyLayout {
    native: PathBuf,
    active_build: PathBuf,
    stale_build: PathBuf,
    version: VersionTriple,
}

impl LegacyLayout {
    fn new(fixture: &Fixture) -> Self {
        let version = VersionTriple::current("legacy-layout-fixture", "debug");
        let builds = fixture.home().join("builds");
        let stale = VersionTriple::current("earlier-legacy-layout-fixture", "debug");
        Self {
            native: fixture
                .home()
                .join(format!("host/runner/v{PROTOCOL_VERSION}/run/runner.sock")),
            active_build: builds.join(daemon::triple_key(&version)),
            stale_build: builds.join(daemon::triple_key(&stale)),
            version,
        }
    }

    fn stale_socket(&self) -> PathBuf {
        self.stale_build.join("retention-v1.sock")
    }

    fn serve(&self, fixture: &Fixture) -> tokio::runtime::Runtime {
        for dir in [
            self.native.parent().unwrap(),
            &self.active_build,
            &self.stale_build,
        ] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .unwrap();
        }
        let host_id = std::fs::read_to_string(fixture.home().join("host-id")).unwrap();
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000001".parse().unwrap()),
            fixture.workspace(),
            ".",
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap();
        let service = RunnerService::with_options(
            environment,
            RunnerIdentity {
                runner_id: RunnerId::new("legacy-layout").unwrap(),
                version: self.version.clone(),
                boot_id: "legacy-boot".into(),
                binary_id: None,
            },
            RunnerOptions {
                home_local: Some(host_id.trim().to_owned()),
                ..RunnerOptions::default()
            },
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let native = self.native.clone();
        let active = self.active_build.clone();
        let serving = runtime.spawn(async move {
            daemon::serve_socket_with_retention(
                service,
                &native,
                &active,
                Duration::from_secs(300),
            )
            .await
            .unwrap();
        });
        let deadline = Instant::now() + WAIT;
        while !self.active_build.join("retention-native.lock").exists() {
            assert!(
                !serving.is_finished() && Instant::now() < deadline,
                "legacy daemon did not serve"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let stale = self.stale_socket();
        let listener = UnixListener::bind(&stale).unwrap();
        std::fs::set_permissions(stale, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::hard_link(
            self.native.with_extension("lock"),
            self.stale_build.join("retention-native.lock"),
        )
        .unwrap();
        drop(listener);
        runtime
    }
}
