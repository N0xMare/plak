use super::*;
use plakous_model::CancellationToken;
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, OperationContext,
};
use std::str::FromStr as _;
use std::time::Duration;

fn refuses_context_special_file(device: bool) {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let temporary = tempfile::tempdir().unwrap();
        let (root, path) = if device {
            (Path::new("/dev"), Path::new("null"))
        } else {
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(temporary.path().join("fifo"))
                    .status()
                    .unwrap()
                    .success()
            );
            (temporary.path(), Path::new("fifo"))
        };
        let environment = ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(
                plakous_model::RunId::from_str("00000000-0000-0000-0000-000000000041").unwrap(),
            ),
            root,
            ".",
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(read_context_bytes(
            &environment,
            path,
            OperationContext::new(CancellationToken::new()),
        ));
        assert_eq!(
            result.unwrap_err().kind(),
            ToolRuntimeErrorKind::WorkspacePolicy
        );
        send.send(()).unwrap();
    });
    let result = receive.recv_timeout(Duration::from_secs(3));
    assert!(
        result.is_ok(),
        "context reread must refuse special files: {result:?}"
    );
}

#[test]
fn fifo_context_reread_is_refused_promptly() {
    refuses_context_special_file(false);
}

#[test]
fn device_context_reread_is_refused_promptly() {
    refuses_context_special_file(true);
}

#[tokio::test]
async fn regular_and_contained_symlink_context_reads_preserve_bytes() {
    let temporary = tempfile::tempdir().unwrap();
    let bytes = b"before\nneedle\nafter\n\xff";
    fs::write(temporary.path().join("file"), bytes).unwrap();
    std::os::unix::fs::symlink("file", temporary.path().join("link")).unwrap();
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(
            plakous_model::RunId::from_str("00000000-0000-0000-0000-000000000041").unwrap(),
        ),
        temporary.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )
    .unwrap();
    for path in ["file", "link"] {
        assert_eq!(
            read_context_bytes(
                &environment,
                Path::new(path),
                OperationContext::new(CancellationToken::new()),
            )
            .await
            .unwrap(),
            bytes
        );
    }
}
