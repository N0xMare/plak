//! A fresh API session placed on a remote runner whose OS is not this
//! process's: the prompt names the runner's OS, and the explicit plan is
//! compiled against the runner's backend, so the session opens on the runner
//! and answers its first turn.
use super::compaction::{endpoint_with_requests, runtime};
use super::*;
use plakous::runner::protocol::{Bind, VersionTriple};
use plakous::runner::{
    ClientWindows, RunnerClient, RunnerIdentity, RunnerOptions, RunnerService, RunnerServices,
};
use plakous::sessions::*;
use plakous::tools::runner::RunnerBackend;
use std::path::Path;
use std::sync::Arc;

use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};

/// The supported runner OS this process is not, with its containment.
fn foreign_os() -> (&'static str, &'static str) {
    if std::env::consts::OS == "linux" {
        ("macos", "HostPathPolicyOnly")
    } else {
        ("linux", "LinuxDescriptorRelativeNoFollow")
    }
}

/// An in-process runner serving `workspace`, whose hello descriptor is
/// rewritten to name `foreign_os()`: a host decodes it as a runner on that
/// OS, while its calls still reach this machine.
async fn foreign_runner(workspace: &Path) -> (RunnerServices, RunnerClient) {
    let version = VersionTriple::current("runner-placement", "debug");
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
        // Instruction discovery reads the workspace through the runner.
        RunnerOptions {
            extension: Some(crate::runner_reader::test_runner_extension()),
            ..RunnerOptions::default()
        },
    )
    .unwrap();
    let (host, runner) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move { service.serve_connection(runner).await });
    let (client, mut hello) = RunnerClient::connect(
        host,
        Bind {
            host_id: "host".into(),
            host_instance: "instance".into(),
            version,
            inspect: false,
        },
        ClientWindows::default(),
    )
    .await
    .unwrap();
    let (os, filesystem) = foreign_os();
    hello.descriptor["operating_system"] = os.into();
    hello.descriptor["containment"]["filesystem"] = filesystem.into();
    let backend = Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(client.clone())).unwrap());
    (
        RunnerServices::new(backend, tokio::runtime::Handle::current()),
        client,
    )
}

/// Creates a fresh OpenAI API session over `runner_services`, runs one turn,
/// and returns the one request the provider received.
async fn first_request(
    workspace: &Path,
    runner_services: Option<RunnerServices>,
) -> serde_json::Value {
    let extensions = crate::tools::ProductToolExtensions::default();
    let provider = PlatformProvider::OpenAi;
    let spec = ApiRuntimeSpec {
        runner_services,
        job_config: plakous::tools::JobConfig::default(),
        account: provider.account().parse().unwrap(),
        sidecars: &NoWorkers,
        product_home: None,
        literal_model: "gpt-5.6-luna",
        effort: Some(ReasoningEffort::Low),
        composition: CompositionSelection::Direct,
        wire: provider.protocol(),
        workspace_root: workspace,
        cwd: workspace,
        plan_state_authority: "runner-placement",
        opening_durable_session: false,
        tool_fence: crate::agent::SessionToolFence::Full,
        scheduled_compaction: false,
        max_model_rounds: crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
        topology_extensions: &extensions,
        telemetry: None,
        spawned_child: false,
    };
    let (client, origin, peer) =
        endpoint_with_requests(provider, "gpt-5.6-luna", None, 1, None).await;
    let binding = runtime(provider, &spec, client, origin).await;
    let store = tempfile::tempdir().unwrap();
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(store.path().join("sessions.sqlite")).unwrap())
            .unwrap();
    let created = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("runner-placement:create").unwrap(),
            binding,
        ))
        .await
        .unwrap_or_else(|error| panic!("the placed session was not created: {error:?}"));
    let CreateSessionOutcome::Created { session, .. } = created else {
        panic!("the placed session was not created")
    };
    let result = session
        .agent()
        .prompt("Return the synthetic fixture response without invoking tools.")
        .await
        .unwrap()
        .result()
        .await
        .unwrap();
    assert!(matches!(
        result.outcome(),
        plakous::agent::TurnResultOutcome::Completed(_)
    ));
    session.release().await.unwrap();
    // A liveness bound only: generous, so load never trips it.
    let mut requests = tokio::time::timeout(std::time::Duration::from_secs(30), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 1);
    requests.remove(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_session_on_a_foreign_os_runner_names_it_and_answers() {
    let (os, _) = foreign_os();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(workspace.path()).unwrap();
    let (services, client) = foreign_runner(&workspace).await;
    let placed = first_request(&workspace, Some(services)).await.to_string();
    client.disconnect();
    assert!(placed.contains(&format!("OS: {os}.")), "{placed}");
    assert!(
        !placed.contains(&format!("OS: {}.", std::env::consts::OS)),
        "{placed}"
    );

    let local = first_request(&workspace, None).await.to_string();
    assert!(
        local.contains(&format!("OS: {}.", std::env::consts::OS)),
        "{local}"
    );
}
