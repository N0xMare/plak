//! Session boundaries exercised through the real hosted publication stream.

use super::test_support::{launch_basic, queue_item};
use super::{App, HostEvent};
use crate::composition_tests::product_injected_attached_host;
use crate::store::sessions::{self, SessionRecord};
use crate::tui::{QueueDrainState, SurfaceIntent, UserAction};

fn seed(record: &mut SessionRecord, label: &str) {
    record.record_prompt(format!("{label} history"));
    record
        .pending_queue
        .push(sessions::QueueItem::new(format!("{label} queued")));
    record.queue_drain_state = sessions::QueueDrainState::RestoredPaused;
}

fn switch_action(
    path: &str,
    root: &std::path::Path,
    config: &mut plakous_hosted::runtime::HostConfig<crate::surface_protocol::PlakExtension>,
    paths: &crate::home::paths::PlakPaths,
) -> Option<UserAction> {
    match path {
        "open" => None,
        "failed-resume" => Some(UserAction::ResumeSession {
            id: Some("missing-session".into()),
        }),
        "clear" => Some(UserAction::NewSession),
        "fork" => Some(UserAction::ForkSession {
            account: None,
            compact: false,
        }),
        "resume" => {
            let (mut target, target_runtime, _) = product_injected_attached_host(root);
            seed(&mut target.session, "target");
            sessions::save_session(paths, &mut target.session).unwrap();
            let id = target.session.id.clone();
            plakous_hosted::runtime::run_scripted_host(
                target,
                target_runtime,
                vec![UserAction::Quit],
            )
            .unwrap();
            Some(UserAction::ResumeSession { id: Some(id) })
        }
        "rebind" => {
            std::fs::create_dir_all(paths.home().join("profiles")).unwrap();
            std::fs::write(paths.user_agents_dir().join("boundary-root.toml"), "account = \"codex\"\nmodel = \"offline-loopback-v1\"\neffort = \"low\"\ncomposition = \"js-ptc\"\ndescription = \"fixture card\"\n").unwrap();
            std::fs::write(
                paths.home().join("profiles/boundary.toml"),
                "root = \"boundary-root\"\nworking_set = [\"boundary-root\"]\n",
            )
            .unwrap();
            config.ready_catalog = plakous_host_protocol::views::ReadyCatalog::new(vec![
                plakous_host_protocol::views::ProviderSection {
                    account: "codex".into(),
                    label: "Codex".into(),
                    ready: true,
                    entries: vec![plakous_host_protocol::views::ModelEntry {
                        id: "offline-loopback-v1".into(),
                        tag: "fixture".into(),
                        reasoning: true,
                        effort_options: vec!["medium".into(), "low".into()],
                        extras: Vec::new(),
                        unavailable_reason: None,
                    }],
                },
            ]);
            Some(UserAction::Extension(SurfaceIntent::ApplyProfile {
                name: "boundary".into(),
                apply: crate::tui::ProfileApply::Fork,
            }))
        }
        _ => unreachable!(),
    }
}

fn stream_case(path: &str) {
    let root = tempfile::tempdir().unwrap();
    let (mut config, injection, paths) = product_injected_attached_host(root.path());
    if matches!(path, "fork" | "rebind") {
        let id = config.session.id.clone();
        seed_canonical_history(config);
        config = product_injected_attached_host(root.path()).0;
        config.session = sessions::load_session(&paths, &id).unwrap();
    }
    seed(&mut config.session, "source");
    // Fork/rebind deliberately refuse queued work; their empty snapshots must
    // still replace the source UI queue rather than leave a ghost behind.
    if matches!(path, "fork" | "rebind") {
        config.session.pending_queue.clear();
    }
    sessions::save_session(&paths, &mut config.session).unwrap();
    let source_id = config.session.id.clone();
    let action = switch_action(path, root.path(), &mut config, &paths);
    let injection = if path == "rebind" {
        rebind_runtime(root.path(), &config.cwd)
    } else {
        injection
    };
    let mut actions = action.into_iter().collect::<Vec<_>>();
    actions.push(UserAction::Quit);
    let observed = plakous_hosted::runtime::run_scripted_host(config, injection, actions).unwrap();
    let target_id = observed
        .iter()
        .rev()
        .find_map(|event| match event {
            HostEvent::Binding {
                session_id: Some(id),
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .unwrap();
    if path != "open" && path != "failed-resume" {
        assert_ne!(target_id, source_id, "{path}: {observed:#?}");
    }
    if path == "failed-resume" {
        assert_eq!(target_id, source_id);
    }
    let target = sessions::load_session(&paths, &target_id).unwrap();
    assert_boundary(path, &observed, &target_id);
    assert_target(path, source_id, &target, &observed);
}

fn rebind_runtime(
    root: &std::path::Path,
    workspace: &std::path::Path,
) -> std::sync::Arc<plakous_hosted::runtime::ScriptedRuntimeFactory> {
    use plakous_offline_crash_harness::{
        BoundaryController, runtime_binding, runtime_binding_for_account,
    };
    let controller = BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        root.join("boundary-runtime.jsonl"),
    )
    .unwrap();
    let target =
        runtime_binding_for_account(workspace, controller.clone(), b"session-boundary-target")
            .unwrap()
            .target_binding();
    let workspace = workspace.to_path_buf();
    std::sync::Arc::new(plakous_hosted::runtime::ScriptedRuntimeFactory {
        runtime: std::sync::Arc::new(move |role| {
            if role == plakous_hosted::runtime::RuntimeRole::Rebind {
                runtime_binding_for_account(
                    &workspace,
                    controller.clone(),
                    b"session-boundary-target",
                )
            } else {
                runtime_binding(&workspace, controller.clone())
            }
            .map_err(|error| error.to_string())
        }),
        rebind_target: target,
        observe: std::sync::Arc::new(|_, _| {}),
        child_runtime: None,
        profile_route: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    })
}

fn seed_canonical_history(
    config: plakous_hosted::runtime::HostConfig<crate::surface_protocol::PlakExtension>,
) {
    let (actions, events, worker) = plakous_hosted::runtime::spawn_host_worker(config).unwrap();
    actions
        .send(UserAction::Prompt("source canonical history".into()))
        .unwrap();
    // A liveness bound only: generous, so load never trips it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("fixture turn must settle");
        if let HostEvent::TurnLifecycle(lifecycle) = event {
            if lifecycle.phase == plakous_host_protocol::TurnPhase::Admitted {
                continue;
            }
            assert_eq!(lifecycle.phase, plakous_host_protocol::TurnPhase::Completed);
            break;
        }
    }
    actions.send(UserAction::Quit).unwrap();
    worker.join().unwrap().unwrap();
}

fn assert_boundary(path: &str, observed: &[HostEvent], target_id: &str) {
    let boundary = observed.iter().position(|event| matches!(event, HostEvent::Binding { session_id: Some(id), .. } if id == target_id)).unwrap();
    // Initial source snapshots can contain the same carried values. The last
    // snapshots must be the target's, and follow its explicit boundary.
    for index in [
        observed
            .iter()
            .rposition(|event| matches!(event, HostEvent::PromptHistorySnapshot(_)))
            .unwrap(),
        observed
            .iter()
            .rposition(|event| matches!(event, HostEvent::QueueSnapshot { .. }))
            .unwrap(),
    ] {
        assert!(
            boundary < index,
            "{path}: target snapshot preceded its boundary: {observed:#?}"
        );
    }
}

fn assert_target(path: &str, source_id: String, target: &SessionRecord, observed: &[HostEvent]) {
    let mut launch = launch_basic();
    launch.session_id = Some(if path == "open" {
        "former-ui-session".into()
    } else {
        source_id
    });
    let mut app = App::new(launch, true);
    app.prompt_history = vec!["source UI history not published by target".into()];
    app.local_queue = vec![queue_item("source-ui-id", 1, "source UI queued")];
    app.queue_drain = QueueDrainState::LiveAutoDrain;
    app.open_approval = Some("source UI approval".into());
    app.last_turn_usage = Some((1, 2, 3, 4, 5, Some(1), Some(1)));
    for event in observed.iter().cloned() {
        app.apply_host_event(event);
    }
    assert_eq!(app.session_id.as_deref(), Some(target.id.as_str()));
    assert_eq!(
        app.prompt_history, target.prompt_history,
        "{path}: {observed:#?}"
    );
    assert_eq!(
        app.local_queue
            .iter()
            .map(|item| &item.text)
            .collect::<Vec<_>>(),
        target
            .pending_queue
            .iter()
            .map(|item| &item.text)
            .collect::<Vec<_>>(),
        "{path}: {observed:#?}"
    );
    assert_eq!(
        app.queue_drain == QueueDrainState::RestoredPaused,
        target.queue_drain_state == sessions::QueueDrainState::RestoredPaused,
        "{path}: {observed:#?}"
    );
    if path == "failed-resume" {
        assert!(app.last_turn_usage.is_some());
    }
    assert!(app.open_approval.is_none());
    if !matches!(path, "clear" | "fork" | "rebind") {
        assert!(
            !app.local_queue.is_empty(),
            "fixture must exercise a nonempty target queue"
        );
    }
    // Rebind keeps the shelf of the conversation it carries; a fork starts
    // with an empty shelf, and the source's UI history never leaks into it.
    if path == "rebind" {
        assert!(
            app.prompt_history
                .iter()
                .any(|text| text == "source canonical history"),
            "rebind keeps the carried shelf: {observed:#?}"
        );
    }
    if path == "fork" {
        assert!(
            app.prompt_history.is_empty(),
            "a fork starts with an empty shelf: {observed:#?}"
        );
    }
}

#[test]
fn open_stream_keeps_target_queue_drain_and_history() {
    stream_case("open");
}
#[test]
fn clear_stream_keeps_target_queue_drain_and_history() {
    stream_case("clear");
}
#[test]
fn fork_stream_keeps_target_queue_drain_and_history() {
    stream_case("fork");
}
#[test]
fn rebind_stream_keeps_target_queue_drain_and_history() {
    stream_case("rebind");
}
#[test]
fn resume_stream_keeps_target_queue_drain_and_history() {
    stream_case("resume");
}

#[test]
fn failed_resume_stream_keeps_source_usable() {
    stream_case("failed-resume");
}
