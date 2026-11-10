//! A session placed on a runner opens with skills on: discovery reads the
//! project root through the runner, which serves only its own roots, and
//! never this host's login home (spec 02 §6.1.1).

use super::*;
use crate::runner_reader::test_kit::{runner_serving, write_skill};

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

    let (services, client) = runner_serving(&runner_runtime, &workspace, None);
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

/// Exercise the host's production exact-open path, with no retained runtime
/// sessions or topology hub, rather than calling discovery on a frozen list.
#[test]
fn a_runner_placed_session_cold_reopens_through_the_host() {
    let root = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root.path()).unwrap();
    let runner_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let login_home = root.join("login");
    write_skill(&login_home, "host-only");
    let trees = root.join("trees/subagents.sqlite3");
    std::fs::create_dir_all(trees.parent().unwrap()).unwrap();
    let setting = crate::skills::SkillsSetting::new(true, Some(login_home));
    let runner_home = root.join("runner-login");
    write_skill(&runner_home, "runner-only");
    let (mut config, parent) = api_topology_root_parent(
        &root,
        None,
        crate::binding::PlatformProvider::Anthropic,
        "claude-sonnet-5",
        setting.clone(),
        Some(Arc::new(SqliteTrees(trees.clone()))),
    );
    drop(parent);
    let workspace = std::fs::canonicalize(&config.session.workspace_root).unwrap();
    write_skill(&workspace, "review-pr");
    let (services, client) = runner_serving(&runner_runtime, &workspace, Some(&runner_home));
    config.state_roots = config
        .state_roots
        .with_runner_services(services.clone())
        .with_runner_client(client.clone());
    let store = config.store.clone();
    let product_home = config.product_home.clone();
    config.store.save(&mut config.session).unwrap();
    let id = config.session.id.clone();
    run_attached_actions(config, Vec::new());
    let locator = store.load(&id).unwrap().plakous_locator.unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let sessions = crate::agent::ProductStateRoots::with_doubles(
        product_home.clone(),
        product_home.join("state"),
    )
    .runtime_sessions()
    .unwrap();
    let frozen = executor
        .block_on(crate::skills::frozen_skills(&sessions, locator))
        .unwrap()
        .unwrap();
    let first = frozen.listing.unwrap();
    assert!(
        first.contains("review-pr")
            && first.contains("runner-only")
            && !first.contains("host-only"),
        "{first}"
    );
    drop(sessions);
    assert!(crate::topology_runtime::lookup(&id).is_none());

    // Fresh roots open new SQLite actors and a new tree-store handle. The
    // product port supplies only the stored record, not a live attachment.
    let (mut config, parent) = api_topology_root_parent(
        &root,
        None,
        crate::binding::PlatformProvider::Anthropic,
        "claude-sonnet-5",
        setting,
        Some(Arc::new(SqliteTrees(trees))),
    );
    drop(parent);
    config.store = store;
    config.session = config.store.load(&id).unwrap();
    config.state_roots = config
        .state_roots
        .with_runner_services(services)
        .with_runner_client(client.clone());
    // A cold exact reopen must retain the listing it froze, not discover a
    // new listing or silently carry to a different canonical branch.
    write_skill(&workspace, "added-later");
    let state_roots = config.state_roots.clone();
    let records = config.store.clone();
    run_attached_actions(config, Vec::new());
    assert_eq!(records.load(&id).unwrap().plakous_locator, Some(locator));
    let sessions = state_roots.runtime_sessions().unwrap();
    let frozen = executor
        .block_on(crate::skills::frozen_skills(&sessions, locator))
        .unwrap()
        .unwrap();
    assert_eq!(frozen.listing.as_deref(), Some(first.as_str()));
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
    let (services, client) = runner_serving(&runner_runtime, &workspace, None);
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
