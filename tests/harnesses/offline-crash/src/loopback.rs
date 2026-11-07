//! The test-only route that runs a process's sessions on an in-process
//! loopback runner (spec 01 §8.5): the runner serves every workspace a
//! session names from this machine's filesystem, and the host reaches it
//! only through the runner seam, as it would a remote runner.

use std::sync::{Arc, OnceLock};

use plakous::model::RunId;
use plakous::tools::runner::{RunnerExtension, RunnerServices};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};
use plakous_runner::{ClientWindows, Loopback, RunnerIdentity, RunnerOptions, RunnerService};
use plakous_runner_protocol::{Bind, VersionTriple};

/// The runner and the runtime its link runs on, kept for the process.
struct Route {
    services: RunnerServices,
    _runtime: tokio::runtime::Runtime,
    _loopback: Loopback,
}

static ROUTE: OnceLock<Route> = OnceLock::new();

/// Explicit test-route services for the fixture composition root to inject.
#[must_use]
pub fn services() -> Option<RunnerServices> {
    ROUTE.get().map(|route| route.services.clone())
}

/// Selects the loopback runner for every session this process constructs;
/// `extension` answers the reads the embedding program serves, such as
/// instruction sources. The embedder calls it when the offline script asks
/// for the loopback runner.
///
/// # Errors
/// Fails when the runner cannot start.
pub fn select(extension: Arc<dyn RunnerExtension>) -> Result<(), String> {
    if ROUTE.get().is_some() {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("plak-loopback-runner")
        .enable_all()
        .build()
        .map_err(|error| format!("the loopback runner's runtime did not start: {error}"))?;
    let scope = ExecutionEnvironmentScope::Run(
        RunId::try_from(uuid::Uuid::from_bytes([0x5a; 16])).map_err(|error| error.to_string())?,
    );
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let environment =
        ExecutionEnvironment::host(scope, &cwd, &cwd, ExecutionEnvironmentConfig::default())
            .map_err(|error| error.to_string())?;
    let version = VersionTriple::current("loopback", "test");
    let service = RunnerService::with_options(
        environment,
        RunnerIdentity {
            runner_id: RunnerId::new("loopback").map_err(|error| error.to_string())?,
            version: version.clone(),
            boot_id: "loopback".into(),
            binary_id: None,
        },
        RunnerOptions {
            workspaces: Some(Arc::new(move |root, cwd| {
                ExecutionEnvironment::host(scope, root, cwd, ExecutionEnvironmentConfig::default())
            })),
            extension: Some(extension),
            ..RunnerOptions::default()
        },
    )
    .map_err(|error| error.to_string())?;
    let bind = Bind {
        host_id: "loopback-host".into(),
        host_instance: uuid::Uuid::new_v4().to_string(),
        version,
        inspect: false,
    };
    let loopback = runtime
        .block_on(Loopback::join(service, bind, ClientWindows::default()))
        .map_err(|error| error.to_string())?;
    let services = RunnerServices::new(loopback.backend(), runtime.handle().clone());
    let _ = ROUTE.set(Route {
        services,
        _runtime: runtime,
        _loopback: loopback,
    });
    Ok(())
}
