//! Workspace-only loopback runner for hosted construction regressions.

use plakous::runner::protocol::{Bind, VersionTriple};
use plakous::runner::{
    ClientWindows, RunnerClient, RunnerIdentity, RunnerOptions, RunnerService, RunnerServices,
};
use plakous::tools::runner::RunnerBackend;
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};
use std::path::Path;
use std::sync::Arc;

pub(crate) fn runner_serving(
    runtime: &tokio::runtime::Runtime,
    workspace: &Path,
    login_home: Option<&Path>,
) -> (RunnerServices, RunnerClient) {
    let admitted = workspace.to_path_buf();
    let admits: Arc<dyn Fn(&Path) -> bool + Send + Sync> =
        Arc::new(move |path: &Path| path.is_absolute() && path.starts_with(&admitted));
    let extension = super::runner_extension(
        plakous_recursive_subagents::remote_vcs::vcs_runner_extension(admits.clone(), None),
        admits,
        login_home.map(Path::to_path_buf),
    );
    let version = VersionTriple::current("remote-skills", "debug");
    let service = RunnerService::with_options(
        ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run(uuid::Uuid::new_v4().try_into().unwrap()),
            workspace,
            ".",
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap(),
        RunnerIdentity {
            runner_id: RunnerId::new("remote").unwrap(),
            version: version.clone(),
            boot_id: "boot".into(),
            binary_id: None,
        },
        RunnerOptions {
            extension: Some(extension),
            ..RunnerOptions::default()
        },
    )
    .unwrap();
    let (host, runner) = tokio::io::duplex(1 << 16);
    runtime.spawn(async move { service.serve_connection(runner).await });
    let (client, hello) = runtime
        .block_on(RunnerClient::connect(
            host,
            Bind {
                host_id: "host".into(),
                host_instance: "instance".into(),
                version,
                inspect: false,
            },
            ClientWindows::default(),
        ))
        .unwrap();
    let backend = Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(client.clone())).unwrap());
    (
        RunnerServices::new(backend, runtime.handle().clone()),
        client,
    )
}

pub(crate) fn write_skill(root: &Path, name: &str) {
    let directory = root.join(".agents/skills").join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: The {name} skill.\n---\nSteps.\n"),
    )
    .unwrap();
}
