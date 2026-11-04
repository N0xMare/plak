use std::path::Path;

use plakous::runner::protocol::{Bind, VersionTriple};
use plakous::runner::{ClientWindows, Loopback, RunnerIdentity, RunnerOptions, RunnerService};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};

use super::*;

/// A loopback runner serving the tree's version control for the paths
/// `admits` accepts, and its link.
fn runner(
    root: &Path,
    admits: Arc<VcsPathAdmission>,
) -> (tokio::runtime::Runtime, Loopback, Arc<dyn RunnerLink>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let scope =
        ExecutionEnvironmentScope::Run("00000000-0000-0000-0000-000000000082".parse().unwrap());
    let service = RunnerService::with_options(
        ExecutionEnvironment::host(scope, root, ".", ExecutionEnvironmentConfig::default())
            .unwrap(),
        RunnerIdentity {
            runner_id: RunnerId::new("loopback").unwrap(),
            version: VersionTriple::current("remote-vcs", "test"),
            boot_id: "boot-1".into(),
            binary_id: None,
        },
        RunnerOptions {
            extension: Some(vcs_runner_extension(admits, None)),
            ..RunnerOptions::default()
        },
    )
    .unwrap();
    let loopback = runtime
        .block_on(Loopback::join(
            service,
            Bind {
                host_id: "host-1".into(),
                host_instance: "instance-1".into(),
                version: VersionTriple::current("remote-vcs", "test"),
                inspect: false,
            },
            ClientWindows::default(),
        ))
        .unwrap();
    let link = loopback.backend().link();
    (runtime, loopback, link)
}

/// Each effect step runs at most once per key: the same key sent again
/// after its answer runs nothing, and another key runs again.
#[test]
fn an_effect_step_runs_once_per_key() {
    let root = tempfile::tempdir().unwrap();
    let (runtime, _loopback, link) = runner(root.path(), Arc::new(|_| true));
    let hook = |key: &str| {
        RunnerCall::tree(
            RunnerVerb::VcsEffect,
            serde_json::to_value(VcsCall::MintHook {
                cwd: root.path().to_owned(),
                command: "echo ran >> count".to_owned(),
                env: BTreeMap::new(),
            })
            .unwrap(),
            "tree1",
            key,
        )
    };
    runtime.block_on(async {
        link.call(hook("plak:vcs_stage:a/0")).await.unwrap();
        // The answer was taken, so the key is spent: sent again it runs
        // nothing, and says so rather than running a second time.
        assert!(matches!(
            link.call(hook("plak:vcs_stage:a/0")).await,
            Err(RunnerFailure::Spent)
        ));
        link.call(hook("plak:vcs_stage:b/0")).await.unwrap();
    });
    assert_eq!(
        std::fs::read_to_string(root.path().join("count")).unwrap(),
        "ran\nran\n"
    );
}

/// The runner answers only about the paths it admits; any other is
/// unavailable, and an effect there runs nothing. A question sent as an
/// effect is not a call it answers.
#[test]
fn the_runner_answers_only_about_admitted_paths() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let inside = root.path().to_owned();
    let (runtime, _loopback, link) = runner(
        root.path(),
        Arc::new(move |path: &Path| path.starts_with(&inside)),
    );
    let vcs = RemoteVcs::new(Arc::clone(&link), "loopback".into(), "tree1".into());
    runtime.block_on(async {
        assert_eq!(vcs.path_exists(root.path()).await, Ok(true));
        let refused = vcs.path_exists(Path::new("/etc")).await.unwrap_err();
        assert_eq!(refused.kind, VcsErrorKind::MintUnavailable);
        assert!(refused.detail.contains("does not serve"), "{refused}");
        // A path that walks out of a root, or a relative one, never reaches
        // the admission.
        let walked = root
            .path()
            .join("..")
            .join(outside.path().file_name().unwrap());
        for path in [walked.as_path(), Path::new("relative")] {
            let refused = vcs.path_exists(path).await.unwrap_err();
            assert_eq!(refused.kind, VcsErrorKind::MintUnavailable, "{path:?}");
        }
        let hook = stage::unstaged(
            "a test probe",
            vcs.run_mint_hook(outside.path(), "touch ran", &BTreeMap::new()),
        )
        .await
        .unwrap_err();
        assert_eq!(hook.kind, VcsErrorKind::MintUnavailable);
        let question = link
            .call(RunnerCall::tree(
                RunnerVerb::VcsEffect,
                serde_json::to_value(VcsCall::PathExists {
                    path: root.path().to_owned(),
                })
                .unwrap(),
                "tree1",
                "plak:vcs:question",
            ))
            .await;
        assert!(question.is_err());
    });
    assert!(
        !outside.path().join("ran").exists(),
        "nothing ran outside the roots"
    );
    assert_eq!(vcs.stage_runner(), Some("loopback"));
}
