//! `plak runner` over real processes (spec 02 §1): pairing writes the
//! allowlist, `connect --stdio` starts the daemon detached and bridges a
//! host to it, probes stay under the roots, an unpaired host is refused at
//! once, `ps` lists through an inspecting connection, the daemon exits once
//! idle, and a root in or around the runner home is refused.

#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration test crate may unwrap"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use plakous::runner::protocol::{Bind, Refusal, VersionTriple};
use plakous::runner::{ClientWindows, ConnectError, RunnerClient};
use plakous::tools::runner::{RunnerCall, RunnerFailure, RunnerLink as _, RunnerVerb};

mod binary;

fn plak_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_plak"))
}

fn plak_command(home: &Path) -> Command {
    let mut command = Command::new(plak_bin());
    command
        .env("PLAK_TEST_STORAGE_CAPACITY", "available")
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .current_dir(home);
    command
}

fn isolated_home() -> (tempfile::TempDir, PathBuf) {
    let base = std::env::temp_dir();
    std::fs::create_dir_all(&base).unwrap();
    // Credential storage refuses a home path with `..` in it.
    let base = base.canonicalize().unwrap();
    let temporary = tempfile::Builder::new()
        .prefix("r")
        .tempdir_in(&base)
        .unwrap();
    let home = temporary.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    (temporary, home)
}

/// One host link through `plak runner connect --stdio`, as ssh would run it.
async fn bridge(
    home: &Path,
    host_id: &str,
    version: VersionTriple,
) -> (Result<RunnerClient, ConnectError>, tokio::process::Child) {
    selected_bridge(home, host_id, version, None).await
}

async fn selected_bridge(
    home: &Path,
    host_id: &str,
    version: VersionTriple,
    binary: Option<&str>,
) -> (Result<RunnerClient, ConnectError>, tokio::process::Child) {
    let mut command = tokio::process::Command::new(plak_bin());
    command.args(["runner", "connect", "--stdio"]);
    if let Some(binary) = binary {
        command.args(["--binary", binary]);
    }
    let mut child = command
        .env("PLAK_TEST_STORAGE_CAPACITY", "available")
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .current_dir(home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stream = tokio::io::join(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    let connected = RunnerClient::connect(
        stream,
        Bind {
            host_id: host_id.to_owned(),
            host_instance: uuid::Uuid::new_v4().hyphenated().to_string(),
            version,
            inspect: false,
        },
        ClientWindows::default(),
    )
    .await
    .map(|(client, _)| client);
    (connected, child)
}

fn probe(path: &Path) -> RunnerCall {
    RunnerCall::unkeyed(
        RunnerVerb::ProbeWorkspace,
        serde_json::json!({ "op": "is_file", "path": path }),
    )
}

/// A workspace probe answers only under the roots, never about the runner
/// home or anything else on the machine.
async fn probes_stay_under_the_roots(paired: &RunnerClient, home: &Path, work: &Path) {
    std::fs::write(work.join("file"), "x").unwrap();
    assert_eq!(
        paired.call(probe(&work.join("file"))).await.unwrap(),
        serde_json::json!({ "file": true })
    );
    for outside in [
        Path::new("/etc/passwd").to_path_buf(),
        home.join("runner/runner.toml"),
        home.join("secrets/runner/tok.secret"),
        work.join("../home/runner/runner.toml"),
    ] {
        assert!(
            matches!(
                paired.call(probe(&outside)).await,
                Err(RunnerFailure::Backend(_))
            ),
            "{}",
            outside.display()
        );
    }
}

fn vcs_read(call: serde_json::Value) -> RunnerCall {
    RunnerCall::unkeyed(RunnerVerb::VcsRead, call)
}

/// The tree's version control answers under the roots and the copies root,
/// and refuses the runner home and every other path with no effect.
async fn vcs_reads_stay_under_the_roots(paired: &RunnerClient, home: &Path, work: &Path) {
    let exists = |path: &Path| vcs_read(serde_json::json!({ "op": "path_exists", "path": path }));
    assert_eq!(
        paired.call(exists(work)).await.unwrap(),
        serde_json::json!({ "Ok": true })
    );
    let copies = work.parent().unwrap().join("copies");
    assert_eq!(
        paired
            .call(vcs_read(
                serde_json::json!({ "op": "transfer_root", "runner_id": "r1", "tree_id": "tree1" })
            ))
            .await
            .unwrap(),
        serde_json::json!({ "Ok": copies.join("r1").join("tree1") })
    );
    for outside in [
        Path::new("/etc").to_path_buf(),
        home.join("runner"),
        work.join("../home/runner"),
        Path::new("relative").to_path_buf(),
    ] {
        let answer = paired.call(exists(&outside)).await.unwrap();
        assert!(
            answer.get("Err").is_some(),
            "{}: {answer}",
            outside.display()
        );
    }
}

/// An unpaired host is refused before any request, and its calls fail at
/// once rather than waiting out their window.
async fn an_unpaired_host_fails_at_once(home: &Path, work: &Path, runner: VersionTriple) {
    let stranger = uuid::Uuid::new_v4().hyphenated().to_string();
    let (unpaired, _child) = bridge(home, &stranger, runner).await;
    let unpaired = unpaired.unwrap();
    let asked = Instant::now();
    assert!(unpaired.lines().await.is_err());
    let refused = unpaired.call(probe(&work.join("file"))).await;
    assert!(
        matches!(refused, Err(RunnerFailure::Unavailable(_))),
        "a refused link takes no call: {refused:?}"
    );
    // Half the 30 s call window: a refusal that waited the window out fails,
    // and a loaded host's bridge start-up does not.
    assert!(
        asked.elapsed() < Duration::from_secs(15),
        "{:?}",
        asked.elapsed()
    );
    assert_eq!(
        unpaired.refusal(),
        Some(Refusal::NotPaired { host_id: stranger })
    );
}

/// The bridge ends as soon as the daemon closes, with the host's input still
/// open: the host sees its stdout end, not only a later failed write.
async fn a_refused_link_ends_the_bridge_at_once(home: &Path, runner: VersionTriple) {
    use plakous::runner::protocol::{HostFrame, frame::encode};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut child = tokio::process::Command::new(plak_bin())
        .args(["runner", "connect", "--stdio"])
        .env("PLAK_TEST_STORAGE_CAPACITY", "available")
        .env("PLAK_HOME", home)
        .env("HOME", home)
        .current_dir(home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let bind = HostFrame::Bind(Bind {
        host_id: uuid::Uuid::new_v4().hyphenated().to_string(),
        host_instance: uuid::Uuid::new_v4().hyphenated().to_string(),
        version: runner,
        inspect: false,
    });
    stdin.write_all(&encode(&bind).unwrap()).await.unwrap();
    stdin.flush().await.unwrap();
    let mut output = Vec::new();
    let ended = tokio::time::timeout(
        Duration::from_secs(5),
        child.stdout.take().unwrap().read_to_end(&mut output),
    )
    .await;
    assert!(
        ended.is_ok(),
        "the bridge's stdout outlived the refusing daemon"
    );
    let exited = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    assert!(
        exited.is_ok_and(|status| status.is_ok()),
        "the bridge outlived its link"
    );
    drop(stdin);
}

#[test]
fn a_runner_daemon_pairs_bridges_lists_and_exits_idle() {
    let (guard, home) = isolated_home();
    // Workspaces lie outside the product home.
    let work = guard.path().join("work");
    std::fs::create_dir(&work).unwrap();
    std::fs::create_dir(home.join("runner")).unwrap();
    std::fs::write(
        home.join("runner/runner.toml"),
        format!(
            "roots = [{:?}]\ncopies = {:?}\npath = \"/usr/bin:/bin\"\nidle_exit_seconds = 1\n",
            work.display().to_string(),
            guard.path().join("copies").display().to_string()
        ),
    )
    .unwrap();

    // Pairing writes the host's id into the allowlist.
    let host_id = uuid::Uuid::new_v4().hyphenated().to_string();
    let allowed = plak_command(&home)
        .args(["runner", "allow", &host_id])
        .output()
        .unwrap();
    assert!(allowed.status.success(), "{allowed:?}");
    assert!(String::from_utf8_lossy(&allowed.stdout).contains("may use this runner"));
    let toml = std::fs::read_to_string(home.join("runner/runner.toml")).unwrap();
    assert!(toml.contains(&host_id), "{toml}");
    assert!(
        !plak_command(&home)
            .args(["runner", "allow", "Not-A-Uuid"])
            .output()
            .unwrap()
            .status
            .success()
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let triple = runtime.block_on(async {
        // The first connect starts the daemon; a host of another version is
        // refused before any request, which names the runner's triple.
        let (refused, _first) =
            bridge(&home, &host_id, VersionTriple::current("other", "release")).await;
        let Err(ConnectError::VersionMismatch { runner }) = refused else {
            panic!("another version bound");
        };
        let (paired, _second) = bridge(&home, &host_id, runner.clone()).await;
        let paired = paired.unwrap();
        assert_eq!(paired.lines().await.unwrap(), Vec::new());

        probes_stay_under_the_roots(&paired, &home, &work).await;
        vcs_reads_stay_under_the_roots(&paired, &home, &work).await;
        an_unpaired_host_fails_at_once(&home, &work, runner.clone()).await;
        a_refused_link_ends_the_bridge_at_once(&home, runner.clone()).await;
        runner
    });

    let listed = plak_command(&home).args(["runner", "ps"]).output().unwrap();
    assert!(listed.status.success(), "{listed:?}");
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "no lines\n");

    // With every link gone and nothing held, the daemon exits.
    drop(runtime);
    let run = home
        .join("runner/builds")
        .join(plakous::runner::daemon::triple_key(&triple))
        .join("run");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let sockets = std::fs::read_dir(&run).map_or(0, |entries| {
            entries
                .filter(|entry| {
                    entry.as_ref().is_ok_and(|entry| {
                        entry.path().extension().is_some_and(|ext| ext == "sock")
                    })
                })
                .count()
        });
        if sockets == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "the idle runner kept running");
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = plak_command(&home).args(["runner", "ps"]).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "no runner is running\n"
    );
    // The lock file the daemon held is private.
    for entry in std::fs::read_dir(&run).unwrap() {
        let mode = entry.unwrap().metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
}

/// `serve` refuses a root inside or around the product home: a host would
/// otherwise reach the runner's allowlist and floors, and the home's secrets
/// and credentials, through a workspace.
#[test]
fn a_runner_refuses_a_root_in_or_around_its_home() {
    let (guard, home) = isolated_home();
    let runner_home = home.join("runner");
    std::fs::create_dir(&runner_home).unwrap();
    for roots in [
        format!("[{:?}]", home.display().to_string()),
        format!("[{:?}]", runner_home.display().to_string()),
        format!("[{:?}]", runner_home.join("run").display().to_string()),
        // The home's secrets, the runner's and the host's named ones alike.
        format!("[{:?}]", home.join("secrets").display().to_string()),
    ] {
        std::fs::write(
            runner_home.join("runner.toml"),
            format!("roots = {roots}\npath = \"/usr/bin:/bin\"\nidle_exit_seconds = 1\n"),
        )
        .unwrap();
        let served = plak_command(&home)
            .args(["runner", "serve"])
            .output()
            .unwrap();
        assert!(!served.status.success(), "{roots}: {served:?}");
        let stderr = String::from_utf8_lossy(&served.stderr);
        assert!(
            stderr.contains("inside or around the product home"),
            "{roots}: {stderr}"
        );
    }
    // A copies root inside or around a workspace root is refused too.
    let work = guard.path().join("work");
    std::fs::create_dir(&work).unwrap();
    for copies in [work.join("copies"), guard.path().to_path_buf()] {
        std::fs::write(
            runner_home.join("runner.toml"),
            format!(
                "roots = [{:?}]\ncopies = {:?}\npath = \"/usr/bin:/bin\"\nidle_exit_seconds = 1\n",
                work.display().to_string(),
                copies.display().to_string()
            ),
        )
        .unwrap();
        let served = plak_command(&home)
            .args(["runner", "serve"])
            .output()
            .unwrap();
        assert!(!served.status.success(), "{}", copies.display());
        let stderr = String::from_utf8_lossy(&served.stderr);
        assert!(
            stderr.contains("inside or around"),
            "{}: {stderr}",
            copies.display()
        );
    }
    // Serving never started, so no socket was made.
    assert!(
        std::fs::read_dir(runner_home.join("run"))
            .ok()
            .is_none_or(|mut entries| entries.next().is_none())
    );
}
