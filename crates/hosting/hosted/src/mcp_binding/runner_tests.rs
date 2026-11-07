//! MCP servers placed on a runner, over real processes: the runner daemon in
//! its own process and, for host loss, the host too (spec 04 §2.9, spec 07
//! §3.5). The server runs on the runner with the runner's own secret, a
//! runner restart takes it down and a new runner starts it again on the next
//! lease, and a lost host takes its servers with it.

#![allow(clippy::unwrap_used, reason = "process fixture")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use plakous::runner::protocol::{Bind, VersionTriple};
use plakous::runner::{
    ClientWindows, RunnerClient, RunnerIdentity, RunnerOptions, RunnerService, daemon,
};
use plakous::tools::runner::RunnerBackend;
use plakous::tools::{ExecutionEnvironment, ExecutionEnvironmentConfig, RunnerId};

use super::*;

const DAEMON: &str = "PLAKOUS_HOSTED_MCP_RUNNER_DAEMON";
const HOST: &str = "PLAKOUS_HOSTED_MCP_RUNNER_HOST";
const MODULE: &str = "mcp_binding::available::runner_tests";

fn version() -> VersionTriple {
    VersionTriple::current("mcp-runner", "debug")
}

fn socket(directory: &Path) -> PathBuf {
    daemon::socket_path(&directory.join("run"), &version())
}

fn workspace(directory: &Path) -> PathBuf {
    directory.join("workspace")
}

fn pid_file(directory: &Path) -> PathBuf {
    directory.join("server.pid")
}

/// The host's named secrets hold `tok` too, with a value the server
/// refuses: only the runner's value may reach it.
struct HostSecrets;

impl NamedSecrets for HostSecrets {
    fn read(&self, name: &str) -> Result<Option<NamedSecretValue>, String> {
        Ok((name == "tok").then(|| NamedSecretValue::new("host-value".to_owned())))
    }

    fn read_runner(&self, name: &str) -> Result<Option<NamedSecretValue>, String> {
        Ok((name == "tok").then(|| NamedSecretValue::new("host-runner-shelf".to_owned())))
    }
}

/// The fixture server, started through a shell that records its pid and
/// exits unless `TOKEN` is the runner's value.
fn table(directory: &Path) -> String {
    let executable = std::env::current_exe().unwrap();
    let server = executable
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("mcp-fake-server");
    assert!(server.is_file(), "{} is missing", server.display());
    let script = format!(
        "echo $$ > '{}'; exec '{}' --expect-env TOKEN=runner-value",
        pid_file(directory).display(),
        server.display()
    );
    format!(
        "[mcp_servers.fake]\ntransport = \"stdio\"\ncommand = \"/bin/sh\"\nargs = [\"-c\", {script:?}]\n\
         secret_env = {{ TOKEN = \"tok\" }}\nplacement = \"runner\"\n\
         startup_seconds = 20\ncall_seconds = 120\n"
    )
}

/// The runner daemon process: the test binary again, running only this
/// test, with a runner secret store holding `tok`.
#[test]
fn mcp_runner_daemon_process() {
    let Ok(directory) = std::env::var(DAEMON) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let scope =
            ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000071".parse().unwrap());
        let store = |name: &str| -> Result<Option<String>, String> {
            Ok((name == "tok").then(|| "runner-value".to_owned()))
        };
        let service = RunnerService::with_options(
            ExecutionEnvironment::host(
                scope,
                workspace(&directory),
                ".",
                ExecutionEnvironmentConfig::default(),
            )
            .unwrap(),
            RunnerIdentity {
                runner_id: RunnerId::new("mcp-runner").unwrap(),
                version: version(),
                boot_id: "boot-1".into(),
                binary_id: None,
            },
            RunnerOptions {
                secrets: Some(Arc::new(store)),
                ..RunnerOptions::default()
            },
        )
        .unwrap();
        daemon::serve_socket(service, &socket(&directory), Duration::from_secs(600))
            .await
            .unwrap();
    });
}

/// A host process that leases the server on the runner, says so and waits
/// to be killed.
#[test]
fn mcp_runner_host_process() {
    let Ok(directory) = std::env::var(HOST) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (_client, backend) = bind(&directory, "host-instance").await;
        let (pool, _lease, _environment) = lease_on_runner(&directory, backend).await;
        std::fs::write(directory.join("host-ready"), b"").unwrap();
        std::future::pending::<()>().await;
        drop(pool);
    });
}

fn spawn(test: &str, variable: &str, directory: &Path) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{MODULE}::{test}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env(variable, directory)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

fn kill(mut child: std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

async fn bind(directory: &Path, instance: &str) -> (RunnerClient, Arc<RunnerBackend>) {
    let (client, hello) = RunnerClient::connect(
        connect_socket(directory).await,
        Bind {
            host_id: "host-1".into(),
            host_instance: instance.into(),
            version: version(),
            inspect: false,
        },
        ClientWindows::default(),
    )
    .await
    .unwrap();
    let backend = Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(client.clone())).unwrap());
    (client, backend)
}

async fn connect_socket(directory: &Path) -> tokio::net::UnixStream {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match tokio::net::UnixStream::connect(socket(directory)).await {
            Ok(stream) => return stream,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "the daemon never listened: {error}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// Leases the runner-placed server through `backend`, as a bind does, on a
/// pool of its own.
async fn lease_on_runner(
    directory: &Path,
    backend: Arc<RunnerBackend>,
) -> (McpPool, McpLease, Arc<ExecutionEnvironment>) {
    let server = McpServerName::new("fake").unwrap();
    let secrets: Arc<dyn NamedSecrets> = Arc::new(HostSecrets);
    let (spec, site) =
        placed_server_spec(&server, &table(directory), &secrets, Some(backend)).unwrap();
    assert!(matches!(site, ServerSite::Runner(_)));
    let environment = Arc::new(service_environment(&workspace(directory), &site, None).unwrap());
    assert_eq!(
        environment
            .backend()
            .runner_id()
            .map(|runner| runner.as_str().to_owned()),
        Some("mcp-runner".to_owned())
    );
    let pool = McpPool::new();
    let lease = pool.lease(spec, Arc::clone(&environment)).await.unwrap();
    (pool, lease, environment)
}

fn server_pid(directory: &Path) -> String {
    std::fs::read_to_string(pid_file(directory))
        .unwrap()
        .trim()
        .to_owned()
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn wait_gone(pid: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(pid) {
        assert!(
            Instant::now() < deadline,
            "server process {pid} outlived its owner"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_on_a_runner_runs_there_and_restarts_after_the_runner_does() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(directory.path())).unwrap();
    let runner = spawn("mcp_runner_daemon_process", DAEMON, directory.path());
    let (client, backend) = bind(directory.path(), "instance-1").await;

    // The server runs on the runner with the runner's secret: the fixture
    // exits at once on any other value.
    let (pool, lease, environment) = lease_on_runner(directory.path(), backend).await;
    assert!(
        lease
            .listed_tool_names()
            .await
            .unwrap()
            .contains(&"echo".to_owned())
    );
    let first = server_pid(directory.path());
    assert!(alive(&first));

    // The runner dies, and its server with it.
    kill(runner);
    wait_gone(&first).await;
    // With no link, a lease fails at once; nothing is started anywhere.
    std::fs::remove_file(pid_file(directory.path())).unwrap();
    let asked = Instant::now();
    let spec = lease.spec().clone();
    drop(lease);
    assert!(
        pool.lease(spec.clone(), Arc::clone(&environment))
            .await
            .is_err()
    );
    assert!(
        asked.elapsed() < Duration::from_secs(10),
        "{:?}",
        asked.elapsed()
    );
    assert!(!pid_file(directory.path()).exists());

    // A new runner takes the link back, and the next lease starts the
    // server there.
    let runner = spawn("mcp_runner_daemon_process", DAEMON, directory.path());
    client
        .reconnect(connect_socket(directory.path()).await)
        .await
        .unwrap();
    let lease = pool.lease(spec, environment).await.unwrap();
    assert!(
        lease
            .listed_tool_names()
            .await
            .unwrap()
            .contains(&"echo".to_owned())
    );
    let second = server_pid(directory.path());
    assert_ne!(first, second);
    assert!(alive(&second));
    drop(lease);
    pool.shutdown().await;
    wait_gone(&second).await;
    kill(runner);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_on_a_runner_ends_with_its_host() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace(directory.path())).unwrap();
    let runner = spawn("mcp_runner_daemon_process", DAEMON, directory.path());
    let host = spawn("mcp_runner_host_process", HOST, directory.path());
    wait_for(&directory.path().join("host-ready")).await;
    let pid = server_pid(directory.path());
    assert!(alive(&pid));

    // The host is lost: its link closes, and the runner ends the services
    // that link spawned rather than leaving them behind.
    kill(host);
    wait_gone(&pid).await;
    kill(runner);
}
