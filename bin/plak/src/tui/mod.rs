//! Ratatui-based Plak terminal UI.
//!
//! A3 hybrid product shell: chrome + structured scrollback + slash palette +
//! queue chips + control-plane routing. The module is inert presentation: it
//! reads the host protocol (`plakous_host_protocol`) and the product's settings
//! vocabulary (`crate::config`), and nothing else of the product; agent wiring
//! lives in the host.

use std::fmt;
use std::path::PathBuf;

// The product's settings vocabulary and its extension of the host protocol,
// re-exported so the module reads them as `crate::tui::*`.
pub(crate) use crate::config::{
    AgentAcknowledgement, HostEvent, HostEventReceiver, ProfileOutcome, ProfileRole, ProfileRow,
    ProfilesView, SettingsSnapshot, SettingsTheme, SurfaceIntent, SurfaceObservation, UserAction,
    UserActionSendError, UserActionSender,
};
#[cfg(test)]
pub(crate) use crate::config::{host_event_channel, user_action_channel};

#[cfg(feature = "test-support")]
pub(crate) mod benchmark;

mod activity;
mod agent_list;
mod blocks;
#[allow(
    dead_code,
    reason = "staged internal composer selection API is exercised directly until ui.rs adopts it"
)]
pub(crate) mod composer_selection;
mod composer_view;
mod highlight;
pub mod keymap;
mod launch_intro;
pub mod markdown;
mod modal_chrome;
mod picker;
mod providers;
mod quit_guard;
#[allow(
    dead_code,
    reason = "the published cadence table and state names exist for tests and telemetry"
)]
mod redraw;
mod resume_pane;
mod selection;
mod settings;
pub mod settings_registry;
mod slash;
#[cfg(test)]
mod stability;
mod text_input;
mod theme;
mod tree_navigator;
mod tree_view;
mod usage_tab;
#[cfg(test)]
mod wave;

mod ui;

#[cfg(test)]
mod markdown_tests;
#[cfg(test)]
mod perf_gate;
#[cfg(test)]
mod perf_gate_tests;

pub(crate) use highlight::warm_in_background as warm_highlighting;
pub use ui::PersistentTerminal;
pub(crate) use ui::force_restore_terminal;

#[cfg(test)]
use blocks::LineLayout;
#[cfg(any(test, feature = "test-support"))]
use blocks::{DisplayMode, ScrollBlock, TranscriptRenderCache, layout_blocks};
#[cfg(feature = "test-support")]
use blocks::{RenderPrefs, TranscriptChange, format_tool_plain, layout_anchor, wrap_plain};
#[cfg(test)]
use providers::ProviderRowPresentation;
pub(crate) use providers::StatusTone;
#[cfg(test)]
pub(crate) use providers::remedy_catalog;
/// The transcript line the round after a declined launch resume picker opens
/// with, so the operator is never left to discover `/attach` on their own.
pub const DECLINED_RESUME_NOTE: &str =
    "no session resumed \u{b7} attached this workspace's default model instead";

/// The status detail shown while that reattach is in flight.
pub const DECLINED_RESUME_STATUS: &str = "attaching the default model\u{2026}";

#[cfg(test)]
use resume_pane::ResumeRow;
pub use resume_pane::{ResumeMenu, ResumePickerOutcome, SHORT_ID_CHARS};
#[cfg(test)]
use settings::SettingsPane;
pub(crate) use settings::{EnumChooser, IntStepper, SettingsMenu};
#[cfg(any(test, feature = "test-support"))]
use theme::Theme;
pub use tree_navigator::{TopologyNavigatorKey, TopologyNavigatorOutcome, TreeNavigator};
pub use tree_view::{CanvasNode, CanvasTree};

// The host protocol the TUI speaks; crate-internal names for the moved types.
pub(crate) use plakous_host_protocol::views::presentation;
pub(crate) use plakous_host_protocol::views::{
    AuthKind, ProviderDef, ProviderState, ResumeSessionEntry, TemplateRow, status_glyph,
};
pub(crate) use plakous_host_protocol::{
    ApprovalChoice, CompositionSelection, ContextCapacityRecovery, ProtocolSelection,
    QueueDrainState, QueueItem,
};

/// Launch options for the interactive TUI.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "inert launch DTO keeps independent host-selected presentation flags explicit"
)]
pub struct TuiLaunch {
    /// Workspace package version supplied by the product launcher.
    pub version: &'static str,
    /// Optional session identifier shown in the header when set.
    pub session_id: Option<String>,
    /// Workspace root associated with this session (display only in shell).
    pub workspace: PathBuf,
    /// Fully interpreted workspace label for terminal chrome.
    pub workspace_chrome: String,
    /// When true, the first frame owns an outstanding `/resume` request so the
    /// host's cwd-scoped session list opens the picker (`plak resume`).
    pub open_resume_picker: bool,
    /// Optional account label for the header strip.
    pub account: Option<String>,
    /// Optional model label for the header strip.
    pub model: Option<String>,
    /// Optional reasoning effort label for the header strip.
    pub effort: Option<String>,
    /// Optional exact provider option for the bound model (`priority`).
    pub extra: Option<String>,
    /// Persisted composition selection for the exact session.
    pub composition: CompositionSelection,
    /// Persisted `OpenRouter` wire selection, or `Auto` for native providers.
    pub protocol: ProtocolSelection,
    /// Effective model-visible tool presentation compiled by the product host.
    pub effective_presentation: String,
    /// When true, host will drive agent; shell shows control-plane hints.
    pub agent_enabled: bool,
    /// Whether the terminal session enables mouse event capture.
    pub mouse_enabled: bool,
    /// Product-selected color theme.
    pub theme: SettingsTheme,
    /// Product-loaded settings shown immediately while host preparation continues.
    pub settings: Option<SettingsSnapshot>,
    /// Product-interpreted provider rows for `/login`.
    pub providers: Vec<ProviderDef>,
    /// Resolved product home for read-only refresh display telemetry.
    pub telemetry_paths: Option<crate::product_paths::PlakPaths>,
    /// The zone every wall-clock date and send time the presentation prints
    /// is read in: the system zone, read once at startup, in production; a
    /// fixed zone in tests. Nothing inside the TUI asks the environment.
    pub time_zone: &'static jiff::tz::TimeZone,
    /// One line the launcher wants in the transcript from the first frame.
    ///
    /// A round the product started on the operator's behalf has to say so:
    /// declining the launch resume picker reattaches under the workspace
    /// default, and the operator must read why the session they are looking at
    /// is not the one they were choosing between.
    pub opening_note: Option<String>,
}

/// UTC, the zone a launch reads dates in unless the product names another.
pub static UTC_ZONE: jiff::tz::TimeZone = jiff::tz::TimeZone::UTC;

impl Default for TuiLaunch {
    fn default() -> Self {
        Self {
            version: "",
            session_id: None,
            workspace: PathBuf::from("."),
            workspace_chrome: ".".to_owned(),
            open_resume_picker: false,
            account: None,
            model: None,
            effort: None,
            extra: None,
            composition: CompositionSelection::FollowCatalog,
            protocol: ProtocolSelection::Auto,
            effective_presentation: "direct".to_owned(),
            agent_enabled: false,
            mouse_enabled: true,
            theme: SettingsTheme::DefaultDark,
            settings: None,
            providers: Vec::new(),
            telemetry_paths: None,
            time_zone: &UTC_ZONE,
            opening_note: None,
        }
    }
}

/// Error returned by the TUI entrypoints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiError {
    /// Stdout is not a TTY (or the terminal cannot host an interactive UI).
    NotATty,
    /// Terminal setup, restore, or event I/O failed.
    Io(String),
}

impl fmt::Display for TuiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotATty => formatter
                .write_str("plak TUI requires an interactive terminal (stdout is not a TTY)"),
            Self::Io(message) => write!(formatter, "plak TUI I/O error: {message}"),
        }
    }
}

impl std::error::Error for TuiError {}

/// Runs the TUI with default launch options (current directory workspace).
///
/// # Errors
///
/// Returns [`TuiError::NotATty`] when stdout is not a capable terminal or
/// [`TuiError::Io`] on terminal setup/event failures.
#[cfg(test)]
pub fn run() -> Result<TuiOutcome, TuiError> {
    run_with(TuiLaunch::default())
}

/// Runs the TUI with the given launch options (local shell; no host channels).
///
/// # Errors
///
/// Same as [`run`].
#[cfg(test)]
pub fn run_with(launch: TuiLaunch) -> Result<TuiOutcome, TuiError> {
    run_hosted(launch, None, None)
}

/// Session-oriented entry; same as [`run_with`].
///
/// # Errors
///
/// Same as [`run_with`].
#[cfg(test)]
pub fn run_session(launch: TuiLaunch) -> Result<TuiOutcome, TuiError> {
    run_hosted(launch, None, None)
}

/// Run the interactive shell.
///
/// When `actions` / `events` are both `None`, a pure local shell (echoes `you:`
/// lines). When `Some`, every non-quit action is sent on `actions`
/// and [`HostEvent`]s from `events` update the UI. Uses `std::sync::mpsc` so
/// Plak can drive the agent from a worker thread.
///
/// Returns [`TuiOutcome::RetryAttach`] when `/login` (or `/attach`) asks the
/// product host to re-resolve OAuth and re-open with an agent.
///
/// # Errors
///
/// Same as [`run`].
#[cfg(test)]
pub fn run_hosted(
    launch: TuiLaunch,
    actions: Option<UserActionSender>,
    events: Option<HostEventReceiver>,
) -> Result<TuiOutcome, TuiError> {
    ui::run_hosted(launch, actions, events)
}

/// Pure control-plane key / submit routing (testable without a TTY).
///
/// Shared by the UI shell and unit tests so idle/busy mapping stays single-sourced.
pub mod control {
    use super::UserAction;

    /// Classify a non-empty, non-command submit line under the current busy flag.
    ///
    /// - Idle + normal → [`UserAction::Prompt`]
    /// - Busy + normal → [`UserAction::FollowUp`]
    /// - Busy + steer → [`UserAction::Steer`]
    /// - Idle + steer → [`UserAction::Prompt`] (steer is a no-op distinction when idle)
    #[must_use]
    pub fn route_submit(busy: bool, steer: bool, text: String) -> UserAction {
        if steer && busy {
            UserAction::Steer(text)
        } else if busy {
            UserAction::FollowUp(text)
        } else {
            UserAction::Prompt(text)
        }
    }

    /// Simplified empty-draft Ctrl-C for tests: busy → interrupt; idle → quit.
    ///
    /// The live TUI uses [`crate::tui::quit_guard::classify_ctrl_c`]: non-empty draft
    /// clears first, busy cancels the turn, and idle or stuck-cancel requires a
    /// second press inside [`crate::tui::quit_guard::QUIT_CONFIRM_WINDOW`] (spec 02 §5.3).
    #[must_use]
    #[cfg(test)]
    pub const fn route_ctrl_c(busy: bool) -> UserAction {
        if busy {
            UserAction::Interrupt
        } else {
            UserAction::Quit
        }
    }

    /// True when the trimmed line is a product quit command. Only the slash
    /// spellings quit: a bare `quit`, `exit` or `:q` is an ordinary prompt.
    #[must_use]
    pub fn is_quit_command(line: &str) -> bool {
        matches!(line, "/quit" | "/exit")
    }

    /// True when the trimmed line requests dropping the last queued follow-up.
    #[must_use]
    pub fn is_drop_command(line: &str) -> bool {
        matches!(line, "/drop")
    }

    /// Local transcript prefix for a text-bearing action.
    ///
    /// When the agent host is attached it already emits `HostEvent::UserMessage`;
    /// local echo must be skipped in that path to avoid duplicate user bubbles.
    #[must_use]
    pub fn local_echo_line(action: &UserAction, host_echoes_user: bool) -> Option<String> {
        if host_echoes_user {
            return None;
        }
        match action {
            UserAction::Prompt(text) => Some(text.clone()),
            UserAction::FollowUp(text) => Some(format!("[queue] {text}")),
            UserAction::Steer(text) => Some(format!("[steer] {text}")),
            UserAction::Interrupt
            | UserAction::Background
            | UserAction::StopProcesses { .. }
            | UserAction::DropQueued
            | UserAction::SendQueuedNow { .. }
            | UserAction::DeleteQueued { .. }
            | UserAction::EditQueued { .. }
            | UserAction::MoveQueued { .. }
            | UserAction::ResumeQueueDrain
            | UserAction::ResolveApproval { .. }
            | UserAction::Quit
            | UserAction::ClearSession
            | UserAction::Compact
            | UserAction::ResumeSession { .. }
            | UserAction::ResumeSessionHandoff { .. }
            | UserAction::ForkSession { .. }
            | UserAction::RefreshReadiness
            | UserAction::RetryAttach
            | UserAction::Recover
            | UserAction::OpenTopologyNavigator
            | UserAction::OpenAgentTranscript { .. }
            | UserAction::CloseAgentTranscript
            | UserAction::RefreshTopologyTree
            | UserAction::ResumeTopologyTree
            | UserAction::InterruptTopologyNode { .. }
            | UserAction::CloseTopologyNode { .. }
            | UserAction::StopAllTopology
            | UserAction::RefreshTemplates
            | UserAction::InvokeTool { .. }
            | UserAction::RequestCallDetail { .. }
            | UserAction::Extension(_) => None,
        }
    }
}

/// How the interactive shell finished.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiOutcome {
    /// Operator quit; do not re-open for attach.
    Exited {
        /// The last host-published binding, so the exit line names the session
        /// the operator was actually in after an in-host `/resume` or `/clear`.
        attached_session: Option<String>,
    },
    /// Auth/binding changed — app should re-resolve credentials and re-open the TUI.
    RetryAttach {
        /// Host-authoritative attached product session when the presentation
        /// released the terminal (see [`TuiOutcome::Handoff`]).
        attached_session: Option<String>,
        /// The operator declined the launch resume picker, so this reattach is
        /// the product attaching the workspace default rather than an explicit
        /// `/attach`. When no default is attachable the launcher quits cleanly
        /// with the ordinary exit hint instead of parking in a not-ready shell
        /// the operator never asked for.
        declined_resume_picker: bool,
    },
    /// Presentation restored the terminal and requests one app-owned workflow.
    Handoff {
        /// The workflow the app owns while the terminal is released.
        handoff: TuiHandoff,
        /// Host-authoritative attached product session when the presentation
        /// released the terminal.
        ///
        /// `/clear`, an in-host `/resume`, and a rebind branch all replace the
        /// attached session inside the host, so the launcher's own record can
        /// name a session the operator left long ago. Every re-entry must come
        /// back on the session that was on screen, so the identity travels with
        /// the outcome rather than being reconstructed by the launcher.
        attached_session: Option<String>,
    },
}

/// Raw-terminal workflow requested by an emitted UI intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiHandoff {
    /// Open the product configuration editor.
    EditConfig,
    /// Relaunch the exact recorded session binding in the same Plak process.
    ResumeSession {
        /// Exact durable product session id.
        id: String,
    },
    /// Release the session, then replace this process with the installed
    /// `plak` resuming it, so a rebuilt binary takes over the same session.
    Reload {
        /// The installed build and its state key, asked before teardown.
        target: Option<crate::carry::ReloadTarget>,
    },
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::{HostEvent, QueueDrainState, QueueItem, TuiError, TuiLaunch, UserAction, control};
    use plakous_hosted::sessions::{NoticeScope, TranscriptRow};
    use std::path::PathBuf;

    #[test]
    fn launch_defaults_are_stable() {
        let launch = TuiLaunch::default();
        assert_eq!(launch.session_id, None);
        assert_eq!(launch.workspace, PathBuf::from("."));
        assert_eq!(launch.account, None);
        assert_eq!(launch.model, None);
        assert_eq!(launch.effort, None);
        assert!(!launch.agent_enabled);
    }

    #[test]
    fn error_display_is_clear() {
        assert!(
            TuiError::NotATty
                .to_string()
                .contains("interactive terminal")
        );
        assert!(TuiError::Io("boom".into()).to_string().contains("boom"));
    }

    #[test]
    fn control_plane_routing_table() {
        assert_eq!(
            control::route_submit(false, false, "p".into()),
            UserAction::Prompt("p".into())
        );
        assert_eq!(
            control::route_submit(true, false, "f".into()),
            UserAction::FollowUp("f".into())
        );
        assert_eq!(
            control::route_submit(true, true, "s".into()),
            UserAction::Steer("s".into())
        );
        assert_eq!(control::route_ctrl_c(true), UserAction::Interrupt);
        assert_eq!(control::route_ctrl_c(false), UserAction::Quit);
        assert!(control::is_quit_command("/quit"));
        assert!(control::is_drop_command("/drop"));
    }
    #[test]
    fn host_event_variants_are_constructible() {
        let _ = HostEvent::Row(TranscriptRow::user("x"));
        let _ = HostEvent::ProviderSnapshot {
            providers: Vec::new(),
        };
        let _ = HostEvent::Activity {
            label: "starting".into(),
            active: true,
        };
        let _ = HostEvent::Status {
            phase: "idle".into(),
            queue_len: 0,
            detail: None,
        };
        let _ = HostEvent::QueueSnapshot {
            items: vec![QueueItem {
                rejected: false,
                id: "q1".into(),
                revision: 2,
                text: "same".into(),
            }],
            drain: QueueDrainState::RestoredPaused,
        };
        let _ = HostEvent::SetBusy(true);
        let _ = HostEvent::PromptHistorySnapshot(vec![]);
        let _ = HostEvent::UserMessage {
            text: "u".into(),
            sent_at_ms: None,
        };
        let _ = HostEvent::AssistantDelta("d".into());
        let _ = HostEvent::AssistantDone;
        let _ = HostEvent::ToolLine {
            call_id: None,
            parent_call_id: None,
            cell_id: None,
            kind: crate::tui::presentation::ActivityKind::Tool,
            name: "t".into(),
            state: "start".into(),
            subject: None,
            detail: None,
            duration_ms: None,
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        };
        let _ = HostEvent::Row(TranscriptRow::info(NoticeScope::Session, "s"));
        let _ = HostEvent::Row(TranscriptRow::error(NoticeScope::Host, "e"));
        let _ = HostEvent::Binding {
            session_id: None,
            account: None,
            model: None,
            effort: None,
            extra: None,
            composition: crate::tui::CompositionSelection::FollowCatalog,
            protocol: crate::tui::ProtocolSelection::Auto,
            effective_presentation: "direct".into(),
        };
        let _ = HostEvent::ClearView;
        let _ = HostEvent::Usage(crate::tui::presentation::ContextUsage {
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
        });
        let _ = HostEvent::TurnUsage {
            input_tokens: 10,
            output_tokens: 4,
            reasoning_tokens: 2,
            cached_input_tokens: 3,
            cache_write_tokens: 0,
            model_operations: Some(2),
            reporting_model_operations: Some(1),
            peak_prepared_input_tokens: None,
            terminal: true,
        };
    }
}

#[cfg(test)]
mod prop_tests {
    use super::UserAction;
    use super::control::{route_ctrl_c, route_submit};
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn route_submit_props(busy in any::<bool>(), steer in any::<bool>(), s in ".{1,40}") {
            let action = route_submit(busy, steer, s.clone());
            match (busy, steer) {
                (true, true) => assert_eq!(action, UserAction::Steer(s)),
                (true, false) => assert_eq!(action, UserAction::FollowUp(s)),
                (false, _) => assert_eq!(action, UserAction::Prompt(s)),
            }
        }

        #[test]
        fn route_ctrl_c_props(busy in any::<bool>()) {
            if busy {
                assert_eq!(route_ctrl_c(busy), UserAction::Interrupt);
            } else {
                assert_eq!(route_ctrl_c(busy), UserAction::Quit);
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests_entry {
    use super::{
        CompositionSelection, SettingsTheme, TuiError, TuiLaunch, run, run_hosted, run_session,
        run_with,
    };
    use std::path::PathBuf;

    #[test]
    fn default_feature_surface_compiles_without_terminal_dependencies() {
        use std::io::stdout;

        use crossterm::tty::IsTty;

        let launch = TuiLaunch {
            version: "0.1.0",
            session_id: Some("test-session".into()),
            workspace: PathBuf::from("/tmp"),
            workspace_chrome: "/tmp".into(),
            open_resume_picker: false,
            account: None,
            model: None,
            effort: None,
            extra: None,
            composition: CompositionSelection::FollowCatalog,
            protocol: super::ProtocolSelection::Auto,
            effective_presentation: "direct".into(),
            agent_enabled: false,
            mouse_enabled: true,
            theme: SettingsTheme::DefaultDark,
            settings: None,
            providers: Vec::new(),
            telemetry_paths: None,
            time_zone: &super::UTC_ZONE,
            opening_note: None,
        };
        assert_eq!(launch.session_id.as_deref(), Some("test-session"));

        if !stdout().is_tty() {
            assert_eq!(run(), Err(TuiError::NotATty));
            assert_eq!(run_with(launch.clone()), Err(TuiError::NotATty));
            assert_eq!(run_session(launch.clone()), Err(TuiError::NotATty));
            assert_eq!(run_hosted(launch, None, None), Err(TuiError::NotATty));
        }
    }

    #[test]
    fn error_display_still_clear() {
        assert!(
            TuiError::NotATty
                .to_string()
                .contains("interactive terminal")
        );
    }
}
