//! Equal remote spellings remain isolated through explicit service ownership.

use super::*;

struct VirtualRootLink {
    source: ExecutionEnvironment,
}

impl RunnerLink for VirtualRootLink {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        if call.verb() == RunnerVerb::ProbeWorkspace {
            let result = match call.payload()["op"].as_str() {
                Some("canonicalize") => {
                    let path = Path::new(call.payload()["path"].as_str().unwrap())
                        .components()
                        .collect::<std::path::PathBuf>();
                    Ok(json!({"path": path}))
                }
                Some("filesystem_identity") => {
                    Ok(serde_json::to_value(self.source.descriptor().root_object()).unwrap())
                }
                _ => Err(RunnerFailure::Unsupported),
            };
            return Box::pin(std::future::ready(result));
        }
        let mut payload = call.payload().clone();
        payload["root"] = json!(self.source.workspace().root());
        payload["cwd"] = json!(self.source.workspace().root());
        let served = serve(&self.source, call.verb().as_str(), payload, control());
        Box::pin(async move {
            served.await.map_err(|failure| match failure {
                ServeFailure::Unsupported => RunnerFailure::Unsupported,
                ServeFailure::Backend { error, .. } => RunnerFailure::Backend(error),
            })
        })
    }
}

fn placed(root: &Path, id: &str, logical: &Path) -> (RunnerServices, ExecutionEnvironment) {
    let source = ExecutionEnvironment::host(scope(), root, ".", config()).unwrap();
    let descriptor = runner_descriptor(&source, RunnerId::new(id).unwrap()).unwrap();
    let backend =
        Arc::new(RunnerBackend::new(descriptor, Arc::new(VirtualRootLink { source })).unwrap());
    let services = RunnerServices::new(backend, tokio::runtime::Handle::current());
    let environment =
        runner_environment(scope(), logical, ".", config(), services.backend()).unwrap();
    (services, environment)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equal_paths_on_two_runners_keep_bytes_and_admission_barriers_separate() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(first.path().join("marker"), "first").unwrap();
    std::fs::write(second.path().join("marker"), "second").unwrap();
    let logical = std::path::PathBuf::from(format!(
        "/runner-only-{}",
        first.path().file_name().unwrap().to_string_lossy()
    ));
    assert!(!logical.exists());
    let (first_service, first_environment) = placed(first.path(), "first", &logical);
    let (second_service, second_environment) = placed(second.path(), "second", &logical);
    let read = || WorkspaceReadSliceRequest::start("marker", 64).unwrap();
    let (left, right) = tokio::join!(
        first_environment.read_workspace(read(), control()),
        second_environment.read_workspace(read(), control())
    );
    assert_eq!(left.unwrap().bytes().as_ref(), b"first");
    assert_eq!(right.unwrap().bytes().as_ref(), b"second");
    first_service.set_available(false);
    assert!(second_service.is_available());
    assert_eq!(
        first_environment
            .read_workspace(read(), control())
            .await
            .unwrap()
            .bytes()
            .as_ref(),
        b"first",
        "admitted reads continue while new effects are blocked"
    );
    let effect = LiteralProcessRequest::new(
        "/bin/sh",
        ["-c".into(), "printf changed > side-effect".into()],
        ".",
        Vec::new(),
        LiteralProcessIntent::default(),
    )
    .unwrap();
    first_environment
        .execute_literal_process(effect, control())
        .await
        .unwrap_err();
    assert!(!first.path().join("side-effect").exists());
    assert!(!second.path().join("side-effect").exists());
    assert!(!logical.exists());
}
