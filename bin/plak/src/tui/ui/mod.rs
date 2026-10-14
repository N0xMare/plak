//! Interactive A3 hybrid shell: chrome, structured scrollback, slash palette, queue chips.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::stdout;
use std::path::Path;
use std::sync::Once;
use std::time::{Duration, Instant};

use crossterm::cursor::Show as ShowCursor;
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use unicode_width::UnicodeWidthStr;

/// Wall clock in unix seconds, for the durations the product renders against
/// host timestamps. A clock behind the epoch reads as the epoch rather than
/// panicking a draw.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// A frozen instant and the unix milliseconds it stands for.
#[derive(Clone, Copy)]
struct PinnedClock {
    at: Instant,
    unix_ms: u64,
}

impl App {
    /// The instant painted elapsed times are measured from and against.
    fn now(&self) -> Instant {
        self.clock.map_or_else(Instant::now, |clock| clock.at)
    }

    /// Wall clock in unix milliseconds, against host start stamps.
    fn unix_now_ms(&self) -> u64 {
        self.clock.map_or_else(
            || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| {
                        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
                    })
            },
            |clock| clock.unix_ms,
        )
    }
}

fn resume_controls_suffix(values: [Option<String>; 2]) -> String {
    values
        .into_iter()
        .flatten()
        .fold(String::new(), |mut suffix, value| {
            let _ = write!(suffix, " · {value}");
            suffix
        })
}

use crate::tui::blocks::{
    self, DisplayMode, MAX_LIVE_TRANSCRIPT_BLOCKS, MAX_LIVE_TRANSCRIPT_BYTES, PROMPT_PREFIX,
    ScrollBlock, TRANSCRIPT_TRUNCATION_MARKER, TranscriptChange, format_tool_elapsed,
    layout_anchor, sticky_user_lines, terminal_state_keeping_duration, tool_is_running,
};
use crate::tui::composer_selection::{
    ClickSelection, ComposerClick, ComposerSelection, ComposerUndo, EditKind, PointerButton,
};
use crate::tui::composer_view::{
    DraftWrapCache, cursor_from_visual_click, cursor_from_visual_rows, desired_block_height,
    format_context_meter, logical_line_end, logical_line_start, next_grapheme_boundary,
    next_word_boundary, previous_grapheme_boundary, previous_word_boundary, visual_cursor_xy,
    visual_cursor_xy_from_rows, visual_line_count,
};
use crate::tui::control::{is_drop_command, is_quit_command, local_echo_line, route_submit};
use crate::tui::keymap;
use crate::tui::launch_intro::LaunchIntro;
use crate::tui::modal_chrome::{self, Hint, ModalChrome, ModalSizing};
use crate::tui::picker;
use crate::tui::presentation::{ContextUsage, SubscriptionUsageView};
use crate::tui::quit_guard::{
    CtrlCOutcome, ESC_CLEAR_WINDOW, EscOutcome, QuitArm, classify_ctrl_c, classify_esc,
};
use crate::tui::redraw::{self, FrameWork, RedrawDemand, RedrawSchedule};
use crate::tui::selection::{ContentPosition, TranscriptSelection};
use crate::tui::settings::SettingsPane;
use crate::tui::settings_registry::{self, SettingKind};
use crate::tui::slash::{self, SlashContext, SlashEffect, fuzzy_list, parse_slash};
use crate::tui::text_input::{
    is_accepted_input_char, is_accepted_single_line_char, sanitize_composer_text,
    sanitize_single_line,
};
use crate::tui::theme::{Theme, ThemeId};
use crate::tui::tree_navigator::RESTORED_PAUSED_NAVIGATOR_HINT;
use crate::tui::usage_tab;
use crate::tui::{
    CanvasTree, HostEvent, QueueDrainState, QueueItem, ResumeMenu, ResumePickerOutcome,
    SettingsMenu, SettingsSnapshot, SettingsTheme, TopologyNavigatorKey, TopologyNavigatorOutcome,
    TreeNavigator, TuiError, TuiHandoff, TuiLaunch, TuiOutcome, UserAction,
};

/// How often the TUI rechecks whether `plak` on `PATH` was replaced.
const BUILD_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Terminal events consumed before one frame is composed. Bounded so a paste
/// or key-repeat burst cannot postpone the frame indefinitely; anything left
/// over makes the next wait zero (input first, bounded drain).
const MAX_INPUT_DRAIN_PER_FRAME: usize = 32;
/// Host observations coalesced into one frame. The channel already tail-merges
/// assistant deltas, so this only bounds the worst-case burst.
const MAX_HOST_EVENTS_PER_FRAME: usize = 256;
const WHEEL_FAST_WINDOW: Duration = Duration::from_millis(30);
const WHEEL_QUEUE_CAP: i16 = 30;
/// Lines one wheel notch moves when no settings snapshot has arrived yet. It
/// is the §5.5 schema default for `tui.scroll_lines`, so the very first frame
/// scrolls the same distance the configured value will.
const WHEEL_NOTCH_DEFAULT: u16 = 3;
/// Extra lines a notch inside [`WHEEL_FAST_WINDOW`] moves, on top of the
/// configured notch. It keeps the shipped 3 → 5 flick ratio at the default.
const WHEEL_FAST_BONUS: i16 = 2;
const PROMPT_HISTORY_LIMIT: usize = 500;
/// The context projection before any vertical has reported one.
///
/// After a cross-provider rebind the header kept painting the old
/// provider's reading — `1%/1M` beside a `codex · gpt-5.6-luna · low` binding
/// whose window is 272k — until the next turn corrected it. A window measured
/// against a binding that no longer exists is not a smaller truth than `—`; it
/// is a wrong one.
fn unknown_context_usage() -> ContextUsage {
    ContextUsage {
        breakdown: None,
        prepared_input: None,
        total_window: None,
        admitted_ceiling: None,
        gauge_percent: None,
        display_percent: None,
        ceiling_excess: None,
        evidence: "unavailable".into(),
        non_guarantee: None,
        status: "unavailable".into(),
    }
}
/// The §5.4 row a restored-paused tree owns until the operator resumes it.
///
/// `restored_paused` blocked every spawn and stopped the queue
/// draining, silently. The row states the condition, the exit, and the second
/// consequence the operator would otherwise discover by waiting.
/// Status-row label painted on the last frame before the UI hands the
/// terminal back (§6).
pub(crate) const QUITTING_STATUS: &str = "quitting…";
pub(crate) const TREE_PAUSED_STATUS: &str = "agents paused · /tree resume";
/// Matches Plak's durable prompt admission bound so the renderer cannot retain
/// an input the product could never accept.
const MAX_COMPOSER_BYTES: usize = 1024 * 1024;

/// The seven Menu tabs (spec 02 §5.5). Order is normative and drives both
/// the tab bar and `Left`/`Right` cycling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlTab {
    Settings,
    Models,
    Providers,
    Usage,
    Context,
    Session,
    Keybinds,
}

impl ControlTab {
    const ALL: [Self; 7] = [
        Self::Settings,
        Self::Models,
        Self::Providers,
        Self::Usage,
        Self::Context,
        Self::Session,
        Self::Keybinds,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::Models => "Agents",
            Self::Providers => "Providers",
            Self::Usage => "Usage",
            Self::Context => "Context",
            Self::Session => "Session",
            Self::Keybinds => "Keybinds",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Settings => 0,
            Self::Models => 1,
            Self::Providers => 2,
            Self::Usage => 3,
            Self::Context => 4,
            Self::Session => 5,
            Self::Keybinds => 6,
        }
    }

    fn shifted(self, delta: i8) -> Self {
        let index = self.index();
        let next = if delta < 0 {
            index.checked_sub(1).unwrap_or(Self::ALL.len() - 1)
        } else {
            (index + 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

/// Render preferences for one settings source.
///
/// `reduced_motion` is the one §5.5 row here; it suppresses the spinner, the
/// approval pulse and the Agents list's running glyphs, and never the session
/// banner.
fn render_prefs_for(settings: Option<&SettingsSnapshot>) -> blocks::RenderPrefs {
    settings.map_or_else(blocks::RenderPrefs::default, blocks::RenderPrefs::from)
}

/// A queued prompt loaded into the composer, and the draft it displaced.
/// The displaced draft comes back when the edit ends (saved, cancelled or
/// its item left the queue), as one undoable step.
#[derive(Clone, Debug, PartialEq, Eq)]
struct QueueEdit {
    id: String,
    revision: u64,
    replaced_draft: String,
}

impl QueueEdit {
    #[cfg(test)]
    fn target(&self) -> (&str, u64) {
        (&self.id, self.revision)
    }
}

/// A queued row's painted `[now]` and `[drop]` chips, and when the row
/// first painted them there (spec 02 §5.2): a click fires a chip only once it
/// has held its place for CHIP_SETTLE.
#[derive(Clone, Debug, PartialEq, Eq)]
struct QueueChipHit {
    abs_idx: usize,
    id: String,
    now: Rect,
    drop: Rect,
    since: Instant,
}

/// The open approval's painted `[allow]` or `[deny]` chip, and when that
/// approval's chip first painted there: a new approval's chips start fresh.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ApprovalChipHit {
    approval_id: String,
    rect: Rect,
    choice: crate::tui::ApprovalChoice,
    since: Instant,
}

/// Every chord the Agents list consumes, as `(sheet token, code, key)`.
///
/// This table is the single source the input layer and the §5.2 keymap
/// registry are both checked against: `tree_bindings_match_the_input_layer`
/// asserts every token here appears in a `When::Tree` binding's chord and that
/// every token those chords name appears here, so an Agents list chord cannot exist
/// off the sheet and the sheet cannot invent one.
const TREE_KEYS: &[(&str, KeyCode, TopologyNavigatorKey)] = &[
    ("↑", KeyCode::Up, TopologyNavigatorKey::Up),
    ("k", KeyCode::Char('k'), TopologyNavigatorKey::Up),
    ("↓", KeyCode::Down, TopologyNavigatorKey::Down),
    ("j", KeyCode::Char('j'), TopologyNavigatorKey::Down),
    ("←", KeyCode::Left, TopologyNavigatorKey::Left),
    ("h", KeyCode::Char('h'), TopologyNavigatorKey::Left),
    ("→", KeyCode::Right, TopologyNavigatorKey::Right),
    ("l", KeyCode::Char('l'), TopologyNavigatorKey::Right),
    ("Home", KeyCode::Home, TopologyNavigatorKey::Home),
    ("Enter", KeyCode::Enter, TopologyNavigatorKey::Enter),
    ("f", KeyCode::Char('f'), TopologyNavigatorKey::Filter),
    ("x", KeyCode::Char('x'), TopologyNavigatorKey::Acknowledge),
    ("y", KeyCode::Char('y'), TopologyNavigatorKey::Copy),
    ("R", KeyCode::Char('R'), TopologyNavigatorKey::Refresh),
    ("r", KeyCode::Char('r'), TopologyNavigatorKey::Resume),
    ("i", KeyCode::Char('i'), TopologyNavigatorKey::Interrupt),
    ("c", KeyCode::Char('c'), TopologyNavigatorKey::Close),
    ("s", KeyCode::Char('s'), TopologyNavigatorKey::StopAll),
    ("Esc", KeyCode::Esc, TopologyNavigatorKey::Escape),
];

/// The canvas key one `KeyCode` names, if any.
fn tree_navigator_key(code: KeyCode) -> Option<TopologyNavigatorKey> {
    TREE_KEYS
        .iter()
        .find(|(_, candidate, _)| *candidate == code)
        .map(|(_, _, key)| *key)
}

fn theme_for(value: SettingsTheme) -> Theme {
    Theme::for_id(match value {
        SettingsTheme::DefaultDark => ThemeId::DefaultDark,
        SettingsTheme::DefaultLight => ThemeId::DefaultLight,
        SettingsTheme::CatppuccinLatte => ThemeId::CatppuccinLatte,
        SettingsTheme::CatppuccinFrappe => ThemeId::CatppuccinFrappe,
        SettingsTheme::CatppuccinMacchiato => ThemeId::CatppuccinMacchiato,
        SettingsTheme::CatppuccinMocha => ThemeId::CatppuccinMocha,
    })
}

/// Tail blocks the spec 02 §5.4 HUD inspects for the activity it names.
const HUD_SCAN_BLOCKS: usize = 64;

fn bounded_panel_height(desired: u16, available: u16) -> u16 {
    let height = desired.min(available);
    if height >= 3 { height } else { 0 }
}

fn utf8_prefix(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn truncate_utf8_bytes(text: &mut String, max_bytes: usize) {
    if text.len() > max_bytes {
        let end = utf8_prefix(text, max_bytes).len();
        text.truncate(end);
    }
}

fn char_range_bytes(text: &str, range: std::ops::Range<usize>) -> usize {
    let start = text
        .char_indices()
        .nth(range.start)
        .map_or(text.len(), |(index, _)| index);
    let end = text
        .char_indices()
        .nth(range.end)
        .map_or(text.len(), |(index, _)| index);
    end.saturating_sub(start)
}

fn user_phase(phase: &str) -> &'static str {
    match phase {
        "idle" => "ready",
        "running" => "thinking…",
        "rebind" => "switching model…",
        "preparing" => "starting…",
        "interrupting" => "stopping…",
        "interrupted" => "interrupted",
        "error" => "needs attention",
        "not_ready" => "setup needed",
        _ => "working",
    }
}

/// Duration ladder for the HUD and hint line (spec 02 §5.4): `0.5s` under a
/// second, `5.2s` to ten seconds, then the shared whole-second ladder.
fn format_status_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 10 {
        return blocks::format_duration_secs(secs);
    }
    format!("{}.{}s", secs, elapsed.subsec_millis() / 100)
}

fn user_status_detail(detail: &str) -> String {
    if detail.contains("queue delivery requires an attached canonical agent") {
        return "connect a model to send queued messages".to_owned();
    }
    if detail == "turn started" {
        return String::new();
    }
    detail
        .replace("canonical ", "")
        .replace("Plakous ", "session ")
        .replace("recovery-required", "needs recovery")
        .replace("locator", "saved position")
}

/// One provider usage observation as the host reported it: input, output,
/// reasoning, cached-input and cache-write counters, then the coverage pair,
/// which is `None` when the host does not know how many model operations the
/// report covers. Coverage is never estimated to fill it in.
type LastTurnUsage = (u64, u64, u64, u64, u64, Option<u64>, Option<u64>);

/// Shell application state.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent session/UI flags, not one state machine"
)]
pub(crate) struct App {
    pub(crate) session_id: Option<String>,
    #[allow(
        dead_code,
        reason = "kept for status/debug; chrome uses workspace_chrome"
    )]
    pub(crate) workspace: String,
    /// Bottom-right label `~/path · branch`.
    pub(crate) workspace_chrome: String,
    #[allow(
        dead_code,
        reason = "resume note already injected into blocks at launch"
    )]
    pub(crate) account: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) composition: crate::tui::CompositionSelection,
    pub(crate) protocol: crate::tui::ProtocolSelection,
    pub(crate) effective_presentation: String,
    /// Optional model extra (e.g. `pro`).
    pub(crate) model_extra: Option<String>,
    pub(crate) agent_enabled: bool,
    /// Prompt submitted before attach completes; flushed on `AgentAttached(true)`.
    pending_prompt: Option<String>,
    /// True when the app host attached action/event channels.
    pub(crate) hosted: bool,
    /// The transcript on screen: the root's, or an open agent's.
    ///
    /// While an agent view is open the root pane waits in
    /// [`AgentView::hidden`]; root host events are applied through
    /// [`App::with_root_pane`], so they always land in the root pane.
    pub(crate) pane: pane::TranscriptPane,
    /// The agent transcript shown as the Activity > Agents body.
    agent_view: Option<pane::AgentView>,
    /// The agent transcript the host was last asked to serve.
    host_agent_transcript: Option<(String, u64)>,
    /// Serial of the last agent pane installed; see [`pane::AgentView`].
    agent_pane_serial: u64,
    /// The last agent tree snapshot, kept while Activity is closed for the
    /// agent view's bar and `[`/`]` order.
    agent_tree: Option<CanvasTree>,
    /// The agent view's bar; a click opens Activity > Agents there.
    agent_bar_hit: Option<Rect>,
    /// The announced replay of the model request, until the replay shows
    /// progress or the turn ends.
    pending_retry: Option<(plakous_host_protocol::ModelRetryNotice, Instant)>,
    pub(crate) input: String,
    /// Cursor as a **char** index into `input` (end-of-string when `== len`).
    input_cursor: usize,
    composer_selection: ComposerSelection,
    /// Bounded, non-durable composer undo/redo history (spec 02 §5.2).
    composer_undo: ComposerUndo,
    composer_dragging: bool,
    composer_drag_pointer: Option<(u16, u16)>,
    composer_down_cell: Option<(u16, u16)>,
    composer_click_epoch: Instant,
    composer_visual_scroll: usize,
    /// Explicit wheel viewport; keyboard/editing restores caret-follow mode.
    composer_wheel_scroll: Option<usize>,
    pub(crate) should_quit: bool,
    /// The operator asked to quit: the last frame names the state (§6) while
    /// the launcher hands the terminal back and exits the process.
    pub(crate) quitting: bool,
    pub(crate) busy: bool,
    pub(crate) phase: String,
    pub(crate) queue_len: usize,
    pub(crate) status_detail: Option<String>,
    /// A clear request crossed the UI mailbox but has not reached the host's
    /// authoritative [`HostEvent::ClearView`] publication boundary.
    clear_pending: bool,
    /// Last complete queue snapshot supplied by the authoritative host.
    pub(crate) local_queue: Vec<QueueItem>,
    /// Queue delivery authority supplied alongside the snapshot.
    queue_drain: QueueDrainState,
    /// Queue panel focused (Tab); keyboard select/steer/drop.
    queue_focused: bool,
    /// Selected index in `local_queue` (0 = oldest / FIFO head).
    queue_selected: usize,
    /// Exact queue item currently being edited in the composer.
    queue_editing: Option<QueueEdit>,
    /// Text actions accepted by the process-local mailbox but not yet durably acknowledged.
    pending_drafts: VecDeque<UserAction>,
    /// Exact queued edit awaiting a durable host disposition.
    pending_queue_edit: Option<(String, String)>,
    /// Prior submitted prompts (newest last).
    pub(crate) prompt_history: Vec<String>,
    recovered_drafts: Vec<String>,
    /// Index into `prompt_history` when browsing with ↑ (`None` = not browsing).
    history_browse: Option<usize>,
    /// Painted history row hit targets `(history index, rect)`.
    prompt_history_hits: Vec<(usize, Rect)>,
    /// Slash palette open (composer starts with `/`).
    pub(crate) slash_open: bool,
    slash_popup: Option<Rect>,
    slash_view_start: usize,
    pub(crate) slash_selected: usize,
    theme: Theme,
    persisted_theme: SettingsTheme,
    /// Dirty-redraw flag; host events and input set this.
    dirty: bool,
    /// When quitting, ask the app to re-resolve OAuth and re-attach the agent.
    request_reattach: bool,
    host_disconnected: bool,
    terminal_handoff: Option<TuiHandoff>,
    /// `plak` on `PATH` is no longer the file this process runs; the composer
    /// offers `/reload`. Rechecked every [`BUILD_CHECK_INTERVAL`].
    newer_build_installed: bool,
    next_build_check: Instant,
    /// `/reload` was accepted; the loop starts the installed-build probe.
    pending_reload: bool,
    /// The running installed-build probe; the loop hands off when it answers.
    reload_probe: Option<std::sync::mpsc::Receiver<Option<crate::carry::ReloadTarget>>>,
    background_processes: usize,
    processes: std::sync::Arc<Vec<plakous_host_protocol::views::ProcessView>>,
    activity_tab: Option<processes::ActivityTab>,
    activity_last_tab: processes::ActivityTab,
    process_selected: Option<String>,
    process_output: bool,
    process_scroll: usize,
    /// The open destructive confirmation pop-up, over whatever view.
    destructive_confirm: Option<destructive_confirm::DestructiveConfirm>,
    /// How long since the operator last typed in the composer: empty `Enter`
    /// send-now and `?` on an empty draft read it (spec 02 §5.2).
    typing: typing_guard::TypingGuard,
    /// What [`Self::typing`] knew before the key being handled now.
    key_typing: typing_guard::Typing,
    /// Key labels painted on this frame's footers that a click presses, so
    /// the destructive controls are reachable by mouse alone.
    key_label_hits: Vec<(Rect, KeyCode)>,
    status_processes_hit: Option<Rect>,
    process_hits: Vec<(String, Rect)>,
    activity_agents_hit: Option<Rect>,
    activity_close_hit: Option<Rect>,
    activity_close_hover: bool,
    activity_processes_hit: Option<Rect>,
    /// The Agents tab's profile list and its dialog.
    profiles: agents_tab::ProfilesPane,
    /// Host-supplied cwd-scoped exact-session picker (`/resume`).
    resume_menu: Option<ResumeMenu>,
    /// An irreversible palette command `Enter` completed, and the draft it
    /// left: the next `Enter` within `plain_key::SLASH_DOUBLED_ENTER` runs
    /// nothing.
    slash_enter_completion: Option<(Instant, String)>,
    resume_popup: Option<Rect>,
    resume_close_hit: Option<Rect>,
    /// The resume picker's `[×]` brightens under the pointer, as the control
    /// center's does.
    resume_close_hover: bool,
    resume_hits: Vec<picker::RowHit>,
    /// Row under the pointer. §5.5: hover highlights and never moves focus, so
    /// a click still lands on an unfocused row.
    resume_hover: Option<usize>,
    tree_navigator: Option<TreeNavigator>,
    agents_unavailable: Option<String>,
    /// After attach or in-process resume, request a tree snapshot when closed.
    pending_open_tree: bool,
    tree_popup: Option<Rect>,
    /// Painted Agents rows by agent id.
    tree_hits: Vec<(String, Rect)>,
    /// Acknowledged attention, keyed by tree; the product store's answer.
    agent_acks: tree::AgentAcknowledgements,
    /// Pinned wall clock for the Agents list; `None` reads the real one.
    /// Frame tests pin it so ages and the five-minute window are exact.
    agents_clock: Option<u64>,
    /// Pinned clock for every painted elapsed (tips, measured durations,
    /// HUD timers, restored tips) and the wheel's flick and settle windows;
    /// `None` reads the real clocks. Tests pin it so no frame or gesture
    /// depends on how long the test ran.
    clock: Option<PinnedClock>,
    /// Host-supplied typed product settings pane.
    settings_menu: Option<SettingsMenu>,
    /// Last authoritative or launch-time settings snapshot. This remains
    /// inert until the Settings tab opens and never captures composer input.
    settings_snapshot: Option<SettingsSnapshot>,
    settings_error: Option<String>,
    /// Painted rows of the Settings pane for mouse selection: registry rows
    /// while browsing, the choices while a chooser is open.
    settings_hits: Vec<picker::RowHit>,
    /// Row the pointer is over in Settings. Reveals that row's description
    /// only: hover must never move focus (spec 02 §5.5).
    settings_hover: Option<usize>,
    /// Pairs list clicks into double-clicks for every selectable list.
    list_clicks: picker::ListClicks,
    control_tab: Option<ControlTab>,
    last_control_tab: ControlTab,
    keybinds_full: bool,
    transcript_scrollbar: Option<scrollbar::Scrollbar>,
    /// Row offset into the thumb while the transcript scrollbar is dragged.
    scrollbar_drag: Option<usize>,
    control_tab_hits: Vec<(ControlTab, Rect)>,
    control_popup: Option<Rect>,
    control_close_hit: Option<Rect>,
    /// Whether the pointer is over the close glyph. Hover brightens `[×]` and
    /// does nothing else: the glyph is the one clickable thing on the border.
    control_close_hover: bool,
    /// Plain text of the body each of the three read-only tabs last painted,
    /// so `y` copies exactly what is on screen.
    tab_copy_text: Vec<(ControlTab, String)>,
    /// Host-owned typed session facts for the Session tab.
    session_info: crate::tui::presentation::SessionInfoView,
    /// Rows the Session tab last painted, kept so `y` and a click copy
    /// exactly what is on screen.
    session_rows: Vec<usage_tab::SessionRow>,
    /// Painted Session tab value rows: `(row index, screen rect)`.
    session_row_hits: Vec<(usize, Rect)>,
    /// Last observed tree epoch admission state, from the navigator snapshot.
    tree_epoch_state: Option<String>,
    /// One scroll cursor per tab, indexed by [`ControlTab::index`]. Every tab
    /// scrolls (spec 02 §5.5).
    control_scroll: [modal_chrome::ScrollState; ControlTab::ALL.len()],
    /// Host-projected `[agent.*]` template readiness rows, used only to keep
    /// the Session tab's `Root template` row honest (spec 02 §5.5).
    templates: Vec<crate::tui::TemplateRow>,
    providers: Vec<crate::tui::ProviderDef>,
    /// Focused vertical on the Providers tab (spec 02 §5.5). The tab has no
    /// actions, so this only selects which vertical the description panel
    /// explains.
    providers_focus: usize,
    /// Hit rect per painted Providers vertical, so a click focuses it.
    provider_row_hits: Vec<(usize, Rect)>,
    /// Transcript area for selection (set each draw).
    transcript_area: Option<Rect>,
    /// Real scrollback body (excludes the navigation-only sticky preview).
    transcript_body_area: Option<Rect>,
    /// Jump-to-live spacer hit target when the viewport is unfollowed.
    jump_to_live_hit: Option<Rect>,
    /// Sticky prompt hit target and exact scroll offset back to its source row.
    sticky_prompt_jump: Option<(Rect, usize)>,
    /// Queue panel area for [now]/[drop] hit-testing (set each draw).
    queue_area: Option<Rect>,
    /// Floating composer interior for mouse focus/cursor placement.
    composer_inner: Option<Rect>,
    /// Wrap rows for the current draft; reused across drag/paint while unchanged.
    composer_wrap: DraftWrapCache,
    /// Per-row queue chips painted this frame.
    queue_button_hits: Vec<QueueChipHit>,
    capacity_recovery: Option<crate::tui::ContextCapacityRecovery>,
    capacity_recovery_action: Option<Rect>,
    /// Tool summary row hits for expand toggle: (block index, absolute screen row).
    tool_hit_rows: Vec<(usize, u16)>,
    /// The open approval row's painted `[allow]` and `[deny]` chips.
    approval_chip_hits: Vec<ApprovalChipHit>,
    /// Call detail asked of the host for open items, and its cache.
    call_details: call_detail::CallDetails,
    pending_tool_click: Option<(usize, blocks::RowTarget)>,
    /// What each [`Self::tool_hit_rows`] entry toggles, index for index.
    tool_hit_targets: Vec<blocks::RowTarget>,
    /// Drag-select over transcript.
    selection: TranscriptSelection,
    /// Last pointer position while transcript drag ownership is active.
    transcript_drag_pointer: Option<(u16, u16)>,
    /// The operator's last gesture was a transcript wheel, click or drag.
    ///
    /// The terminal cursor belongs to the composer; while the operator is
    /// scrolling or selecting scrollback it is hidden, so a burst of wheel
    /// frames does not flicker a caret in and out of the composer. A key press
    /// or a composer click hands the cursor back. Decided once per frame in
    /// [`App::draw`], never toggled mid-frame.
    transcript_gesture: bool,
    /// The gesture holding the cursor is a wheel burst rather than a click or a
    /// drag-select. A burst ends by itself, so the cursor comes back when the
    /// queue drains; a selection has to be released first.
    transcript_gesture_is_wheel: bool,
    transcript_down_cell: Option<(u16, u16)>,
    /// After mouse-up, copy once the next paint makes the cache coherent.
    copy_selection_after_draw: bool,
    /// Floating info overlay (`/help`, `/status`) — not dumped into transcript.
    info_overlay: Option<(String, String)>,
    /// Scroll position of the open info overlay. `/help` now carries the whole
    /// generated keymap sheet, which is taller than any terminal, so the
    /// overlay scrolls exactly like the Keybinds tab instead of clipping.
    info_overlay_scroll: modal_chrome::ScrollState,
    /// Double Ctrl+C arm.
    quit_arm: Option<QuitArm>,
    /// Idle Esc-Esc draft-clear arm (800 ms). Esc never quits.
    esc_clear_arm: Option<QuitArm>,
    /// A cancel intent was emitted for the running turn and has not settled.
    /// A stuck cancel or idle empty draft lets the Ctrl+C ladder escalate to quit (§6.1).
    cancel_pending: bool,
    /// One internally coherent host-authoritative context snapshot.
    context_usage: ContextUsage,
    /// Latest authoritative terminal provider-usage observation.
    last_turn_usage: Option<LastTurnUsage>,
    /// The last correlated canonical turn lifecycle observed from the host.
    /// A live read-model projection, never a persistence authority.
    pub(super) last_turn: Option<plakous_host_protocol::TurnLifecycle>,
    /// When the operator last sent the root a prompt or steer in this session.
    /// In memory only: no stored time exists for a restored prompt.
    pub(super) last_prompt_at: Option<Instant>,
    /// The idle prompt clock text last painted, so the clock repaints only
    /// when its text changes.
    idle_clock_painted: Option<String>,
    /// Conservative monotonic revision fencing semantic-pilot observations.
    /// Advances on every mutation entry point; it is not a coverage identity.
    pub(super) semantic_revision: u64,
    /// Latest replaceable subscription allowance presentation.
    subscription_usage: SubscriptionUsageView,
    /// The complete host-owned Usage projection. Replaced wholesale, never
    /// patched locally; the Limits section renders from this.
    usage_limits: crate::tui::presentation::UsageLimitsView,
    /// Session banner at the top of the transcript. `None` in unit tests unless a
    /// test arms it; a real launch arms it once the first surface is out of the
    /// way. There is no setting: the wordmark is drawn whenever the pane fits.
    launch_intro: Option<LaunchIntro>,
    version: &'static str,
    /// The §5.5 rows that steer transcript rendering (§5.3, §5.5, §5.6).
    render_prefs: blocks::RenderPrefs,
    /// Braille frame for the §5.4 composer HUD.
    status_spinner_frame: usize,
    status_spinner_at: Instant,
    /// Start of the current turn, for the bare hint-line timer.
    turn_started: Option<Instant>,
    /// Start of the current activity phase, for the HUD timer.
    phase_started: Option<Instant>,
    /// Start of the latest dispatched model round, independent of tool phases.
    model_request_started: Option<Instant>,
    composer_chrome: composer_chrome::ComposerChrome,
    /// Hit rect for the composer HUD agents count; a click opens the canvas.
    status_agents_hit: Option<Rect>,
    /// Active descendants from the last topology snapshot the host sent.
    tree_live_descendants: usize,
    /// The host's last word on whether the agent tree is `RestoredPaused`.
    ///
    /// While it is, every spawn is refused and the queue stops
    /// auto-draining. The idle HUD pending indicator names it until the host
    /// reports the tree resumed.
    tree_restored_paused: bool,
    /// Latest idle pending-work snapshot. Empty until the host publishes one.
    pending_continuation: plakous_host_protocol::PendingContinuation,
    /// The host's open approval handle, when one is pending. Kept beside the
    /// row so the render loop never walks the transcript to find it (§11).
    open_approval: Option<String>,
    /// Coalesced trackpad/wheel intent; drained once per frame.
    mouse_scroll_queue: i16,
    last_wheel_at: Option<Instant>,
    /// Last transcript content width, used to warm projection after resume.
    last_transcript_width: usize,
    /// Set while the host is blocked opening the canonical session; the
    /// startup row ticks its own elapsed seconds from here (spec 02 §5.4).
    session_open_started: Option<Instant>,
    /// Where this open's notes start in `blocks`: the length at
    /// `SessionOpening`. The host says what a reopen recovered or re-derived
    /// before it publishes the presentation snapshot, and the snapshot
    /// replaces the transcript, so notes from this open are carried across
    /// it rather than lost. Cleared once a snapshot or replacement has run.
    open_notes_from: Option<usize>,
    /// The host's own `session_open_ms`. Kept so the first busy turn can
    /// retire it; it is not painted as chrome.
    session_open_ms: Option<u64>,
    /// True while an operator request for the control centre is outstanding.
    /// Input is drained ahead of host observations, so the reply can land in
    /// the same frame as the Esc that dismissed the surface; without this the
    /// snapshot would reopen a panel the operator already closed.
    pending_open_settings: bool,
    /// True between asking the host to open Usage and its reply. Closing the
    /// Menu clears it, so a late reply cannot reopen a modal the
    /// operator already dismissed.
    pending_open_usage: bool,
    /// True between `/resume` and the host's session list. Cleared on Esc, on
    /// accept, and whenever another modal takes input, so a late reply cannot
    /// reopen a picker the operator dismissed or stack one over a newer
    /// surface while the key chain still routes to that owner.
    pending_open_resume: bool,
    /// The picker on screen was opened by `plak resume` / `plak --resume`,
    /// which parks the launch on a shell host with nothing attached while the
    /// operator chooses. Declining it therefore has to do something: see
    /// [`App::decline_resume_menu`].
    launch_resume_picker: bool,
    /// The operator declined that launch picker, so the reattach this loop is
    /// asking for is the product attaching the workspace default, not an
    /// explicit `/attach`.
    declined_launch_resume_picker: bool,
    /// Status line for the reattach round about to start, when it is something
    /// other than a model switch.
    reattach_note: Option<String>,
    /// The launch's zone, so every wall-clock date and send time the
    /// presentation prints is the operator's own.
    telemetry_paths: Option<crate::product_paths::PlakPaths>,
    refresh_failure_notes: Vec<String>,
    time_zone: &'static jiff::tz::TimeZone,
    /// The launcher's one-line explanation of the round it started. The host
    /// replaces the transcript with the session's own lines when it opens, so
    /// the note is kept here and re-landed after each replacement.
    opening_note: Option<String>,
    opening_note_position: Option<opening_note::OpeningNotePosition>,
    /// True from launch until the host reports a settled state. Startup is a
    /// chain of host observations, so the loop keeps a spinner cadence rather
    /// than parking between them.
    startup_pending: bool,
}

impl App {
    #[allow(
        clippy::too_many_lines,
        reason = "one explicit initialization keeps the closed TUI state reviewable"
    )]
    pub(crate) fn new(launch: TuiLaunch, hosted: bool) -> Self {
        let opening_note = launch.opening_note;
        let blocks: Vec<ScrollBlock> = opening_note
            .clone()
            .map(ScrollBlock::System)
            .into_iter()
            .collect();
        let workspace_chrome = launch.workspace_chrome;
        let initial_phase = if hosted && !launch.agent_enabled {
            "preparing"
        } else {
            "idle"
        };
        Self {
            session_id: launch.session_id,
            workspace: display_workspace(&launch.workspace),
            workspace_chrome,
            account: launch.account,
            model: launch.model,
            effort: launch.effort,
            composition: launch.composition,
            protocol: launch.protocol,
            effective_presentation: launch.effective_presentation,
            model_extra: launch.extra,
            agent_enabled: launch.agent_enabled,
            pending_prompt: None,
            hosted,
            pane: pane::TranscriptPane::new(blocks),
            agent_view: None,
            host_agent_transcript: None,
            agent_pane_serial: 0,
            agent_tree: None,
            agent_bar_hit: None,
            pending_retry: None,
            input: String::new(),
            input_cursor: 0,
            composer_selection: ComposerSelection::default(),
            composer_undo: ComposerUndo::default(),
            composer_dragging: false,
            composer_drag_pointer: None,
            composer_down_cell: None,
            composer_click_epoch: Instant::now(),
            composer_visual_scroll: 0,
            composer_wheel_scroll: None,
            should_quit: false,
            quitting: false,
            busy: false,
            phase: initial_phase.to_owned(),
            queue_len: 0,
            status_detail: None,
            clear_pending: false,
            local_queue: Vec::new(),
            queue_drain: QueueDrainState::LiveAutoDrain,
            queue_focused: false,
            queue_selected: 0,
            queue_editing: None,
            pending_drafts: VecDeque::new(),
            pending_queue_edit: None,
            prompt_history: Vec::new(),
            recovered_drafts: Vec::new(),
            history_browse: None,
            prompt_history_hits: Vec::new(),
            slash_open: false,
            slash_popup: None,
            slash_view_start: 0,
            slash_selected: 0,
            theme: theme_for(launch.theme),
            persisted_theme: launch.theme,
            dirty: true,
            request_reattach: false,
            host_disconnected: false,
            terminal_handoff: None,
            newer_build_installed: false,
            next_build_check: Instant::now(),
            pending_reload: false,
            reload_probe: None,
            background_processes: 0,
            processes: std::sync::Arc::new(Vec::new()),
            activity_tab: None,
            activity_last_tab: processes::ActivityTab::Agents,
            process_selected: None,
            process_output: false,
            process_scroll: 0,
            destructive_confirm: None,
            typing: typing_guard::TypingGuard::default(),
            key_typing: typing_guard::Typing::SETTLED,
            key_label_hits: Vec::new(),
            status_processes_hit: None,
            process_hits: Vec::new(),
            activity_agents_hit: None,
            activity_close_hit: None,
            activity_close_hover: false,
            activity_processes_hit: None,
            profiles: agents_tab::ProfilesPane::default(),
            resume_menu: None,
            slash_enter_completion: None,
            resume_popup: None,
            resume_close_hit: None,
            resume_close_hover: false,
            resume_hits: Vec::new(),
            resume_hover: None,
            tree_navigator: None,
            agents_unavailable: None,
            pending_open_tree: false,
            tree_popup: None,
            tree_hits: Vec::new(),
            agent_acks: tree::AgentAcknowledgements::default(),
            agents_clock: None,
            clock: None,
            settings_snapshot: launch.settings.clone(),
            settings_menu: None,
            settings_error: None,
            settings_hits: Vec::new(),
            list_clicks: picker::ListClicks::default(),
            settings_hover: None,
            control_tab: None,
            last_control_tab: ControlTab::Session,
            keybinds_full: false,
            transcript_scrollbar: None,
            scrollbar_drag: None,
            control_tab_hits: Vec::new(),
            control_popup: None,
            control_close_hit: None,
            control_close_hover: false,
            tab_copy_text: Vec::new(),
            session_info: crate::tui::presentation::SessionInfoView::default(),
            session_rows: Vec::new(),
            session_row_hits: Vec::new(),
            tree_epoch_state: None,
            control_scroll: [modal_chrome::ScrollState::default(); ControlTab::ALL.len()],
            templates: Vec::new(),
            providers: launch.providers,
            providers_focus: 0,
            provider_row_hits: Vec::new(),
            transcript_area: None,
            transcript_body_area: None,
            jump_to_live_hit: None,
            sticky_prompt_jump: None,
            queue_area: None,
            composer_inner: None,
            composer_wrap: DraftWrapCache::default(),
            queue_button_hits: Vec::new(),
            capacity_recovery: None,
            capacity_recovery_action: None,
            tool_hit_rows: Vec::new(),
            approval_chip_hits: Vec::new(),
            call_details: call_detail::CallDetails::default(),
            pending_tool_click: None,
            tool_hit_targets: Vec::new(),
            selection: TranscriptSelection::default(),
            transcript_drag_pointer: None,
            transcript_gesture: false,
            transcript_gesture_is_wheel: false,
            transcript_down_cell: None,
            copy_selection_after_draw: false,
            info_overlay: None,
            info_overlay_scroll: modal_chrome::ScrollState::default(),
            quit_arm: None,
            esc_clear_arm: None,
            cancel_pending: false,
            context_usage: unknown_context_usage(),
            last_turn_usage: None,
            last_turn: None,
            last_prompt_at: None,
            idle_clock_painted: None,
            semantic_revision: 1,
            subscription_usage: SubscriptionUsageView::Loading,
            usage_limits: crate::tui::providers::unread_usage_limits(),
            launch_intro: None,
            version: launch.version,
            render_prefs: render_prefs_for(launch.settings.as_ref()),
            status_spinner_frame: 0,
            status_spinner_at: Instant::now(),
            turn_started: None,
            phase_started: None,
            model_request_started: None,
            composer_chrome: composer_chrome::ComposerChrome::default(),
            status_agents_hit: None,
            tree_live_descendants: 0,
            tree_restored_paused: false,
            pending_continuation: plakous_host_protocol::PendingContinuation::default(),
            open_approval: None,
            mouse_scroll_queue: 0,
            last_wheel_at: None,
            last_transcript_width: 0,
            session_open_started: None,
            open_notes_from: None,
            session_open_ms: None,
            pending_open_settings: false,
            pending_open_usage: false,
            pending_open_resume: launch.open_resume_picker,
            launch_resume_picker: launch.open_resume_picker,
            telemetry_paths: launch.telemetry_paths,
            refresh_failure_notes: Vec::new(),
            time_zone: launch.time_zone,
            opening_note,
            opening_note_position: None,
            declined_launch_resume_picker: false,
            reattach_note: None,
            startup_pending: hosted,
        }
    }
}

#[cfg(test)]
fn format_turn_usage(
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cached_input_tokens: u64,
    cache_write_tokens: u64,
    model_operations: u64,
    reporting_model_operations: u64,
) -> String {
    if model_operations > 0 && reporting_model_operations == 0 {
        return format!(
            "usage · input unknown · output unknown · reasoning unknown · provider-reported cached input unknown · provider-reported cache write unknown · coverage missing (0/{model_operations} model operations)"
        );
    }

    let (subtotal, coverage) = if reporting_model_operations > model_operations {
        (
            "reported subtotal · ",
            format!("invalid ({reporting_model_operations}/{model_operations} model operations)"),
        )
    } else if reporting_model_operations < model_operations {
        (
            "reported subtotal · ",
            format!("partial ({reporting_model_operations}/{model_operations} model operations)"),
        )
    } else if model_operations == 0 {
        ("", "not applicable (0 model operations)".to_owned())
    } else {
        (
            "",
            format!("complete ({model_operations}/{model_operations} model operations)"),
        )
    };
    format!(
        "usage · {subtotal}input {input_tokens} · output {output_tokens} · reasoning {reasoning_tokens} · provider-reported cached input {cached_input_tokens} · provider-reported cache write {cache_write_tokens} · coverage {coverage}"
    )
}

/// Outcome of a Ctrl-modified key.
enum CtrlKey {
    /// Not a recognized ctrl binding; fall through.
    Pass,
    /// Handled; no `UserAction` (e.g. clear input).
    Consumed,
    /// Handled with a control-plane action.
    Action(UserAction),
}

pub use self::terminal::PersistentTerminal;
pub(crate) use self::terminal::force_restore_terminal;
#[cfg(test)]
pub(crate) use self::terminal::run_hosted;

mod agent_view;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod agent_view_regression_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod agent_view_review_probes;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod agent_view_tests;
mod agents_tab;
mod call_detail;
mod composer;
mod composer_chrome;
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod composer_chrome_tests;
mod control_center;
mod destructive_confirm;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod destructive_path_tests;
mod host_events;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod instant_confirm_tests;
mod keys;
mod launch;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod launch_intro_tests;
mod mouse;
mod opening_note;
mod pane;
#[cfg(any(test, feature = "test-hooks"))]
mod pilot;
#[cfg(feature = "test-hooks")]
mod pilot_sidecar;
mod plain_key;
mod processes;
#[cfg(test)]
mod processes_tests;
mod queue;
mod reload;
mod render;
mod render_control;
mod render_overlays;
mod render_transcript;
mod scrollbar;
mod settings_tab;
mod state;
mod terminal;
mod transcript;
mod tree;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod typing_grace_tests;
mod typing_guard;
use self::terminal::display_workspace;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod composer_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod control_center_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod copy_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod detail_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod models_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod pilot_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod resume_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod settings_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod shell_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod test_support;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod transcript_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod tree_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod typing_safety_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod typing_sweep_tests;
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unit tests may assert with unwrap/expect"
)]
mod typing_timed_tests;
