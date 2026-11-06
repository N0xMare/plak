use super::*;
use std::time::Duration;

#[derive(Clone, Copy)]
enum FileVerb {
    Read,
    Query,
    Mutate,
}

fn refuses_promptly(verb: FileVerb, device: bool) {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let workspace = tempfile::tempdir().unwrap();
        let (root, path) = if device {
            (Path::new("/dev"), "null")
        } else {
            let status = std::process::Command::new("mkfifo")
                .arg(workspace.path().join("fifo"))
                .status()
                .unwrap();
            assert!(status.success());
            (workspace.path(), "fifo")
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (direct, remote) = pair(root);
            for environment in [direct, remote] {
                let error = match verb {
                    FileVerb::Read => environment
                        .read_workspace(
                            WorkspaceReadSliceRequest::start(path, 16).unwrap(),
                            control(),
                        )
                        .await
                        .unwrap_err(),
                    FileVerb::Query => environment
                        .query_repository(
                            RepositoryQuery::new(
                                path,
                                "needle",
                                RepositoryMatchCase::Sensitive,
                                limits(),
                            )
                            .unwrap(),
                            control(),
                        )
                        .await
                        .unwrap_err(),
                    FileVerb::Mutate => environment
                        .mutate_workspace(
                            WorkspaceMutationTransaction::new([WorkspaceMutation::new(
                                path,
                                WorkspaceExpectedFileState::Missing,
                                WorkspaceReplacement::Missing,
                            )
                            .unwrap()])
                            .unwrap(),
                            control(),
                        )
                        .await
                        .unwrap_err(),
                };
                assert_eq!(
                    error.kind(),
                    match verb {
                        FileVerb::Read => ExecutionBackendErrorKind::NotARegularFile,
                        FileVerb::Query | FileVerb::Mutate =>
                            ExecutionBackendErrorKind::WorkspacePolicy,
                    }
                );
            }
        });
        send.send(()).unwrap();
    });
    // A blocked open cannot be cancelled or joined. Keep the deadline outside
    // the runtime so even its blocking-pool shutdown cannot hang this test.
    let result = receive.recv_timeout(Duration::from_secs(3));
    assert!(
        result.is_ok(),
        "special file must be refused without a writer: {result:?}"
    );
}

#[test]
fn fifo_workspace_read_is_refused_promptly() {
    refuses_promptly(FileVerb::Read, false);
}

#[test]
fn fifo_query_root_is_refused_promptly() {
    refuses_promptly(FileVerb::Query, false);
}

#[test]
fn fifo_mutation_preflight_is_refused_promptly() {
    refuses_promptly(FileVerb::Mutate, false);
}

#[test]
fn device_workspace_read_is_refused_promptly() {
    refuses_promptly(FileVerb::Read, true);
}

#[test]
fn device_query_root_is_refused_promptly() {
    refuses_promptly(FileVerb::Query, true);
}

#[test]
fn device_mutation_preflight_is_refused_promptly() {
    refuses_promptly(FileVerb::Mutate, true);
}
