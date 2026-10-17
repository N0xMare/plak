//! Semantic TUI pilot projection and driver contracts.

use std::collections::HashSet;

use serde_json::json;

use super::pilot::{PilotAction, SNAPSHOT_SCHEMA};
use super::test_support::{
    launch_basic, models_app, open_tree, queue_item, resume_fixture, tool_line, tree_node,
    tree_snapshot,
};
use super::{App, HostEvent, UserAction};
use crate::tui::blocks::ScrollBlock;
use crate::tui::presentation::ActivityKind;
use crate::tui::{SettingsSnapshot, SurfaceIntent, SurfaceObservation, settings_registry};
use plakous_host_protocol::{TurnLifecycle, TurnPhase};

fn action_ids(app: &App) -> Vec<String> {
    app.legal_pilot_actions()
        .into_iter()
        .map(|action| action.id)
        .collect()
}

#[test]
fn every_canvas_owner_advertises_activity_processes() {
    for factory in [canvas_loading_app, canvas_app] {
        let mut app = factory();
        assert!(action_ids(&app).contains(&"activity-processes".to_owned()));
        app.dispatch_pilot_action(PilotAction::Invoke("activity-processes".into()))
            .unwrap();
        assert_eq!(
            app.activity_tab,
            Some(super::processes::ActivityTab::Processes)
        );
    }
}

#[test]
fn canvas_tab_registry_covers_both_directions() {
    use crate::tui::keymap::{BINDINGS, When};
    assert!(
        BINDINGS
            .iter()
            .any(|binding| binding.when == When::Tree && binding.chord == "Tab / Shift+Tab")
    );
    for code in [super::KeyCode::Tab, super::KeyCode::BackTab] {
        let mut app = canvas_loading_app();
        app.handle_key(super::test_support::key(code));
        assert_eq!(
            app.activity_tab,
            Some(super::processes::ActivityTab::Processes)
        );
    }
    let mut app = canvas_loading_app();
    app.handle_key(super::test_support::key(super::KeyCode::Esc));
    assert!(app.activity_tab.is_none());
}

fn slash_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.handle_key(super::KeyEvent::new(
        super::KeyCode::Char('/'),
        super::KeyModifiers::NONE,
    ));
    app
}

#[test]
fn menu_pilot_opens_last_tab_and_descends_into_full_keybinds() {
    let mut app = App::new(launch_basic(), true);
    app.dispatch_pilot_action(PilotAction::Invoke("open-menu".into()))
        .unwrap();
    assert_eq!(app.semantic_snapshot().owner, "menu");
    assert_eq!(app.control_tab, Some(super::ControlTab::Session));
    app.activate_control_tab(super::ControlTab::Keybinds);
    assert!(action_ids(&app).contains(&"all-keybinds".to_owned()));
    app.dispatch_pilot_action(PilotAction::Invoke("all-keybinds".into()))
        .unwrap();
    assert!(app.keybinds_full);
    assert!(!action_ids(&app).contains(&"all-keybinds".to_owned()));
    app.dispatch_pilot_action(PilotAction::CloseFocusedOwner)
        .unwrap();
    assert!(!app.keybinds_full);
    assert!(app.control_tab.is_some());
    app.activate_control_tab(super::ControlTab::Session);
    app.dispatch_pilot_action(PilotAction::CloseFocusedOwner)
        .unwrap();
    app.dispatch_pilot_action(PilotAction::Invoke("open-menu".into()))
        .unwrap();
    assert_eq!(app.control_tab, Some(super::ControlTab::Session));
}

fn queue_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.replace_queue_snapshot(
        vec![queue_item("queued", 4, "private queue text")],
        super::QueueDrainState::RestoredPaused,
    );
    app.queue_focused = true;
    app
}

fn settings_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.open_control_tab(super::ControlTab::Settings);
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    app
}

fn settings_chooser_app() -> App {
    let mut app = settings_app();
    let row = settings_registry::index_of(settings_registry::THEME).unwrap();
    app.settings_menu.as_mut().unwrap().selected = row;
    app.activate_settings_row(row);
    app
}

fn settings_stepper_app() -> App {
    let mut app = settings_app();
    let row = settings_registry::index_of(settings_registry::SCROLL_LINES).unwrap();
    app.settings_menu.as_mut().unwrap().selected = row;
    app.activate_settings_row(row);
    app
}

fn settings_reset_app() -> App {
    let mut app = settings_app();
    let row = settings_registry::index_of(settings_registry::REDUCED_MOTION).unwrap();
    app.settings_menu.as_mut().unwrap().selected = row;
    app.handle_key(super::KeyEvent::new(
        super::KeyCode::Char('d'),
        super::KeyModifiers::NONE,
    ));
    app
}

fn overlay_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.info_overlay = Some(("help".to_owned(), "private body".to_owned()));
    app
}

fn canvas_app() -> App {
    let mut app = App::new(launch_basic(), true);
    open_tree(
        &mut app,
        tree_snapshot(vec![
            tree_node("root", None, 0),
            tree_node("child", Some("root"), 1),
        ]),
    );
    app
}

fn canvas_loading_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.activity_tab = Some(super::processes::ActivityTab::Agents);
    app.tree_navigator = Some(super::TreeNavigator::awaiting());
    app
}

fn destructive_confirmation_app() -> App {
    let mut app = canvas_app();
    app.handle_key(super::KeyEvent::new(
        super::KeyCode::Char('i'),
        super::KeyModifiers::NONE,
    ));
    app
}

fn providers_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.open_control_tab(super::ControlTab::Providers);
    app
}

fn composer_with_text_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.set_input("inspect".to_owned());
    app
}

fn busy_composer_app() -> App {
    let mut app = composer_with_text_app();
    app.busy = true;
    app
}

fn queue_editor_app() -> App {
    let mut app = queue_app();
    app.handle_key(super::KeyEvent::new(
        super::KeyCode::Char('e'),
        super::KeyModifiers::NONE,
    ));
    app
}

fn history_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.prompt_history = vec!["private prior prompt".to_owned()];
    app.handle_key(super::KeyEvent::new(
        super::KeyCode::Up,
        super::KeyModifiers::NONE,
    ));
    app
}

#[test]
fn semantic_snapshot_is_stable_and_redacts_composer_text() {
    let mut app = App::new(launch_basic(), true);
    app.input = "PRIVATE_COMPOSER_SENTINEL\nsecond line".to_owned();
    app.input_cursor = app.input.chars().count();
    let first = app.semantic_snapshot();
    app.status_spinner_frame = 7;
    app.pane.transcript_revision = 99;
    app.dirty = !app.dirty;
    let second = app.semantic_snapshot();
    assert_eq!(first, second, "volatile render state entered the snapshot");
    let encoded = serde_json::to_string(&first).unwrap();
    assert!(!encoded.contains("PRIVATE_COMPOSER_SENTINEL"));
    assert_eq!(first.composer.char_count, 37);
    assert!(first.composer.multiline);
}

#[test]
fn settings_actions_are_derived_from_the_visible_registry_rows() {
    let mut app = App::new(launch_basic(), true);
    app.dispatch_pilot_action(PilotAction::OpenSettings)
        .unwrap();
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    let ids = action_ids(&app);
    for meta in settings_registry::ROWS {
        assert!(
            ids.contains(&format!("select-setting:{}", meta.key)),
            "missing registry row {}",
            meta.key
        );
    }
    assert!(!ids.contains(&"toggle-setting:tui.theme".to_owned()));
    assert!(ids.contains(&"accept".to_owned()));
    assert!(ids.contains(&"close-focused-owner".to_owned()));
    assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len());

    let menu = app.settings_menu.as_mut().unwrap();
    menu.filter = "motion".to_owned();
    let filtered = action_ids(&app);
    assert_eq!(
        filtered
            .iter()
            .filter(|id| id.starts_with("select-setting:"))
            .cloned()
            .collect::<Vec<_>>(),
        vec!["select-setting:tui.reduced_motion".to_owned()]
    );
}

#[test]
fn settings_browse_accepts_value_rows_and_action_rows_exactly() {
    for (key, expected_owner) in [
        (settings_registry::THEME, "settings-chooser"),
        (settings_registry::SCROLL_LINES, "settings-stepper"),
    ] {
        let mut app = settings_app();
        app.dispatch_pilot_action(PilotAction::SelectSetting(key.to_owned()))
            .unwrap();
        assert!(action_ids(&app).contains(&"accept".to_owned()));
        assert_eq!(
            app.dispatch_pilot_action(PilotAction::Invoke("accept".to_owned()))
                .unwrap(),
            None
        );
        assert_eq!(app.semantic_snapshot().owner, expected_owner);
    }

    let mut app = settings_app();
    app.dispatch_pilot_action(PilotAction::SelectSetting(
        settings_registry::EDIT_RAW_CONFIG.to_owned(),
    ))
    .unwrap();
    let ids = action_ids(&app);
    assert!(ids.contains(&"accept".to_owned()));
    assert!(!ids.contains(&"reset".to_owned()));
    assert_eq!(
        app.dispatch_pilot_action(PilotAction::Invoke("accept".to_owned()))
            .unwrap(),
        Some(UserAction::Extension(SurfaceIntent::OpenConfigFile))
    );
    assert_eq!(app.semantic_snapshot().owner, "composer");
}

#[test]
fn settings_can_close_while_its_snapshot_is_loading() {
    let mut app = App::new(launch_basic(), true);
    app.dispatch_pilot_action(PilotAction::OpenSettings)
        .unwrap();
    assert_eq!(app.semantic_snapshot().owner, "settings");
    assert_eq!(
        action_ids(&app),
        vec!["quit".to_owned(), "close-focused-owner".to_owned()]
    );
    app.dispatch_pilot_action(PilotAction::CloseFocusedOwner)
        .unwrap();
    assert_eq!(app.semantic_snapshot().owner, "composer");
}

#[test]
fn higher_priority_overlay_hides_settings_actions_and_private_text() {
    let mut app = App::new(launch_basic(), true);
    app.dispatch_pilot_action(PilotAction::OpenSettings)
        .unwrap();
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    app.settings_error = Some("PRIVATE_SETTINGS_ERROR".to_owned());
    app.info_overlay = Some((
        "PRIVATE_OVERLAY_TITLE".to_owned(),
        "PRIVATE_OVERLAY_BODY".to_owned(),
    ));

    let snapshot = app.semantic_snapshot();
    assert_eq!(snapshot.owner, "overlay");
    let ids = action_ids(&app);
    assert!(ids.contains(&"close-focused-owner".to_owned()));
    assert!(!ids.iter().any(|id| id.contains("setting")));
    let encoded = serde_json::to_string(&snapshot).unwrap();
    assert!(!encoded.contains("PRIVATE_SETTINGS_ERROR"));
    assert!(!encoded.contains("PRIVATE_OVERLAY"));
}

#[test]
fn settings_subpanes_do_not_advertise_browse_actions() {
    let mut app = App::new(launch_basic(), true);
    app.dispatch_pilot_action(PilotAction::OpenSettings)
        .unwrap();
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    let row = settings_registry::ROWS
        .iter()
        .position(|meta| meta.key == settings_registry::THEME)
        .unwrap();
    app.settings_menu.as_mut().unwrap().selected = row;
    app.activate_settings_row(row);
    assert_eq!(
        app.semantic_snapshot()
            .control_center
            .unwrap()
            .settings
            .unwrap()
            .pane,
        "chooser"
    );
    let ids = action_ids(&app);
    assert!(ids.contains(&"close-focused-owner".to_owned()));
    assert!(!ids.iter().any(|id| id.starts_with("select-setting:")));
    app.dispatch_pilot_action(PilotAction::CloseFocusedOwner)
        .unwrap();
    assert_eq!(app.semantic_snapshot().owner, "settings");
}

#[test]
fn typed_settings_story_toggles_persists_and_restores_composer_owner() {
    let mut app = App::new(launch_basic(), true);
    assert_eq!(app.semantic_snapshot().owner, "composer");
    assert!(action_ids(&app).contains(&"open-settings".to_owned()));

    let open = app
        .dispatch_pilot_action(PilotAction::OpenSettings)
        .unwrap();
    assert_eq!(
        open,
        Some(UserAction::Extension(SurfaceIntent::OpenSettings))
    );
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    assert_eq!(app.semantic_snapshot().owner, "settings");

    app.dispatch_pilot_action(PilotAction::SelectSetting(
        settings_registry::REDUCED_MOTION.to_owned(),
    ))
    .unwrap();
    assert!(
        action_ids(&app).contains(&"toggle-setting:tui.reduced_motion".to_owned()),
        "the selected bool row is actionable"
    );
    let save = app
        .dispatch_pilot_action(PilotAction::ToggleSetting(
            settings_registry::REDUCED_MOTION.to_owned(),
        ))
        .unwrap();
    let Some(UserAction::Extension(SurfaceIntent::SaveSettings(update))) = save else {
        panic!("toggle must emit one complete settings replacement: {save:?}");
    };
    assert!(update.reduced_motion);
    assert!(
        app.render_prefs.reduced_motion,
        "preview applies immediately"
    );

    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsApplied(
        update,
    )));
    let applied = app.semantic_snapshot();
    assert_eq!(
        serde_json::to_value(&applied).unwrap(),
        json!({
            "schema": SNAPSHOT_SCHEMA,
            "owner": "settings",
            "phase": "idle",
            "busy": false,
            "binding": {
                "attached": true,
                "session_id": "s1",
                "account": "acct",
                "model": "model-x",
                "effort": "high",
                "extra": null,
                "composition": "plakous-default",
                "protocol": "messages",
                "effective_presentation": "direct"
            },
            "composer": {
                "char_count": 0,
                "cursor_char": 0,
                "selection": null,
                "multiline": false,
                "slash_open": false,
                "history_browsing": false,
                "editing_queue_item": false,
                "pending_prompt": false,
                "pending_draft_count": 0,
                "attachment_count": 0,
                "cancel_pending": false,
                "host_disconnected": false
            },
            "queue": {
                "count": 0,
                "focused": false,
                "selected": null,
                "selected_id": null,
                "selected_revision": null,
                "editing": false,
                "edit_pending": false,
                "drain": "live-auto-drain",
                "restored_paused": false,
                "items": []
            },
            "turn": null,
            "transcript": {
                "agent": null,
                "block_count": 0,
                "focused_block": null,
                "scroll_from_bottom": 0,
                "approval_pending": false,
                "streaming": false,
                "focused_agent_transcript_available": false,
                "tail": []
            },
            "control_center": {
                "tab": "Settings",
                "provider_count": 4,
                "focused_provider": null,
                "session_row_count": 0,
                "template_count": 0,
                "settings": {
                    "pane": "browse",
                    "selected": "tui.reduced_motion",
                    "filter_char_count": 0,
                    "values": [
                        {"key": "tui.theme", "value": "default-dark"},
                        {"key": "tui.mouse", "value": "on"},
                        {"key": "tui.scroll_lines", "value": "3"},
                        {"key": "tui.show_thinking", "value": "on"},
                        {"key": "tui.group_tool_calls", "value": "on"},
                        {"key": "compaction.enabled", "value": "on"},
                        {"key": "tui.reduced_motion", "value": "on"}
                    ],
                    "error": false
                }
            },
            "modal": {
                "kind": "settings",
                "item_count": 8,
                "selected": 6,
                "selected_id": null,
                "selection_in_place": null,
                "confirmation": null,
                "filter_char_count": 0
            },
            "canvas": null,
            "background_processes": 0,
            "processes": null
        })
    );

    assert_eq!(
        app.dispatch_pilot_action(PilotAction::CloseFocusedOwner)
            .unwrap(),
        None
    );
    assert_eq!(app.semantic_snapshot().owner, "composer");
    assert!(
        app.render_prefs.reduced_motion,
        "persisted value survives close"
    );
}

#[test]
fn composer_slash_busy_and_queue_actions_use_the_real_input_routes() {
    let mut app = App::new(launch_basic(), true);
    app.dispatch_pilot_action(PilotAction::TypeText("inspect the workspace".to_owned()))
        .unwrap();
    assert!(!action_ids(&app).contains(&"open-slash".to_owned()));
    let sent = app
        .dispatch_pilot_action(PilotAction::Invoke("send".to_owned()))
        .unwrap();
    assert_eq!(
        sent,
        Some(UserAction::Prompt(
            "inspect the workspace".to_owned().into()
        ))
    );

    let mut busy = App::new(launch_basic(), true);
    busy.busy = true;
    busy.dispatch_pilot_action(PilotAction::TypeText("use the new facts".to_owned()))
        .unwrap();
    let steered = busy
        .dispatch_pilot_action(PilotAction::Invoke("steer".to_owned()))
        .unwrap();
    assert_eq!(
        steered,
        Some(UserAction::Steer("use the new facts".to_owned().into()))
    );

    let mut slash = App::new(launch_basic(), true);
    slash
        .dispatch_pilot_action(PilotAction::Invoke("open-slash".to_owned()))
        .unwrap();
    assert_eq!(slash.semantic_snapshot().owner, "slash-palette");
    assert_eq!(
        action_ids(&slash)
            .iter()
            .filter(|id| id.starts_with("select-command:"))
            .count(),
        crate::tui::slash::COMMANDS.len()
    );
    slash
        .dispatch_pilot_action(PilotAction::Invoke("select-command:settings".to_owned()))
        .unwrap();
    let open = slash
        .dispatch_pilot_action(PilotAction::Invoke("accept".to_owned()))
        .unwrap();
    assert_eq!(
        open,
        Some(UserAction::Extension(SurfaceIntent::OpenSettings))
    );

    let mut queue = App::new(launch_basic(), true);
    queue.replace_queue_snapshot(
        vec![queue_item("queued", 4, "PRIVATE_QUEUE_TEXT")],
        super::QueueDrainState::LiveAutoDrain,
    );
    queue
        .dispatch_pilot_action(PilotAction::Invoke("focus-queue".to_owned()))
        .unwrap();
    let queue_snapshot = queue.semantic_snapshot();
    assert_eq!(queue_snapshot.owner, "queue");
    assert_eq!(queue_snapshot.queue.selected_id.as_deref(), Some("queued"));
    assert_eq!(queue_snapshot.queue.selected_revision, Some(4));
    assert_eq!(queue_snapshot.queue.drain, "live-auto-drain");
    assert_eq!(queue_snapshot.queue.items[0].id, "queued");
    assert_eq!(queue_snapshot.queue.items[0].revision, 4);
    assert!(
        !serde_json::to_string(&queue.semantic_snapshot())
            .unwrap()
            .contains("PRIVATE_QUEUE_TEXT")
    );
    let dropped = queue
        .dispatch_pilot_action(PilotAction::Invoke("drop".to_owned()))
        .unwrap();
    assert_eq!(
        dropped,
        Some(UserAction::DeleteQueued {
            id: "queued".to_owned(),
            revision: 4
        })
    );
}

#[test]
fn transcript_fold_action_matches_the_real_enter_preconditions() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(tool_line(
        "tool-1",
        None,
        None,
        ActivityKind::Tool,
        "read_file",
        "done",
        Some("README.md"),
    ));
    app.apply_host_event(tool_line(
        "tool-2",
        None,
        None,
        ActivityKind::Tool,
        "read_file",
        "done",
        Some("NOTES.md"),
    ));
    app.apply_host_event(HostEvent::AssistantDelta("Finished.".into()));
    app.apply_host_event(HostEvent::AssistantDone);
    app.pane.focused_block = Some(0);
    let before = crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs)[0].group_folded;
    assert!(action_ids(&app).contains(&"toggle-focused-block".to_owned()));
    assert_eq!(
        app.dispatch_pilot_action(PilotAction::Invoke("toggle-focused-block".to_owned()))
            .unwrap(),
        None
    );
    assert_ne!(
        crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs)[0].group_folded,
        before
    );
    app.dispatch_pilot_action(PilotAction::Invoke("toggle-focused-block".to_owned()))
        .unwrap();
    assert_eq!(
        crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs)[0].group_folded,
        before
    );

    app.set_input("draft".to_owned());
    assert!(!action_ids(&app).contains(&"toggle-focused-block".to_owned()));

    app.set_input(String::new());
    app.pane.blocks[0] = ScrollBlock::System("not foldable".to_owned());
    assert!(!action_ids(&app).contains(&"toggle-focused-block".to_owned()));

    let mut agent = App::new(launch_basic(), true);
    agent.apply_host_event(tool_line(
        "agent-1",
        None,
        None,
        ActivityKind::Agent,
        "spawn_agent",
        "running",
        Some("worker"),
    ));
    agent.pane.focused_block = Some(0);
    assert!(!action_ids(&agent).contains(&"toggle-focused-block".to_owned()));
}

#[test]
fn picker_confirmation_and_canvas_owners_expose_only_executable_actions() {
    let assert_table = |app: &App| {
        let actions = app.legal_pilot_actions();
        assert!(
            !actions.is_empty(),
            "{} has no actions",
            app.semantic_snapshot().owner
        );
        assert!(actions.len() <= 60, "action table is too wide");
        assert!(actions.iter().all(|action| !action.description.is_empty()));
        assert_eq!(
            actions.len(),
            actions
                .iter()
                .map(|action| &action.id)
                .collect::<HashSet<_>>()
                .len()
        );
    };

    let mut resume = resume_fixture();
    let resume_snapshot = resume.semantic_snapshot();
    assert_eq!(resume_snapshot.owner, "resume-picker");
    let resume_modal = resume_snapshot.modal.expect("resume modal");
    assert_eq!(
        resume_modal.selected_id.as_deref(),
        Some("018f2537-5ed2-7b11-a4b1-f009be85f101")
    );
    assert_eq!(resume_modal.selection_in_place, Some(true));
    assert_table(&resume);
    resume
        .dispatch_pilot_action(PilotAction::Invoke("move-down".to_owned()))
        .unwrap();
    resume
        .dispatch_pilot_action(PilotAction::CloseFocusedOwner)
        .unwrap();
    assert_eq!(resume.semantic_snapshot().owner, "composer");

    let mut models = models_app();
    assert_eq!(models.semantic_snapshot().owner, "agents");
    assert_table(&models);
    models
        .dispatch_pilot_action(PilotAction::CloseFocusedOwner)
        .unwrap();
    assert_eq!(models.semantic_snapshot().owner, "composer");

    let mut canvas = App::new(launch_basic(), true);
    open_tree(
        &mut canvas,
        tree_snapshot(vec![
            tree_node("root", None, 0),
            tree_node("child", Some("root"), 1),
        ]),
    );
    let canvas_snapshot = canvas.semantic_snapshot();
    assert_eq!(canvas_snapshot.owner, "canvas");
    let canvas_state = canvas_snapshot.canvas.expect("canvas state");
    assert_eq!(canvas_state.tree_id.as_deref(), Some("tree1"));
    assert_eq!(canvas_state.revision, Some(3));
    assert_eq!(canvas_state.selected.as_deref(), Some("root"));
    assert_eq!(canvas_state.selected_status.as_deref(), Some("idle"));
    assert_eq!(canvas_state.selected_finished, Some(false));
    assert_eq!(canvas_state.selected_has_session, Some(true));
    assert_table(&canvas);
    // The root row offers no interrupt or close.
    for inapplicable in ["interrupt", "close-node"] {
        assert!(!action_ids(&canvas).contains(&inapplicable.to_owned()));
    }
    canvas.tree_navigator.as_mut().unwrap().focus_on("child");
    canvas
        .dispatch_pilot_action(PilotAction::Invoke("interrupt".to_owned()))
        .unwrap();
    let interrupt_snapshot = canvas.semantic_snapshot();
    assert_eq!(interrupt_snapshot.owner, "destructive-confirmation");
    assert_eq!(
        interrupt_snapshot
            .canvas
            .expect("interrupt canvas")
            .confirmation,
        "interrupt"
    );
    assert_table(&canvas);
    let interrupt = canvas
        .dispatch_pilot_action(PilotAction::Invoke("confirm".to_owned()))
        .unwrap();
    assert_eq!(
        interrupt,
        Some(UserAction::InterruptTopologyNode {
            agent_id: "child".to_owned()
        })
    );
    assert_eq!(canvas.semantic_snapshot().owner, "canvas");

    for (action, expected) in [("close-node", "close"), ("stop-all", "stop-all")] {
        let mut variant = App::new(launch_basic(), true);
        open_tree(
            &mut variant,
            tree_snapshot(vec![
                tree_node("root", None, 0),
                tree_node("child", Some("root"), 1),
            ]),
        );
        variant.tree_navigator.as_mut().unwrap().focus_on("child");
        variant
            .dispatch_pilot_action(PilotAction::Invoke(action.to_owned()))
            .unwrap();
        assert_eq!(
            variant
                .semantic_snapshot()
                .canvas
                .expect("confirmation canvas")
                .confirmation,
            expected
        );
    }

    canvas
        .dispatch_pilot_action(PilotAction::Invoke("interrupt".to_owned()))
        .unwrap();
    canvas
        .dispatch_pilot_action(PilotAction::CloseFocusedOwner)
        .unwrap();
    assert_eq!(canvas.semantic_snapshot().owner, "canvas");

    let mut empty_canvas = App::new(launch_basic(), true);
    open_tree(&mut empty_canvas, tree_snapshot(Vec::new()));
    let empty_actions = action_ids(&empty_canvas);
    for inapplicable in ["interrupt", "close-node", "stop-all"] {
        assert!(!empty_actions.contains(&inapplicable.to_owned()));
    }

    let mut reset = settings_reset_app();
    assert_eq!(reset.semantic_snapshot().owner, "destructive-confirmation");
    assert_eq!(
        reset
            .semantic_snapshot()
            .modal
            .and_then(|modal| modal.confirmation),
        Some("settings-reset")
    );
    let saved = reset
        .dispatch_pilot_action(PilotAction::Invoke("confirm".to_owned()))
        .unwrap();
    assert!(matches!(
        saved,
        Some(UserAction::Extension(SurfaceIntent::SaveSettings(_)))
    ));
    assert_eq!(reset.semantic_snapshot().owner, "settings");
}

#[test]
fn every_control_tab_and_terminal_owner_has_a_closed_action_table() {
    for tab in super::ControlTab::ALL {
        let mut app = App::new(launch_basic(), true);
        app.open_control_tab(tab);
        if tab == super::ControlTab::Settings {
            app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
                SettingsSnapshot::default(),
            )));
        }
        let actions = app.legal_pilot_actions();
        assert!(!actions.is_empty(), "{} has no actions", tab.label());
        assert!(actions.len() <= 60, "{} has too many actions", tab.label());
        assert!(
            actions
                .iter()
                .any(|action| action.id == "close-focused-owner")
        );
    }

    let mut shutdown = App::new(launch_basic(), true);
    shutdown.quitting = true;
    assert_eq!(shutdown.semantic_snapshot().owner, "shutdown");
    assert_eq!(
        shutdown.legal_pilot_actions(),
        [] as [crate::tui::ui::pilot::LegalAction; 0]
    );
}

#[test]
fn deterministic_long_session_projection_remains_bounded_and_omits_composer_text() {
    let mut app = App::new(launch_basic(), true);
    for turn in 0..200 {
        let prompt = format!("PRIVATE_PROMPT_{turn}");
        app.dispatch_pilot_action(PilotAction::TypeText(prompt.clone()))
            .unwrap();
        let sent = app
            .dispatch_pilot_action(PilotAction::Invoke("send".to_owned()))
            .unwrap();
        assert_eq!(sent, Some(UserAction::Prompt(prompt.into())));
        app.apply_host_event(HostEvent::SetBusy(true));
        app.apply_host_event(HostEvent::AssistantDelta(format!(
            "PRIVATE_TRANSCRIPT_{turn}"
        )));
        app.apply_host_event(HostEvent::AssistantDone);
        app.apply_host_event(HostEvent::SetBusy(false));

        let snapshot = app.semantic_snapshot();
        assert_eq!(snapshot.owner, "composer");
        assert_eq!(snapshot.composer.char_count, 0);
        assert!(app.legal_pilot_actions().len() <= 60);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("PRIVATE_PROMPT"));
        assert!(snapshot.transcript.tail.len() <= 8);
        assert!(
            snapshot
                .transcript
                .tail
                .last()
                .is_some_and(|line| line.contains(&format!("PRIVATE_TRANSCRIPT_{turn}")))
        );
        assert!(
            snapshot
                .transcript
                .tail
                .iter()
                .all(|line| line.chars().count() <= 500)
        );
    }
    assert!(app.semantic_snapshot().transcript.block_count >= 200);
}

#[test]
fn every_advertised_action_executes_from_its_representative_state() {
    let factories: &[fn() -> App] = &[
        composer_with_text_app,
        busy_composer_app,
        slash_app,
        queue_app,
        queue_editor_app,
        history_app,
        settings_app,
        settings_chooser_app,
        settings_stepper_app,
        settings_reset_app,
        overlay_app,
        resume_fixture,
        models_app,
        providers_app,
        canvas_loading_app,
        canvas_app,
        destructive_confirmation_app,
    ];
    for factory in factories {
        let owner = factory().semantic_snapshot().owner;
        let action_ids = action_ids(&factory());
        for id in action_ids {
            let mut app = factory();
            let action = if id == "open-settings" {
                PilotAction::OpenSettings
            } else if let Some(key) = id.strip_prefix("select-setting:") {
                PilotAction::SelectSetting(key.to_owned())
            } else if let Some(key) = id.strip_prefix("toggle-setting:") {
                PilotAction::ToggleSetting(key.to_owned())
            } else if id == "type-text" {
                PilotAction::TypeText("fixture text".to_owned())
            } else {
                PilotAction::Invoke(id.clone())
            };
            app.dispatch_pilot_action(action)
                .unwrap_or_else(|error| panic!("{owner} advertised {id}: {error}"));
        }
    }
}

#[test]
fn driver_refuses_actions_absent_from_the_current_legal_table() {
    let mut app = App::new(launch_basic(), true);
    let error = app
        .dispatch_pilot_action(PilotAction::ToggleSetting(
            settings_registry::REDUCED_MOTION.to_owned(),
        ))
        .unwrap_err();
    assert!(error.contains("not legal"));

    let error = app
        .dispatch_pilot_action(PilotAction::TypeText("valid prefix\n".to_owned()))
        .unwrap_err();
    assert!(error.contains("printable text"));
    assert!(
        app.input.is_empty(),
        "rejected text must not mutate a prefix"
    );
}

fn lifecycle(turn_id: &str, phase: TurnPhase) -> TurnLifecycle {
    TurnLifecycle {
        turn_id: turn_id.to_owned(),
        phase,
        model_error_kind: None,
        http_status: None,
    }
}

fn char_key(character: char) -> super::KeyEvent {
    super::KeyEvent::new(super::KeyCode::Char(character), super::KeyModifiers::NONE)
}

#[test]
fn turn_lifecycle_projection_correlates_terminal_with_the_matching_turn() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::TurnLifecycle(lifecycle(
        "turn-1",
        TurnPhase::Admitted,
    )));
    app.apply_host_event(HostEvent::SetBusy(false));
    let snapshot = app.semantic_snapshot();
    assert_eq!(
        snapshot.turn.as_ref().map(|turn| turn.phase),
        Some(TurnPhase::Admitted),
        "busy settling is never terminal evidence"
    );
    app.apply_host_event(HostEvent::TurnLifecycle(lifecycle(
        "turn-2",
        TurnPhase::Completed,
    )));
    let turn = app.semantic_snapshot().turn.expect("a projected turn");
    assert_eq!(turn.turn_id, "turn-2");
    assert_eq!(
        turn.phase,
        TurnPhase::Completed,
        "an unrelated terminal keeps its own turn id; it cannot satisfy turn-1"
    );
    app.apply_host_event(HostEvent::TurnLifecycle(lifecycle(
        "turn-1",
        TurnPhase::Completed,
    )));
    let turn = app.semantic_snapshot().turn.expect("a projected turn");
    assert_eq!(turn.turn_id, "turn-1");
    assert_eq!(turn.phase, TurnPhase::Completed);
}

#[test]
fn turn_lifecycle_never_projects_from_busy_or_usage_alone() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::SetBusy(false));
    app.apply_host_event(HostEvent::TurnUsage {
        input_tokens: 10,
        output_tokens: 4,
        reasoning_tokens: 2,
        cached_input_tokens: 3,
        cache_write_tokens: 0,
        model_operations: Some(2),
        reporting_model_operations: Some(1),
        peak_prepared_input_tokens: None,
        terminal: true,
    });
    assert!(
        app.semantic_snapshot().turn.is_none(),
        "busy and usage observations alone must not invent a turn marker"
    );
}

#[test]
fn turn_lifecycle_clears_on_session_rebind_and_clear_view() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::TurnLifecycle(lifecycle(
        "turn-1",
        TurnPhase::Completed,
    )));
    app.apply_host_event(HostEvent::Binding {
        session_id: Some("a-different-session".into()),
        account: Some("codex".into()),
        model: Some("model-x".into()),
        effort: Some("medium".into()),
        extra: None,
        composition: crate::tui::CompositionSelection::FollowCatalog,
        protocol: crate::tui::ProtocolSelection::Auto,
        effective_presentation: "direct".to_owned(),
    });
    assert!(
        app.semantic_snapshot().turn.is_none(),
        "a rebind to another session retires the previous turn's marker"
    );
    app.apply_host_event(HostEvent::TurnLifecycle(lifecycle(
        "turn-2",
        TurnPhase::Completed,
    )));
    app.apply_host_event(HostEvent::ClearView);
    assert!(app.semantic_snapshot().turn.is_none());
}

#[test]
fn same_length_private_draft_edits_advance_the_revision_without_leaking_text() {
    let mut app = App::new(launch_basic(), true);
    for character in "ABCD".chars() {
        app.handle_key(char_key(character));
    }
    let revision_before = app.semantic_revision;
    for _ in 0..4 {
        app.handle_key(super::KeyEvent::new(
            super::KeyCode::Backspace,
            super::KeyModifiers::NONE,
        ));
    }
    for character in "WXYZ".chars() {
        app.handle_key(char_key(character));
    }
    assert_eq!(app.input, "WXYZ");
    assert!(
        app.semantic_revision > revision_before,
        "an equal-length private edit still advances the conservative revision"
    );
    let serialized = serde_json::to_string(&app.semantic_snapshot()).unwrap();
    assert!(!serialized.contains("WXYZ"));
    assert!(!serialized.contains("ABCD"));
}

#[cfg(feature = "test-hooks")]
mod sidecar {
    use super::super::pilot_sidecar::{self, PilotRequest};
    use super::*;

    fn handle_request(app: &mut App, request: &PilotRequest) -> pilot_sidecar::PilotOutcome {
        pilot_sidecar::handle_request(app, request).expect("pilot request outcome")
    }

    fn request(action: Option<&str>, text: Option<&str>, if_revision: Option<u64>) -> PilotRequest {
        PilotRequest {
            schema: "plak.tui-pilot.v2".to_owned(),
            sequence: 1,
            if_revision,
            action: action.map(str::to_owned),
            text: text.map(str::to_owned),
        }
    }

    #[test]
    fn stale_revision_never_mutates_or_emits_a_host_action() {
        let mut app = App::new(launch_basic(), true);
        let revision = app.semantic_revision;
        let typed = handle_request(
            &mut app,
            &request(Some("type-text"), Some("draft"), Some(revision)),
        );
        assert_eq!(typed.status, "applied");
        assert_eq!(app.input, "draft");
        let owner = app.semantic_snapshot().owner;
        for fence in [Some(app.semantic_revision + 7), None, Some(u64::MAX)] {
            let outcome = handle_request(&mut app, &request(Some("send"), None, fence));
            assert_eq!(outcome.status, "stale_observation", "fence {fence:?}");
            assert!(outcome.error.is_none());
            assert!(outcome.action.is_none(), "stale sends emit no UserAction");
            assert_eq!(app.input, "draft", "stale sends keep the draft");
            assert_eq!(app.semantic_snapshot().owner, owner);
        }
        let stale = app.semantic_revision + 1;
        let outcome = handle_request(&mut app, &request(Some("confirm"), None, Some(stale)));
        assert_eq!(outcome.status, "stale_observation");
        assert!(outcome.action.is_none());
        assert_eq!(app.input, "draft");
        assert_eq!(app.semantic_snapshot().owner, owner);
    }

    #[test]
    fn current_revision_send_applies_and_marks_the_host_intent_pending() {
        let mut app = App::new(launch_basic(), true);
        let revision = app.semantic_revision;
        let typed = handle_request(
            &mut app,
            &request(Some("type-text"), Some("draft"), Some(revision)),
        );
        assert_eq!(typed.status, "applied");
        assert!(typed.action.is_none());
        let revision = app.semantic_revision;
        let sent = handle_request(&mut app, &request(Some("send"), None, Some(revision)));
        assert_eq!(sent.status, "applied");
        assert!(
            matches!(sent.action, Some(UserAction::Prompt(_))),
            "send lowers to the same host intent a human gesture emits: {:?}",
            sent.action
        );
        assert!(
            sent.snapshot.turn.is_none(),
            "a pending host intent is not a terminal outcome"
        );
        assert_eq!(sent.revision, app.semantic_revision);
        assert_ne!(sent.observation_digest, "");
    }

    #[test]
    fn observe_requests_cannot_mutate_or_fence() {
        let mut app = App::new(launch_basic(), true);
        let revision = app.semantic_revision;
        for malformed in [
            request(None, Some("text"), None),
            request(Some("observe"), Some("text"), None),
            request(None, None, Some(revision)),
            request(Some("observe"), None, Some(revision)),
        ] {
            let outcome = handle_request(&mut app, &malformed);
            assert_eq!(outcome.status, "rejected");
            assert!(outcome.error.is_some());
            assert!(outcome.action.is_none());
            assert_eq!(app.semantic_revision, revision);
        }
        let outcome = handle_request(&mut app, &request(None, None, None));
        assert_eq!(outcome.status, "observed");
        assert!(outcome.action.is_none());
        assert_eq!(outcome.revision, revision);
    }

    #[test]
    fn illegal_action_with_a_current_fence_is_rejected_not_stale() {
        let mut app = App::new(launch_basic(), true);
        let revision = app.semantic_revision;
        let outcome = handle_request(&mut app, &request(Some("send"), None, Some(revision)));
        assert_eq!(outcome.status, "rejected");
        assert!(outcome.error.is_some());
        assert!(outcome.action.is_none());
    }

    #[test]
    fn max_revision_fence_is_never_a_valid_mutation_precondition() {
        let mut app = App::new(launch_basic(), true);
        app.semantic_revision = u64::MAX;
        let outcome = handle_request(
            &mut app,
            &request(Some("type-text"), Some("x"), Some(u64::MAX)),
        );
        assert_eq!(outcome.status, "stale_observation");
        assert!(outcome.action.is_none());
        assert_eq!(app.input, "");
    }

    #[test]
    fn an_aba_draft_edit_still_invalidates_the_older_fence() {
        let mut app = App::new(launch_basic(), true);
        let first = app.semantic_revision;
        let typed = handle_request(
            &mut app,
            &request(Some("type-text"), Some("AB"), Some(first)),
        );
        assert_eq!(typed.status, "applied");
        let second = app.semantic_revision;
        assert!(second > first);
        let edited = handle_request(
            &mut app,
            &request(Some("type-text"), Some("CD"), Some(second)),
        );
        assert_eq!(edited.status, "applied");
        assert_eq!(app.input, "ABCD");
        for _ in 0..4 {
            app.handle_key(super::super::KeyEvent::new(
                super::super::KeyCode::Backspace,
                super::super::KeyModifiers::NONE,
            ));
        }
        let revision = app.semantic_revision;
        let retyped = handle_request(
            &mut app,
            &request(Some("type-text"), Some("AB"), Some(revision)),
        );
        assert_eq!(retyped.status, "applied");
        assert_eq!(app.input, "AB");
        let stale = handle_request(&mut app, &request(Some("send"), None, Some(second)));
        assert_eq!(
            stale.status, "stale_observation",
            "the ABA sequence advanced the revision past the older fence"
        );
        assert!(stale.action.is_none());
        assert_eq!(app.input, "AB");
    }
}

#[test]
fn composer_image_tokens_attach_by_slash_or_path_drop_and_ride_the_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let shot = dir.path().join("shot one.png");
    std::fs::write(
        &shot,
        crate::config::image_attachments::png(1280, 720, 2048),
    )
    .unwrap();
    let notes = dir.path().join("notes.txt");
    std::fs::write(&notes, "not an image").unwrap();
    let mut app = App::new(launch_basic(), true);

    // `/image <path>` puts a token in the draft without sending anything.
    app.set_input(format!("/image {}", shot.display()));
    assert_eq!(app.submit_line(false), None);
    assert_eq!(app.input, "[image #1] ");
    app.settle_attachments();
    assert_eq!(app.semantic_snapshot().composer.attachment_count, 1);
    assert_eq!(
        app.attachment_chips(200),
        "[image #1: shot one.png 1280x720 3 KB]"
    );
    assert_eq!(app.attachment_chips(20), "[1 image]");

    // A dropped path (escaped space) attaches; a dropped non-image types.
    app.handle_paste(&shot.display().to_string().replace(' ', "\\ "));
    app.settle_attachments();
    assert_eq!(app.input, "[image #1] [image #2] ");
    app.handle_paste(&notes.display().to_string());
    assert_eq!(app.attachment_count(), 2);
    assert!(app.input.ends_with(&notes.display().to_string()));
    app.set_input("[image #2] ".to_owned());

    // Tokens alone send the images alone.
    let sent = app.submit_line(false);
    let Some(UserAction::Prompt(draft)) = sent else {
        panic!("expected an image-only prompt, got {sent:?}")
    };
    assert_eq!(draft.text, "");
    assert_eq!(draft.images.len(), 1);
    assert_eq!(app.attachment_count(), 0);

    // A prompt the host withholds comes back with its images as tokens.
    app.apply_host_event(HostEvent::DraftRestored(draft));
    assert_eq!(app.input, "[image #1]");
    assert_eq!(app.attachment_count(), 1);

    app.set_input("what is [image #1]?".to_owned());
    let sent = app.submit_line(false);
    let Some(UserAction::Prompt(draft)) = sent else {
        panic!("expected a prompt, got {sent:?}")
    };
    assert_eq!(draft.text, "what is [image #1]?");
    assert_eq!(draft.images.len(), 1);
    assert_eq!(draft.images[0].name, "shot one.png");
    assert_eq!(app.attachment_count(), 0);

    // A refused draft returns its images to the composer.
    app.apply_host_event(HostEvent::DraftRejected(UserAction::Prompt(draft)));
    assert_eq!(app.attachment_count(), 1);
    assert_eq!(app.input, "what is [image #1]?");

    // A missing file is a note naming it; its token leaves the draft.
    app.attach_image(&dir.path().join("gone.png"));
    app.settle_attachments();
    assert_eq!(app.attachment_count(), 1);
    assert_eq!(app.input, "what is [image #1]?");
}
