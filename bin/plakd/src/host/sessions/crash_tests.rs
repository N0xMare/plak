use super::*;
use plakous_host_protocol::wire::RunnerLabel;
use plakous_hosted::host_service::wire::Creation;

const REQUEST: &str = "00000000-0000-4000-8000-000000000001";

#[test]
fn create_crash_worker() {
    let Ok(home) = std::env::var("PLAK_LIFECYCLE_CRASH_HOME") else {
        return;
    };
    let workspace = std::env::var("PLAK_CREATE_CRASH_WORKSPACE").unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(home.into());
    let services = Services::for_home(paths)
        .with_runner_overlap(plakous::runner::MachineOverlapGuard::new(None, []));
    let mut product = ProductSessions::new(&services, None, None).unwrap();
    product.placement = Box::new(placement::LocalPlacement {
        product_home: services.paths().unwrap().home().to_owned(),
    });
    let runner = RunnerLabel::local();
    let replay = std::env::var("PLAK_CREATE_CRASH_REPLAY").is_ok();
    product
        .create(&Creation {
            runner: &runner,
            workspace: &workspace,
            profile: replay.then_some("removed-profile"),
            template: None,
            model: None,
            request_id: REQUEST,
        })
        .unwrap();
}

#[test]
fn host_creation_commit_crash_cuts_replay_only_durable_facts() {
    for after in [false, true] {
        let (temporary, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let worker = || {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "host::sessions::crash_tests::create_crash_worker",
                    "--nocapture",
                ])
                .env("PLAK_LIFECYCLE_CRASH_HOME", paths.home())
                .env("PLAK_CREATE_CRASH_WORKSPACE", &workspace);
            command
        };
        let output = worker()
            .env(
                "PLAK_LIFECYCLE_CRASH_CUT",
                if after {
                    "host_create_after_commit"
                } else {
                    "host_create_before_commit"
                },
            )
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(91),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let mut store = ProductStore::open(&paths).unwrap();
        let rows = store.directory(None).unwrap();
        assert_eq!(rows.len(), usize::from(after));
        if after {
            assert!(store.load(&rows[0].id).unwrap().plakous_locator.is_none());
            std::fs::remove_dir(&workspace).unwrap();
        }
        drop(store);
        let mut retry = worker();
        retry.env_remove("PLAK_LIFECYCLE_CRASH_CUT");
        if after {
            retry.env("PLAK_CREATE_CRASH_REPLAY", "1");
        }
        let output = retry.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let mut store = ProductStore::open(&paths).unwrap();
        assert_eq!(store.directory(None).unwrap().len(), 1);
    }
}
