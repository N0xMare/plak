//! Shared fixtures for the `ui` unit tests.

use super::{App, DisplayMode, TranscriptChange};
use crate::tui::SurfaceObservation;
use crate::tui::TreeNavigator;
use crate::tui::blocks::ScrollBlock;
use crate::tui::presentation::{ActivityKind, ContextUsage};
use crate::tui::{AuthKind, ProviderDef, ProviderState};
use crate::tui::{
    HostEvent, QueueDrainState, QueueItem, ResumeMenu, ResumeSessionEntry, SettingsSnapshot,
    TuiLaunch,
};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use plakous_host_protocol::topology::{NodeStatus, TreeAdmissionState, WorkspaceMode};
use plakous_host_protocol::views::transcript::{ToolItem, TranscriptRow};
use plakous_host_protocol::views::{NodeView, TreeView};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use std::fmt::Write as _;
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

/// 2026-09-03T12:00:00Z. Every resume-picker fixture measures its ages
/// against this instant, so the age column is a golden-stable value rather
/// than a wall-clock read.
pub(super) const FIXED_NOW: u64 = 1_788_436_800;

pub(super) fn assert_visible_transcript_rows_match_projection(app: &App, buffer: &Buffer) {
    let body = app.transcript_body_area.expect("transcript body");
    let plain = app.pane.transcript_cache.plain();
    for screen_row in 0..usize::from(body.height) {
        let content_row = app.pane.transcript_visible_start + screen_row;
        let Some(expected) = plain.get(content_row) else {
            continue;
        };
        let y = body.y + u16::try_from(screen_row).expect("test row");
        let painted = (body.x..body.right())
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>();
        let mut padded = String::new();
        for grapheme in expected.graphemes(true) {
            padded.push_str(grapheme);
            padded.extend(std::iter::repeat_n(' ', grapheme.width().saturating_sub(1)));
        }
        padded.push_str(&" ".repeat(usize::from(body.width).saturating_sub(expected.width())));
        assert_eq!(painted, padded, "content row {content_row}");
    }
}

/// Registry index of a settings row, by key.
pub(super) fn settings_row(key: &str) -> usize {
    crate::tui::settings_registry::index_of(key).expect("registered settings row")
}

/// The host-projected credential status the Providers tab paints, with all
/// four verticals in mixed states.
pub(super) fn provider_rows() -> Vec<ProviderDef> {
    vec![
        ProviderDef {
            id: "codex".into(),
            label: "Codex (ChatGPT)".into(),
            kind: AuthKind::Oauth,
            state: ProviderState::Ready,
            detail: String::new(),
            account_label: None,
        },
        ProviderDef {
            id: "grok".into(),
            label: "Grok (SuperGrok)".into(),
            kind: AuthKind::Oauth,
            state: ProviderState::RefreshUnknown,
            detail: "quarantined".into(),
            account_label: None,
        },
        ProviderDef {
            id: "claude".into(),
            label: "Claude (Max)".into(),
            kind: AuthKind::Oauth,
            state: ProviderState::NotLoggedIn,
            detail: String::new(),
            account_label: None,
        },
        ProviderDef {
            id: "openrouter".into(),
            label: "OpenRouter".into(),
            kind: AuthKind::ApiKey,
            state: ProviderState::Ready,
            detail: "stored".into(),
            account_label: Some("sk-or…9f2c".into()),
        },
    ]
}

pub(super) fn launch_basic() -> TuiLaunch {
    TuiLaunch {
        version: "0.1.0",
        session_id: Some("s1".into()),
        workspace: PathBuf::from("/tmp/ws"),
        workspace_chrome: "~/ws · main".into(),
        open_resume_picker: false,
        account: Some("acct".into()),
        model: Some("model-x".into()),
        effort: Some("high".into()),
        extra: None,
        composition: crate::tui::CompositionSelection::FollowCatalog,
        protocol: crate::tui::ProtocolSelection::Messages,
        effective_presentation: "direct".into(),
        agent_enabled: true,
        mouse_enabled: true,
        theme: crate::tui::SettingsTheme::DefaultDark,
        settings: None,
        providers: provider_rows(),
        telemetry_paths: None,
        time_zone: &crate::tui::UTC_ZONE,
        opening_note: None,
    }
}

/// One frame of `app` at `width`×`height`, as plain text lines.
pub(super) fn render_to_string(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The three built-in profiles as the product lists them.
pub(super) fn builtin_profiles_view() -> crate::tui::ProfilesView {
    use crate::tui::{ProfileRole, ProfileRow};
    let role = |label: &str, account: &str, model: &str, effort: &str, fast: bool| ProfileRole {
        label: label.to_owned(),
        account: account.to_owned(),
        model: model.to_owned(),
        effort: effort.to_owned(),
        fast,
        depth: 0,
    };
    let row = |id: &str, aliases: &[&str], roles: [ProfileRole; 4]| {
        let [root, worker, reviewer, researcher] = roles;
        ProfileRow {
            id: id.to_owned(),
            aliases: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
            root,
            cards: vec![worker, reviewer, researcher],
        }
    };
    crate::tui::ProfilesView {
        rows: vec![
            row(
                "codex-default",
                &["codex", "chatgpt"],
                [
                    role("root", "codex", "gpt-6-astra", "medium", false),
                    role("worker", "codex", "gpt-6-sol", "medium", false),
                    role("reviewer", "codex", "gpt-6-astra", "medium", false),
                    role("researcher", "codex", "gpt-6-luna", "xhigh", true),
                ],
            ),
            row(
                "claude-default",
                &["claude"],
                [
                    role("root", "claude", "claude-opus-5-5", "high", false),
                    role("worker", "claude", "claude-sonnet-5", "medium", false),
                    role("reviewer", "claude", "claude-opus-5-5", "high", false),
                    role("researcher", "claude", "claude-opus-5-5", "low", false),
                ],
            ),
            row(
                "grok-default",
                &["grok", "supergrok"],
                [
                    role("root", "grok", "grok-4.7", "medium", false),
                    role("worker", "grok", "grok-4.7-build-fast", "medium", false),
                    role("reviewer", "grok", "grok-4.7", "high", false),
                    role("researcher", "grok", "grok-4.7-build-fast", "medium", false),
                ],
            ),
        ],
        current: None,
        diagnostics: Vec::new(),
    }
}

/// Plain text of `lines`.
pub(super) fn lines_text(lines: &[ratatui::text::Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The three read-only tab bodies, concatenated, as the one Usage page
/// read when it was one page.
pub(super) fn usage_body_text(app: &mut App, width: u16) -> String {
    let mut text = lines_text(&app.usage_tab_lines(120));
    text.push('\n');
    text.push_str(&lines_text(&app.context_tab_lines(width)));
    text.push('\n');
    let rows = app.session_info_rows();
    let (lines, _) = crate::tui::usage_tab::session_lines(&rows, &app.theme);
    text.push_str(&lines_text(&lines));
    text
}

/// One Menu opened on Usage with a complete four-vertical
/// projection, the Ready/NotConnected/Unavailable states the spec names,
/// and the typed session facts.
#[allow(
    clippy::too_many_lines,
    reason = "one explicit fixture pins every state the Usage spec names"
)]
pub(super) fn usage_fixture() -> App {
    use crate::tui::presentation::{
        AdmittedContextCeiling, ContextBreakdownView, ContextCategoryView, ContextInfoRowView,
        PreparedInputUsage, SessionInfoView, SessionLedgerView, UsageAmountView, UsageBalanceView,
        UsageLimitsView, UsageMeterReading, UsageMeterView, UsageVertical, VerticalUsageState,
        VerticalUsageView,
    };
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.control_tab = Some(super::ControlTab::Usage);
    app.model = Some("grok-4.6".into());
    app.account = Some("grok".into());
    app.effort = Some("medium".into());
    app.phase = "idle".into();
    app.tree_epoch_state = Some("live".into());
    app.context_usage = ContextUsage {
        breakdown: Some(ContextBreakdownView {
            used_tokens: 36_700,
            total_tokens: 1_000_000,
            categories: vec![
                ContextCategoryView {
                    label: "System".into(),
                    tokens: 1_200,
                },
                ContextCategoryView {
                    label: "Messages".into(),
                    tokens: 29_900,
                },
                ContextCategoryView {
                    label: "Reasoning".into(),
                    tokens: 5_600,
                },
            ],
            informational: vec![
                ContextInfoRowView {
                    label: "Tool definitions".into(),
                    tokens: 5_600,
                    detail: Some("12 tools".into()),
                },
                ContextInfoRowView {
                    label: "Skills".into(),
                    tokens: 2_400,
                    detail: Some("21 skills".into()),
                },
                ContextInfoRowView {
                    label: "Instructions".into(),
                    tokens: 1_100,
                    detail: Some("2 files".into()),
                },
            ],
            auto_compact_trigger_tokens: Some(900_000),
            occupancy_anchored: true,
            turns: Some(5),
            tool_calls: Some(12),
            compactions: Some(0),
        }),
        prepared_input: Some(PreparedInputUsage::Exact(36_700)),
        total_window: Some(1_000_000),
        admitted_ceiling: Some(AdmittedContextCeiling {
            tokens: 1_000_000,
            basis: "exact".into(),
        }),
        gauge_percent: Some(3),
        display_percent: Some(3),
        ceiling_excess: None,
        evidence: "release-verified".into(),
        non_guarantee: None,
        status: "fits".into(),
    };
    app.subscription_usage = crate::tui::presentation::SubscriptionUsageView::Read;
    app.usage_limits = UsageLimitsView {
        verticals: vec![
            VerticalUsageView {
                vertical: UsageVertical::new("codex", "Codex"),
                account_email: Some("operator@example.com".into()),
                plan_label: Some("Pro".into()),
                observed_at: None,
                state: VerticalUsageState::Ready {
                    meters: vec![
                        UsageMeterView {
                            label: "Weekly limit".into(),
                            group: None,
                            reading: UsageMeterReading::Percent {
                                floored_percent: 1,
                                exact_percent: "0.4".into(),
                                percent_left: Some(99),
                            },
                            resets_at: Some("23:44 on 6 Sep".into()),
                        },
                        UsageMeterView {
                            label: "5h limit".into(),
                            group: None,
                            reading: UsageMeterReading::Percent {
                                floored_percent: 78,
                                exact_percent: "77.9".into(),
                                percent_left: Some(22),
                            },
                            resets_at: Some("16:00 on 3 Sep".into()),
                        },
                        UsageMeterView {
                            label: "Weekly limit".into(),
                            group: Some("GPT-5.3-Codex-Spark limit".into()),
                            reading: UsageMeterReading::Percent {
                                floored_percent: 20,
                                exact_percent: "19.6".into(),
                                percent_left: Some(80),
                            },
                            resets_at: Some("23:44 on 6 Sep".into()),
                        },
                    ],
                },
                balances: Vec::new(),
            },
            VerticalUsageView {
                vertical: UsageVertical::new("grok", "SuperGrok"),
                account_email: None,
                plan_label: Some("SuperGrok Heavy".into()),
                observed_at: None,
                state: VerticalUsageState::Ready {
                    meters: vec![UsageMeterView {
                        label: "Weekly limit".into(),
                        group: None,
                        reading: UsageMeterReading::Percent {
                            floored_percent: 46,
                            exact_percent: "46.4".into(),
                            percent_left: Some(53),
                        },
                        resets_at: Some("00:00 on 8 Sep".into()),
                    }],
                },
                balances: vec![
                    UsageBalanceView {
                        label: "Credits".into(),
                        value: "$99.99".into(),
                    },
                    UsageBalanceView {
                        label: "Pay as you go".into(),
                        value: "$3.55 used of $50.00 cap".into(),
                    },
                ],
            },
            VerticalUsageView {
                vertical: UsageVertical::new("claude", "Claude"),
                account_email: None,
                plan_label: None,
                observed_at: None,
                state: VerticalUsageState::NotConnected,
                balances: Vec::new(),
            },
            VerticalUsageView {
                vertical: UsageVertical::new("openrouter", "OpenRouter"),
                account_email: None,
                plan_label: Some("API key · free tier".into()),
                observed_at: None,
                state: VerticalUsageState::Ready {
                    meters: vec![
                        UsageMeterView {
                            label: "Key limit".into(),
                            group: None,
                            reading: UsageMeterReading::Amounts {
                                entries: vec![
                                    UsageAmountView {
                                        label: "cap".into(),
                                        value: "$10.00".into(),
                                    },
                                    UsageAmountView {
                                        label: "remaining".into(),
                                        value: "$6.42".into(),
                                    },
                                ],
                            },
                            resets_at: Some("never".into()),
                        },
                        UsageMeterView {
                            label: "Key usage".into(),
                            group: None,
                            reading: UsageMeterReading::Amounts {
                                entries: vec![
                                    UsageAmountView {
                                        label: "all time".into(),
                                        value: "$3.58".into(),
                                    },
                                    UsageAmountView {
                                        label: "today".into(),
                                        value: "$0.40".into(),
                                    },
                                ],
                            },
                            resets_at: None,
                        },
                    ],
                },
                balances: Vec::new(),
            },
        ],
        session: SessionLedgerView {
            input_tokens: 1_234_567,
            cached_input_tokens: 1_000_000,
            output_tokens: 45_678,
            reasoning_tokens: 12_000,
            model_calls: 42,
            reporting_model_calls: 40,
            turn_wall_time: Some("3m12s".into()),
            cost: crate::tui::presentation::UsageCostView::default(),
        },
        stale_age_seconds: None,
    };
    app.session_info = SessionInfoView {
        host_version: "0.1.0".into(),
        session_id: "sid-123abc".into(),
        working_dir: "/home/user/project".into(),
        root_template: None,
        root_model: Some("grok-4.6".into()),
        root_effort: Some("medium".into()),
        thinking: "reasoning summaries (concise) requested at medium".into(),
        tool_policy: Some("Code Mode (native-ptc)".into()),
        admitted_options: Vec::new(),
        api_kernel: Some("Responses".into()),
        turn_number: Some(5),
        tree_epoch: Some("live".into()),
        context: None,
    };
    app
}

/// The same page with nothing connected: the states a landed read cannot
/// show at the same time as a Ready one.
pub(super) fn usage_offline_fixture() -> App {
    let mut app = usage_fixture();
    app.subscription_usage = crate::tui::presentation::SubscriptionUsageView::Loading;
    // The roster is product data, not a protocol default: the unread page
    // still names every vertical of `providers::USAGE_VERTICALS`.
    app.usage_limits = crate::tui::providers::unread_usage_limits();
    app
}

pub(super) fn serialize_frame(buffer: &Buffer) -> String {
    let mut out = format!("frame {}x{}\n", buffer.area.width, buffer.area.height);
    let mut palette = Vec::<(Color, Color)>::new();
    for cell in buffer.content() {
        let colors = (cell.fg, cell.bg);
        if !palette.contains(&colors) {
            palette.push(colors);
        }
    }
    for (index, colors) in palette.iter().enumerate() {
        let _ = writeln!(out, "p{index} {:?}/{:?}", colors.0, colors.1);
    }
    for y in 0..buffer.area.height {
        let mut symbols = String::new();
        let mut runs = Vec::new();
        let mut run_start = 0u16;
        let mut previous: Option<(Color, Color)> = None;
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            let shown = match cell.symbol() {
                "" => "∅",
                " " => "·",
                symbol => symbol,
            };
            symbols.push_str(shown);
            let colors = (cell.fg, cell.bg);
            if let Some(prior) = previous
                && prior != colors
            {
                let palette_index = palette.iter().position(|entry| *entry == prior).unwrap();
                runs.push(format!("{}-{}:p{palette_index}", run_start, x - 1));
                run_start = x;
            }
            previous = Some(colors);
        }
        if let Some(colors) = previous {
            let palette_index = palette.iter().position(|entry| *entry == colors).unwrap();
            runs.push(format!(
                "{}-{}:p{palette_index}",
                run_start,
                buffer.area.width.saturating_sub(1)
            ));
        }
        let _ = writeln!(out, "{y:02} symbols {symbols}");
        let _ = writeln!(out, "{y:02} colors  {}", runs.join(", "));
    }
    out
}

#[allow(clippy::too_many_lines, reason = "one shared reviewable fixture")]
pub(super) fn visual_fixture(narrow: bool) -> App {
    let mut launch = launch_basic();
    launch.session_id = Some("018f2537-5ed2-7b11-a4b1-f009be85f101".into());
    launch.account = Some("codex".into());
    launch.model = Some("gpt-5.6-sol".into());
    launch.effort = Some("medium".into());
    let mut app = App::new(launch, true);
    app.pane.blocks = vec![
        ScrollBlock::user("Make the release evidence concise and exact.".into()),
        ScrollBlock::Assistant(
            "I’m checking the focused behavior and preserving the source branch.".into(),
        ),
        ScrollBlock::Tool {
            call_id: Some("run-1".into()),
            name: "exec_command".into(),
            state: "running".into(),
            subject: Some("cargo test -p plak-tui".into()),
            detail: Some("running focused terminal tests".into()),
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Tool,
            manual: false,
            group_open: false,
            wait: None,
            started_at_ms: None,
            finished_at_ms: None,
            view: crate::tui::blocks::ItemView::default(),
            mode: DisplayMode::Collapsed,
        },
        ScrollBlock::Tool {
            call_id: Some("read-1".into()),
            name: "read_file".into(),
            state: "done".into(),
            subject: Some("docs/prd/plak-product-spec.md".into()),
            detail: None,
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Tool,
            manual: false,
            group_open: false,
            wait: None,
            started_at_ms: None,
            finished_at_ms: None,
            view: crate::tui::blocks::ItemView::default(),
            mode: DisplayMode::Collapsed,
        },
        ScrollBlock::Tool {
            call_id: Some("patch-1".into()),
            name: "apply_patch".into(),
            state: "rejected: occupancy fence".into(),
            subject: Some("src/main.rs".into()),
            detail: Some("effect denied by the construction-time tier fence".into()),
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Tool,
            manual: false,
            group_open: false,
            wait: None,
            started_at_ms: None,
            finished_at_ms: None,
            view: crate::tui::blocks::ItemView::default(),
            mode: DisplayMode::Expanded,
        },
    ];
    app.mark_transcript_replaced();
    if narrow {
        if let ScrollBlock::Assistant(text) = &mut app.pane.blocks[1] {
            for line in 1..=14 {
                let _ = write!(
                    text,
                    "\nEvidence line {line:02} remains readable after resize."
                );
            }
            app.mark_transcript_replaced();
        }
        app.phase = "idle".into();
        app.pane.scroll_from_bottom = 2;
    } else {
        app.busy = true;
        app.phase = "running".into();
        app.replace_queue_snapshot(
            vec![queue_item("q-1", 3, "Run the narrow terminal review")],
            QueueDrainState::RestoredPaused,
        );
        app.prompt_history = vec![
            "Check the exact binding".into(),
            "Run the focused TUI tests".into(),
        ];
        app.history_browse = Some(1);
    }
    app
}

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

pub(super) fn key_alt(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::ALT,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

pub(super) fn key_ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

pub(super) fn key_ctrl_shift(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    }
}

pub(super) fn tool_line(
    call_id: &str,
    parent: Option<&str>,
    cell: Option<&str>,
    kind: ActivityKind,
    name: &str,
    state: &str,
    subject: Option<&str>,
) -> HostEvent {
    HostEvent::ToolLine {
        call_id: Some(call_id.to_owned()),
        parent_call_id: parent.map(str::to_owned),
        cell_id: cell.map(str::to_owned),
        kind,
        name: name.to_owned(),
        state: state.to_owned(),
        subject: subject.map(str::to_owned),
        detail: None,
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    }
}

pub(super) fn status_row(app: &mut App, width: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, 14)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let composer_top = app.composer_inner.unwrap().y.saturating_sub(1);
    let row = composer_top;
    (0..width)
        .map(|x| buffer[(x, row)].symbol())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

pub(super) fn live_compose(call_id: &str, subject: &str) -> ScrollBlock {
    ScrollBlock::Tool {
        call_id: Some(call_id.into()),
        parent_call_id: None,
        cell_id: Some("1".into()),
        kind: ActivityKind::Compose,
        name: "exec".into(),
        state: "done:392ms".into(),
        subject: Some(subject.into()),
        detail: Some("const r = await tools.exec_command({cmd:\"ls -1\"});\ntext(r)".into()),
        mode: DisplayMode::Collapsed,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: crate::tui::blocks::ItemView::default(),
    }
}

pub(super) fn live_compose_child(call_id: &str, parent: &str) -> ScrollBlock {
    ScrollBlock::Tool {
        call_id: Some(call_id.into()),
        parent_call_id: Some(parent.into()),
        cell_id: Some("1".into()),
        kind: ActivityKind::Process,
        name: "process_session_start".into(),
        state: "done:88ms".into(),
        subject: Some("ls -1; cargo --version".into()),
        detail: Some("$ ls -1; cargo --version\nREADME.md\na.txt".into()),
        mode: DisplayMode::Collapsed,
        manual: false,
        group_open: false,
        wait: None,
        started_at_ms: None,
        finished_at_ms: None,
        view: crate::tui::blocks::ItemView::default(),
    }
}

pub(super) fn left_click(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

pub(super) fn left_mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

pub(super) fn queue_item(id: &str, revision: u64, text: &str) -> QueueItem {
    QueueItem {
        rejected: false,
        id: id.to_owned(),
        revision,
        text: text.to_owned(),
        images: Vec::new(),
    }
}

pub(super) fn blocks_contain(app: &App, needle: &str) -> bool {
    app.pane
        .blocks
        .iter()
        .any(|b| b.as_plain_line().contains(needle) || b.body().contains(needle))
}

pub(super) fn render_stream_slope(
    history_len: usize,
    fragments: usize,
) -> (String, Vec<String>, usize) {
    let mut app = App::new(launch_basic(), true);
    let history = (0..history_len)
        .map(|index| {
            if index % 2 == 0 {
                TranscriptRow::user(format!("history row {index}"))
            } else {
                TranscriptRow::assistant(format!("history row {index}"))
            }
        })
        .collect();
    app.apply_host_event(restore_snapshot(1, history));
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let revision = app.pane.transcript_revision;

    let complete = "streamed output ".repeat(256);
    let chunk = complete.len() / fragments;
    let mut start = 0;
    for index in 0..fragments {
        let end = if index + 1 == fragments {
            complete.len()
        } else {
            start + chunk
        };
        app.apply_host_event(HostEvent::AssistantDelta(complete[start..end].to_owned()));
        start = end;
    }
    app.apply_host_event(HostEvent::AssistantDone);
    assert_eq!(
        app.pane.transcript_revision, revision,
        "fragments coalesce until draw"
    );
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(app.pane.transcript_revision, revision.wrapping_add(1));

    let revision = app.pane.transcript_revision;
    let plain = app
        .pane
        .transcript_cache
        .get(
            revision,
            &app.pane.blocks,
            &app.theme,
            app.last_transcript_width,
            TranscriptChange::Unchanged,
            app.pane_prefs(),
        )
        .layout
        .plain
        .clone();
    // The first Assistant block is a structural boundary; subsequent deltas
    // still coalesce into one tail update per frame.
    assert_eq!(app.pane.transcript_cache.rebuilds(), 2);
    let (_, tail_rows) = app.pane.transcript_cache.work_rows();
    let screen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    (screen, plain, tail_rows)
}

pub(super) const DAY: u64 = 24 * 60 * 60;

/// One cwd-scoped snapshot, newest first, covering every age branch the
/// column renders and every summary fallback a row can take.
pub(super) fn resume_fixture_entries() -> Vec<ResumeSessionEntry> {
    vec![
        ResumeSessionEntry {
            id: "018f2537-5ed2-7b11-a4b1-f009be85f101".into(),
            account: Some("anthropic".into()),
            model: Some("claude-opus-5".into()),
            first_prompt: Some("Redesign the resume picker to match the sessions list".into()),
            created_at_unix: FIXED_NOW - 3 * DAY,
            updated_at_unix: FIXED_NOW - 2 * 60,
            in_place: true,
            ..ResumeSessionEntry::default()
        },
        ResumeSessionEntry {
            id: "7c1d0e94-3a55-4c0b-9d21-0b5f8e6a2c77".into(),
            account: Some("xai".into()),
            model: Some("grok-4.6".into()),
            effort: Some("high".into()),
            first_prompt: Some(
                "Résumé the ünicode 🚀 clipping path 🇯🇵 で全角の文字も含めて確認する very long opening prompt"
                    .into(),
            ),
            created_at_unix: FIXED_NOW - 4 * DAY,
            updated_at_unix: FIXED_NOW - 3 * 60 * 60,
            ..ResumeSessionEntry::default()
        },
        ResumeSessionEntry {
            id: "b2a4f6c1-77de-4a90-8c31-1e2d3f405162".into(),
            model: Some("gpt-5".into()),
            first_prompt: Some("Investigate the flaky nextest shard".into()),
            created_at_unix: FIXED_NOW - 9 * DAY,
            updated_at_unix: FIXED_NOW - DAY,
            ..ResumeSessionEntry::default()
        },
        ResumeSessionEntry {
            id: "4d5e6f70-8192-4a3b-b5c6-d7e8f9a0b1c2".into(),
            created_at_unix: FIXED_NOW - 4 * DAY,
            updated_at_unix: FIXED_NOW - 4 * DAY,
            ..ResumeSessionEntry::default()
        },
        ResumeSessionEntry {
            id: "9a8b7c6d-5e4f-4302-9182-736455647382".into(),
            account: Some("openrouter".into()),
            model: Some("qwen3-max".into()),
            extra: Some("Fast".into()),
            title: Some("Nightly triage".into()),
            created_at_unix: FIXED_NOW - 40 * DAY,
            updated_at_unix: FIXED_NOW - 30 * DAY,
            ..ResumeSessionEntry::default()
        },
    ]
}

pub(super) fn resume_fixture() -> App {
    let mut app = App::new(launch_basic(), true);
    app.resume_menu = Some(ResumeMenu::open(
        resume_fixture_entries(),
        FIXED_NOW,
        jiff::tz::TimeZone::UTC,
    ));
    app
}

/// The fixtures build what the *host* publishes — the protocol's typed view —
/// so the canvas adapter is exercised by every TUI test that opens a tree.
pub(super) fn tree_node(id: &str, parent: Option<&str>, depth: u8) -> NodeView {
    NodeView {
        agent_id: id.to_owned(),
        parent_agent_id: parent.map(str::to_owned),
        depth,
        status: NodeStatus::Running,
        attempt_status: None,
        attempt_seq: None,
        attempt_id: None,
        attempts: Vec::new(),
        tier: None,
        description: "description".into(),
        prompt: "prompt".into(),
        workspace_path: "/tmp".into(),
        workspace_mode: WorkspaceMode::Inherit,
        workspace_status: None,
        candidate_id: None,
        blocked: None,
        usage: None,
        active_turn: false,
        live_progress: None,
        unread_mail: 0,
        has_session: true,
        template: Some("implement".into()),
        model_account: "grok".into(),
        model_tuple: "grok-4.6".into(),
        model_effort: Some("low".into()),
        turns: Some(1),
        started_at_unix: None,
        ended_at_unix: None,
        fault: None,
        waiting_on: None,
    }
}

pub(super) fn tree_snapshot(nodes: Vec<NodeView>) -> TreeView {
    TreeView {
        tree_id: "tree1".into(),
        revision: 3,
        state: TreeAdmissionState::Live,
        solicited: false,
        nodes,
    }
}

pub(super) fn open_tree(app: &mut App, snapshot: TreeView) {
    app.activity_tab = Some(super::processes::ActivityTab::Agents);
    app.tree_navigator = Some(TreeNavigator::awaiting());
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(snapshot));
}

/// The Agents tab painting the built-in profile list.
pub(super) fn models_app() -> App {
    let mut app = App::new(launch_basic(), true);
    let _ = app.activate_control_tab(super::ControlTab::Models);
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::Profiles(
        builtin_profiles_view(),
    )));
    app
}

/// A transcript app with animations off and its clock pinned, so every
/// golden is deterministic.
pub(super) fn transcript_fixture() -> App {
    let mut app = App::new(launch_basic(), true);
    pin_clock(&mut app);
    app
}

/// The unix milliseconds a pinned clock stands for.
pub(super) const PINNED_UNIX_MS: u64 = 1_800_000_000_000;

/// Freeze the clock every painted elapsed reads: a tip, timer or measured
/// duration then depends only on the stamps a test sets, never on how long
/// the test took to reach its draw.
pub(super) fn pin_clock(app: &mut App) {
    app.clock = Some(super::PinnedClock {
        at: std::time::Instant::now(),
        unix_ms: PINNED_UNIX_MS,
    });
}

/// Moves the clock `ms` on (pinning it first when a test has not), so the
/// composer's quiet period can pass.
pub(super) fn pass_time(app: &mut App, ms: u64) {
    let (at, unix_ms) = app.clock.map_or_else(
        || (std::time::Instant::now(), PINNED_UNIX_MS),
        |clock| (clock.at, clock.unix_ms),
    );
    app.clock = Some(super::PinnedClock {
        at: at + std::time::Duration::from_millis(ms),
        unix_ms: unix_ms + ms,
    });
}

/// Confirms the open pop-up: focus starts on the primary action, so `Enter`
/// presses it at once.
pub(super) fn confirm_action(app: &mut App) -> Option<crate::tui::UserAction> {
    app.handle_key(key(KeyCode::Enter))
}

pub(super) fn say(app: &mut App, text: &str) {
    app.apply_host_event(HostEvent::AssistantDelta(text.to_owned()));
    app.apply_host_event(HostEvent::AssistantDone);
}

/// One Compose cell: the marker row, then its children under it.
pub(super) fn compose_cell(
    app: &mut App,
    cell: &str,
    name: &str,
    state: &str,
    subject: &str,
    program: &str,
    children: &[(&str, &str, &str, &str)],
) {
    app.apply_host_event(HostEvent::ToolLine {
        call_id: Some(cell.into()),
        parent_call_id: None,
        cell_id: Some(cell.into()),
        kind: ActivityKind::Compose,
        name: name.into(),
        state: state.into(),
        subject: Some(subject.into()),
        detail: Some(program.into()),
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    });
    for (index, (name, state, subject, detail)) in children.iter().enumerate() {
        let mut line = tool_line(
            &format!("{cell}-child-{index}"),
            Some(cell),
            Some(cell),
            ActivityKind::Tool,
            name,
            state,
            Some(subject),
        );
        if let HostEvent::ToolLine { detail: slot, .. } = &mut line
            && !detail.is_empty()
        {
            *slot = Some((*detail).to_owned());
        }
        app.apply_host_event(line);
    }
}

pub(super) const JS_PROGRAM: &str = "const out = await exec_command({ cmd: \"cargo test\" });\nconst src = await read_file({ path: \"crates/plak/host/src/runtime.rs\" });\nawait apply_patch({ path: \"crates/plak/tui/src/ui.rs\", patch });";

pub(super) const PY_PROGRAM: &str = "out = await exec_command(cmd=\"cargo build\")\nif out.exit_code == 0:\n    await read_file(path=\"crates/plak/host/src/agent.rs\")";

/// One completed cell with three children, one of them failed, exactly as
/// §5.8's resting mock-up draws it.
pub(super) fn compose_transcript_app() -> App {
    let mut app = transcript_fixture();
    compose_cell(
        &mut app,
        "cell-1",
        "exec_js",
        "done:4.1s",
        "3 tools · 1 failed",
        JS_PROGRAM,
        &[
            (
                "exec_command",
                "done:2.4s",
                "cargo test",
                "running 412 tests\ntest result: ok. 412 passed; 0 failed",
            ),
            (
                "read_file",
                "done:0.1s",
                "crates/plak/host/src/runtime.rs (1-120 of 11k)",
                "",
            ),
            (
                "apply_patch",
                "failed",
                "crates/plak/tui/src/ui.rs +12/-3",
                "",
            ),
        ],
    );
    app
}

/// The spec 02 §5.4 transcript scenarios, built one per
/// golden: Compose in each of its forms, every §5.2 header format, a
/// thought block, and the spacing rule.
#[allow(clippy::too_many_lines, reason = "one arm per golden scenario")]
pub(super) fn transcript_scenario(scenario: &str) -> App {
    match scenario {
        "compose-collapsed" => compose_transcript_app(),
        "compose-expanded-javascript" => {
            let mut app = compose_transcript_app();
            app.toggle_tool_expand_at(0);
            app
        }
        "compose-expanded-python" => {
            let mut app = transcript_fixture();
            compose_cell(
                &mut app,
                "cell-py",
                "exec_py",
                "done:3.3s",
                "2 tools",
                PY_PROGRAM,
                &[
                    ("exec_command", "done:3.1s", "cargo build", ""),
                    (
                        "read_file",
                        "done:0.1s",
                        "crates/plak/host/src/agent.rs (1-80 of 2.4k)",
                        "",
                    ),
                ],
            );
            app.toggle_tool_expand_at(0);
            app
        }
        "compose-inflight" => {
            let mut app = transcript_fixture();
            compose_cell(
                &mut app,
                "cell-live",
                "exec_js",
                "running",
                "2 tools",
                JS_PROGRAM,
                &[
                    ("exec_command", "done:3.1s", "cargo build", ""),
                    ("read_file", "running", "crates/plak/host/src/agent.rs", ""),
                ],
            );
            app
        }
        "compose-cancelled" => {
            let mut app = transcript_fixture();
            compose_cell(
                &mut app,
                "cell-x",
                "exec_js",
                "cancelled after 2 of 5",
                "5 tools",
                JS_PROGRAM,
                &[
                    ("exec_command", "done:3.1s", "cargo build", ""),
                    (
                        "read_file",
                        "done:0.1s",
                        "crates/plak/host/src/agent.rs (1-80 of 2.4k)",
                        "",
                    ),
                    ("apply_patch", "cancelled", "crates/plak/tui/src/ui.rs", ""),
                ],
            );
            app
        }
        "tool-rows" => {
            let mut app = transcript_fixture();
            for (call, name, state, subject) in [
                ("t-run", "exec_command", "done:2.4s", "cargo test --lib"),
                (
                    "t-read",
                    "read_file",
                    "done:0.1s",
                    "crates/plak/host/src/runtime.rs (1-120 of 11k)",
                ),
                (
                    "t-search",
                    "grep",
                    "done:0.3s",
                    "\"TODO\" in src (12 matches in 4 files)",
                ),
                (
                    "t-edit",
                    "apply_patch",
                    "done:0.2s",
                    "crates/plak/tui/src/ui.rs +12/-3",
                ),
                (
                    "t-fetch",
                    "web_fetch",
                    "done:1.8s",
                    "https://docs.rs/ratatui/latest/ratatui/",
                ),
                (
                    "t-list",
                    "list_dir",
                    "done:<1ms",
                    "crates/plak (14 entries)",
                ),
                (
                    "t-ask",
                    "ask_user_question",
                    "done:41s",
                    "What is your favorite language?",
                ),
            ] {
                app.apply_host_event(tool_line(
                    call,
                    None,
                    None,
                    ActivityKind::Tool,
                    name,
                    state,
                    Some(subject),
                ));
            }
            app
        }
        "thought" => {
            let mut app = transcript_fixture();
            app.apply_host_event(HostEvent::UserMessage {
                text: "Why does the gate fail?".into(),
                sent_at_ms: None,
            });
            app.apply_host_event(HostEvent::ReasoningDelta {
                location_key: "0:0".into(),
                text: "The clippy lint is new in 1.97.\nThe fix is a `let ... else`.".into(),
            });
            app.apply_host_event(HostEvent::ReasoningSealed {
                location_key: "0:0".into(),
            });
            for block in &mut app.pane.blocks {
                if let ScrollBlock::Thinking { elapsed_ms, .. } = block {
                    *elapsed_ms = Some(12_300);
                }
            }
            say(
                &mut app,
                "The gate fails on a new clippy lint; the fix is small.",
            );
            app.apply_host_event(HostEvent::ReasoningDelta {
                location_key: "1:0".into(),
                text: "Checking the workspace lints\nreading clippy.toml\nweighing the remaining options\nagainst latency and cost\nthen deciding".into(),
            });
            app
        }
        "spacing" => {
            let mut app = transcript_fixture();
            say(&mut app, "Checking the build.");
            app.apply_host_event(tool_line(
                "s-run",
                None,
                None,
                ActivityKind::Process,
                "exec_command",
                "done:2.4s",
                Some("cargo build"),
            ));
            compose_cell(
                &mut app,
                "cell-a",
                "exec_js",
                "done:1.1s",
                "1 tools",
                "await read_file({ path: \"a.rs\" });",
                &[("read_file", "done:0.1s", "a.rs (1-40)", "")],
            );
            compose_cell(
                &mut app,
                "cell-b",
                "exec_js",
                "done:1.2s",
                "1 tools",
                "await read_file({ path: \"b.rs\" });",
                &[("read_file", "done:0.1s", "b.rs (1-40)", "")],
            );
            app.apply_host_event(HostEvent::ReasoningDelta {
                location_key: "2:0".into(),
                text: "Short thought.".into(),
            });
            app.apply_host_event(HostEvent::ReasoningSealed {
                location_key: "2:0".into(),
            });
            for block in &mut app.pane.blocks {
                if let ScrollBlock::Thinking { elapsed_ms, .. } = block {
                    *elapsed_ms = Some(800);
                }
            }
            app.apply_host_event(tool_line(
                "s-read",
                None,
                None,
                ActivityKind::Tool,
                "read_file",
                "done:0.1s",
                Some("crates/plak/tui/src/blocks.rs (1-60 of 4.3k)"),
            ));
            app
        }
        other => panic!("unknown transcript scenario {other}"),
    }
}

pub(super) fn settings_fixture() -> App {
    let mut app = App::new(launch_basic(), true);
    app.pending_open_settings = true;
    app.apply_host_event(HostEvent::Extension(SurfaceObservation::SettingsSnapshot(
        SettingsSnapshot::default(),
    )));
    // Land on an Int row so the panel shows both the current value and the
    // schema default beside the description.
    for _ in 0..2 {
        app.handle_key(key(KeyCode::Down));
    }
    app
}

pub(super) fn typed(app: &mut App, text: &str) {
    for ch in text.chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
}

pub(super) fn screen_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>()
}

/// Opens the Menu on Providers over the four verticals in mixed
/// states.
pub(super) fn providers_app() -> App {
    let mut app = usage_fixture();
    app.render_prefs.reduced_motion = true;
    app.providers = provider_rows();
    app.activate_control_tab(super::ControlTab::Providers);
    app
}

/// The painted Providers frame, serialized for absence checks.
pub(super) fn providers_text(app: &mut App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    serialize_frame(terminal.backend().buffer())
}

/// A restore: the host's presentation snapshot carrying `rows`.
pub(super) fn restore_snapshot(revision: u64, rows: Vec<TranscriptRow>) -> HostEvent {
    HostEvent::PresentationSnapshot(crate::tui::presentation::PresentationSnapshot {
        schema: crate::tui::presentation::PRESENTATION_SCHEMA_V2,
        revision,
        rows,
    })
}

/// A finished tool item; tests override fields with struct update syntax.
/// Its kind is the one the host's classification gives the names tests use.
pub(super) fn tool_item(call_id: &str, name: &str) -> ToolItem {
    let kind = match name {
        "exec" | "exec_js" | "exec_py" => ActivityKind::Compose,
        "exec_command" | "run_process" => ActivityKind::Process,
        "compact" => ActivityKind::Compact,
        "spawn_agent" | "wait_agent" | "send_agent_message" | "child_result" => ActivityKind::Agent,
        _ => ActivityKind::Tool,
    };
    ToolItem {
        call_id: call_id.to_owned(),
        name: name.to_owned(),
        exposed_name: None,
        kind,
        state: "done".to_owned(),
        subject: None,
        detail: None,
        started_at_ms: None,
        finished_at_ms: None,
        parent_call_id: None,
        cell_id: None,
        wait: None,
    }
}
