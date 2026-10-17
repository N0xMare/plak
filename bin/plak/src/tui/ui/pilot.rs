//! Deterministic semantic projection and typed driver for TUI qualification.
//!
//! This is deliberately view-state, not a serialized [`App`] and not a
//! terminal-cell tree. It exposes only stable meaning needed to drive the
//! product like an operator while excluding geometry, clocks, animation,
//! caches, and mailbox internals. The bounded transcript tail is private local
//! state: external decision services must remove it before making a request.

use std::cell::RefCell;

use serde::Serialize;

use super::{
    App, ControlTab, KeyCode, KeyEvent, KeyModifiers, SettingKind, SettingsMenu, SettingsPane,
    UserAction, settings_registry,
};
use crate::tui::blocks::ScrollBlock;
use crate::tui::keymap::{BINDINGS, When};

pub(super) const SNAPSHOT_SCHEMA: &str = "plak.tui-snapshot.v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct TuiSnapshot {
    pub schema: &'static str,
    pub owner: &'static str,
    pub phase: String,
    pub busy: bool,
    pub binding: BindingSnapshot,
    pub composer: ComposerSnapshot,
    pub queue: QueueSnapshot,
    /// The correlated canonical turn lifecycle: admission, terminal phase or
    /// explicit `unresolved`. Never derived from busy or usage state.
    pub turn: Option<plakous_host_protocol::TurnLifecycle>,
    pub transcript: TranscriptSnapshot,
    pub control_center: Option<ControlCenterSnapshot>,
    pub modal: Option<ModalSnapshot>,
    pub canvas: Option<CanvasSnapshot>,
    pub background_processes: usize,
    pub processes: Option<ProcessesSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct ProcessesSnapshot {
    pub ids: Vec<String>,
    pub commands: Vec<String>,
    pub selected_id: Option<String>,
    pub output: bool,
    pub confirming: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct BindingSnapshot {
    pub attached: bool,
    pub session_id: Option<String>,
    pub account: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub extra: Option<String>,
    pub composition: &'static str,
    pub protocol: &'static str,
    pub effective_presentation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct ComposerSnapshot {
    pub char_count: usize,
    pub cursor_char: usize,
    pub selection: Option<(usize, usize)>,
    pub multiline: bool,
    pub slash_open: bool,
    pub history_browsing: bool,
    pub editing_queue_item: bool,
    pub pending_prompt: bool,
    pub pending_draft_count: usize,
    pub attachment_count: usize,
    pub cancel_pending: bool,
    pub host_disconnected: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct QueueSnapshot {
    pub count: usize,
    pub focused: bool,
    pub selected: Option<usize>,
    pub selected_id: Option<String>,
    pub selected_revision: Option<u64>,
    pub editing: bool,
    pub edit_pending: bool,
    pub drain: &'static str,
    pub restored_paused: bool,
    pub items: Vec<QueueItemSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct QueueItemSnapshot {
    pub id: String,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct TranscriptSnapshot {
    /// The agent whose transcript Activity is showing; `None` for the root.
    pub agent: Option<String>,
    pub block_count: usize,
    pub focused_block: Option<usize>,
    pub scroll_from_bottom: usize,
    pub approval_pending: bool,
    pub streaming: bool,
    pub focused_agent_transcript_available: bool,
    pub tail: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct ControlCenterSnapshot {
    pub tab: &'static str,
    pub provider_count: usize,
    pub focused_provider: Option<usize>,
    pub session_row_count: usize,
    pub template_count: usize,
    pub settings: Option<SettingsSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct ModalSnapshot {
    pub kind: &'static str,
    pub item_count: usize,
    pub selected: Option<usize>,
    pub selected_id: Option<String>,
    pub selection_in_place: Option<bool>,
    pub confirmation: Option<&'static str>,
    pub filter_char_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct CanvasSnapshot {
    pub mode: &'static str,
    pub tree_id: Option<String>,
    pub revision: Option<u64>,
    pub state: Option<String>,
    pub selected: Option<String>,
    pub selected_status: Option<String>,
    pub selected_finished: Option<bool>,
    pub selected_has_session: Option<bool>,
    pub node_count: usize,
    pub filter: &'static str,
    pub restored_paused: bool,
    pub confirmation: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct SettingsSnapshot {
    pub pane: &'static str,
    pub selected: Option<String>,
    pub filter_char_count: usize,
    pub values: Vec<SettingValue>,
    pub error: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct SettingValue {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct LegalAction {
    pub id: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PilotAction {
    OpenSettings,
    SelectSetting(String),
    ToggleSetting(String),
    CloseFocusedOwner,
    Invoke(String),
    TypeText(String),
}

impl PilotAction {
    fn id(&self) -> String {
        match self {
            Self::OpenSettings => "open-settings".to_owned(),
            Self::SelectSetting(key) => format!("select-setting:{key}"),
            Self::ToggleSetting(key) => format!("toggle-setting:{key}"),
            Self::CloseFocusedOwner => "close-focused-owner".to_owned(),
            Self::Invoke(id) => id.clone(),
            Self::TypeText(_) => "type-text".to_owned(),
        }
    }
}

impl App {
    pub(super) fn semantic_snapshot(&self) -> TuiSnapshot {
        TuiSnapshot {
            background_processes: self.background_processes,
            processes: (self.activity_tab == Some(super::processes::ActivityTab::Processes)).then(
                || ProcessesSnapshot {
                    ids: self.processes.iter().map(|row| row.id.clone()).collect(),
                    commands: self
                        .processes
                        .iter()
                        .map(|row| row.command.clone())
                        .collect(),
                    selected_id: self.process_selected.clone(),
                    output: self.process_output,
                    confirming: self.destructive_confirm.is_some(),
                },
            ),
            schema: SNAPSHOT_SCHEMA,
            owner: self.semantic_owner(),
            phase: self.phase.clone(),
            busy: self.busy,
            binding: BindingSnapshot {
                attached: self.agent_enabled,
                session_id: self.session_id.clone(),
                account: self.account.clone(),
                model: self.model.clone(),
                effort: self.effort.clone(),
                extra: self.model_extra.clone(),
                composition: self.composition.as_str(),
                protocol: self.protocol.as_str(),
                effective_presentation: self.effective_presentation.clone(),
            },
            composer: ComposerSnapshot {
                char_count: self.input.chars().count(),
                cursor_char: self.input_cursor,
                selection: self
                    .composer_selection
                    .range(&self.input)
                    .map(|range| (range.start, range.end)),
                multiline: self.input.contains('\n'),
                slash_open: self.slash_open,
                history_browsing: self.history_browse.is_some(),
                editing_queue_item: self.queue_editing.is_some(),
                pending_prompt: self.pending_prompt.is_some(),
                pending_draft_count: self.pending_drafts.len(),
                attachment_count: self.attachment_count(),
                cancel_pending: self.cancel_pending,
                host_disconnected: self.host_disconnected,
            },
            queue: QueueSnapshot {
                count: self.local_queue.len(),
                focused: self.queue_focused,
                selected: self
                    .queue_focused
                    .then_some(self.queue_selected)
                    .filter(|selected| *selected < self.local_queue.len()),
                selected_id: self
                    .queue_focused
                    .then(|| self.local_queue.get(self.queue_selected))
                    .flatten()
                    .map(|item| item.id.clone()),
                selected_revision: self
                    .queue_focused
                    .then(|| self.local_queue.get(self.queue_selected))
                    .flatten()
                    .map(|item| item.revision),
                editing: self.queue_editing.is_some(),
                edit_pending: self.pending_queue_edit.is_some(),
                drain: match self.queue_drain {
                    super::QueueDrainState::LiveAutoDrain => "live-auto-drain",
                    super::QueueDrainState::RestoredPaused => "restored-paused",
                },
                restored_paused: matches!(self.queue_drain, super::QueueDrainState::RestoredPaused),
                items: self
                    .local_queue
                    .iter()
                    .map(|item| QueueItemSnapshot {
                        id: item.id.clone(),
                        revision: item.revision,
                    })
                    .collect(),
            },
            turn: self.last_turn.clone(),
            transcript: TranscriptSnapshot {
                agent: self.viewed_agent_id().map(str::to_owned),
                block_count: self.pane.blocks.len(),
                focused_block: self.pane.focused_block,
                scroll_from_bottom: self.pane.scroll_from_bottom,
                approval_pending: self.open_approval.is_some(),
                streaming: self.pane.assistant_stream_active,
                focused_agent_transcript_available: self.focused_agent_transcript_available(),
                tail: self
                    .pane
                    .blocks
                    .iter()
                    .rev()
                    .take(8)
                    .map(ScrollBlock::as_plain_line)
                    .map(|line| line.chars().take(500).collect())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect(),
            },
            control_center: self.control_tab.map(|tab| ControlCenterSnapshot {
                tab: tab.label(),
                provider_count: self.providers.len(),
                focused_provider: (tab == ControlTab::Providers && !self.providers.is_empty())
                    .then_some(self.providers_focus),
                session_row_count: self.session_rows.len(),
                template_count: self.templates.len(),
                settings: (tab == ControlTab::Settings)
                    .then(|| self.semantic_settings_snapshot())
                    .flatten(),
            }),
            modal: self.semantic_modal_snapshot(),
            canvas: self.semantic_canvas_snapshot(),
        }
    }

    fn semantic_owner(&self) -> &'static str {
        if self.quitting {
            "shutdown"
        } else if self.destructive_confirm.is_some() {
            "destructive-confirmation"
        } else if self.activity_tab == Some(super::processes::ActivityTab::Processes) {
            "processes"
        } else if self.info_overlay.is_some() {
            "overlay"
        } else if let Some(tab) = self.control_tab {
            match tab {
                ControlTab::Settings => match self.settings_menu.as_ref().map(|menu| menu.pane) {
                    Some(SettingsPane::Choosing(_)) => "settings-chooser",
                    Some(SettingsPane::Stepping(_)) => "settings-stepper",
                    _ => "settings",
                },
                ControlTab::Models => "agents",
                _ => "menu",
            }
        } else if self.activity_tab == Some(super::processes::ActivityTab::Agents)
            && self.viewed_agent_id().is_some()
        {
            "agent-view"
        } else if self.activity_tab == Some(super::processes::ActivityTab::Agents) {
            let Some(nav) = self.tree_navigator.as_ref() else {
                return "canvas-loading";
            };
            if nav.snapshot.is_none() {
                "canvas-loading"
            } else {
                "canvas"
            }
        } else if self.resume_menu.is_some() {
            "resume-picker"
        } else if self.history_browse.is_some() {
            "history"
        } else if self.slash_open {
            "slash-palette"
        } else if self.queue_focused {
            "queue"
        } else if self.queue_editing.is_some() {
            "queue-editor"
        } else {
            "composer"
        }
    }

    fn semantic_modal_snapshot(&self) -> Option<ModalSnapshot> {
        if self.info_overlay.is_some() {
            return Some(ModalSnapshot {
                kind: "overlay",
                item_count: 0,
                selected: None,
                selected_id: None,
                selection_in_place: None,
                confirmation: None,
                filter_char_count: 0,
            });
        }
        if let Some(menu) = self.resume_menu.as_ref() {
            let selected = menu.selected_entry();
            return Some(ModalSnapshot {
                kind: "resume-picker",
                item_count: menu.entries.len(),
                selected: (!menu.entries.is_empty()).then_some(menu.selected),
                selected_id: selected.map(|entry| entry.id.clone()),
                selection_in_place: selected.map(|entry| entry.in_place),
                confirmation: None,
                filter_char_count: 0,
            });
        }
        if let Some(confirm) = self.destructive_confirm.as_ref() {
            return Some(ModalSnapshot {
                kind: "destructive-confirmation",
                item_count: confirm.buttons(),
                selected: Some(confirm.focus),
                selected_id: None,
                selection_in_place: None,
                confirmation: Some(confirm.kind()),
                filter_char_count: 0,
            });
        }
        if self.slash_open {
            let matches = self.slash_matches();
            return Some(ModalSnapshot {
                kind: "slash-palette",
                item_count: matches.len(),
                selected: (!matches.is_empty()).then_some(self.slash_selected),
                selected_id: matches
                    .get(self.slash_selected)
                    .map(|(name, _)| (*name).to_owned()),
                selection_in_place: None,
                confirmation: None,
                filter_char_count: self.slash_filter().chars().count(),
            });
        }
        self.control_tab.map(|tab| ModalSnapshot {
            kind: match tab {
                ControlTab::Settings => "settings",
                ControlTab::Models => "agents",
                _ => "menu",
            },
            item_count: if tab == ControlTab::Settings {
                self.settings_menu
                    .as_ref()
                    .map_or(0, |menu| menu.visible_rows().len())
            } else {
                ControlTab::ALL.len()
            },
            selected: (tab == ControlTab::Settings)
                .then(|| self.settings_menu.as_ref().map(|menu| menu.selected))
                .flatten(),
            selected_id: None,
            selection_in_place: None,
            confirmation: None,
            filter_char_count: self
                .settings_menu
                .as_ref()
                .map_or(0, |menu| menu.filter.chars().count()),
        })
    }

    fn semantic_canvas_snapshot(&self) -> Option<CanvasSnapshot> {
        if self.activity_tab != Some(super::processes::ActivityTab::Agents) {
            return None;
        }
        let nav = self.tree_navigator.as_ref()?;
        let tree = nav.snapshot.as_ref();
        let selected = nav.selected_node();
        Some(CanvasSnapshot {
            mode: self.semantic_owner(),
            tree_id: tree.map(|tree| tree.tree_id.clone()),
            revision: tree.map(|tree| tree.revision),
            state: tree.map(|tree| tree.state.clone()),
            selected: nav.selected_agent_id().map(str::to_owned),
            selected_status: selected.map(|node| node.status.clone()),
            selected_finished: selected.map(|node| node.finished),
            selected_has_session: selected.map(|node| node.has_session),
            node_count: tree.map_or(0, |tree| tree.nodes.len()),
            filter: nav.scope.label(),
            restored_paused: tree.is_some_and(|tree| tree.restored_paused),
            confirmation: self
                .destructive_confirm
                .as_ref()
                .map_or("none", super::destructive_confirm::DestructiveConfirm::kind),
        })
    }

    fn semantic_settings_snapshot(&self) -> Option<SettingsSnapshot> {
        let menu = self.settings_menu.as_ref()?;
        let values = settings_registry::ROWS
            .iter()
            .enumerate()
            .filter(|(_, meta)| !matches!(meta.kind, SettingKind::Action))
            .map(|(index, meta)| SettingValue {
                key: meta.key.to_owned(),
                value: menu.value_label(index),
            })
            .collect();
        Some(SettingsSnapshot {
            pane: match menu.pane {
                SettingsPane::Browse => "browse",
                SettingsPane::Choosing(_) => "chooser",
                SettingsPane::Stepping(_) => "stepper",
            },
            selected: SettingsMenu::meta(menu.selected).map(|meta| meta.key.to_owned()),
            filter_char_count: menu.filter.chars().count(),
            values,
            error: self.settings_error.is_some(),
        })
    }

    pub(super) fn legal_pilot_actions(&self) -> Vec<LegalAction> {
        let owner = self.semantic_owner();
        if owner == "shutdown" {
            return Vec::new();
        }
        let actions = RefCell::new(Vec::new());
        let add = |id: &str, description: &str| {
            let mut actions = actions.borrow_mut();
            if !actions.iter().any(|action: &LegalAction| action.id == id) {
                actions.push(LegalAction {
                    id: id.to_owned(),
                    description: description.to_owned(),
                });
            }
        };
        let add_key = |id: &str, when: When, chord: &str| {
            if let Some(description) = BINDINGS
                .iter()
                .find(|binding| binding.when == when && binding.chord == chord)
                .map(|binding| binding.effect)
            {
                add(id, description);
            }
        };

        add_key("quit", When::Global, "Ctrl+Q");
        match owner {
            "overlay" => {
                add_key("move-up", When::Overlay, "↑ / ↓");
                add_key("move-down", When::Overlay, "↑ / ↓");
                add_key("page-up", When::Overlay, "PageUp / PageDown");
                add_key("page-down", When::Overlay, "PageUp / PageDown");
                add_key("home", When::Overlay, "Home / End");
                add_key("end", When::Overlay, "Home / End");
                add_key("close-focused-owner", When::Overlay, "Enter / q / Esc");
            }
            "processes" => {
                add_key("move-up", When::Processes, "↑ / k");
                add_key("move-down", When::Processes, "↓ / j");
                add_key("page-up", When::Processes, "PageUp / PageDown");
                add_key("page-down", When::Processes, "PageUp / PageDown");
                add_key("home", When::Processes, "Home / End");
                add_key("end", When::Processes, "Home / End");
                add_key("activity-agents", When::Processes, "Tab / Shift+Tab");
                add_key("close-focused-owner", When::Processes, "Esc");
                if self.process_selected.is_some() {
                    add_key("process-output", When::Processes, "Enter");
                }
                if self.processes.iter().any(|row| {
                    Some(&row.id) == self.process_selected.as_ref()
                        && row.status == plakous_host_protocol::views::ProcessStatus::Running
                }) {
                    add_key("process-stop", When::Processes, "s");
                }
                if self.background_processes > 0 {
                    add_key("process-stop-all", When::Processes, "S");
                }
            }
            "destructive-confirmation" => {
                add_key("confirm", When::Confirm, "Enter");
                add_key("close-focused-owner", When::Confirm, "Esc");
            }
            "resume-picker" => {
                add_key("move-up", When::Resume, "↑");
                add_key("move-down", When::Resume, "↓");
                if self
                    .resume_menu
                    .as_ref()
                    .is_some_and(|menu| !menu.entries.is_empty())
                {
                    add_key("accept", When::Resume, "Enter");
                }
                add_key("close-focused-owner", When::Resume, "Esc");
            }
            "canvas-loading" => {
                add_key("activity-processes", When::Tree, "Tab / Shift+Tab");
                add_key("close-focused-owner", When::Tree, "Esc");
            }
            "canvas" => {
                add_key("activity-processes", When::Tree, "Tab / Shift+Tab");
                add_key("move-up", When::Tree, "↑ / k");
                add_key("move-down", When::Tree, "↓ / j");
                add_key("move-left", When::Tree, "← / h");
                add_key("move-right", When::Tree, "→ / l");
                add_key("home", When::Tree, "Home");
                if self
                    .tree_navigator
                    .as_ref()
                    .and_then(|nav| nav.selected_node())
                    .is_some()
                {
                    add_key("accept", When::Tree, "Enter");
                }
                add_key("toggle-filter", When::Tree, "f");
                add_key("refresh", When::Tree, "R");
                if self.tree_navigator.as_ref().is_some_and(|nav| {
                    nav.snapshot
                        .as_ref()
                        .is_some_and(|tree| tree.restored_paused)
                }) {
                    add_key("resume", When::Tree, "r");
                }
                if self
                    .tree_navigator
                    .as_ref()
                    .and_then(|nav| nav.selected_node())
                    .is_some_and(|node| node.parent_agent_id.is_some())
                {
                    add_key("interrupt", When::Tree, "i");
                    add_key("close-node", When::Tree, "c");
                }
                if self.tree_navigator.as_ref().is_some_and(|nav| {
                    nav.snapshot.as_ref().is_some_and(|tree| {
                        tree.nodes
                            .iter()
                            .any(|node| node.parent_agent_id.is_some() && !node.finished)
                    })
                }) {
                    add_key("stop-all", When::Tree, "s");
                }
                add_key("close-focused-owner", When::Tree, "Esc");
            }
            "agents" => {
                add_key("move-up", When::Modal, "↑ / ↓");
                add_key("move-down", When::Modal, "↑ / ↓");
                add_key("accept", When::Modal, "Enter");
                add_key("type-text", When::Modal, "any printable key");
                if !self.profiles.filter.is_empty() {
                    add_key("backspace", When::Modal, "Backspace");
                }
                add_key("close-focused-owner", When::Modal, "Esc");
            }
            "settings" => self.add_settings_pilot_actions(&add, &add_key),
            "settings-chooser" | "settings-stepper" => {
                add_key("move-up", When::Modal, "↑ / ↓");
                add_key("move-down", When::Modal, "↑ / ↓");
                add_key("accept", When::Modal, "Enter");
                add_key("close-focused-owner", When::Modal, "Esc");
            }
            "menu" => {
                add_key("tab-previous", When::Modal, "← / →");
                add_key("tab-next", When::Modal, "← / →");
                if self.control_tab == Some(ControlTab::Providers) {
                    add_key("move-up", When::Modal, "↑ / ↓");
                    add_key("move-down", When::Modal, "↑ / ↓");
                }
                add_key("page-up", When::Modal, "PageUp / PageDown");
                add_key("page-down", When::Modal, "PageUp / PageDown");
                add_key("home", When::Modal, "Home / End");
                add_key("end", When::Modal, "Home / End");
                if self.control_tab == Some(ControlTab::Keybinds) && !self.keybinds_full {
                    add_key("all-keybinds", When::Modal, "Enter (Keybinds)");
                }
                if matches!(
                    self.control_tab,
                    Some(
                        ControlTab::Providers
                            | ControlTab::Usage
                            | ControlTab::Context
                            | ControlTab::Session
                    )
                ) {
                    add_key("copy", When::Modal, "y");
                }
                add_key("close-focused-owner", When::Modal, "Esc");
            }
            "slash-palette" => {
                add("move-up", "Select the previous filtered slash command.");
                add("move-down", "Select the next filtered slash command.");
                add_key("complete", When::Composer, "Tab (palette open)");
                add("accept", "Activate the selected slash command.");
                add("close-focused-owner", "Dismiss the slash command palette.");
                for (name, description) in self.slash_matches() {
                    add(&format!("select-command:{name}"), description);
                }
            }
            "history" => {
                add("move-up", "Select the previous prompt-history entry.");
                add("move-down", "Select the next prompt-history entry.");
                add("accept", "Load the selected prompt without submitting it.");
                add("close-focused-owner", "Dismiss prompt-history browsing.");
            }
            "queue" => {
                add_key("move-up", When::Transcript, "↑ / ↓");
                add_key("move-down", When::Transcript, "↑ / ↓");
                add_key("reorder-up", When::Transcript, "Shift+↑ / Shift+↓");
                add_key("reorder-down", When::Transcript, "Shift+↑ / Shift+↓");
                add_key("edit", When::Transcript, "Ctrl+E");
                add_key("drop", When::Global, "Ctrl+D");
                add("send-now", "Send the selected queued follow-up now.");
                if matches!(self.queue_drain, super::QueueDrainState::RestoredPaused) {
                    add_key("resume", When::Transcript, "Ctrl+R");
                }
                add("close-focused-owner", "Return focus to the composer.");
            }
            "queue-editor" => {
                add(
                    "type-text",
                    "Insert fixture- or agent-supplied text into the queue editor.",
                );
                add("submit-edit", "Persist the edited queued follow-up.");
                add("close-focused-owner", "Cancel the queue edit.");
            }
            "agent-view" => {
                add_key("agent-transcript-back", When::Agent, "Esc");
                add_key("page-up", When::Agent, "PageUp / PageDown");
                add_key("page-down", When::Agent, "PageUp / PageDown");
                add_key("previous-agent", When::Agent, "[ / ]");
                add_key("next-agent", When::Agent, "[ / ]");
            }
            "composer" => {
                add(
                    "type-text",
                    "Insert fixture- or agent-supplied text into the composer.",
                );
                add_key("newline", When::Composer, "Shift+Enter / Alt+Enter");
                if !self.input.trim().is_empty() {
                    add_key("send", When::Composer, "Enter");
                    if self.busy {
                        add_key("steer", When::Composer, "Ctrl+Enter / Ctrl+S");
                    }
                }
                if self.input.is_empty() {
                    add_key("open-slash", When::Global, "/");
                }
                if let Some(description) = Self::slash_description("settings") {
                    add("open-settings", description);
                }
                add_key("open-canvas", When::Global, "Ctrl+T");
                add_key("open-menu", When::Global, "Ctrl+X");
                if self.input.is_empty() && !self.prompt_history.is_empty() {
                    add_key("recall-previous", When::Composer, "↑ (empty draft)");
                }
                if !self.local_queue.is_empty() {
                    add_key("focus-queue", When::Transcript, "Tab");
                }
                if self.busy && !self.cancel_pending {
                    add("cancel", "Cancel the running root turn.");
                }
                if self.open_approval.is_some() {
                    add_key("allow", When::Transcript, "Alt+A / Alt+D");
                    add_key("deny", When::Transcript, "Alt+A / Alt+D");
                }
                if self.input.is_empty()
                    && !self.focused_agent_transcript_available()
                    && self
                        .pane
                        .focused_block
                        .is_some_and(|index| self.focused_foldable_block() == Some(index))
                {
                    add_key("toggle-focused-block", When::Transcript, "Enter");
                }
            }
            _ => {}
        }
        actions.into_inner()
    }

    fn slash_description(name: &str) -> Option<&'static str> {
        crate::tui::slash::COMMANDS
            .iter()
            .find_map(|(candidate, description)| (*candidate == name).then_some(*description))
    }

    fn add_settings_pilot_actions(
        &self,
        add: &impl Fn(&str, &str),
        add_key: &impl Fn(&str, When, &str),
    ) {
        add_key("close-focused-owner", When::Modal, "Esc");
        let Some(menu) = self.settings_menu.as_ref() else {
            return;
        };
        add_key("move-up", When::Modal, "↑ / ↓");
        add_key("move-down", When::Modal, "↑ / ↓");
        add_key("accept", When::Modal, "Enter");
        add_key("search", When::Modal, "/");
        for index in menu.visible_rows() {
            let meta = &settings_registry::ROWS[index];
            add(
                &format!("select-setting:{}", meta.key),
                &format!("Focus the {} setting.", meta.label),
            );
        }
        if let Some(meta) = SettingsMenu::meta(menu.selected) {
            if !matches!(meta.kind, SettingKind::Action) {
                add_key("reset", When::Modal, "d");
            }
            if matches!(meta.kind, SettingKind::Bool { .. })
                && menu.visible_rows().contains(&menu.selected)
            {
                add(
                    &format!("toggle-setting:{}", meta.key),
                    &format!("Toggle the {} setting.", meta.label),
                );
            }
        }
    }

    pub(super) fn dispatch_pilot_action(
        &mut self,
        action: PilotAction,
    ) -> Result<Option<UserAction>, String> {
        let id = action.id();
        if !self
            .legal_pilot_actions()
            .iter()
            .any(|legal| legal.id == id)
        {
            return Err(format!(
                "TUI action is not legal in the current state: {id}"
            ));
        }
        match action {
            PilotAction::OpenSettings => Ok(self.open_control_tab(ControlTab::Settings)),
            PilotAction::SelectSetting(key) => {
                let Some(index) = settings_registry::ROWS
                    .iter()
                    .position(|meta| meta.key == key)
                else {
                    return Err("legal setting action lost its registry row".to_owned());
                };
                let Some(menu) = self.settings_menu.as_mut() else {
                    return Err("settings action lost its active menu".to_owned());
                };
                menu.selected = index;
                self.mark_dirty();
                Ok(None)
            }
            PilotAction::ToggleSetting(key) => {
                let Some(index) = settings_registry::ROWS
                    .iter()
                    .position(|meta| meta.key == key)
                else {
                    return Err("legal setting action lost its registry row".to_owned());
                };
                let action = self.activate_settings_row(index);
                self.sync_render_prefs();
                Ok(action)
            }
            PilotAction::CloseFocusedOwner => Ok(self.pilot_key(KeyCode::Esc, KeyModifiers::NONE)),
            PilotAction::Invoke(id) => self.invoke_pilot_action(&id),
            PilotAction::TypeText(text) => {
                if text.chars().any(|character| {
                    character == '\n' || character == '\r' || character.is_control()
                }) {
                    return Err("TUI type-text accepts printable text only".to_owned());
                }
                for character in text.chars() {
                    if self
                        .handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                        .is_some()
                    {
                        return Err("typing unexpectedly emitted a host action".to_owned());
                    }
                }
                Ok(None)
            }
        }
    }

    fn invoke_pilot_action(&mut self, id: &str) -> Result<Option<UserAction>, String> {
        if let Some(command) = id.strip_prefix("select-command:") {
            let Some(index) = self
                .slash_matches()
                .iter()
                .position(|(name, _)| *name == command)
            else {
                return Err(format!("slash command is not visible: {command}"));
            };
            self.slash_selected = index;
            self.mark_dirty();
            return Ok(None);
        }
        let (code, modifiers) = match id {
            "quit" => (KeyCode::Char('q'), KeyModifiers::CONTROL),
            "close-focused-owner" | "cancel" | "agent-transcript-back" => {
                (KeyCode::Esc, KeyModifiers::NONE)
            }
            // The pop-up confirms only from an action button: the pilot
            // presses the first, as a click on it does.
            "confirm" if self.semantic_owner() == "destructive-confirmation" => {
                return Ok(self.press_confirm_action());
            }
            // The focused queue takes only arrows; its commands are chords.
            "send-now" => (KeyCode::Char('s'), KeyModifiers::CONTROL),
            "edit" => (KeyCode::Char('e'), KeyModifiers::CONTROL),
            "drop" => (KeyCode::Char('d'), KeyModifiers::CONTROL),
            "resume" if self.semantic_owner() == "queue" => {
                (KeyCode::Char('r'), KeyModifiers::CONTROL)
            }
            // The pilot's accept is deliberate, as a double-click on the row is.
            "accept" if self.semantic_owner() == "resume-picker" => {
                return Ok(self.accept_resume_selection());
            }
            "confirm" | "accept" | "send" | "submit-edit" | "process-output" => {
                (KeyCode::Enter, KeyModifiers::NONE)
            }
            "all-keybinds" => (KeyCode::Enter, KeyModifiers::NONE),
            "steer" => (KeyCode::Enter, KeyModifiers::CONTROL),
            "newline" => (KeyCode::Enter, KeyModifiers::SHIFT),
            "move-up" | "recall-previous" => (KeyCode::Up, KeyModifiers::NONE),
            "move-down" => (KeyCode::Down, KeyModifiers::NONE),
            "move-left" | "tab-previous" => (KeyCode::Left, KeyModifiers::NONE),
            "move-right" | "tab-next" => (KeyCode::Right, KeyModifiers::NONE),
            "reorder-up" => (KeyCode::Up, KeyModifiers::SHIFT),
            "reorder-down" => (KeyCode::Down, KeyModifiers::SHIFT),
            "page-up" => (KeyCode::PageUp, KeyModifiers::NONE),
            "previous-agent" => (KeyCode::Char('['), KeyModifiers::NONE),
            "next-agent" => (KeyCode::Char(']'), KeyModifiers::NONE),
            "page-down" => (KeyCode::PageDown, KeyModifiers::NONE),
            "home" => (KeyCode::Home, KeyModifiers::NONE),
            "end" => (KeyCode::End, KeyModifiers::NONE),
            "complete" | "focus-queue" => (KeyCode::Tab, KeyModifiers::NONE),
            "backspace" => (KeyCode::Backspace, KeyModifiers::NONE),
            "open-slash" | "search" => (KeyCode::Char('/'), KeyModifiers::NONE),
            "open-canvas" => (KeyCode::Char('t'), KeyModifiers::CONTROL),
            "open-menu" => (KeyCode::Char('x'), KeyModifiers::CONTROL),
            "toggle-filter" => (KeyCode::Char('f'), KeyModifiers::NONE),
            "refresh" => (KeyCode::Char('R'), KeyModifiers::NONE),
            "resume" => (KeyCode::Char('r'), KeyModifiers::NONE),
            "interrupt" => (KeyCode::Char('i'), KeyModifiers::NONE),
            "close-node" => (KeyCode::Char('c'), KeyModifiers::NONE),
            "stop-all" => (KeyCode::Char('s'), KeyModifiers::NONE),
            "process-stop" => (KeyCode::Char('s'), KeyModifiers::NONE),
            "process-stop-all" => (KeyCode::Char('S'), KeyModifiers::SHIFT),
            "activity-agents" | "activity-processes" => (KeyCode::Tab, KeyModifiers::NONE),
            "reset" => (KeyCode::Char('d'), KeyModifiers::NONE),
            "copy" => (KeyCode::Char('y'), KeyModifiers::NONE),
            "allow" => (KeyCode::Char('a'), KeyModifiers::ALT),
            "deny" => (KeyCode::Char('d'), KeyModifiers::ALT),
            "toggle-focused-block" => (KeyCode::Enter, KeyModifiers::NONE),
            "type-text" | "open-settings" => {
                return Err(format!("{id} requires its typed pilot action"));
            }
            _ => return Err(format!("unknown TUI pilot action: {id}")),
        };
        Ok(self.pilot_key(code, modifiers))
    }

    /// A semantic action's key: deliberate, so it never reads as part of a
    /// typed run (typed text does, through `type-text`).
    fn pilot_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<UserAction> {
        self.typing.forget_run();
        self.handle_key(KeyEvent::new(code, modifiers))
    }
}
