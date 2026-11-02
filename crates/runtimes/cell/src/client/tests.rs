use super::*;
use crate::protocol::QUICKJS_WORKER;
use std::os::unix::fs::PermissionsExt as _;

#[test]
fn the_hello_bound_stays_thirty_seconds_unless_a_positive_override_is_parsed() {
    assert_eq!(WORKER_HELLO_TIMEOUT, Duration::from_secs(30));
    assert!(hello_override(None).is_none());
    assert!(hello_override(Some("0")).is_none());
    assert!(hello_override(Some("nope")).is_none());
    assert!(hello_override(Some("30001")).is_none());
    assert_eq!(
        hello_override(Some("200")),
        Some(Duration::from_millis(200))
    );
    assert_eq!(hello_override(Some("30000")), Some(Duration::from_secs(30)));
}

/// A worker that writes `opening` and then sleeps.
fn worker(directory: &Path, opening: &[u8]) -> std::path::PathBuf {
    let bytes = directory.join("opening.bin");
    std::fs::write(&bytes, opening).expect("opening bytes");
    let program = directory.join("worker");
    let staging = directory.join("worker.pending");
    std::fs::write(
        &staging,
        format!("#!/bin/sh\ncat '{}'\nexec sleep 30\n", bytes.display()),
    )
    .expect("program");
    std::fs::rename(staging, &program).expect("publish program");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
        .expect("permissions");
    program
}

fn hello(revision: &str, engine: &str, adapter: &str) -> Vec<u8> {
    encode_frame(&WorkerFrame::Hello {
        revision: revision.to_owned(),
        engine: engine.to_owned(),
        adapter: adapter.to_owned(),
    })
    .expect("hello frame")
}

async fn refusal(opening: &[u8], bound: Duration) -> String {
    let directory = tempfile::tempdir().expect("directory");
    let program = worker(directory.path(), opening);
    let started = tokio::time::Instant::now();
    let Err(error) =
        CellClient::spawn_within(&program, QUICKJS_WORKER, EngineConfig::QUICKJS, bound, None)
            .await
    else {
        panic!("the worker must be refused before it is configured");
    };
    // Refused and reaped well inside the hello bound plus the reap bound.
    assert!(
        started.elapsed() < bound + WORKER_SHUTDOWN_SETTLE_TIMEOUT,
        "{error}"
    );
    error.to_string()
}

#[tokio::test]
async fn a_worker_naming_another_revision_engine_or_adapter_is_refused() {
    let quickjs = QUICKJS_WORKER;
    let cases = [
        (
            hello("plakous-cell-protocol.v0", quickjs.engine, quickjs.adapter),
            "speaks plakous-cell-protocol.v0",
        ),
        (
            hello(CELL_PROTOCOL_REVISION, "monty", quickjs.adapter),
            "cell worker is monty/",
        ),
        (
            hello(
                CELL_PROTOCOL_REVISION,
                quickjs.engine,
                "plakous-js-eval-bootstrap.v5",
            ),
            "plakous-js-eval-bootstrap.v5, not",
        ),
    ];
    for (opening, expected) in cases {
        let error = refusal(&opening, Duration::from_secs(10)).await;
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn incompatible_hello_diagnostics_bound_worker_supplied_fields() {
    let oversized = format!("{}DO_NOT_ECHO", "λ".repeat(100));
    for (revision, engine, adapter) in [
        (
            oversized.as_str(),
            QUICKJS_WORKER.engine,
            QUICKJS_WORKER.adapter,
        ),
        (
            CELL_PROTOCOL_REVISION,
            oversized.as_str(),
            QUICKJS_WORKER.adapter,
        ),
        (
            CELL_PROTOCOL_REVISION,
            QUICKJS_WORKER.engine,
            oversized.as_str(),
        ),
    ] {
        let frame = WorkerFrame::Hello {
            revision: revision.into(),
            engine: engine.into(),
            adapter: adapter.into(),
        };
        let error = check_hello(&frame, QUICKJS_WORKER).expect_err("incompatible worker");
        assert!(error.len() < 256, "diagnostic has {} bytes", error.len());
        assert!(error.contains("..."));
        assert!(!error.contains("DO_NOT_ECHO"));
    }
}

#[tokio::test]
async fn a_malformed_or_missing_hello_is_refused() {
    let not_hello = encode_frame(&WorkerFrame::Yielded { cell: 1 }).expect("frame");
    let error = refusal(&not_hello, Duration::from_secs(10)).await;
    assert!(error.contains("did not open with hello"), "{error}");
    let malformed = [&8_u32.to_be_bytes()[..], b"not-json"].concat();
    let error = refusal(&malformed, Duration::from_secs(10)).await;
    assert!(error.contains("malformed hello"), "{error}");
}

#[tokio::test]
async fn a_late_hello_is_refused_at_its_bound() {
    let error = refusal(&[], Duration::from_millis(300)).await;
    assert!(error.contains("did not say hello in time"), "{error}");
    assert!(error.contains("within 0.3 s"), "{error}");
}

#[tokio::test]
async fn a_delayed_hello_uses_the_supplied_startup_bound() {
    let directory = tempfile::tempdir().expect("directory");
    let opening = hello(
        CELL_PROTOCOL_REVISION,
        QUICKJS_WORKER.engine,
        QUICKJS_WORKER.adapter,
    );
    let program = worker(directory.path(), &opening);
    let source = std::fs::read_to_string(&program).expect("program");
    std::fs::write(&program, source.replacen("cat ", "sleep 0.1\ncat ", 1)).expect("delay hello");
    let client = CellClient::spawn_within(
        &program,
        QUICKJS_WORKER,
        EngineConfig::QUICKJS,
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("a slow worker inside the bound is usable");
    client.stop().await.expect("reaped slow worker");
}

#[tokio::test]
async fn a_worker_that_ignores_sigterm_is_killed_inside_the_shutdown_bound() {
    let directory = tempfile::tempdir().expect("directory");
    let opening = hello(
        CELL_PROTOCOL_REVISION,
        QUICKJS_WORKER.engine,
        QUICKJS_WORKER.adapter,
    );
    let program = worker(directory.path(), &opening);
    let source = std::fs::read_to_string(&program).expect("program");
    std::fs::write(
        &program,
        source.replace("exec sleep 30", "trap '' TERM; exec sleep 30"),
    )
    .expect("ignore TERM");
    let client = CellClient::spawn(&program, QUICKJS_WORKER, EngineConfig::QUICKJS)
        .await
        .expect("worker");
    let began = std::time::Instant::now();
    let first = client.stop().await.expect("reaped after forced stop");
    assert!(first.forced);
    assert!(began.elapsed() < WORKER_SHUTDOWN_SETTLE_TIMEOUT * 2 + Duration::from_secs(2));
    assert_eq!(
        client.stop().await.expect("same receipt"),
        first,
        "a later wait cannot relabel a kill as voluntary"
    );
    assert!(client.terminate().await.is_err());
    assert!(client.stop().await.is_err(), "a loss stays a loss");
}

#[tokio::test]
async fn a_worker_that_exits_on_its_own_is_never_signalled() {
    let directory = tempfile::tempdir().expect("directory");
    let opening = hello(
        CELL_PROTOCOL_REVISION,
        QUICKJS_WORKER.engine,
        QUICKJS_WORKER.adapter,
    );
    let program = worker(directory.path(), &opening);
    let source = std::fs::read_to_string(&program).expect("program");
    let configure = encode_frame(&HostFrame::Configure {
        sdk_stubs: None,
        revision: CELL_PROTOCOL_REVISION.to_owned(),
        engine: EngineConfig::QUICKJS,
    })
    .expect("configure frame");
    // Keep stdin open through Configure rather than racing spawn's write
    // with the voluntary exit this test exercises after the handshake.
    let exit_after_configure = format!("head -c {} >/dev/null\nexit 0", configure.len());
    std::fs::write(
        &program,
        source.replace("exec sleep 30", &exit_after_configure),
    )
    .expect("voluntary exit");
    let client = CellClient::spawn(&program, QUICKJS_WORKER, EngineConfig::QUICKJS)
        .await
        .expect("worker");
    let first = client.stop().await.expect("voluntary receipt");
    assert!(!first.forced);
    assert_eq!(client.stop().await.expect("same receipt"), first);
}

#[tokio::test]
async fn unreaped_worker_never_certifies_joined_shutdown() {
    let directory = tempfile::tempdir().expect("directory");
    let opening = hello(
        CELL_PROTOCOL_REVISION,
        QUICKJS_WORKER.engine,
        QUICKJS_WORKER.adapter,
    );
    let program = worker(directory.path(), &opening);
    let client = CellClient::spawn(&program, QUICKJS_WORKER, EngineConfig::QUICKJS)
        .await
        .expect("worker");
    // Consume the OS status outside the supervisor, so its wait cannot
    // establish a receipt. No successful join may be invented from ECHILD.
    client
        .child
        .lock()
        .await
        .start_kill()
        .expect("kill owned worker");
    let pid = nix::unistd::Pid::from_raw(i32::try_from(client.pid).expect("pid"));
    nix::sys::wait::waitpid(pid, None).expect("consume status");
    assert!(client.stop().await.is_err());
    assert!(client.terminate().await.is_err());
}
