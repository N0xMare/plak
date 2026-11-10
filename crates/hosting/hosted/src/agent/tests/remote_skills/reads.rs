//! Stop and file-replacement boundaries while collecting runner skill slices.

use super::*;
use plakous::tools::runner::{RunnerCall, RunnerCallFuture, RunnerLink};
use plakous::tools::{ToolPack, ToolRegistryBuilder};
use std::time::Duration;

struct ReadFixture {
    runtime: tokio::runtime::Runtime,
    _root: tempfile::TempDir,
    workspace: PathBuf,
    user_root: PathBuf,
    services: plakous::runner::RunnerServices,
    client: plakous::runner::RunnerClient,
}

impl ReadFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(root.path()).unwrap();
        let workspace = path.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let home = path.join("runner-home");
        write_skill(&home, "review");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (services, client) = runner_serving(&runtime, &workspace, Some(&home));
        Self {
            runtime,
            _root: root,
            workspace,
            user_root: home.join(".agents/skills"),
            services,
            client,
        }
    }

    fn registry(&self, link: Arc<dyn RunnerLink>) -> plakous::tools::FrozenToolRegistry {
        let backend = plakous::tools::runner::RunnerBackend::new(
            serde_json::to_value(self.services.backend().descriptor()).unwrap(),
            link,
        )
        .unwrap();
        let services =
            plakous::runner::RunnerServices::new(Arc::new(backend), self.runtime.handle().clone());
        let pack = plakous::tools::FilesToolPack::with_runner_skill_roots(
            [self.user_root.clone()],
            services,
        );
        let mut builder = ToolRegistryBuilder::default();
        for registration in pack.registrations().unwrap() {
            builder.register(registration).unwrap();
        }
        builder.freeze()
    }
}

struct StalledRead(Arc<tokio::sync::Notify>);

impl RunnerLink for StalledRead {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        assert_eq!(call.payload()["op"], "read_skill_slice");
        let entered = self.0.clone();
        Box::pin(async move {
            entered.notify_one();
            std::future::pending().await
        })
    }
}

#[test]
fn stop_cancels_a_stalled_runner_skill_read_without_waiting_for_its_answer() {
    let fixture = ReadFixture::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let registry = fixture.registry(Arc::new(StalledRead(entered.clone())));
    let cancellation = CancellationToken::new();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let path = fixture.user_root.join("review/SKILL.md");
    let answer = executor
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(30), async {
                let stop = async {
                    entered.notified().await;
                    cancellation.cancel();
                };
                let read = read_file_with_cancellation(
                    &registry,
                    &fixture.workspace,
                    &path,
                    cancellation.clone(),
                );
                let (answer, ()) = tokio::join!(read, stop);
                answer
            })
            .await
        })
        .unwrap()
        .expect("Stop must not await the stalled runner")
        .unwrap_err();
    assert_eq!(
        answer
            .downcast_ref::<plakous::tools::ToolRuntimeError>()
            .unwrap()
            .kind(),
        plakous::tools::ToolRuntimeErrorKind::Cancelled
    );
    fixture.client.disconnect();
}

struct ReplaceBetweenSlices {
    inner: Arc<dyn RunnerLink>,
    replacement: PathBuf,
    path: PathBuf,
    replaced: std::sync::atomic::AtomicBool,
}

impl RunnerLink for ReplaceBetweenSlices {
    fn call(&self, call: RunnerCall) -> RunnerCallFuture {
        if call.payload()["offset"]
            .as_u64()
            .is_some_and(|offset| offset != 0)
            && !self.replaced.swap(true, Ordering::Relaxed)
        {
            std::fs::rename(&self.replacement, &self.path).unwrap();
        }
        self.inner.call(call)
    }
}

#[test]
fn replacing_a_runner_skill_file_between_slices_refuses_the_whole_page() {
    let fixture = ReadFixture::new();
    let path = fixture.user_root.join("review/large.txt");
    let replacement = fixture.user_root.join("review/replacement.txt");
    let length = plakous_tools::MAX_STANDARD_TEXT_BYTES as usize + 64;
    std::fs::write(&path, vec![b'a'; length]).unwrap();
    std::fs::write(&replacement, vec![b'b'; length]).unwrap();
    let registry = fixture.registry(Arc::new(ReplaceBetweenSlices {
        inner: fixture.services.backend().link(),
        replacement,
        path: path.clone(),
        replaced: std::sync::atomic::AtomicBool::new(false),
    }));
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let error = executor
        .block_on(read_file(&registry, &fixture.workspace, &path))
        .unwrap()
        .unwrap_err();
    assert!(
        error.to_string().contains("changed between read slices"),
        "{error}"
    );
    fixture.client.disconnect();
}
