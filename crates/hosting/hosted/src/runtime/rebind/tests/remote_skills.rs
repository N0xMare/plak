//! A session placed on a runner opens with skills on: discovery reads the
//! project root through the runner, which serves only its own roots, and
//! never this host's login home (spec 02 §6.1.1).

use super::*;
use plakous::runner::protocol::{Bind, VersionTriple};
use plakous::runner::{
    ClientWindows, RunnerClient, RunnerIdentity, RunnerOptions, RunnerService, RunnerServices,
};
use plakous::tools::runner::RunnerBackend;
use plakous::tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
};
use std::path::Path;

/// A runner on this machine serving only `workspace`, as a daemon whose
/// roots name it does: the production extension with its roots predicate.
fn runner_serving(
    runtime: &tokio::runtime::Runtime,
    workspace: &Path,
) -> (RunnerServices, RunnerClient) {
    let admitted = workspace.to_path_buf();
    let admits: Arc<dyn Fn(&Path) -> bool + Send + Sync> =
        Arc::new(move |path: &Path| path.is_absolute() && path.starts_with(&admitted));
    let extension = crate::runner_reader::runner_extension(
        plakous_recursive_subagents::remote_vcs::vcs_runner_extension(admits.clone(), None),
        admits,
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

fn write_skill(root: &Path, name: &str) {
    let directory = root.join(".agents/skills").join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: The {name} skill.\n---\nSteps.\n"),
    )
    .unwrap();
}

fn listing(opened: &AttachedCodingSession) -> Option<String> {
    instruction_sources(opened)
        .unwrap()
        .iter()
        .find(|source| source.source_id() == plakous::prompts::skills::SKILLS_SOURCE_ID)
        .map(|source| source.content().to_owned())
}

/// The first open of a runner-placed session builds its runtime and
/// discovers skills, as a host's first attach of a `new_session` does; an
/// exact reopen's discovery against the listing it froze reads the same
/// runner. Both succeed with the project skill listed, and the login home's
/// user root, which the runner does not serve, is never read.
#[test]
fn a_runner_placed_session_opens_with_skills_on() {
    let root = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root.path()).unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let runner_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let login_home = root.join("login");
    write_skill(&login_home, "host-only");
    let (mut config, mut parent) = api_topology_root_parent(
        &root,
        None,
        crate::binding::PlatformProvider::Anthropic,
        "claude-sonnet-5",
        crate::skills::SkillsSetting::new(true, Some(login_home)),
        None,
    );
    let workspace = std::fs::canonicalize(&config.session.workspace_root).unwrap();
    write_skill(&workspace, "review-pr");

    let (services, client) = runner_serving(&runner_runtime, &workspace);
    // As the host's placement does: the node's services and the shared
    // client's control port.
    config.state_roots = config
        .state_roots
        .clone()
        .with_runner_services(services.clone())
        .with_runner_client(client.clone());
    parent.state_roots = parent
        .state_roots
        .with_runner_services(services)
        .with_runner_client(client.clone());
    let opened = executor
        .block_on(crate::agent::build_api_root_coding_session(
            parent,
            "offline-topology-root",
            None,
        ))
        .unwrap()
        .unwrap_or_else(|error| panic!("the placed session did not open: {}", error.message()));
    let first = listing(&opened).expect("the project skill is listed");
    assert!(first.contains("review-pr"), "{first}");
    assert!(!first.contains("host-only"), "{first}");
    let (config, opened) = bind_api_topology_root(config, opened);
    executor.block_on(opened.release()).unwrap().unwrap();
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let frozen = executor
        .block_on(crate::skills::frozen_skills(
            &sessions,
            config.session.plakous_locator.unwrap(),
        ))
        .unwrap()
        .unwrap();
    assert!(frozen.on);

    // An exact reopen discovers again against the listing it froze, through
    // the same runner.
    let reopened = crate::skills::NodeSkills::discover(
        config.state_roots.skills(),
        crate::skills::SkillsMode::Frozen(&frozen),
        &workspace,
        plakous::prompts::skills::SkillsPresentation::Claude,
        config.state_roots.runner_services(),
    )
    .unwrap_or_else(|error| panic!("the placed session did not reopen: {}", error.message()));
    assert!(reopened.presented());
    assert_eq!(reopened.read_roots(), [] as [PathBuf; 0]);
    client.disconnect();
}

/// A runner that cannot answer still fails discovery closed, and the
/// refusal names what the link said rather than only "unavailable".
#[test]
fn an_unreachable_runner_fails_skills_discovery_closed_and_names_why() {
    let root = tempfile::tempdir().unwrap();
    let workspace = std::fs::canonicalize(root.path()).unwrap();
    write_skill(&workspace, "review-pr");
    let runner_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (services, client) = runner_serving(&runner_runtime, &workspace);
    client.disconnect();
    let error = crate::skills::NodeSkills::discover(
        &crate::skills::SkillsSetting::new(true, None),
        crate::skills::SkillsMode::New,
        &workspace,
        plakous::prompts::skills::SkillsPresentation::Claude,
        Some(&services),
    )
    .expect_err("an unreachable runner is never an empty listing");
    let message = error.message();
    assert!(
        message.starts_with(
            "skills could not be discovered: the instruction source reader is unavailable: "
        ),
        "{message}"
    );
}
