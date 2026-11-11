//! The scripted child daemon route, with the same windows as product dials.

use std::sync::{Arc, OnceLock};

use plakous::runner::{CallJournal, FileCallJournal, RunnerClient};
use plakous::tools::runner::RunnerBackend;
use plakous_offline_crash_harness::script::ChildRunnerPlan;

/// The link and the runtime it runs on, kept for the process.
struct Route {
    services: plakous::runner::RunnerServices,
    _runtime: tokio::runtime::Runtime,
    _bridge: tokio::process::Child,
}

static ROUTE: OnceLock<Route> = OnceLock::new();

pub(super) fn services() -> Option<plakous::runner::RunnerServices> {
    ROUTE.get().map(|route| route.services.clone())
}

pub(super) fn select(plan: &ChildRunnerPlan) -> Result<(), String> {
    if ROUTE.get().is_some() {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("plak-child-runner")
        .enable_all()
        .build()
        .map_err(|error| format!("the child runner's runtime did not start: {error}"))?;
    // A cleared environment: the bridge must not read this process's
    // offline script and select a runner of its own.
    let mut bridge =
        tokio::process::Command::new(std::env::current_exe().map_err(|error| error.to_string())?);
    bridge
        .args(["runner", "connect", "--stdio"])
        .env_clear()
        .env(
            "PATH",
            crate::environment::inherited_path().unwrap_or_default(),
        )
        .env("HOME", &plan.home)
        .env("PLAK_HOME", &plan.home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let guard = runtime.enter();
    let mut bridge = bridge
        .spawn()
        .map_err(|error| format!("the child runner's bridge did not start: {error}"))?;
    let (Some(stdin), Some(stdout)) = (bridge.stdin.take(), bridge.stdout.take()) else {
        return Err("the child runner's bridge gave no link".to_owned());
    };
    let journal: Arc<dyn CallJournal> = Arc::new(
        FileCallJournal::open(plan.home.join("host-calls"))
            .map_err(|error| format!("the child runner's call journal failed: {error}"))?,
    );
    let bind = crate::commands::runner::bind(plan.host_id.clone(), false);
    let (client, hello) = runtime
        .block_on(RunnerClient::connect_journaled(
            tokio::io::join(stdout, stdin),
            bind,
            super::runner_windows(),
            Some(journal),
        ))
        .map_err(|error| format!("the child runner did not bind: {error}"))?;
    let backend = RunnerBackend::new(hello.descriptor, Arc::new(client))
        .map_err(|error| format!("the child runner's descriptor: {error}"))?;
    let services =
        plakous::runner::RunnerServices::new(Arc::new(backend), runtime.handle().clone());
    drop(guard);
    let _ = ROUTE.set(Route {
        services,
        _runtime: runtime,
        _bridge: bridge,
    });
    Ok(())
}
