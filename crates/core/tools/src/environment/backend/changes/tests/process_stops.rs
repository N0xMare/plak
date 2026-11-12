use super::*;
use crate::environment::backend::changes::process::Executor;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::Instant;

fn shell(root: &Path, tail: &str) -> Command {
    let mut command = Command::new("sh");
    command.current_dir(root).arg("-c").arg(format!(
        "echo $$ > leader; sleep 60 & echo $! > member; {tail}"
    ));
    command
}

async fn terminated(root: &Path) {
    for name in ["leader", "member"] {
        let pid = std::fs::read_to_string(root.join(name)).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let output = std::process::Command::new("ps")
                    .args(["-p", pid.trim(), "-o", "stat="])
                    .output()
                    .unwrap();
                let state = String::from_utf8(output.stdout).unwrap();
                if !output.status.success() || state.trim_start().starts_with('Z') {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn cap_kills_the_whole_group_including_pipe_holders() {
    let root = tempfile::tempdir().unwrap();
    let executor = Executor::new(
        Instant::now() + Duration::from_secs(5),
        CancellationToken::new(),
        &std::env::var_os("PATH").unwrap(),
    );
    let error = executor
        .output(
            shell(
                root.path(),
                "n=0; while [ \"$((n += 1))\" -le 10000 ]; do printf 0123456789; done",
            ),
            32,
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error, ChangesRefusal::OutputLimit);
    terminated(root.path()).await;
}

#[tokio::test]
async fn deadline_and_cancellation_kill_stalled_groups() {
    for cancel in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cancellation = CancellationToken::new();
        let executor = Executor::new(
            Instant::now() + Duration::from_millis(200),
            cancellation.clone(),
            &std::env::var_os("PATH").unwrap(),
        );
        let answer = executor.output(shell(root.path(), "wait"), 1024);
        let cancel_task = async {
            if cancel {
                while !root.path().join("member").exists() {
                    tokio::task::yield_now().await;
                }
                cancellation.cancel();
            }
        };
        let (answer, ()) = tokio::join!(answer, cancel_task);
        assert_eq!(
            answer.err().unwrap(),
            if cancel {
                ChangesRefusal::Cancelled
            } else {
                ChangesRefusal::Deadline
            }
        );
        terminated(root.path()).await;
    }
}

#[tokio::test]
async fn dropping_a_request_kills_its_group() {
    let root = tempfile::tempdir().unwrap();
    let executor = Executor::new(
        Instant::now() + Duration::from_secs(5),
        CancellationToken::new(),
        &std::env::var_os("PATH").unwrap(),
    );
    {
        let answer = executor.output(shell(root.path(), "wait"), 1024);
        tokio::pin!(answer);
        tokio::select! {
            _ = &mut answer => panic!("unexpected completion"),
            () = async { while !root.path().join("member").exists() { tokio::task::yield_now().await; } } => {}
        }
    }
    terminated(root.path()).await;
}

#[tokio::test]
async fn a_stalled_filesystem_open_is_killed_at_the_five_second_deadline() {
    let root = checkout();
    let environment = environment(root.path());
    std::fs::rename(
        root.path().join(".git/config"),
        root.path().join(".git/config.saved"),
    )
    .unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(root.path().join(".git/config"))
            .status()
            .unwrap()
            .success()
    );
    let result = tokio::time::timeout(
        Duration::from_secs(7),
        ask(&environment, ChangesRequest::status()),
    )
    .await
    .unwrap();
    assert_eq!(result, ChangesAnswer::Refused(ChangesRefusal::Deadline));
}
