//! Agents tab, binding, and provider tests.

use super::terminal::{commit_sent_action, dispatch_action, send_user_action};
use super::test_support::{
    blocks_contain, key, launch_basic, left_click, lines_text, models_app, provider_rows,
    providers_app, providers_text, serialize_frame, usage_body_text, usage_fixture,
    usage_offline_fixture,
};
use super::{App, user_phase};
use crate::tui::ProviderRowPresentation as _;
use crate::tui::blocks::ScrollBlock;
use crate::tui::presentation::{ContextUsage, SessionLedgerView};
use crate::tui::{HostEvent, UserAction, user_action_channel};
use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

#[test]
fn tui_usage_last_failed_refresh_uses_launch_home_and_fixed_zone()
-> Result<(), Box<dyn std::error::Error>> {
    use plakous::model::{
        SubscriptionUsageCredentialState, SubscriptionUsageRead, SubscriptionUsageUnavailable,
        Timestamp,
    };
    use plakous_hosted::subscription_usage::{self as usage, VerticalObservation};
    static ZONE: jiff::tz::TimeZone = jiff::tz::TimeZone::fixed(jiff::tz::offset(-4));
    let (_guard, paths) = crate::product_paths::PlakPaths::isolated_test_home();
    paths.ensure_layout()?;
    drop(crate::telemetry_store::TelemetryStore::open(&paths)?);
    let db = rusqlite::Connection::open(paths.telemetry_database())?;
    db.execute_batch(
        "INSERT INTO credential_refreshes
        (at, provider, phase, failure_class, transport_phase, generation) VALUES
        (1720000100000000000, 'claude', 'restored', 'timeout', 'resolution', 2);",
    )?;
    let mut launch = launch_basic();
    launch.telemetry_paths = Some(paths);
    launch.time_zone = &ZONE;
    let mut app = App::new(launch, true);
    let limits = usage::project_limits(
        vec![(
            usage::claude_vertical(),
            VerticalObservation::Native(SubscriptionUsageRead::Unavailable {
                observed_at: Timestamp::new(1_720_000_200, 0)?,
                reason: SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::RefreshRequired,
                ),
            }),
        )],
        SessionLedgerView::default(),
    );
    app.apply_host_event(HostEvent::UsageLimits(limits));
    let body = lines_text(&app.usage_tab_lines(200));
    assert!(body.contains("refresh failed at 05:48: network unreachable (DNS lookup timed out); credential unchanged; retry when online"), "{body}");
    assert!(!body.contains("credential needs a token refresh"), "{body}");
    Ok(())
}

/// Right after a Claude → Codex rebind the header read
/// `1%/1M` — Claude's window — while the binding line already read
/// `codex · gpt-5.6-luna · low`, whose window is 272k, until the next turn
/// corrected it. A reading measured against a binding that no longer
/// exists is retired, not carried over.
#[test]
fn a_cross_provider_rebind_retires_the_previous_context_window() {
    let mut app = App::new(launch_basic(), true);
    app.account = Some("claude".into());
    app.model = Some("claude-opus-5".into());
    app.apply_host_event(HostEvent::Usage(ContextUsage {
        total_window: Some(1_000_000),
        display_percent: Some(1),
        evidence: "observed".into(),
        status: "ok".into(),
        ..super::unknown_context_usage()
    }));
    assert_eq!(app.context_usage.total_window, Some(1_000_000));

    app.apply_host_event(HostEvent::Binding {
        session_id: None,
        account: Some("codex".into()),
        model: Some("gpt-5.6-luna".into()),
        effort: Some("low".into()),
        extra: None,
        composition: crate::tui::CompositionSelection::FollowCatalog,
        protocol: crate::tui::ProtocolSelection::Auto,
        effective_presentation: "direct".to_owned(),
    });
    assert_eq!(
        app.context_usage.total_window, None,
        "the previous vertical's window does not outlive its binding"
    );
    assert_eq!(app.context_usage.display_percent, None);

    // An effort-only change is the same binding: nothing is retired.
    app.apply_host_event(HostEvent::Usage(ContextUsage {
        total_window: Some(272_000),
        display_percent: Some(3),
        evidence: "observed".into(),
        status: "ok".into(),
        ..super::unknown_context_usage()
    }));
    app.apply_host_event(HostEvent::Binding {
        session_id: None,
        account: Some("codex".into()),
        model: Some("gpt-5.6-luna".into()),
        effort: Some("medium".into()),
        extra: None,
        composition: crate::tui::CompositionSelection::FollowCatalog,
        protocol: crate::tui::ProtocolSelection::Auto,
        effective_presentation: "direct".to_owned(),
    });
    assert_eq!(app.context_usage.total_window, Some(272_000));
}

#[test]
fn retry_attach_uses_compact_model_transition_copy() {
    let mut app = App::new(launch_basic(), true);
    dispatch_action(&mut app, &UserAction::RetryAttach, None);
    assert_eq!(app.status_detail.as_deref(), Some("switching model…"));
    assert_eq!(user_phase("rebind"), "switching model…");

    app.apply_host_event(HostEvent::Status {
        phase: "rebind".into(),
        queue_len: 0,
        detail: Some("resolving exact target codex/gpt-secret".into()),
    });
    app.apply_host_event(HostEvent::SetBusy(true));
    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let frame = serialize_frame(terminal.backend().buffer());
    assert!(frame.contains("switching·model…"));
    assert!(!frame.contains("gpt-secret"));
}

#[test]
fn configured_light_theme_is_applied_at_launch() {
    let mut launch = launch_basic();
    launch.theme = crate::tui::SettingsTheme::DefaultLight;
    let app = App::new(launch, true);
    assert_eq!(app.theme, crate::tui::Theme::light());
    let mut launch = launch_basic();
    launch.theme = crate::tui::SettingsTheme::CatppuccinLatte;
    let app = App::new(launch, true);
    assert_eq!(app.theme, crate::tui::Theme::catppuccin_latte());
}

#[test]
fn saturated_dispatch_restores_prompt_and_edit() {
    fn saturated_sender() -> crate::tui::UserActionSender {
        let (sender, _receiver) = user_action_channel();
        for index in 0..plakous_host_protocol::limits::INTENT_QUEUE_CAPACITY {
            sender
                .send(UserAction::Prompt(index.to_string().into()))
                .unwrap();
        }
        sender
    }

    let mut prompt_app = App::new(launch_basic(), true);
    prompt_app.apply_host_event(HostEvent::PromptHistorySnapshot(vec!["published".into()]));
    let prompt = UserAction::Prompt("keep this draft".into());
    assert!(!send_user_action(
        &mut prompt_app,
        Some(&saturated_sender()),
        prompt
    ));
    assert_eq!(prompt_app.input, "keep this draft");
    assert_eq!(prompt_app.prompt_history, ["published"]);
    assert!(blocks_contain(&prompt_app, "action not sent"));

    let mut edit_app = App::new(launch_basic(), true);
    edit_app.queue_editing = Some(super::QueueEdit {
        id: "q-1".into(),
        revision: 4,
        replaced_draft: String::new(),
    });
    assert!(!send_user_action(
        &mut edit_app,
        Some(&saturated_sender()),
        UserAction::EditQueued {
            id: "q-1".into(),
            revision: 4,
            text: "edited draft".into(),
        }
    ));
    assert_eq!(edit_app.input, "edited draft");
    assert_eq!(
        edit_app
            .queue_editing
            .as_ref()
            .map(super::QueueEdit::target),
        Some(("q-1", 4))
    );
}

#[test]
fn failed_clear_keeps_source_view_and_binding() {
    let mut app = App::new(launch_basic(), true);
    app.pane
        .blocks
        .push(ScrollBlock::Assistant("source transcript".into()));
    app.session_id = Some("source-session".into());
    commit_sent_action(&mut app, &UserAction::ClearSession);

    app.apply_host_event(HostEvent::Row(
        plakous_host_protocol::views::transcript::TranscriptRow::info(
            plakous_host_protocol::views::transcript::NoticeScope::Host,
            "clear session failed; previous session remains active",
        ),
    ));
    app.apply_host_event(HostEvent::Status {
        phase: "idle".into(),
        queue_len: 0,
        detail: Some("clear refused · source remains active".into()),
    });

    assert!(blocks_contain(&app, "source transcript"));
    assert!(blocks_contain(&app, "previous session remains active"));
    assert_eq!(app.session_id.as_deref(), Some("source-session"));
    assert!(!app.clear_pending);
}

#[test]
fn error_note_http_400_provider_code_is_error_block() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::Row(
        plakous_host_protocol::views::transcript::TranscriptRow::error(
            plakous_host_protocol::views::transcript::NoticeScope::Host,
            "HTTP 400 · provider code: context_length_exceeded",
        ),
    ));
    assert!(app.pane.blocks.iter().any(|block| matches!(
        block,
        ScrollBlock::Error(text)
            if text.contains("HTTP 400") && text.contains("provider code:")
    )));
}

#[test]
/// Spec 02 §7.2: a non-Ready vertical carries no numbers and draws no bar; the
/// line it does carry names the CLI command that fixes it.
fn a_non_ready_vertical_is_one_plain_line_with_its_cli_remedy() {
    let mut app = usage_offline_fixture();
    let body = usage_body_text(&mut app, 120);
    // The remedy follows the account's auth route. OpenRouter is an
    // API key, and `plak login openrouter` is a command the CLI refuses.
    for (label, remedy) in [
        ("Codex", "plak login codex"),
        ("SuperGrok", "plak login grok"),
        ("OpenRouter", "plak key set openrouter"),
    ] {
        assert!(body.contains(label), "{label} must be listed");
        assert!(
            body.contains(&format!("not connected · run `{remedy}`")),
            "{body}"
        );
    }
    assert!(
        !body.contains("plak login openrouter"),
        "an API-key account is never given an OAuth verb: {body}"
    );
    assert!(body.contains("not connected · run `plak login claude`"));
    let limits = body
        .split("Limits\n")
        .nth(1)
        .expect("the Limits section renders");
    let limits = limits.split("Session\n").next().unwrap_or(limits);
    assert!(
        !limits.contains(crate::tui::usage_tab::LIMIT_BAR_CELL),
        "no bar is drawn: {limits}"
    );
}

#[test]
fn the_limits_section_lists_all_four_verticals_and_never_prints_zero_dollars() {
    let mut app = usage_fixture();
    let body = usage_body_text(&mut app, 120);
    for label in ["Codex", "SuperGrok", "Claude", "OpenRouter"] {
        assert!(body.contains(label), "{label} must be listed");
    }
    assert!(body.contains("not connected · run `plak login claude`") || body.contains("Claude"));
    assert!(body.contains("not available (no provider reported a cost)"));
    assert!(!body.contains("$0.00 "), "an unknown cost is never $0");
    // The bar fills with what is LEFT, floored over 20 cells, and the label
    // column is measured once across every vertical so the bars line up.
    let bar_at = |needle: &str| {
        body.lines()
            .find(|line| line.contains(needle))
            .and_then(|line| line.find('['))
    };
    assert!(body.contains("99% left (resets 23:44 on 6 Sep)"), "{body}");
    assert!(body.contains("22% left (resets 16:00 on 3 Sep)"), "{body}");
    assert!(body.contains("53% left (resets 00:00 on 8 Sep)"), "{body}");
    assert_eq!(
        bar_at("99% left"),
        bar_at("22% left"),
        "Codex bars share a column:\n{body}"
    );
    assert_eq!(
        bar_at("99% left"),
        bar_at("53% left"),
        "SuperGrok bars share the Codex column:\n{body}"
    );
    assert!(body.contains("$99.99"), "{body}");
    assert!(body.contains("$3.55 used of $50.00 cap"), "{body}");
    // The account is its own line, and the Spark group an indented block.
    assert!(
        body.contains("Account: operator@example.com (Pro)"),
        "{body}"
    );
    assert!(body.contains("GPT-5.3-Codex-Spark limit:"), "{body}");
}

/// Spec 02 §5.5: the Agents tab is the profile list and nothing else, so there
/// is no readiness header, no Templates section, and no login action.
#[test]
fn the_models_tab_carries_no_readiness_header() {
    let mut app = models_app();
    app.providers = provider_rows();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let frame = serialize_frame(terminal.backend().buffer());
    assert!(!frame.contains("Readiness"), "no readiness header: {frame}");
    assert!(
        !frame.contains("plak login"),
        "no CLI remedy on Models: {frame}"
    );
    assert!(
        !frame.contains("Templates"),
        "no Templates section: {frame}"
    );
    assert!(frame.contains("codex"), "the profile list paints: {frame}");
}

/// Spec 02 §5.1: `/providers` opens the Providers tab through the real
/// dispatch, and the tab sits between Models and Usage in the strip.
#[test]
fn the_providers_command_opens_the_read_only_providers_tab() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("/providers".to_owned());
    let emitted = app.submit_line(false);
    assert_eq!(app.control_tab, Some(super::ControlTab::Providers));
    assert_eq!(emitted, Some(UserAction::RefreshReadiness));

    // `←`/`→` reach it from its neighbours in the normative order.
    app.handle_key(key(KeyCode::Left));
    assert_eq!(app.control_tab, Some(super::ControlTab::Models));
    app.handle_key(key(KeyCode::Right));
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.control_tab, Some(super::ControlTab::Usage));
}

/// Spec 02 §5.5: every vertical is three lines under its name — the account
/// the host knows, the status, and the CLI command that connects it.
#[test]
fn every_vertical_states_its_account_status_and_cli_command() {
    let mut app = providers_app();
    providers_text(&mut app);
    let text = lines_text(&app.provider_lines(app.providers_focus));
    for (label, account, status, cli) in [
        (
            "Codex (ChatGPT)",
            "operator@example.com (Pro)",
            "ready",
            "plak login codex",
        ),
        (
            "Grok (SuperGrok)",
            "not reported",
            "refresh outcome unknown",
            "plak login grok",
        ),
        (
            "Claude (Max)",
            "not reported",
            "login required",
            "plak login claude",
        ),
        (
            "OpenRouter",
            "sk-or…9f2c",
            "ready",
            "plak key set openrouter",
        ),
    ] {
        assert!(text.contains(label), "{label}: {text}");
        assert!(
            text.contains(&format!("account: {account}")),
            "{label} account: {text}"
        );
        assert!(
            text.contains(&format!("status:  {status}")),
            "{label} status: {text}"
        );
        assert!(
            text.contains(&format!("cli:     {cli}")),
            "{label} cli: {text}"
        );
    }
    // Every command the tab prints is one the CLI actually parses.
    let catalog = crate::tui::providers::remedy_catalog();
    for row in &app.providers {
        assert!(catalog.contains(&row.cli_direction()), "{}", row.id);
    }
}

/// Spec 02 §5.5: nothing on the tab is actionable. `Enter` emits no intent
/// and a click only focuses.
#[test]
fn the_providers_tab_has_no_actionable_rows() {
    let mut app = providers_app();
    providers_text(&mut app);
    for _ in 0..app.providers.len().saturating_add(2) {
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None, "Enter is inert");
    }
    assert_eq!(app.providers_focus, 0, "Enter does not move the cursor");
    assert_eq!(app.control_tab, Some(super::ControlTab::Providers));

    assert_eq!(app.handle_key(key(KeyCode::Down)), None);
    assert_eq!(app.providers_focus, 1, "↓ focuses the next vertical");

    let (index, rect) = *app
        .provider_row_hits
        .last()
        .expect("every painted vertical is hit-testable");
    assert_eq!(
        app.handle_control_mouse(left_click(rect.x + 2, rect.y)),
        None,
        "a click emits no intent"
    );
    assert_eq!(app.providers_focus, index, "the click only focuses");

    let painted = providers_text(&mut app);
    for forbidden in ["Login", "Log out", "Logout", "Paste key", "Set key"] {
        assert!(!painted.contains(forbidden), "{forbidden}: {painted}");
    }
}
