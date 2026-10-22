//! The one confirmation pop-up for destructive operator actions (spec 02
//! §5.7): stopping the root turn, interrupting, closing, stopping or resuming
//! agents, stopping background processes, and quitting over them. It opens
//! instantly: focus starts on the primary action; `Enter` presses the focused
//! button; `Esc` cancels; `←`/`→` and `Tab` move between buttons at once; a
//! click presses the button it lands on. Over the composer a printable key or
//! `Backspace` closes the pop-up and is typed into the draft. In a full-page
//! view those keys do nothing.

use super::{
    App, Block, BorderType, Borders, Clear, Frame, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    Line, MouseEvent, MouseEventKind, Paragraph, Rect, ScrollBlock, Span, Style, UnicodeWidthStr,
    UserAction, event, modal_chrome,
};
use crate::tui::blocks::humanize_tool;
use crate::tui::tree_navigator::short_id;
use crate::tui::{ApprovalChoice, agent_list};
use plakous_host_protocol::views::ProcessStatus;

const CANCEL: &str = "[ Cancel ]";
/// How to confirm, under the consequence. Focus already sits on the action.
const HINT: &str = "Enter confirms · Esc cancels";
const BUTTON_GAP: u16 = 2;
/// A long process command wraps rather than widening the pop-up past this.
const MAX_TEXT_WIDTH: usize = 72;

/// What a confirmation button does. A settings reset names the row and reads
/// the live draft when it is pressed, so a snapshot that arrives while the
/// pop-up is open is not written back.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ConfirmButton {
    Send(UserAction),
    ResetSetting(&'static str),
    Reload(crate::carry::ReloadTarget),
}

/// An open confirmation: what it says, the exact intents its buttons send,
/// and the geometry its last frame painted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DestructiveConfirm {
    title: String,
    lines: Vec<String>,
    /// The action buttons after `Cancel`. Each send is bound to its exact
    /// intent when the pop-up opened, so snapshots never retarget it. A
    /// settings reset binds the row key and reads the live draft on press.
    actions: Vec<(&'static str, ConfirmButton)>,
    /// The focused button: 0 is `Cancel`, `n` is `actions[n - 1]`. Opens on
    /// the primary action (`1` when there is one).
    pub(super) focus: usize,
    popup: Option<Rect>,
    /// Painted buttons, `Cancel` first.
    hits: Vec<Option<Rect>>,
}

impl DestructiveConfirm {
    /// The pop-up's short kind, for the semantic snapshot.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn kind(&self) -> &'static str {
        match self.actions.first().map(|(_, button)| button) {
            Some(ConfirmButton::ResetSetting(_)) => "settings-reset",
            Some(ConfirmButton::Reload(_)) => "reload-carry",
            Some(ConfirmButton::Send(UserAction::Interrupt)) => "turn-stop",
            Some(ConfirmButton::Send(UserAction::InterruptTopologyNode { .. })) => "interrupt",
            Some(ConfirmButton::Send(UserAction::CloseTopologyNode { .. })) => "close",
            Some(ConfirmButton::Send(UserAction::StopAllTopology)) => "stop-all",
            Some(ConfirmButton::Send(UserAction::ResumeTopologyTree)) => "resume",
            Some(ConfirmButton::Send(UserAction::StopProcesses { id: Some(_), .. })) => {
                "process-stop"
            }
            Some(ConfirmButton::Send(UserAction::Quit)) => "quit",
            Some(ConfirmButton::Send(UserAction::ResolveApproval {
                choice: ApprovalChoice::Allow,
                ..
            })) => "approval-allow",
            Some(ConfirmButton::Send(UserAction::ResolveApproval { .. })) => "approval-deny",
            Some(ConfirmButton::Send(UserAction::Extension(_))) => "profile",
            _ => "process-stop-all",
        }
    }

    /// Buttons, `Cancel` included.
    pub(super) const fn buttons(&self) -> usize {
        self.actions.len() + 1
    }

    /// The intent the first action button sends.
    #[cfg(test)]
    pub(super) fn first_action(&self) -> Option<&UserAction> {
        match self.actions.first().map(|(_, button)| button) {
            Some(ConfirmButton::Send(action)) => Some(action),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn cancel_hit(&self) -> Option<Rect> {
        self.hits.first().copied().flatten()
    }

    /// The first action button's painted rectangle.
    #[cfg(test)]
    pub(super) fn action_hit(&self) -> Option<Rect> {
        self.hits.get(1).copied().flatten()
    }

    /// Every action's painted rectangle, in order.
    #[cfg(test)]
    pub(super) fn action_hits(&self) -> Vec<Option<Rect>> {
        self.hits.iter().skip(1).copied().collect()
    }
}

/// Hits for the key labels painted in `text`, drawn from `x` on row `y`:
/// each label found (in order, left to right, as whole words, so a short `s`
/// never matches inside another word) presses its key when clicked.
pub(super) fn key_label_hits(
    text: &str,
    x: u16,
    y: u16,
    labels: &[(&str, KeyCode)],
) -> Vec<(Rect, KeyCode)> {
    let mut hits = Vec::new();
    let mut from = 0;
    for (label, code) in labels {
        let Some(start) = text
            .match_indices(label)
            .map(|(at, _)| at)
            .filter(|at| *at >= from)
            .find(|at| {
                let before = text[..*at].chars().next_back();
                let after = text[at + label.len()..].chars().next();
                before.is_none_or(|ch| ch == ' ') && after.is_none_or(|ch| ch == ' ')
            })
        else {
            continue;
        };
        let column = u16::try_from(text[..start].width()).unwrap_or(u16::MAX);
        let width = u16::try_from(label.width()).unwrap_or(u16::MAX);
        hits.push((Rect::new(x.saturating_add(column), y, width, 1), *code));
        from = start + label.len();
    }
    hits
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// Button rows for `widths` inside `width` cells: as many buttons per row as
/// fit, in order, so a narrow pop-up stacks them rather than cutting one.
fn button_rows(widths: &[u16], width: u16) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut used = 0_u16;
    for (index, button) in widths.iter().enumerate() {
        match rows.last_mut() {
            Some(row) if used.saturating_add(BUTTON_GAP + button) <= width => {
                row.push(index);
                used = used.saturating_add(BUTTON_GAP + button);
            }
            _ => {
                rows.push(vec![index]);
                used = *button;
            }
        }
    }
    rows
}

impl App {
    /// Opens the pop-up for `action`, focused on its primary button. Any other
    /// intent is not destructive and is returned to be sent as is.
    pub(super) fn confirm_destructive(&mut self, action: UserAction) -> Option<UserAction> {
        let (title, lines, button) = match &action {
            UserAction::InterruptTopologyNode { agent_id } => (
                format!("Interrupt {}?", short_id(agent_id)),
                vec![
                    "Its running turn is cancelled; work in flight is lost.".to_owned(),
                    "Mail to the agent resumes it.".to_owned(),
                ],
                "[ Interrupt ]",
            ),
            UserAction::CloseTopologyNode { agent_id } => {
                let descendants = self.open_descendants(agent_id);
                let subject = if descendants == 0 {
                    short_id(agent_id)
                } else {
                    format!(
                        "{} and {}",
                        short_id(agent_id),
                        plural(descendants, "open descendant", "open descendants")
                    )
                };
                (
                    format!("Close {}?", short_id(agent_id)),
                    vec![
                        format!("{subject}: closed for good."),
                        "Running turns are cancelled; work in flight is lost.".to_owned(),
                    ],
                    "[ Close ]",
                )
            }
            UserAction::StopAllTopology => (
                "Stop all agents?".to_owned(),
                vec![
                    format!(
                        "{}: closed for good.",
                        plural(self.open_agents(), "open agent", "open agents")
                    ),
                    "Running turns are cancelled; work in flight is lost.".to_owned(),
                    "No new agents start until your next prompt.".to_owned(),
                ],
                "[ Stop all ]",
            ),
            UserAction::ResumeTopologyTree => (
                "Resume all agents?".to_owned(),
                vec![
                    format!(
                        "{} of the paused tree start working again.",
                        plural(self.open_agents(), "open agent", "open agents")
                    ),
                    "Their turns continue and spend again.".to_owned(),
                ],
                "[ Resume all ]",
            ),
            UserAction::StopProcesses { id: Some(id), .. } => (
                "Stop this process?".to_owned(),
                vec![
                    self.processes.iter().find(|row| &row.id == id).map_or_else(
                        || id.clone(),
                        |row| format!("{} · {}", row.owner, row.command),
                    ),
                    "Stopped; work in flight is lost.".to_owned(),
                ],
                "[ Stop ]",
            ),
            UserAction::StopProcesses { id: None, .. } => (
                "Stop all processes?".to_owned(),
                vec![
                    format!(
                        "{}: stopped.",
                        plural(
                            self.background_processes,
                            "running background process",
                            "running background processes"
                        )
                    ),
                    "Their work in flight is lost.".to_owned(),
                ],
                "[ Stop all ]",
            ),
            UserAction::Quit => ("Quit plak?".to_owned(), self.quit_lines(), "[ Quit ]"),
            UserAction::ResolveApproval {
                approval_id,
                choice,
            } => {
                let (tool, subject) = self.approval_summary(approval_id);
                let allow = *choice == ApprovalChoice::Allow;
                let mut lines = Vec::new();
                if let Some(subject) = subject {
                    lines.push(subject);
                }
                lines.push(if allow {
                    format!("{tool} runs with your approval.")
                } else {
                    format!("{tool} is refused.")
                });
                if allow {
                    (format!("Allow {tool}?"), lines, "[ Allow ]")
                } else {
                    (format!("Deny {tool}?"), lines, "[ Deny ]")
                }
            }
            _ => return Some(action),
        };
        self.open_confirm(title, lines, vec![(button, action)]);
        None
    }

    /// What quitting stops: the running turn, live agents, background
    /// processes.
    fn quit_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.turn_in_flight() {
            lines.push("The root turn is cancelled; its work in flight is lost.".to_owned());
        }
        if self.tree_live_descendants > 0 {
            lines.push(format!(
                "{} interrupted, resumable.",
                plural(
                    self.tree_live_descendants,
                    "running agent is",
                    "running agents are"
                )
            ));
        }
        if self.background_processes > 0 {
            lines.push(format!(
                "{} will be stopped; work in flight is lost.",
                plural(
                    self.background_processes,
                    "background process",
                    "background processes"
                )
            ));
        }
        lines.push("The session stays resumable.".to_owned());
        lines
    }

    /// The open approval's tool and subject, for its pop-up.
    fn approval_summary(&self, approval_id: &str) -> (String, Option<String>) {
        self.pane
            .blocks
            .iter()
            .rev()
            .find_map(|block| match block {
                ScrollBlock::Approval {
                    approval_id: id,
                    tool_name,
                    subject,
                    ..
                } if id == approval_id => Some((
                    humanize_tool(tool_name),
                    subject.clone().filter(|subject| !subject.is_empty()),
                )),
                _ => None,
            })
            .unwrap_or_else(|| ("the tool".to_owned(), None))
    }

    /// The root's busy `Esc` (spec 02 §5.3): stop the turn, or the turn and
    /// every background process.
    pub(super) fn confirm_turn_stop(&mut self) {
        if matches!(self.phase.as_str(), "fork" | "rebind") {
            self.open_confirm(
                "Stop this operation?".to_owned(),
                vec![
                    "Stops before the fork attaches when possible; any saved fork stays resumable."
                        .to_owned(),
                ],
                vec![("[ Stop operation ]", UserAction::Interrupt)],
            );
            return;
        }
        let mut lines = vec!["The root turn is cancelled; its work in flight is lost.".to_owned()];
        if self.tree_live_descendants > 0 {
            lines.push(format!(
                "{} interrupted, resumable.",
                plural(
                    self.tree_live_descendants,
                    "running agent is",
                    "running agents are"
                )
            ));
        }
        let mut actions = vec![("[ Stop turn ]", UserAction::Interrupt)];
        if self.background_processes > 0 {
            lines.push(if self.background_processes == 1 {
                "1 background process keeps running unless you stop it too.".to_owned()
            } else {
                format!(
                    "{} background processes keep running unless you stop them too.",
                    self.background_processes
                )
            });
            actions.push((
                "[ Stop turn + processes ]",
                UserAction::StopProcesses {
                    id: None,
                    interrupt: true,
                },
            ));
        }
        self.open_confirm("Stop this turn?".to_owned(), lines, actions);
    }

    pub(super) fn open_confirm(
        &mut self,
        title: String,
        lines: Vec<String>,
        actions: Vec<(&'static str, UserAction)>,
    ) {
        self.show_confirm(
            title,
            lines,
            actions
                .into_iter()
                .map(|(label, action)| (label, ConfirmButton::Send(action)))
                .collect(),
        );
    }

    /// Settings `d`: the button names the row. The value is the live draft
    /// at press, with that one key restored.
    pub(super) fn open_setting_reset(
        &mut self,
        key: &'static str,
        title: String,
        lines: Vec<String>,
    ) {
        self.show_confirm(
            title,
            lines,
            vec![("[ Reset ]", ConfirmButton::ResetSetting(key))],
        );
    }

    pub(super) fn open_reload_confirm(
        &mut self,
        target: crate::carry::ReloadTarget,
        lines: Vec<String>,
    ) {
        self.show_confirm(
            "Carry to a smaller route?".to_owned(),
            lines,
            vec![("[ Carry anyway ]", ConfirmButton::Reload(target))],
        );
    }

    fn show_confirm(
        &mut self,
        title: String,
        lines: Vec<String>,
        actions: Vec<(&'static str, ConfirmButton)>,
    ) {
        let focus = usize::from(!actions.is_empty());
        self.destructive_confirm = Some(DestructiveConfirm {
            title,
            lines,
            actions,
            focus,
            popup: None,
            hits: Vec::new(),
        });
        self.mark_dirty();
    }

    /// Open (not settled) non-root agents in the tree the TUI last saw.
    fn open_agents(&self) -> usize {
        self.agent_tree.as_ref().map_or(0, |tree| {
            tree.nodes
                .iter()
                .filter(|node| node.parent_agent_id.is_some() && agent_list::is_active(node))
                .count()
        })
    }

    /// Open agents below `agent_id`, which closing it closes too.
    fn open_descendants(&self, agent_id: &str) -> usize {
        let Some(tree) = self.agent_tree.as_ref() else {
            return 0;
        };
        let mut below = std::collections::BTreeSet::from([agent_id]);
        let mut grew = true;
        while grew {
            grew = false;
            for node in &tree.nodes {
                if node
                    .parent_agent_id
                    .as_deref()
                    .is_some_and(|parent| below.contains(parent))
                    && below.insert(node.agent_id.as_str())
                {
                    grew = true;
                }
            }
        }
        tree.nodes
            .iter()
            .filter(|node| node.agent_id != agent_id && below.contains(node.agent_id.as_str()))
            .filter(|node| agent_list::is_active(node))
            .count()
    }

    /// Whether `action` still has a live target. A pop-up whose target
    /// stopped being active closes rather than send a stale intent.
    fn confirm_target_active(&self, action: &UserAction) -> bool {
        let agent_active = |agent_id: &str| {
            self.agent_tree.as_ref().is_some_and(|tree| {
                tree.nodes
                    .iter()
                    .any(|node| node.agent_id == agent_id && agent_list::is_active(node))
            })
        };
        match action {
            UserAction::Interrupt => self.busy && !self.cancel_pending,
            UserAction::StopProcesses {
                interrupt: true, ..
            } => self.busy && !self.cancel_pending,
            UserAction::InterruptTopologyNode { agent_id }
            | UserAction::CloseTopologyNode { agent_id } => agent_active(agent_id),
            UserAction::StopAllTopology => self.open_agents() > 0,
            UserAction::ResumeTopologyTree => self
                .agent_tree
                .as_ref()
                .is_some_and(|tree| tree.restored_paused),
            UserAction::StopProcesses { id: Some(id), .. } => self
                .processes
                .iter()
                .any(|row| &row.id == id && row.status == ProcessStatus::Running),
            UserAction::StopProcesses { id: None, .. } => self.background_processes > 0,
            UserAction::ResolveApproval { approval_id, .. } => {
                self.pending_approval() == Some(approval_id.as_str())
            }
            _ => true,
        }
    }

    /// After host state changed: closes a pop-up whose first action lost its
    /// target, and drops a later action that did (the turn stop's
    /// `+ processes` once no process runs).
    pub(super) fn retire_stale_confirm(&mut self) {
        let Some(confirm) = self.destructive_confirm.as_ref() else {
            return;
        };
        let live: Vec<bool> = confirm
            .actions
            .iter()
            .map(|(_, button)| match button {
                ConfirmButton::ResetSetting(_) => true,
                ConfirmButton::Reload(_) => {
                    self.reload_refusal().is_none() && !self.reload_source_changed()
                }
                ConfirmButton::Send(action) => {
                    if matches!(
                        action,
                        UserAction::StopProcesses {
                            interrupt: true,
                            ..
                        }
                    ) {
                        self.busy && !self.cancel_pending && self.background_processes > 0
                    } else {
                        self.confirm_target_active(action)
                    }
                }
            })
            .collect();
        if !live.first().copied().unwrap_or(false) {
            self.destructive_confirm = None;
            self.mark_dirty();
            return;
        }
        if live.iter().all(|live| *live) {
            return;
        }
        if let Some(confirm) = self.destructive_confirm.as_mut() {
            let mut index = 0;
            confirm.actions.retain(|_| {
                index += 1;
                live[index - 1]
            });
            confirm.focus = confirm.focus.min(confirm.buttons().saturating_sub(1));
            confirm.hits.clear();
        }
        self.mark_dirty();
    }

    /// Presses button `index` (0 is `Cancel`) and closes the pop-up.
    fn press_confirm_button(&mut self, index: usize) -> Option<UserAction> {
        let confirm = self.destructive_confirm.take()?;
        self.mark_dirty();
        let (_, button) = confirm.actions.into_iter().nth(index.checked_sub(1)?)?;
        let action = match button {
            ConfirmButton::Reload(target) => {
                self.reload_confirmed_target = Some(target);
                return None;
            }
            ConfirmButton::ResetSetting(key) => return self.commit_settings_reset(key),
            ConfirmButton::Send(action) => action,
        };
        match action {
            UserAction::Interrupt => Some(self.cancel_current_turn()),
            UserAction::StopProcesses {
                interrupt: true, ..
            } => {
                self.cancel_current_turn();
                Some(action)
            }
            UserAction::Quit => {
                self.begin_quit();
                Some(action)
            }
            // Fork now continues in the transcript, where the outcome lands
            // and the next prompt is typed.
            UserAction::Extension(crate::tui::SurfaceIntent::ApplyProfile {
                fork_now: true,
                ..
            }) => {
                self.close_control_center();
                Some(action)
            }
            action => Some(action),
        }
    }

    /// Restores one settings key on the draft as it is now. A snapshot that
    /// landed while the pop-up was open stays, apart from that key.
    fn commit_settings_reset(&mut self, key: &str) -> Option<UserAction> {
        let row = crate::tui::settings_registry::ROWS
            .iter()
            .position(|meta| meta.key == key)?;
        let menu = self.settings_menu.as_mut()?;
        menu.reset_to_default(row);
        menu.pane = super::SettingsPane::Browse;
        let update = menu.draft.clone();
        self.theme = super::theme_for(update.theme);
        self.mark_dirty();
        Some(UserAction::Extension(
            crate::tui::SurfaceIntent::SaveSettings(update),
        ))
    }

    /// The pilot's `confirm`: presses the first action, as a click on it does.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn press_confirm_action(&mut self) -> Option<UserAction> {
        self.press_confirm_button(1)
    }

    /// The pop-up is over the composer, the only view with a prompt. Activity,
    /// the agent view, process output, Menu and the other promptless modals
    /// are not: a letter there does nothing.
    fn confirm_covers_the_composer(&self) -> bool {
        !self.has_no_prompt()
            && self.control_tab.is_none()
            && self.resume_menu.is_none()
            && self.info_overlay.is_none()
    }

    /// Every key while the pop-up is open. `Enter` presses the focused button,
    /// `Esc` cancels, and `←`/`→` and `Tab` move focus at once. Over the
    /// composer a printable key or `Backspace` closes the pop-up and is typed
    /// into the draft; elsewhere those keys do nothing. A `Ctrl` or `Alt`
    /// chord is `Esc`: a legacy terminal delivers `Esc` coalesced with the
    /// next key as one `Alt` chord, and that must never press the focused
    /// button.
    pub(super) fn handle_destructive_confirm_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        // A held key repeats; the press that opened the pop-up must not act.
        if key.kind != KeyEventKind::Press {
            return None;
        }
        self.destructive_confirm.as_ref()?;
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.destructive_confirm = None;
            self.mark_dirty();
            return None;
        }
        let over_composer = self.confirm_covers_the_composer();
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                let confirm = self.destructive_confirm.as_mut()?;
                confirm.focus = confirm.focus.saturating_sub(1);
                self.mark_dirty();
                None
            }
            KeyCode::Tab if shift => {
                let confirm = self.destructive_confirm.as_mut()?;
                confirm.focus = confirm.focus.saturating_sub(1);
                self.mark_dirty();
                None
            }
            KeyCode::Right | KeyCode::Tab => {
                let confirm = self.destructive_confirm.as_mut()?;
                confirm.focus = (confirm.focus + 1).min(confirm.buttons() - 1);
                self.mark_dirty();
                None
            }
            KeyCode::Enter => {
                let focus = self.destructive_confirm.as_ref()?.focus;
                self.press_confirm_button(focus)
            }
            KeyCode::Esc => {
                self.destructive_confirm = None;
                self.mark_dirty();
                None
            }
            // Typing over the composer: the operator is in the draft. Close
            // first so the key is routed as ordinary composer input.
            KeyCode::Char(_) | KeyCode::Backspace if over_composer => {
                self.destructive_confirm = None;
                self.mark_dirty();
                self.route_key(key)
            }
            _ => None,
        }
    }

    /// Mouse while the pop-up is open: a click on a button presses it, a
    /// click outside cancels, and nothing else reaches the views below.
    pub(super) fn handle_destructive_confirm_mouse(
        &mut self,
        mouse: MouseEvent,
    ) -> Option<UserAction> {
        let confirm = self.destructive_confirm.as_ref()?;
        if mouse.kind != MouseEventKind::Down(event::MouseButton::Left) {
            return None;
        }
        let point = (mouse.column, mouse.row).into();
        if let Some(index) = confirm
            .hits
            .iter()
            .position(|hit| hit.is_some_and(|hit| hit.contains(point)))
        {
            return self.press_confirm_button(index);
        }
        if confirm.popup.is_none_or(|popup| !popup.contains(point)) {
            self.destructive_confirm = None;
            self.mark_dirty();
        }
        None
    }

    /// The pop-up's buttons, for hover.
    pub(super) fn destructive_confirm_hits(&self) -> impl Iterator<Item = Rect> {
        self.destructive_confirm
            .as_ref()
            .into_iter()
            .flat_map(|confirm| confirm.hits.iter().copied())
            .flatten()
    }

    pub(super) fn draw_destructive_confirm(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(confirm) = self.destructive_confirm.as_ref() else {
            return;
        };
        let labels: Vec<&str> = std::iter::once(CANCEL)
            .chain(confirm.actions.iter().map(|(label, _)| *label))
            .collect();
        let widths: Vec<u16> = labels
            .iter()
            .map(|label| u16::try_from(label.width()).unwrap_or(u16::MAX))
            .collect();
        let one_row = widths
            .iter()
            .map(|width| usize::from(*width))
            .sum::<usize>()
            + usize::from(BUTTON_GAP) * (widths.len() - 1);
        let content_width = confirm
            .lines
            .iter()
            .map(|line| line.width())
            .chain([confirm.title.width() + 2, one_row])
            .max()
            .unwrap_or(0)
            .min(MAX_TEXT_WIDTH);
        let width = u16::try_from(content_width + 4)
            .unwrap_or(u16::MAX)
            .min(area.width);
        // One cell of margin each side inside the border.
        let room = width.saturating_sub(4).max(1);
        // Lines wrap rather than clip: the consequence is never cut.
        let text: Vec<String> = confirm
            .lines
            .iter()
            .flat_map(|line| {
                crate::tui::blocks::wrap_plain(line, usize::from(room))
                    .into_iter()
                    .map(|row| format!(" {}", row.trim()))
            })
            .collect();
        let rows = button_rows(&widths, room);
        // Border, lines, the hint, a blank row, the button rows.
        let height = u16::try_from(text.len() + rows.len() + 4)
            .unwrap_or(u16::MAX)
            .min(area.height);
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        modal_chrome::render_scrim(frame, area, &self.theme);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(
                format!(" {} ", confirm.title),
                self.theme.error,
            ))
            .border_style(self.theme.menu_border)
            .style(Style::default().bg(self.theme.menu_bg));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let buttons_height = u16::try_from(rows.len()).unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(
                text.into_iter()
                    .map(|line| Line::from(Span::styled(line, self.theme.slash_item)))
                    .chain([Line::from(Span::styled(
                        format!(" {HINT}"),
                        self.theme.slash_item_help,
                    ))])
                    .collect::<Vec<_>>(),
            ),
            Rect {
                height: inner.height.saturating_sub(buttons_height),
                ..inner
            },
        );
        let mut hits = vec![None; labels.len()];
        let first_row = inner.bottom().saturating_sub(buttons_height);
        for (offset, row) in rows.iter().enumerate() {
            let y = first_row.saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
            if y < inner.y {
                continue;
            }
            let row_width = row.iter().map(|index| widths[*index]).sum::<u16>()
                + BUTTON_GAP * u16::try_from(row.len() - 1).unwrap_or(0);
            // Right-aligned like a dialog's buttons; clipped by the inner area.
            let mut x = inner.right().saturating_sub(row_width + 1).max(inner.x);
            for index in row {
                let rect = Rect::new(x, y, widths[*index], 1).intersection(inner);
                x = x.saturating_add(widths[*index] + BUTTON_GAP);
                if !rect.is_empty() {
                    hits[*index] = Some(rect);
                }
            }
        }
        let focus = confirm.focus;
        for (index, hit) in hits.iter().enumerate() {
            let Some(rect) = *hit else {
                continue;
            };
            let style = if index == focus {
                self.theme.slash_selected
            } else if self.chrome_hovered(rect) {
                self.theme.title
            } else {
                self.theme.slash_item
            };
            frame.render_widget(Paragraph::new(Span::styled(labels[index], style)), rect);
        }
        if let Some(confirm) = self.destructive_confirm.as_mut() {
            confirm.popup = Some(popup);
            confirm.hits = hits;
        }
    }
}
