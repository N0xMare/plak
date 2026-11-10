//! Production builders must refresh jobs under the current runner owner.

use super::*;
use plakous::runner::protocol::{Bind, VersionTriple};
use plakous::runner::{
    ClientWindows, RunnerClient, RunnerIdentity, RunnerOptions, RunnerService, RunnerServices,
};
use plakous::tools::runner::{LocalDaemonBackend, RunnerBackend};
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};

fn production_transition(placed: bool, switch: bool) {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut parent) = api_topology_root_parent(
        root.path(),
        None,
        crate::binding::PlatformProvider::OpenAi,
        "gpt-5.6-luna",
        crate::skills::SkillsSetting::default(),
        None,
    );
    let runner_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (client, backend) = runner_runtime.block_on(async {
        let version = VersionTriple::current("runner-owner", "test");
        let admitted = config.cwd.clone();
        let admits: Arc<dyn Fn(&std::path::Path) -> bool + Send + Sync> =
            Arc::new(move |path| path.is_absolute() && path.starts_with(&admitted));
        let extension = crate::runner_reader::runner_extension(
            plakous_recursive_subagents::remote_vcs::vcs_runner_extension(admits.clone(), None),
            admits,
            None,
        );
        let service = RunnerService::with_options(
            ExecutionEnvironment::host(
                ExecutionEnvironmentScope::Run(uuid::Uuid::new_v4().try_into().unwrap()),
                &config.cwd,
                &config.cwd,
                ExecutionEnvironmentConfig::default(),
            )
            .unwrap(),
            RunnerIdentity {
                runner_id: RunnerId::new(if placed { "placed" } else { "home-daemon" }).unwrap(),
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
        let (host, runner) = tokio::io::duplex(65536);
        tokio::spawn(async move { service.serve_connection(runner).await });
        let (client, hello) = RunnerClient::connect(
            host,
            Bind {
                host_id: "home".into(),
                host_instance: "host".into(),
                version,
                inspect: false,
            },
            ClientWindows::default(),
        )
        .await
        .unwrap();
        let backend =
            Arc::new(RunnerBackend::new(hello.descriptor, Arc::new(client.clone())).unwrap());
        (client, backend)
    });
    config.state_roots = if placed {
        config
            .state_roots
            .with_runner_services(RunnerServices::new(
                backend,
                runner_runtime.handle().clone(),
            ))
            .with_runner_client(client.clone())
    } else {
        config.state_roots.with_local_daemon(
            LocalDaemonBackend::new(
                Arc::new(client.clone()),
                runner_runtime.handle().clone(),
                std::env::var("PATH").unwrap_or_default(),
                None,
            ),
            client.clone(),
        )
    };
    parent.state_roots = config.state_roots.clone();
    let opened = executor
        .block_on(crate::agent::build_api_root_coding_session(
            parent,
            "production-runner-owner",
            None,
        ))
        .unwrap()
        .unwrap();
    let (mut config, mut opened) = bind_api_topology_root(config, opened);
    let source_node = config.session.node.clone();
    let mut roster = crate::orchestration::RosterInput::unpinned("gpt-5.6-sol");
    roster.root.account = "openai".to_owned();
    roster.root.effort = Some("medium".to_owned());
    let (events, _notes) = host_event_sink();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let result = executor
        .block_on(handle_rebind(
            InterruptWatch::new(&receiver, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "openai",
            "gpt-5.6-sol",
            Some("medium"),
            None,
            Some(ProductComposition::Direct),
            None,
            &roster,
            switch.then_some("successor"),
        ))
        .unwrap();
    assert!(
        result.is_ok(),
        "placed={placed}, switch={switch}: {result:?}"
    );
    assert_eq!(config.session.node == source_node, switch);
    let opened = reopen_exactly(&executor, &mut config, opened);
    executor.block_on(opened.release()).unwrap().unwrap();
    drop(config);
    client.disconnect();
}

#[test]
fn production_profile_switch_reopens_the_successors_runner_port() {
    for placed in [false, true] {
        production_transition(placed, true);
    }
}

#[test]
fn production_profile_fork_reopens_the_successors_runner_port() {
    for placed in [false, true] {
        production_transition(placed, false);
    }
}
