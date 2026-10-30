use super::*;
use plakous_cell::{
    CELL_PROTOCOL_REVISION, EngineConfig, HostFrame, QUICKJS_WORKER, WorkerFrame, encode_frame,
};
use std::{os::unix::fs::PermissionsExt, time::Duration};

/// Bytes of the one `Configure` frame the client sends after `Hello`.
fn configure_len() -> usize {
    encode_frame(&HostFrame::Configure {
        sdk_stubs: None,
        revision: CELL_PROTOCOL_REVISION.to_owned(),
        engine: EngineConfig::QUICKJS,
    })
    .expect("configure frame")
    .len()
}

/// A shell worker that says hello as the `QuickJS` worker, then runs `body`.
fn shell_worker(directory: &Path, body: &str) -> PathBuf {
    let hello = encode_frame(&WorkerFrame::Hello {
        revision: CELL_PROTOCOL_REVISION.to_owned(),
        engine: QUICKJS_WORKER.engine.to_owned(),
        adapter: QUICKJS_WORKER.adapter.to_owned(),
    })
    .expect("hello frame");
    let hello_path = directory.join("hello.bin");
    std::fs::write(&hello_path, hello).expect("hello bytes");
    let program = directory.join("worker");
    let staging = directory.join("worker.pending");
    std::fs::write(
        &staging,
        format!("#!/bin/sh\ncat '{}'\n{body}", hello_path.display()),
    )
    .expect("program");
    std::fs::rename(staging, &program).expect("publish program");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
        .expect("permissions");
    program
}

async fn connect(program: &Path) -> LocalCompositionSessionBundle {
    let client = CellClient::spawn(program, QUICKJS_WORKER, EngineConfig::QUICKJS)
        .await
        .expect("worker hello");
    session_bundle(Arc::new(client), Box::new(Quickjs))
}

#[tokio::test]
async fn accepted_start_eof_and_malformed_frame_complete_without_turn_timeout() {
    // The worker consumes `Configure` and the first byte of the start, so
    // the start is accepted before the worker ends or misbehaves.
    let consumed = configure_len() + 1;
    for (name, after) in [
        ("eof", String::new()),
        (
            "malformed",
            "printf '\\000\\000\\000\\010not-json'\n".to_owned(),
        ),
    ] {
        let directory = tempfile::tempdir().expect("directory");
        let program = shell_worker(
            directory.path(),
            &format!("head -c {consumed} >/dev/null\n{after}"),
        );
        let bundle = connect(&program).await;
        let started = bundle
            .session
            .execute(LocalCompositionExecuteRequest {
                start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(
                    || Box::pin(async { Ok(()) }),
                ),
                tool_call_id: "start".into(),
                source: "text('once')".into(),
                enabled_tools: vec![],
                yield_time_ms: 10_000,
                max_output_tokens: 100,
            })
            .await
            .expect("start admitted once");
        // Well under the 10 s yield, so a pass still proves the loss did
        // not wait for the turn; the margin is for process start under load.
        let result = tokio::time::timeout(Duration::from_secs(5), started.initial_response)
            .await
            .expect("prompt loss");
        assert!(result.is_err(), "{name} must be worker loss");
        assert!(matches!(
            bundle.session.shutdown().await,
            LocalCompositionShutdownResult::Joined { .. }
        ));
    }
}

#[tokio::test]
async fn forced_termination_remains_lost_on_shutdown() {
    let directory = tempfile::tempdir().expect("directory");
    let program = shell_worker(directory.path(), "exec sleep 60\n");
    let bundle = connect(&program).await;
    assert!(matches!(
        bundle.session.terminate("1".into()).await,
        LocalCompositionTerminateResult::Lost(_)
    ));
    assert!(matches!(
        bundle.session.shutdown().await,
        LocalCompositionShutdownResult::Lost(_)
    ));
}

#[test]
fn non_identifier_names_get_codex_aliases_unless_they_collide() {
    let names = ["exec_command", "mcp__everything__get-sum", "mcp__a__x.y"];
    assert_eq!(
        javascript_globals(names.into_iter()),
        ["exec_command", "mcp__everything__get_sum", "mcp__a__x_y"]
    );
    let colliding = [
        "mcp__s__get-sum",
        "mcp__s__get_sum",
        "mcp__s__a-b",
        "mcp__s__a.b",
    ];
    assert_eq!(
        javascript_globals(colliding.into_iter()),
        [
            "mcp__s__get-sum",
            "mcp__s__get_sum",
            "mcp__s__a-b",
            "mcp__s__a.b"
        ]
    );
}
