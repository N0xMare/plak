#![allow(
    clippy::too_many_lines,
    reason = "Settings scenarios assert immutable host facts, exact catalog admission, saved configuration, and binding handoffs together."
)]

use crate::surface_protocol::HostEvent;

#[test]
fn settings_cannot_clear_an_existing_root_with_an_incomplete_binding() {
    let (_root, paths) = PlakPaths::isolated_test_home();
    let mut config = ProductConfig::default_product();
    config.bind_root_tuple("grok", "grok-4.6", None);
    config.save(&paths).unwrap();
    let before = std::fs::read(paths.config_file()).unwrap();
    let mut update = settings_view(&config);
    update.product.default_account = None;
    update.product.default_model = None;
    update.product.default_effort = None;
    update.product.default_extra = None;
    let (events, observed) = host_event_sink();
    assert!(save_settings(&paths, update, &[], &events).is_none());
    assert!(observed.try_iter().any(|event| matches!(event,
        HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(message))
            if message.contains("plak config set root.agent <template>")
    )));
    assert_eq!(std::fs::read(paths.config_file()).unwrap(), before);
}

#[test]
fn nearest_profile_ties_keep_the_first_profile_list_row() {
    use crate::surface_protocol::ProfileDifference;
    let (_root, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let prepared = crate::profiles::prepare(
        &ProductConfig::default_product(),
        paths.home(),
        "claude-default",
    )
    .unwrap();
    prepared.config.save(&paths).unwrap();
    let working = prepared
        .config
        .working_set
        .names()
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::create_dir_all(paths.home().join("profiles")).unwrap();
    for (name, model, effort) in [
        ("a-tie", prepared.root.model.as_str(), "high"),
        ("b-tie", "claude-other", "low"),
    ] {
        std::fs::write(
            paths.user_agents_dir().join(format!("{name}.toml")),
            format!("account = \"claude\"\nmodel = \"{model}\"\neffort = \"{effort}\"\n"),
        )
        .unwrap();
        std::fs::write(
            paths.home().join(format!("profiles/{name}.toml")),
            format!("root = \"{name}\"\nworking_set = [{working}]\n"),
        )
        .unwrap();
    }
    let config = ProductConfig::load(&paths).unwrap();
    let list = crate::profiles::list(&config, paths.home());
    let a = list
        .rows
        .iter()
        .position(|row| row.id == "a-tie")
        .unwrap_or_else(|| panic!("{list:#?}"));
    let b = list
        .rows
        .iter()
        .position(|row| row.id == "b-tie")
        .unwrap_or_else(|| panic!("{list:#?}"));
    assert!(a < b);
    let session = plakous_hosted::runtime::SurfaceSession {
        account: Some("claude".into()),
        model: Some("claude-other".into()),
        effort: Some("high".into()),
        fast: false,
        workspace: paths.home().to_path_buf(),
        mcp: None,
        roster: Some(crate::roster_input::snapshot_from_config(
            &prepared.config,
            "claude",
            "claude-other",
            Some("high".into()),
        )),
    };
    assert_eq!(
        session_profile(&config, paths.home(), &session).differences,
        [ProfileDifference::RootModel]
    );
}
#[test]
fn stale_composer_refresh_cannot_publish_profile_or_workspace() {
    let config = ProductConfig::default_product();
    let session = plakous_hosted::runtime::SurfaceSession {
        account: None,
        model: None,
        effort: None,
        fast: false,
        workspace: std::path::PathBuf::from("."),
        roster: None,
        mcp: None,
    };
    let row = session_profile(&config, std::path::Path::new("."), &session);
    let (events, rx) = host_event_sink();
    let generation = std::sync::Mutex::new(2);
    publish_composer_chrome(
        &generation,
        1,
        row.clone(),
        crate::surface_protocol::WorkspaceChrome {
            path: "old".into(),
            ..Default::default()
        },
        &events,
    );
    assert!(rx.try_recv().is_err());
    publish_composer_chrome(
        &generation,
        2,
        row,
        crate::surface_protocol::WorkspaceChrome {
            path: "current".into(),
            ..Default::default()
        },
        &events,
    );
    assert!(matches!(
        rx.try_recv().unwrap(),
        HostEvent::Extension(SurfaceObservation::SessionProfile(_))
    ));
    assert_eq!(
        rx.try_recv().unwrap(),
        HostEvent::Extension(SurfaceObservation::WorkspaceChrome(
            crate::surface_protocol::WorkspaceChrome {
                path: "current".into(),
                ..Default::default()
            }
        ))
    );
}
#[test]
fn composer_refresh_uses_the_launch_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let workspace = dir.path().join("launch-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let paths = PlakPaths::explicit(home);
    ProductConfig::default_product().save(&paths).unwrap();
    let expected = crate::shell::workspace::read(&workspace);
    let (events, rx) = host_event_sink();
    let session = plakous_hosted::runtime::SurfaceSession {
        account: None,
        model: None,
        effort: None,
        fast: false,
        workspace,
        roster: None,
        mcp: None,
    };
    ProductSurface::new(paths).dispatch(
        SurfaceIntent::RefreshComposerChrome,
        SurfaceFacts {
            phase: SurfacePhase::Detached,
            attached_one_shot: false,
            catalog: &[],
            queue_len: 0,
            usage_reads: UsageReads::default(),
            session: Some(session),
        },
        &events,
    );
    assert!(
        matches!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
        HostEvent::Extension(SurfaceObservation::SessionProfile(row)) if row.id == "custom")
    );
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
        HostEvent::Extension(SurfaceObservation::WorkspaceChrome(expected))
    );
}

#[test]
fn session_identity_uses_frozen_cards_not_current_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = ProductConfig::default_product();
    let prepared = crate::profiles::prepare(&config, dir.path(), "claude-default").unwrap();
    let root = &prepared.root;
    let frozen = crate::roster_input::snapshot_from_config(
        &prepared.config,
        root.account.clone(),
        root.model.clone(),
        root.effort.clone(),
    );
    let mut session = plakous_hosted::runtime::SurfaceSession {
        account: Some(root.account.clone()),
        model: Some(root.model.clone()),
        effort: root.effort.clone(),
        fast: root.is_fast(),
        workspace: dir.path().to_owned(),
        roster: Some(frozen),
        mcp: None,
    };
    let matched = session_profile(&prepared.config, dir.path(), &session);
    assert_eq!(matched.id, "claude-default");
    assert_eq!(
        matched.differences,
        [] as [crate::surface_protocol::ProfileDifference; 0]
    );
    session.model = Some("another-root-model".into());
    let custom_root = session_profile(&prepared.config, dir.path(), &session);
    assert_eq!(
        custom_root.differences,
        [crate::surface_protocol::ProfileDifference::RootModel]
    );
    session.model = Some(root.model.clone());
    let working_set = |row: &crate::surface_protocol::ProfileRow| {
        row.cards.iter().filter(|card| card.depth == 0).count()
    };
    assert_eq!(working_set(&matched), 3);
    let frozen = session.roster.as_mut().unwrap();
    let (name, card) = frozen.roster.iter_mut().next().unwrap();
    let changed_card = name.to_string();
    card.tuple_id = "frozen-session-model".into();
    frozen.templates_digest = "different-frozen-roster".into();
    let custom = session_profile(&prepared.config, dir.path(), &session);
    assert_eq!(custom.id, "custom");
    assert_eq!(
        custom.differences,
        [crate::surface_protocol::ProfileDifference::Card(
            changed_card
        )]
    );
    assert_eq!(working_set(&custom), 3);
    assert!(
        custom
            .cards
            .iter()
            .any(|card| card.model == "frozen-session-model")
    );
    session.roster = None;
    let without_tree = session_profile(&prepared.config, dir.path(), &session);
    assert_eq!(without_tree.id, "custom");
    assert_eq!(
        without_tree.cards,
        [] as [crate::surface_protocol::ProfileRole; 0]
    );
}

/// Settings lists the frozen working set in config order, the same shape
/// Profiles uses, including a one-line reference when one working-set
/// card names another.
#[test]
fn session_profile_keeps_config_order_and_sibling_references() {
    let dir = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(dir.path().join("home"));
    paths.ensure_layout().unwrap();
    for (name, extra) in [
        ("zeta", "spawns = [\"alpha\", \"zeta\"]\n"),
        ("alpha", "spawns = [\"note\"]\n"),
        ("note", ""),
        ("rootish", ""),
    ] {
        std::fs::write(
            paths.user_agents_dir().join(format!("{name}.toml")),
            format!(
                "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\ndescription = \"{name}\"\n{extra}"
            ),
        )
        .unwrap();
    }
    std::fs::create_dir_all(paths.home().join("profiles")).unwrap();
    std::fs::write(
        paths.home().join("profiles/nest.toml"),
        "root = \"rootish\"\nworking_set = [\"zeta\", \"alpha\"]\n",
    )
    .unwrap();
    let config = ProductConfig::load(&paths).unwrap();
    let prepared = crate::profiles::prepare(&config, paths.home(), "nest").unwrap();
    let root = &prepared.root;
    let session = plakous_hosted::runtime::SurfaceSession {
        account: Some(root.account.clone()),
        model: Some(root.model.clone()),
        effort: root.effort.clone(),
        fast: root.is_fast(),
        workspace: dir.path().to_owned(),
        roster: Some(crate::roster_input::snapshot_from_config(
            &prepared.config,
            root.account.clone(),
            root.model.clone(),
            root.effort.clone(),
        )),
        mcp: None,
    };
    let shape = |cards: &[crate::surface_protocol::ProfileRole]| {
        cards
            .iter()
            .map(|card| (card.label.clone(), card.depth))
            .collect::<Vec<_>>()
    };
    let profile = crate::profiles::list(&prepared.config, paths.home())
        .rows
        .into_iter()
        .find(|row| row.id == "nest")
        .unwrap();
    let settings = session_profile(&prepared.config, paths.home(), &session);
    assert_eq!(settings.id, "nest");
    assert_eq!(
        shape(&settings.cards),
        [
            ("zeta".to_owned(), 0),
            ("alpha".to_owned(), 1),
            ("alpha".to_owned(), 0),
            ("note".to_owned(), 1),
        ]
    );
    assert_eq!(shape(&settings.cards), shape(&profile.cards));
}

#[test]
fn default_effort_identity_survives_none_and_explicit_default() {
    let dir = tempfile::tempdir().unwrap();
    let config = ProductConfig::default_product();
    let mut prepared = crate::profiles::prepare(&config, dir.path(), "claude-default").unwrap();
    let root_name = prepared.config.root_agent.clone().unwrap();
    prepared
        .config
        .templates
        .get_mut(&root_name)
        .unwrap()
        .effort = None;
    let root = prepared.config.root_template().unwrap();
    let session = plakous_hosted::runtime::SurfaceSession {
        account: Some(root.account.clone()),
        model: Some(root.model.clone()),
        effort: None,
        fast: root.is_fast(),
        workspace: dir.path().to_owned(),
        roster: Some(crate::roster_input::snapshot_from_config(
            &prepared.config,
            root.account.clone(),
            root.model.clone(),
            None,
        )),
        mcp: None,
    };
    assert_eq!(
        session_profile(&prepared.config, dir.path(), &session).id,
        "claude-default"
    );
    let explicit = plakous_hosted::runtime::SurfaceSession {
        effort: Some("default".into()),
        ..session
    };
    assert_eq!(
        session_profile(&prepared.config, dir.path(), &explicit).id,
        "claude-default"
    );
}

#[test]
fn child_wake_reads_only_config_and_observes_immediate_changes() {
    use super::*;
    use std::ffi::OsStr;
    let dir = tempfile::tempdir().unwrap();
    let paths =
        PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
    paths.ensure_layout().unwrap();
    let surface = ProductSurface::new(paths.clone());
    assert_eq!(surface.child_wake_enabled(), Some(true));
    std::fs::write(
        paths.user_agents_dir().join("broken.toml"),
        "not valid [toml",
    )
    .unwrap();
    std::fs::write(paths.config_file(), "[turn]\nauto_continue = false\n").unwrap();
    assert_eq!(surface.child_wake_enabled(), Some(false));
    std::fs::write(paths.config_file(), "[turn]\nauto_continue = true\n").unwrap();
    assert_eq!(surface.child_wake_enabled(), Some(true));
    std::fs::write(paths.config_file(), "[turn]\nauto_continue = 1\n").unwrap();
    assert_eq!(surface.child_wake_enabled(), None);
}
use super::*;
use crate::surface_protocol::{AgentAcknowledgement, SurfaceIntent, UserAction};
use plakous_hosted::runtime::{ShellHostConfig, note_idle_only};
use plakous_hosted::sessions::SessionRecord;

use crate::home::paths::PlakPaths;
use crate::product_config::ProductConfig;
use plakous_hosted::binding::CompositionSelection as ProductComposition;

/// The event sink the hosted fixtures hand a settings save.
fn host_event_sink() -> (
    Sender<PlakExtension>,
    crate::surface_protocol::HostEventReceiver,
) {
    let (tx, rx) = crate::surface_protocol::host_event_channel();
    (Sender::over(tx), rx)
}

#[test]
fn an_agent_acknowledgement_is_persisted_and_answered_with_the_complete_set() {
    let (_root, paths) = PlakPaths::isolated_test_home();
    let facts = || SurfaceFacts {
        phase: SurfacePhase::Busy,
        attached_one_shot: false,
        catalog: &[],
        queue_len: 0,
        usage_reads: UsageReads::default(),
        session: None,
    };
    let (events, rx) = host_event_sink();
    let surface = ProductSurface::new(paths.clone());
    let occurrence = AgentAcknowledgement {
        agent_id: "c3".into(),
        attempt_id: "c3-a1".into(),
        attention: "needs recovery".into(),
        ended_at: Some(10),
    };
    surface.dispatch(
        SurfaceIntent::AcknowledgeAgent {
            tree_id: "tree-1".into(),
            agent_id: occurrence.agent_id.clone(),
            attempt_id: occurrence.attempt_id.clone(),
            attention: occurrence.attention.clone(),
            ended_at: occurrence.ended_at,
        },
        facts(),
        &events,
    );
    let expected = HostEvent::Extension(SurfaceObservation::AgentAcknowledgements {
        tree_id: "tree-1".into(),
        attempts: vec![occurrence],
    });
    assert_eq!(rx.try_recv().unwrap(), expected);
    // A fresh surface over the same home is a restart: the set is still there.
    ProductSurface::new(paths).dispatch(
        SurfaceIntent::ListAgentAcknowledgements {
            tree_id: "tree-1".into(),
        },
        facts(),
        &events,
    );
    assert_eq!(rx.try_recv().unwrap(), expected);
    surface.dispatch(
        SurfaceIntent::AcknowledgeAgent {
            tree_id: "tree-1".into(),
            agent_id: String::new(),
            attempt_id: "c3-a2".into(),
            attention: "failed".into(),
            ended_at: None,
        },
        facts(),
        &events,
    );
    assert!(
        rx.try_recv().is_err(),
        "an id-less acknowledgement is dropped"
    );
}

#[test]
fn every_product_intent_uses_immutable_phase_facts_and_bounded_observations() {
    for phase in [
        SurfacePhase::Idle,
        SurfacePhase::Busy,
        SurfacePhase::Detached,
    ] {
        for (intent_index, attached_one_shot) in
            (0..4).flat_map(|intent| [false, true].map(|attached| (intent, attached)))
        {
            let (_root, paths) = PlakPaths::isolated_test_home();
            let config = ProductConfig::default_product();
            config.save(&paths).unwrap();
            let handler = ProductSurface::new(paths.clone())
                .with_test_state_home(paths.home().join("account-state"));
            let intent = match intent_index {
                0 => SurfaceIntent::OpenConfigFile,
                1 => SurfaceIntent::OpenSettings,
                2 => SurfaceIntent::SaveSettings(settings_view(&config)),
                3 => SurfaceIntent::OpenUsage,
                _ => unreachable!(),
            };
            assert_eq!(handler.needs_usage(&intent), intent_index == 3);
            let (events, received) = host_event_sink();
            // A host usage read stays running until the dispatch is observed.
            let reads = UsageReads::default();
            let running_read = reads.begin();
            let effect = handler.dispatch(
                intent.clone(),
                SurfaceFacts {
                    session: None,
                    phase,
                    attached_one_shot,
                    catalog: &[],
                    queue_len: 7,
                    usage_reads: reads,
                },
                &events,
            );
            let observed = received.try_iter().collect::<Vec<_>>();
            drop(running_read);
            assert_eq!(effect.saved.is_some(), intent_index == 2);
            match intent {
                SurfaceIntent::OpenConfigFile if phase == SurfacePhase::Busy => assert_eq!(
                    observed,
                    vec![HostEvent::Row(TranscriptRow::info(
                        NoticeScope::Session,
                        "config: available when idle (finish or interrupt the turn)"
                    ))]
                ),
                SurfaceIntent::OpenConfigFile => {
                    assert!(observed.iter().any(|event| matches!(event,
                    HostEvent::Status { phase, queue_len: 7, detail: Some(detail) }
                    if phase == "idle" && detail.starts_with("config: "))));
                }
                SurfaceIntent::OpenSettings => assert!(matches!(
                    observed.as_slice(),
                    [HostEvent::Extension(SurfaceObservation::SettingsView(_))]
                )),
                SurfaceIntent::SaveSettings(_) => {
                    let saved = ProductConfig::load(&paths).unwrap();
                    assert_eq!(
                        effect.saved.unwrap().root_template,
                        saved.root_template_fact()
                    );
                    assert!(observed.iter().any(|event| matches!(
                        event,
                        HostEvent::Extension(SurfaceObservation::SettingsViewApplied(_))
                    )));
                }
                SurfaceIntent::OpenUsage if phase == SurfacePhase::Idle && attached_one_shot => {
                    assert_eq!(
                        observed,
                        [] as [plakous_host_protocol::SessionObservation<PlakExtension>; 0]
                    );
                }
                // The refresh-account list is read off the dispatch and
                // follows it.
                SurfaceIntent::OpenUsage => {
                    assert!(matches!(
                        observed.as_slice(),
                        [
                            HostEvent::Extension(SurfaceObservation::SettingsView(_)),
                            HostEvent::Extension(SurfaceObservation::OpenUsageOverlay),
                        ]
                    ));
                    assert!(matches!(
                        received
                            .recv_timeout(std::time::Duration::from_secs(10))
                            .unwrap(),
                        HostEvent::Extension(SurfaceObservation::UsageRefreshAccounts(_))
                    ));
                }
                SurfaceIntent::RefreshUsageCredential { .. }
                | SurfaceIntent::RereadUsage
                | SurfaceIntent::ListProfiles
                | SurfaceIntent::RefreshComposerChrome
                | SurfaceIntent::ShowMcp
                | SurfaceIntent::ApplyProfile { .. }
                | SurfaceIntent::AcknowledgeAgent { .. }
                | SurfaceIntent::ListAgentAcknowledgements { .. } => {
                    unreachable!("profile and acknowledgement intents have their own tests")
                }
            }
        }
    }
}

/// Creates the subscription layout under `state_home` and returns the
/// directory every provider's compatibility lock `flock`s.
fn compatibility_lock_dir(state_home: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let dir = state_home.join("plak/subscriptions/compatibility");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    dir
}

/// Receives until `wanted` matches, failing after `limit`; returns every
/// event received on the way, the match last.
fn receive_until(
    received: &crate::surface_protocol::HostEventReceiver,
    limit: std::time::Duration,
    wanted: impl Fn(&plakous_host_protocol::SessionObservation<PlakExtension>) -> bool,
) -> Vec<plakous_host_protocol::SessionObservation<PlakExtension>> {
    let deadline = std::time::Instant::now() + limit;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let event = received
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("no matching event within {limit:?}: {seen:#?}"));
        let done = wanted(&event);
        seen.push(event);
        if done {
            return seen;
        }
    }
}

/// Usage reads each provider's posture under its compatibility lock. With
/// that lock held by another holder, opening Usage leaves the action loop
/// free: the next action is answered while the lock is still held, and the
/// offered `[refresh]` list arrives once the lock frees.
#[test]
fn usage_never_waits_on_a_held_compatibility_lock_on_the_action_loop() {
    let (root, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let state_home = root.path().join("account-state");
    let holder = std::fs::File::open(compatibility_lock_dir(&state_home)).unwrap();
    holder.lock().unwrap();
    let surface = ProductSurface::new(paths.clone()).with_test_state_home(state_home);
    let (tx, rx, join) = crate::shell::host::spawn_shell_host(ShellHostConfig {
        surface: Some(Box::new(surface)),
        binding: crate::shell::binding_document::ProductRootBindingDocument::shared(&paths),
        subscriptions: None,
        openrouter_credentials: crate::openrouter_product::ProductOpenRouterCredentials::shared(
            &paths,
        ),
        product_home: paths.home().to_path_buf(),
        home: paths.session_home(),
        store: std::sync::Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        )),
        session: SessionRecord::new(root.path().to_path_buf()),
        cwd: root.path().to_path_buf(),
        reason: "missing_defaults".into(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::default(),
    });
    for intent in [
        SurfaceIntent::OpenUsage,
        SurfaceIntent::RereadUsage,
        SurfaceIntent::OpenConfigFile,
    ] {
        tx.send(UserAction::Extension(intent)).unwrap();
    }
    let answered = receive_until(&rx, std::time::Duration::from_secs(5), |event| {
        matches!(event, HostEvent::Status { detail: Some(detail), .. }
            if detail.starts_with("config: "))
    });
    assert!(
        !answered.iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::UsageRefreshAccounts(_))
        )),
        "no posture can be read while the lock is held: {answered:#?}"
    );
    holder.unlock().unwrap();
    // The newer intent's list is the one published; no consent is
    // recorded in this store, so nothing is offered.
    let offered = receive_until(&rx, std::time::Duration::from_secs(10), |event| {
        matches!(
            event,
            HostEvent::Extension(SurfaceObservation::UsageRefreshAccounts(_))
        )
    });
    assert_eq!(
        offered.last(),
        Some(&HostEvent::Extension(
            SurfaceObservation::UsageRefreshAccounts(Vec::new())
        ))
    );
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        !rx.try_iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::UsageRefreshAccounts(_))
        )),
        "the superseded list is discarded"
    );
    drop(tx);
    join.join().unwrap().unwrap();
}

/// The host's usage read holds the compatibility lock across its network
/// exchange and only try-locks it. The posture read for the same Usage
/// intent waits for that read to settle, so it never holds the lock the
/// read is trying to take.
#[test]
fn usage_posture_waits_for_the_host_usage_read_to_settle() {
    let (root, paths) = PlakPaths::isolated_test_home();
    let state_home = root.path().join("account-state");
    let lock_dir = compatibility_lock_dir(&state_home);
    let surface = ProductSurface::new(paths).with_test_state_home(state_home);
    let reads = UsageReads::default();
    let running_read = reads.begin();
    let (events, received) = host_event_sink();
    surface.dispatch(
        SurfaceIntent::RereadUsage,
        SurfaceFacts {
            session: None,
            phase: SurfacePhase::Idle,
            attached_one_shot: false,
            catalog: &[],
            queue_len: 0,
            usage_reads: reads,
        },
        &events,
    );
    // Act as the running read: every try-lock finds the lock free.
    let read_lock = std::fs::File::open(&lock_dir).unwrap();
    for _ in 0..20 {
        read_lock.try_lock().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        read_lock.unlock().unwrap();
    }
    assert!(received.try_recv().is_err(), "posture was read mid-read");
    drop(running_read);
    assert_eq!(
        received
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap(),
        HostEvent::Extension(SurfaceObservation::UsageRefreshAccounts(Vec::new()))
    );
}

#[test]
fn refused_product_save_returns_no_binding_effect_and_preserves_the_file() {
    let (_root, paths) = PlakPaths::isolated_test_home();
    let config = ProductConfig::default_product();
    config.save(&paths).unwrap();
    let before = std::fs::read(paths.config_file()).unwrap();
    let mut update = settings_view(&config);
    update.product.default_account = Some("unadmitted".into());
    update.product.default_model = Some("unadmitted-model".into());
    let (events, received) = host_event_sink();
    let effect = ProductSurface::new(paths.clone()).dispatch(
        SurfaceIntent::SaveSettings(update),
        SurfaceFacts {
            session: None,
            phase: SurfacePhase::Busy,
            attached_one_shot: false,
            catalog: &[],
            queue_len: 3,
            usage_reads: UsageReads::default(),
        },
        &events,
    );
    assert!(effect.saved.is_none());
    assert_eq!(std::fs::read(paths.config_file()).unwrap(), before);
    assert!(received.try_iter().any(|event| matches!(
        event,
        HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(_))
    )));
}

/// A config whose `[root] agent` names the one working-set template, so a
/// Settings save is deciding whether to rewrite a file-authored root.
fn roster_shared_root_config() -> ProductConfig {
    let mut config = ProductConfig::default_product();
    config.templates.insert(
        "qa".to_owned(),
        crate::product_config::AgentTemplate {
            account: "codex".to_owned(),
            model: "gpt-5.6-luna".to_owned(),
            effort: Some("low".to_owned()),
            description: "qa worker".to_owned(),
            ..crate::product_config::AgentTemplate::default()
        },
    );
    config.root_agent = Some("qa".to_owned());
    config.working_set = plakous_hosted::agent_tree_protocol::WorkingSet::parse(["qa"]).unwrap();
    config
}

/// A settings notice names the row the operator actually changed.
///
/// The fixed line said `theme applied` after every toggle, so flipping
/// `Show thinking blocks` reported a theme change that had not happened
/// and had in fact just been reverted.
#[test]
fn a_settings_notice_names_only_what_changed() {
    let before = ProductConfig::default_product();

    let mut after = before.clone();
    after.show_thinking = !before.show_thinking;
    let note = settings_saved_note(&before, &after, false);
    assert_eq!(note, "settings saved \u{b7} show thinking blocks");
    assert!(
        !note.contains("theme"),
        "a toggle must not report a theme change: {note}"
    );

    let mut themed = before.clone();
    themed.theme = if before.theme == crate::product_config::ProductTheme::DefaultDark {
        crate::product_config::ProductTheme::DefaultLight
    } else {
        crate::product_config::ProductTheme::DefaultDark
    };
    assert_eq!(
        settings_saved_note(&before, &themed, false),
        "settings saved \u{b7} theme applied"
    );

    let mut both = themed.clone();
    both.reduced_motion = !before.reduced_motion;
    assert_eq!(
        settings_saved_note(&before, &both, false),
        "settings saved \u{b7} theme applied, reduced motion"
    );

    // A save that changed nothing says so rather than claiming a theme.
    assert_eq!(
        settings_saved_note(&before, &before, false),
        "settings saved \u{b7} nothing changed"
    );
    assert_eq!(
        settings_saved_note(&before, &before, true),
        "settings saved \u{b7} defaults (next launch)"
    );
}

#[test]
fn settings_status_rejects_composition_without_an_exact_tuple() {
    for composition in [
        ProductComposition::Direct,
        ProductComposition::NativePtc,
        ProductComposition::JsPtc,
        ProductComposition::PyPtc,
    ] {
        assert!(
            composition_conflict(composition, None, None, None)
                .is_some_and(|reason| reason.contains("exact default account/model tuple"))
        );
    }
    assert_eq!(
        composition_conflict(ProductComposition::FollowCatalog, None, None, None),
        None
    );
    assert!(
        composition_conflict(
            ProductComposition::NativePtc,
            Some("openrouter"),
            Some("vendor/model"),
            None,
        )
        .is_some_and(|reason| reason.contains("OpenRouter has no native programmatic"))
    );
}

/// `native-ptc` follows each vertical's own enrollment and the refusal
/// names the vertical and the harness alternatives.
#[test]
fn settings_native_ptc_follows_the_vertical_enrollment() {
    let conflict = |account: &str, model: &str| {
        composition_conflict(
            ProductComposition::NativePtc,
            Some(account),
            Some(model),
            None,
        )
    };
    // Codex enrolls Code Mode under either account spelling; the OpenAI
    // API enrolls it on its CodeModeOnly models.
    assert_eq!(conflict("codex", "gpt-5.6-sol"), None);
    assert_eq!(conflict("chatgpt", "gpt-5.6-sol"), None);
    assert_eq!(conflict("openai", "gpt-5.6-sol"), None);
    for (account, model, vertical) in [
        ("claude", "claude-sonnet-5", "Claude has no native"),
        ("grok", "grok-4.6", "Grok has no native"),
        ("supergrok", "grok-4.6", "Grok has no native"),
        (
            "anthropic",
            "claude-sonnet-5",
            "the Anthropic API has no native",
        ),
        ("xai-api", "grok-4.6", "the xAI API has no native"),
        ("openrouter", "vendor/model", "OpenRouter has no native"),
    ] {
        let reason = conflict(account, model).unwrap_or_else(|| panic!("{account} admitted"));
        assert!(reason.contains(vertical), "{account}: {reason}");
        assert!(
            reason.contains("use js-ptc, py-ptc or direct"),
            "{account}: {reason}"
        );
    }
}

/// The exact-Ready-catalog gate ran before the host knew
/// whether the save changed the binding at all, so a theme, mouse or
/// reading-width save was refused with
/// `defaults are not admitted by the exact Ready catalog` whenever the root
/// template's account was not logged in — the seeded config's own state.
/// readiness is an affordance, not a gate on unrelated writes.
#[test]
fn a_view_only_save_under_a_non_ready_root_applies_and_leaves_the_templates_alone() {
    use std::ffi::OsStr;
    let dir = tempfile::tempdir().unwrap();
    let paths =
        PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
    paths.ensure_layout().unwrap();
    let mut seeded = ProductConfig::default_product();
    seeded.templates.insert(
        "primary".to_owned(),
        crate::product_config::AgentTemplate {
            account: "grok".to_owned(),
            model: "grok-4.6".to_owned(),
            ..crate::product_config::AgentTemplate::default()
        },
    );
    seeded.root_agent = Some("primary".to_owned());
    seeded.save(&paths).unwrap();
    let before = std::fs::read_to_string(paths.config_file()).unwrap();
    let before_template =
        std::fs::read_to_string(paths.user_agents_dir().join("primary.toml")).unwrap();
    // Not logged in: the catalog holds no Ready section for the root.
    let catalog: Vec<plakous_host_protocol::views::ProviderSection> = Vec::new();

    let view_only = crate::surface_protocol::SettingsUpdate {
        product: crate::surface_protocol::SettingsSnapshot {
            default_account: Some("grok".into()),
            default_model: Some("grok-4.6".into()),
            default_effort: None,
            default_extra: None,
            composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
            ..(crate::surface_protocol::SettingsUpdate::default()).product
        },
        tui: crate::tui_preferences::TuiPreferences {
            theme: crate::surface_protocol::SettingsTheme::CatppuccinMacchiato,
            ..(crate::surface_protocol::SettingsUpdate::default()).tui
        },
    };
    let (events, rx) = host_event_sink();
    save_settings(&paths, view_only, &catalog, &events);
    let observed = rx.try_iter().collect::<Vec<_>>();
    assert!(
        observed.iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::SettingsViewApplied(_))
        )),
        "a view-only save always succeeds: {observed:?}"
    );
    assert!(!observed.iter().any(|event| matches!(
        event,
        HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(_))
    )));
    let saved = ProductConfig::load(&paths).unwrap();
    assert_eq!(
        saved.theme,
        crate::product_config::ProductTheme::CatppuccinMacchiato
    );
    assert_eq!(saved.root_agent.as_deref(), Some("primary"));
    assert_eq!(saved.templates.len(), 1);
    assert_eq!(
        saved.templates.get("primary").map(|t| t.model.as_str()),
        Some("grok-4.6")
    );
    assert!(
        !saved.templates.contains_key("default"),
        "no default template is minted by a view row"
    );
    let after = std::fs::read_to_string(paths.config_file()).unwrap();
    let root_lines = |text: &str| {
        text.lines()
            .filter(|line| line.starts_with("[root]") || line.starts_with("agent ="))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        root_lines(&before),
        root_lines(&after),
        "[root] is byte-identical"
    );
    assert_eq!(
        std::fs::read_to_string(paths.user_agents_dir().join("primary.toml")).unwrap(),
        before_template,
        "the template file body is byte-identical"
    );

    // The identity rows are still validated, with the exact reason.
    let identity = crate::surface_protocol::SettingsUpdate {
        product: crate::surface_protocol::SettingsSnapshot {
            default_account: Some("grok".into()),
            default_model: Some("grok-4.5".into()),
            default_effort: None,
            default_extra: None,
            composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
            ..(crate::surface_protocol::SettingsUpdate::default()).product
        },
        ..crate::surface_protocol::SettingsUpdate::default()
    };
    let (events, rx) = host_event_sink();
    save_settings(&paths, identity, &catalog, &events);
    let observed = rx.try_iter().collect::<Vec<_>>();
    assert!(observed.iter().any(|event| matches!(
        event,
        HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(message))
            if message == "settings: defaults are not admitted by the exact Ready catalog"
    )));
    assert_eq!(
        ProductConfig::load(&paths)
            .unwrap()
            .templates
            .get("primary")
            .map(|t| t.model.as_str()),
        Some("grok-4.6"),
        "a refused identity save writes nothing"
    );
}

fn codex_catalog() -> Vec<plakous_host_protocol::views::ProviderSection> {
    vec![plakous_host_protocol::views::ProviderSection {
        account: "codex".into(),
        label: "Codex".into(),
        ready: true,
        entries: vec![plakous_host_protocol::views::ModelEntry {
            id: "gpt-exact".into(),
            tag: "reasoning".into(),
            reasoning: true,
            effort_options: vec!["medium".into()],
            extras: Vec::new(),
            unavailable_reason: None,
        }],
    }]
}

/// A home with a user profile `mine` rooted on codex `gpt-exact`.
fn profile_paths(dir: &std::path::Path) -> PlakPaths {
    use std::ffi::OsStr;
    let paths = PlakPaths::resolve(Some(OsStr::new(dir)), Some(OsStr::new("/home/u"))).unwrap();
    paths.ensure_layout().unwrap();
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::create_dir_all(dir.join("profiles")).unwrap();
    std::fs::write(
        dir.join("agents/mine-root.toml"),
        "account = \"codex\"\nmodel = \"gpt-exact\"\neffort = \"medium\"\ndescription = \"root\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("agents/mine-worker.toml"),
        "account = \"codex\"\nmodel = \"gpt-exact\"\neffort = \"medium\"\ndescription = \"worker\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("profiles/mine.toml"),
        "root = \"mine-root\"\nworking_set = [\"mine-worker\"]\n",
    )
    .unwrap();
    paths
}

fn apply(
    paths: &PlakPaths,
    name: &str,
    fork_now: bool,
    phase: SurfacePhase,
) -> (SurfaceEffect, Vec<HostEvent>) {
    let (events, rx) = host_event_sink();
    let catalog = codex_catalog();
    let effect = ProductSurface::new(paths.clone()).dispatch(
        SurfaceIntent::ApplyProfile {
            name: name.to_owned(),
            fork_now,
        },
        SurfaceFacts {
            session: None,
            phase,
            attached_one_shot: false,
            catalog: &catalog,
            queue_len: 0,
            usage_reads: UsageReads::default(),
        },
        &events,
    );
    drop(events);
    (effect, rx.try_iter().collect())
}

#[test]
fn next_session_saves_the_profile_and_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    let paths = profile_paths(dir.path());
    let (effect, observed) = apply(&paths, "mine", false, SurfacePhase::Idle);
    assert!(effect.profile_fork.is_none());
    assert!(observed.iter().any(|event| matches!(
        event,
        HostEvent::Extension(SurfaceObservation::ProfileApplied { name, outcome: ProfileOutcome::Saved })
            if name == "mine"
    )));
    assert!(observed.iter().any(|event| matches!(
        event,
        HostEvent::Extension(SurfaceObservation::Profiles(view)) if view.current.as_deref() == Some("mine")
    )));
    assert_eq!(
        ProductConfig::load(&paths).unwrap().root_agent.as_deref(),
        Some("mine-root")
    );
}

#[test]
fn applying_claude_profile_discards_saved_sdk_selection() {
    let dir = tempfile::tempdir().unwrap();
    let paths = profile_paths(dir.path());
    let mut config = ProductConfig::load(&paths).unwrap();
    config.bind_root_tuple("codex", "gpt-exact", Some("medium"));
    config.save(&paths).unwrap();
    let mut catalog = codex_catalog();
    let (events, rx) = host_event_sink();
    let mut update = settings_view(&config);
    update.product.composition = plakous_host_protocol::CompositionSelection::PySdk;
    let config = save_settings(&paths, update, &catalog, &events)
        .unwrap_or_else(|| panic!("{:?}", rx.try_iter().collect::<Vec<_>>()));
    assert_eq!(
        ProductConfig::load(&paths).unwrap().root_composition,
        Some(crate::product_config::ProductCompositionSelection::PySdk)
    );
    let unchanged = save_settings(&paths, settings_view(&config), &catalog, &events).unwrap();
    assert_eq!(
        unchanged.root_agent, config.root_agent,
        "an unchanged SDK selection does not mint a template"
    );
    catalog[0].account = "claude".into();
    let effect = ProductSurface::new(paths.clone()).dispatch(
        SurfaceIntent::ApplyProfile {
            name: "claude-default".into(),
            fork_now: true,
        },
        SurfaceFacts {
            session: None,
            phase: SurfacePhase::Idle,
            attached_one_shot: false,
            catalog: &catalog,
            queue_len: 0,
            usage_reads: UsageReads::default(),
        },
        &events,
    );
    let fork = effect
        .profile_fork
        .unwrap_or_else(|| panic!("{:?}", rx.try_iter().collect::<Vec<_>>()));
    let prepared = crate::profiles::prepare(&config, paths.home(), "claude-default").unwrap();
    assert_eq!(
        fork.composition,
        prepared
            .root
            .composition
            .map(|c| plakous_host_protocol::CompositionSelection::from(c).into())
    );
    assert_ne!(
        fork.composition,
        Some(plakous_host_protocol::CompositionSelection::PySdk.into())
    );
    (fork.finish)(ProfileForkOutcome::Forked);
    let saved = ProductConfig::load(&paths).unwrap();
    assert_eq!(saved.root_agent.as_deref(), Some("claude-default-root"));
    assert_eq!(saved.root_composition, None);
    assert_eq!(
        saved.default_composition(),
        prepared.root.composition.unwrap_or_default()
    );
}

#[test]
fn fork_now_saves_only_once_the_host_reports_it_landed() {
    let dir = tempfile::tempdir().unwrap();
    let paths = profile_paths(dir.path());
    let before = std::fs::read(paths.config_file()).ok();

    let (effect, observed) = apply(&paths, "mine", true, SurfacePhase::Idle);
    assert!(
        observed.is_empty(),
        "nothing is reported before the host answers"
    );
    let fork = effect.profile_fork.expect("a fork request");
    assert_eq!(
        (fork.account.as_str(), fork.model.as_str()),
        ("codex", "gpt-exact")
    );
    assert!(
        fork.roster
            .templates
            .keys()
            .any(|name| name.as_str() == "mine-worker")
    );
    assert_eq!(
        std::fs::read(paths.config_file()).ok(),
        before,
        "nothing saved yet"
    );
    fork.refuse("rebind refused");
    assert_eq!(
        std::fs::read(paths.config_file()).ok(),
        before,
        "a refusal saves nothing"
    );

    let (effect, _) = apply(&paths, "mine", true, SurfacePhase::Idle);
    let fork = effect.profile_fork.expect("a fork request");
    (fork.finish)(ProfileForkOutcome::Forked);
    assert_eq!(
        ProductConfig::load(&paths).unwrap().root_agent.as_deref(),
        Some("mine-root")
    );
}

#[test]
fn fork_now_during_a_turn_is_refused_before_the_host_sees_it() {
    let dir = tempfile::tempdir().unwrap();
    let paths = profile_paths(dir.path());
    let (effect, observed) = apply(&paths, "mine", true, SurfacePhase::Busy);
    assert!(effect.profile_fork.is_none());
    assert!(observed.iter().any(|event| matches!(
        event,
        HostEvent::Extension(SurfaceObservation::ProfileRefused { reason, .. }) if reason.contains("turn is running")
    )));
}

/// A fresh home's unattached session takes the profile's root and
/// attaches on it, instead of refusing "fork now needs an idle attached
/// session".
#[test]
fn an_unattached_empty_session_binds_the_profile_and_reattaches() {
    let dir = tempfile::tempdir().unwrap();
    let paths = profile_paths(dir.path());
    let session = SessionRecord::new(dir.path().to_path_buf());
    let (tx, rx, join) = crate::shell::host::spawn_shell_host(ShellHostConfig {
        surface: Some(ProductSurface::boxed(&paths)),
        binding: crate::shell::binding_document::ProductRootBindingDocument::shared(&paths),
        subscriptions: crate::shell::host::open_product_subscriptions(),
        openrouter_credentials: crate::openrouter_product::ProductOpenRouterCredentials::shared(
            &paths,
        ),
        product_home: paths.home().to_path_buf(),
        home: paths.session_home(),
        store: std::sync::Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        )),
        session,
        cwd: dir.path().to_path_buf(),
        reason: "missing_defaults".into(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::new(codex_catalog()),
    });
    tx.send(UserAction::Extension(SurfaceIntent::ApplyProfile {
        name: "mine".into(),
        fork_now: true,
    }))
    .unwrap();
    join.join().unwrap().unwrap();
    let observed = rx.try_iter().collect::<Vec<_>>();
    assert!(
        observed.iter().any(|event| matches!(
            event,
            HostEvent::Extension(SurfaceObservation::ProfileApplied {
                outcome: ProfileOutcome::Bound,
                ..
            })
        )),
        "{observed:#?}"
    );
    assert!(
        observed
            .iter()
            .any(|event| matches!(event, HostEvent::ReattachReady))
    );
    assert_eq!(
        ProductConfig::load(&paths).unwrap().root_agent.as_deref(),
        Some("mine-root")
    );
}

/// Use now on an unattached, empty session binds an OpenRouter root with
/// the wire and composition its template names, or a direct API root with
/// its route and effort, then attaches on it.
#[test]
fn use_now_binds_an_openrouter_or_api_root_from_its_template() {
    use crate::product_config::{
        ProductOpenRouterRoutingSelection, ProductProtocolSelection, ProductTargetSelection,
        ProductWireBinding,
    };
    let openrouter = (
        "account = \"openrouter\"\nmodel = \"vendor/model\"\nprotocol = \"responses\"\ncomposition = \"py-ptc\"\n",
        "vendor/model",
        ProductTargetSelection::OpenRouter {
            wire: ProductWireBinding::resolve(ProductProtocolSelection::Responses),
            routing: ProductOpenRouterRoutingSelection::GatewayManaged,
        },
        None,
        crate::product_config::ProductCompositionSelection::PyPtc,
    );
    let api = (
        "account = \"openai\"\nmodel = \"gpt-5.6-sol\"\neffort = \"low\"\n",
        "gpt-5.6-sol",
        ProductTargetSelection::Platform {
            provider: plakous_hosted::binding::PlatformProvider::OpenAi,
        },
        Some("low"),
        // plakous-default follows the OpenAI API's enrolled CodeModeOnly row.
        crate::product_config::ProductCompositionSelection::NativePtc,
    );
    for (template, model, target, effort, composition) in [openrouter, api] {
        let account = template.split('"').nth(1).unwrap().to_owned();
        let dir = tempfile::tempdir().unwrap();
        let paths = PlakPaths::resolve(
            Some(std::ffi::OsStr::new(dir.path())),
            Some(std::ffi::OsStr::new("/home/u")),
        )
        .unwrap();
        paths.ensure_layout().unwrap();
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        std::fs::create_dir_all(dir.path().join("profiles")).unwrap();
        std::fs::write(
            dir.path().join("agents/api-root.toml"),
            format!("{template}description = \"root\"\n"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("agents/api-worker.toml"),
            format!("{template}description = \"worker\"\n"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("profiles/api.toml"),
            "root = \"api-root\"\nworking_set = [\"api-worker\"]\n",
        )
        .unwrap();
        let session = SessionRecord::new(dir.path().to_path_buf());
        let id = session.id.clone();
        let store = std::sync::Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        ));
        let (tx, rx, join) = crate::shell::host::spawn_shell_host(ShellHostConfig {
            surface: Some(ProductSurface::boxed(&paths)),
            binding: crate::shell::binding_document::ProductRootBindingDocument::shared(&paths),
            subscriptions: crate::shell::host::open_product_subscriptions(),
            openrouter_credentials: crate::openrouter_product::ProductOpenRouterCredentials::shared(
                &paths,
            ),
            product_home: paths.home().to_path_buf(),
            home: paths.session_home(),
            store: store.clone(),
            session,
            cwd: dir.path().to_path_buf(),
            reason: "missing_defaults".into(),
            ready_catalog: plakous_host_protocol::views::ReadyCatalog::new(vec![
                plakous_host_protocol::views::ProviderSection {
                    account: account.clone(),
                    label: account.clone(),
                    ready: true,
                    entries: vec![plakous_host_protocol::views::ModelEntry {
                        id: model.into(),
                        tag: "api".into(),
                        reasoning: false,
                        effort_options: Vec::new(),
                        extras: Vec::new(),
                        unavailable_reason: None,
                    }],
                },
            ]),
        });
        tx.send(UserAction::Extension(SurfaceIntent::ApplyProfile {
            name: "api".into(),
            fork_now: true,
        }))
        .unwrap();
        drop(tx);
        join.join().unwrap().unwrap();
        let observed = rx.try_iter().collect::<Vec<_>>();
        assert!(
            observed.iter().any(|event| matches!(
                event,
                HostEvent::Extension(SurfaceObservation::ProfileApplied {
                    outcome: ProfileOutcome::Bound,
                    ..
                })
            )),
            "{account}: {observed:#?}"
        );
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, HostEvent::ReattachReady)),
            "{account}: {observed:#?}"
        );
        assert_eq!(
            ProductConfig::load(&paths).unwrap().root_agent.as_deref(),
            Some("api-root"),
            "{account}"
        );
        let bound = plakous_hosted::session_store::SessionRecordStore::load(&*store, &id).unwrap();
        assert_eq!(bound.account.as_deref(), Some(account.as_str()));
        assert_eq!(bound.model.as_deref(), Some(model));
        assert_eq!(bound.target_selection, target, "{account}");
        assert_eq!(bound.effort.as_deref(), effort, "{account}");
        assert_eq!(bound.composition, composition, "{account}");
    }
}

#[test]
fn settings_save_uses_exact_ready_catalog_not_effort_name_heuristics() {
    use std::ffi::OsStr;
    let dir = tempfile::tempdir().unwrap();
    let paths =
        PlakPaths::resolve(Some(OsStr::new(dir.path())), Some(OsStr::new("/home/u"))).unwrap();
    paths.ensure_layout().unwrap();
    let catalog = vec![plakous_host_protocol::views::ProviderSection {
        account: "codex".into(),
        label: "Codex".into(),
        ready: true,
        entries: vec![
            plakous_host_protocol::views::ModelEntry {
                id: "gpt-exact".into(),
                tag: "reasoning".into(),
                reasoning: true,
                effort_options: vec!["bespoke".into()],
                extras: Vec::new(),
                unavailable_reason: None,
            },
            plakous_host_protocol::views::ModelEntry {
                id: "gpt-unqualified".into(),
                tag: "not qualified".into(),
                reasoning: true,
                effort_options: vec!["medium".into()],
                extras: Vec::new(),
                unavailable_reason: Some("tuple not qualified".into()),
            },
        ],
    }];
    let session = SessionRecord::new(dir.path().to_path_buf());
    let (tx, rx, join) = crate::shell::host::spawn_shell_host(ShellHostConfig {
        surface: Some(ProductSurface::boxed(&paths)),
        binding: crate::shell::binding_document::ProductRootBindingDocument::shared(&paths),
        subscriptions: crate::shell::host::open_product_subscriptions(),
        openrouter_credentials: crate::openrouter_product::ProductOpenRouterCredentials::shared(
            &paths,
        ),
        product_home: paths.home().to_path_buf(),
        home: paths.session_home(),
        store: std::sync::Arc::new(crate::store::session_records::SqliteSessionStore::new(
            paths.clone(),
        )),
        session,
        cwd: dir.path().to_path_buf(),
        reason: "missing_defaults".into(),
        ready_catalog: plakous_host_protocol::views::ReadyCatalog::new(catalog),
    });
    tx.send(UserAction::Extension(SurfaceIntent::SaveSettings(
        crate::surface_protocol::SettingsUpdate {
            product: crate::surface_protocol::SettingsSnapshot {
                default_account: Some("codex".into()),
                default_model: Some("gpt-exact".into()),
                default_effort: Some("bespoke".into()),
                default_extra: None,
                composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
                composition_options: vec![crate::surface_protocol::SettingsCompositionOption {
                    composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
                    effective_presentation: "direct".into(),
                    unavailable_reason: None,
                }]
                .into_boxed_slice(),
                ..(crate::surface_protocol::SettingsUpdate::default()).product
            },
            ..crate::surface_protocol::SettingsUpdate::default()
        },
    )))
    .unwrap();
    tx.send(UserAction::Extension(SurfaceIntent::SaveSettings(
        crate::surface_protocol::SettingsUpdate {
            product: crate::surface_protocol::SettingsSnapshot {
                default_effort: Some("medium".into()),
                default_account: Some("codex".into()),
                default_model: Some("gpt-unqualified".into()),
                default_extra: None,
                composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
                composition_options: vec![crate::surface_protocol::SettingsCompositionOption {
                    composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
                    effective_presentation: "direct".into(),
                    unavailable_reason: None,
                }]
                .into_boxed_slice(),
                ..(crate::surface_protocol::SettingsUpdate::default()).product
            },
            ..crate::surface_protocol::SettingsUpdate::default()
        },
    )))
    .unwrap();
    drop(tx);
    join.join().unwrap().unwrap();
    let observed = rx.try_iter().collect::<Vec<_>>();
    assert!(observed.iter().any(|event| matches!(
        event,
        HostEvent::Extension(SurfaceObservation::SettingsViewApplied(_))
    )));
    assert!(observed.iter().any(|event| matches!(
        event,
        HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(message)) if message.contains("Ready catalog")
    )));
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                event,
                HostEvent::Extension(SurfaceObservation::SettingsSaveFailed(_))
            ))
            .count(),
        1
    );
    let config = ProductConfig::load(&paths).unwrap();
    assert_eq!(config.default_effort(), Some("bespoke"));
}

#[test]
fn note_idle_only_names_the_live_intent() {
    let (events, rx) = host_event_sink();

    note_idle_only(&events, "fork");

    assert_eq!(
        rx.try_recv(),
        Ok(HostEvent::Row(TranscriptRow::info(
            NoticeScope::Session,
            "fork: available when idle (finish or interrupt the turn)"
        )))
    );
    note_idle_only(&events, "invoke_tool");
    assert_eq!(
        rx.try_recv(),
        Ok(HostEvent::Row(TranscriptRow::info(
            NoticeScope::Session,
            "invoke_tool: available when idle (finish or interrupt the turn)"
        )))
    );
    assert_ne!(
        rx.try_recv(),
        Ok(HostEvent::Row(TranscriptRow::info(
            NoticeScope::Session,
            "auth/binding: available when idle (finish or interrupt the turn)"
        )))
    );
}

/// A Settings save that changes only a view row must leave `[root]` and the
/// template bodies exactly as the files authored them. Committing the
/// unchanged tuple anyway ran the working-set mint and
/// orphaned a file-authored root template.
#[test]
fn a_view_only_settings_save_leaves_the_root_template_alone() {
    let current = roster_shared_root_config();
    let mut requested = current.root_template().unwrap().clone();
    // The Settings tab paints the omitted file fields as their defaults.
    requested.composition = Some(plakous_hosted::binding::CompositionSelection::FollowCatalog);
    assert!(
        !settings_save_changes_root_binding(&current, &requested),
        "a theme-only save carries no identity change"
    );

    let mut config = current.clone();
    if settings_save_changes_root_binding(&current, &requested) {
        commit_root_template(&mut config, requested).unwrap();
    }
    config.theme = crate::product_config::ProductTheme::CatppuccinMacchiato;
    assert_eq!(config.root_agent.as_deref(), Some("qa"));
    assert!(
        !config.templates.contains_key("default"),
        "no default template may be minted: {:?}",
        config.templates.keys().collect::<Vec<_>>()
    );
    assert_eq!(config.templates.len(), 1);
}

/// The same save with a real identity change still rewrites the root
/// template, so the guard narrows the trigger without removing it.
#[test]
fn an_identity_settings_save_still_rewrites_the_root_template() {
    let current = roster_shared_root_config();
    let mut requested = current.root_template().unwrap().clone();
    requested.model = "gpt-5.3-codex-spark".to_owned();
    assert!(settings_save_changes_root_binding(&current, &requested));

    let mut config = current.clone();
    commit_root_template(&mut config, requested).unwrap();
    assert_eq!(config.root_agent.as_deref(), Some("default"));
    assert_eq!(
        config
            .templates
            .get("qa")
            .map(|template| template.model.as_str()),
        Some("gpt-5.6-luna"),
        "the working-set template keeps its own construction"
    );
    assert_eq!(
        config
            .templates
            .get("default")
            .map(|template| template.model.as_str()),
        Some("gpt-5.3-codex-spark")
    );
}
