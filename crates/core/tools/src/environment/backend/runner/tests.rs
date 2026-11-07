use std::str::FromStr;
use std::sync::Arc;

use plakous_model::{CancellationToken, RunId};
use serde_json::json;

use super::*;
use crate::{
    ExecutionEnvironmentConfig, ExecutionEnvironmentLimits, LiteralProcessIntent,
    LiteralProcessRequest, RepositoryMatchCase, RepositoryQueryLimits, WorkspaceExpectedFileState,
    WorkspaceMutation, WorkspaceReplacement,
};

mod services;
mod special_files;

/// A link that answers each call by serving it from `runner`, in process.
struct DirectLink {
    runner: ExecutionEnvironment,
}

impl RunnerLink for DirectLink {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        let served = if call.verb() == RunnerVerb::ProbeWorkspace {
            serve_probe(call.payload().clone(), &|_| true)
        } else {
            serve(
                &self.runner,
                call.verb().as_str(),
                call.payload().clone(),
                OperationContext::new(call.cancellation().clone()),
            )
        };
        Box::pin(async move {
            served.await.map_err(|failure| match failure {
                ServeFailure::Unsupported => RunnerFailure::Unsupported,
                ServeFailure::Backend { error, .. } => RunnerFailure::Backend(error),
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_workspace_identity_never_requires_a_host_path_and_local_bytes_stay_identical() {
    struct RemoteProbe;
    impl RunnerLink for RemoteProbe {
        fn call(&self, call: RunnerCall) -> RunnerCallFuture {
            let answer = match call.payload()["op"].as_str() {
                Some("canonicalize") => {
                    let path = Path::new(call.payload()["path"].as_str().unwrap())
                        .components()
                        .collect::<std::path::PathBuf>();
                    Ok(json!({"path": path}))
                }
                Some("filesystem_identity") => Ok(json!({"device": 123, "inode": 456})),
                _ => Err(RunnerFailure::Unsupported),
            };
            Box::pin(std::future::ready(answer))
        }
    }
    let root = tempfile::tempdir().unwrap();
    let direct = ExecutionEnvironment::host(scope(), root.path(), ".", config()).unwrap();
    let old_policy = crate::WorkspacePolicy::host(root.path(), ".").unwrap();
    let probed_policy =
        crate::WorkspacePolicy::on_runner(root.path(), Path::new("."), &crate::LocalWorkspaceProbe)
            .unwrap();
    let old = ExecutionEnvironment::from_workspace_and_backend(
        scope(),
        old_policy,
        config(),
        direct.operation_backend.clone(),
    )
    .unwrap();
    let probed = ExecutionEnvironment::from_workspace_and_backend(
        scope(),
        probed_policy,
        config(),
        direct.operation_backend.clone(),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_vec(old.descriptor()).unwrap(),
        serde_json::to_vec(probed.descriptor()).unwrap()
    );
    let descriptor = runner_descriptor(&direct, RunnerId::new("remote-probe").unwrap()).unwrap();
    let backend = Arc::new(RunnerBackend::new(descriptor, Arc::new(RemoteProbe)).unwrap());
    let absent = root.path().join("remote-only");
    assert!(!absent.exists());
    let remote = runner_environment(scope(), &absent, ".", config(), backend).unwrap();
    assert_eq!(remote.descriptor().root_object().device(), 123);
    assert_eq!(remote.descriptor().root_object().inode(), 456);
    assert!(remote.matches_descriptor(remote.descriptor()));
    assert!(!absent.exists());
}

fn scope() -> ExecutionEnvironmentScope {
    ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000041").unwrap())
}

fn config() -> ExecutionEnvironmentConfig {
    ExecutionEnvironmentConfig::new(ExecutionEnvironmentLimits::default(), [], []).unwrap()
}

/// The direct environment and one whose operations run through the seam on
/// a second environment over the same workspace.
fn pair(root: &Path) -> (ExecutionEnvironment, ExecutionEnvironment) {
    let direct = ExecutionEnvironment::host(scope(), root, ".", config()).unwrap();
    let runner = ExecutionEnvironment::host(scope(), root, ".", config()).unwrap();
    let descriptor = runner_descriptor(&runner, RunnerId::new("loopback").unwrap()).unwrap();
    let backend = RunnerBackend::new(descriptor, Arc::new(DirectLink { runner })).unwrap();
    let remote = runner_environment(scope(), root, ".", config(), Arc::new(backend)).unwrap();
    (direct, remote)
}

fn control() -> OperationContext {
    OperationContext::new(CancellationToken::new())
}

fn limits() -> RepositoryQueryLimits {
    RepositoryQueryLimits::new(100, 100, 1 << 20, 1 << 22, 100, 4096, 1 << 20).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_seam_answers_each_verb_as_the_direct_backend_does() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("a.txt"), "alpha\nbeta needle\n").unwrap();
    let (direct, remote) = pair(workspace.path());
    assert_eq!(
        remote
            .descriptor()
            .backend()
            .runner_id()
            .map(RunnerId::as_str),
        Some("loopback")
    );

    let read = || WorkspaceReadSliceRequest::start("a.txt", 4).unwrap();
    let (left, right) = (
        direct.read_workspace(read(), control()).await.unwrap(),
        remote.read_workspace(read(), control()).await.unwrap(),
    );
    assert_eq!(left, right);
    let resume = || WorkspaceReadSliceRequest::resume(left.next().unwrap().clone(), 64).unwrap();
    assert_eq!(
        direct.read_workspace(resume(), control()).await.unwrap(),
        remote.read_workspace(resume(), control()).await.unwrap()
    );

    for query in [
        RepositoryQuery::new(".", "needle", RepositoryMatchCase::Sensitive, limits()).unwrap(),
        RepositoryQuery::regex(".", "^be", RepositoryMatchCase::AsciiInsensitive, limits())
            .unwrap()
            .with_path_glob("*.txt")
            .unwrap(),
        RepositoryQuery::list(".", None, 2, limits()).unwrap(),
    ] {
        assert_eq!(
            direct
                .query_repository(query.clone(), control())
                .await
                .unwrap(),
            remote.query_repository(query, control()).await.unwrap()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_seam_runs_processes_and_mutations_as_the_direct_backend_does() {
    let workspace = tempfile::tempdir().unwrap();
    let (direct, remote) = pair(workspace.path());
    let process = LiteralProcessRequest::new(
        "/bin/sh",
        [
            "-c".to_owned(),
            "cat; echo out; echo err >&2; exit 3".to_owned(),
        ],
        ".",
        b"in\n".to_vec(),
        LiteralProcessIntent::default(),
    )
    .unwrap();
    assert_eq!(
        direct
            .execute_literal_process(process.clone(), control())
            .await
            .unwrap(),
        remote
            .execute_literal_process(process, control())
            .await
            .unwrap()
    );

    let created = WorkspaceMutationTransaction::new([WorkspaceMutation::new(
        "b.txt",
        WorkspaceExpectedFileState::Missing,
        WorkspaceReplacement::content("bee"),
    )
    .unwrap()])
    .unwrap();
    let result = remote
        .mutate_workspace(created.clone(), control())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(workspace.path().join("b.txt")).unwrap(),
        b"bee"
    );
    let conflict = remote
        .mutate_workspace(created.clone(), control())
        .await
        .unwrap_err();
    let direct_conflict = direct
        .mutate_workspace(created, control())
        .await
        .unwrap_err();
    assert_eq!(conflict.kind(), direct_conflict.kind());
    assert_eq!(
        conflict.proven_no_effect(),
        direct_conflict.proven_no_effect()
    );
    assert_eq!(
        conflict.workspace_conflict_path(),
        direct_conflict.workspace_conflict_path()
    );
    assert_eq!(result.items().len(), 1);

    let missing = || WorkspaceReadSliceRequest::start("absent.txt", 4).unwrap();
    let (left, right) = (
        direct
            .read_workspace(missing(), control())
            .await
            .unwrap_err(),
        remote
            .read_workspace(missing(), control())
            .await
            .unwrap_err(),
    );
    assert_eq!(
        (left.kind(), left.message()),
        (right.kind(), right.message())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_without_streams_carries_no_services() {
    let workspace = tempfile::tempdir().unwrap();
    let (_, remote) = pair(workspace.path());
    let request = crate::ServiceProcessRequest::new(
        "/bin/cat",
        Vec::<String>::new(),
        Vec::<(crate::EnvironmentVariableName, String)>::new(),
        Vec::<(crate::EnvironmentVariableName, crate::ServiceSecretValue)>::new(),
        1024,
    )
    .unwrap();
    let error = remote.spawn_service(request, control()).await.unwrap_err();
    assert_eq!(error.kind(), ExecutionBackendErrorKind::Unsupported);
    assert!(error.proven_no_effect());
}

#[test]
fn a_runner_backend_presents_only_a_named_runner() {
    let workspace = tempfile::tempdir().unwrap();
    let runner = ExecutionEnvironment::host(scope(), workspace.path(), ".", config()).unwrap();
    let link = || -> Arc<dyn RunnerLink> {
        Arc::new(DirectLink {
            runner: runner.clone(),
        })
    };
    assert!(runner_descriptor(&runner, RunnerId::local()).is_err());
    let mut local = serde_json::to_value(runner.descriptor().backend()).unwrap();
    assert!(RunnerBackend::new(local.clone(), link()).is_err());
    local["kind"] = json!("deterministic_fake");
    assert!(RunnerBackend::new(local, link()).is_err());
    assert!(RunnerBackend::new(json!({"kind": "runner"}), link()).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_runner_side_refuses_another_workspace_and_unknown_verbs() {
    let workspace = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let runner = ExecutionEnvironment::host(scope(), workspace.path(), ".", config()).unwrap();
    let request =
        wire::encode_read_request(&WorkspaceReadSliceRequest::start("a", 1).unwrap()).unwrap();
    let payload = json!({
        "root": other.path().canonicalize().unwrap().to_str().unwrap(),
        "cwd": other.path().canonicalize().unwrap().to_str().unwrap(),
        "request": request,
    });
    match serve(&runner, "workspace.read", payload.clone(), control()).await {
        Err(ServeFailure::Backend {
            error,
            proven_no_effect,
        }) => {
            assert!(proven_no_effect);
            assert_eq!(
                wire::decode_error(error).kind(),
                ExecutionBackendErrorKind::WorkspacePolicy
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        serve(&runner, "process.start", payload, control()).await,
        Err(ServeFailure::Unsupported)
    );
}

#[test]
fn every_verb_has_one_wire_name() {
    for verb in RunnerVerb::ALL {
        assert_eq!(RunnerVerb::parse(verb.as_str()), Some(verb));
    }
    assert_eq!(RunnerVerb::parse("process.spawn"), None);
    assert_eq!(
        RunnerVerb::ALL
            .iter()
            .filter(|verb| verb.family() == RunnerVerbFamily::Environment)
            .count(),
        4
    );
}

#[test]
fn wire_values_refuse_what_no_constructor_produces() {
    let query = wire::encode_query(
        &RepositoryQuery::new(".", "x", RepositoryMatchCase::Sensitive, limits()).unwrap(),
    )
    .unwrap();
    let mut listed = query.clone();
    listed["list_only"] = json!(true);
    assert!(wire::decode_query(listed).is_err());
    let mut traversal = query;
    traversal["root"] = json!("../outside");
    assert!(wire::decode_query(traversal).is_err());
    assert!(
        wire::decode_read_request(
            json!({"path": "a", "expected": null, "offset": 3, "max_bytes": 1})
        )
        .is_err()
    );
    assert!(
        wire::decode_read_slice(json!({
            "content": {"byte_len": 1, "digest": null}, "offset": 0, "bytes": "", "next": null
        }))
        .is_err()
    );
    let error = wire::decode_error(json!({"kind": "teleported", "message": "x",
        "proven_no_effect": true, "conflict_path": null}));
    assert_eq!(error.kind(), ExecutionBackendErrorKind::RecoveryRequired);
    assert!(!error.proven_no_effect());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_environment_refuses_every_process_start() {
    let workspace = tempfile::tempdir().unwrap();
    let (_, remote) = pair(workspace.path());
    let sessions = remote.joined_process_sessions().unwrap();
    let request = crate::JoinedProcessStartRequest::new(
        crate::JoinedProcessCommand::literal(
            "/bin/sh",
            ["-c".to_owned(), "printf ran > ran.txt".to_owned()],
        )
        .unwrap(),
        ".",
        Vec::new(),
        false,
        crate::JoinedProcessTerminalMode::Pipes,
    )
    .unwrap();
    let error = sessions.start(request, control()).unwrap_err();
    assert_eq!(error.kind(), ExecutionBackendErrorKind::Unsupported);
    assert!(error.proven_no_effect());
    assert!(!workspace.path().join("ran.txt").exists());
    assert_eq!(sessions.jobs(), Vec::new());
}
