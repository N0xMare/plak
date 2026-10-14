//! The agent transcript as the Activity > Agents body (spec 02 §5.7):
//! breadcrumb, scrollable transcript, the agent bar (the Agents list's own
//! row, unboxed), and the frame footer's keys. There is no composer, and
//! nothing here looks like one. The transcript is shown in exactly this place.

use std::collections::BTreeMap;

use super::render_overlays::agent_row_line;
use super::terminal::copy_text_osc52;
use super::{
    App, Constraint, Frame, KeyCode, KeyEvent, Layout, Line, MouseEvent, MouseEventKind, Paragraph,
    Rect, Span, UserAction, event,
};
use crate::tui::agent_list::{self, AgentScope};
use crate::tui::tree_navigator::{
    AgentRowClock, RECOVERY_REMEDY, acknowledge_intent, agent_row, agent_view_label,
};

impl App {
    /// The Agents tab's body while an agent's transcript is open: breadcrumb,
    /// transcript, the recovery remedy when one is due, and the agent bar.
    /// The frame's footer carries the keys.
    pub(super) fn draw_agent_activity_body(&mut self, frame: &mut Frame<'_>, body: Rect) {
        self.agent_bar_hit = None;
        if body.height == 0 || body.width == 0 {
            return;
        }
        let recovery = self.viewed_agent_needs_recovery();
        let crumb = u16::from(body.height >= 2);
        let bar = u16::from(body.height >= 3);
        let remedy = u16::from(recovery && body.height >= 4);
        let chunks = Layout::vertical([
            Constraint::Length(crumb),
            Constraint::Fill(1),
            Constraint::Length(remedy),
            Constraint::Length(bar),
        ])
        .split(body);
        if crumb > 0 {
            self.draw_agent_breadcrumb(frame, chunks[0]);
        }
        self.draw_scrollback(frame, chunks[1]);
        if remedy > 0 {
            self.draw_agent_remedy(frame, chunks[2]);
        }
        if bar > 0 {
            self.draw_agent_bar(frame, chunks[3]);
        }
    }

    /// Footer hints while the agent transcript is the Agents body.
    pub(super) fn agent_activity_hints(&self) -> Vec<super::Hint> {
        let mut hints = Vec::new();
        if let Some(need) = self.root_needs_operator() {
            hints.push(super::Hint::alert("Esc", need));
        }
        hints.push(super::Hint::new("Esc", "back"));
        hints.push(super::Hint::new("↑↓ PgUp/PgDn", "scroll"));
        hints.push(super::Hint::new("Enter", "expand"));
        hints.push(super::Hint::new("[ ]", "prev/next agent"));
        hints.push(super::Hint::new("i", "interrupt"));
        hints.push(super::Hint::new("c", "close"));
        hints.push(super::Hint::new("y", "copy"));
        if self.viewed_agent_acknowledgement().is_some() {
            hints.push(super::Hint::new("x", "acknowledge"));
        }
        hints
    }

    /// Clicks on `i` / `c` / `y` / `x` in the centered Activity footer.
    pub(super) fn record_agent_footer_hits(&mut self, footer: Rect, hints: &[super::Hint]) {
        let labels: &[(&str, KeyCode)] = &[
            ("i interrupt", KeyCode::Char('i')),
            ("c close", KeyCode::Char('c')),
            ("y copy", KeyCode::Char('y')),
            ("x acknowledge", KeyCode::Char('x')),
        ];
        for (x, y, text) in super::modal_chrome::footer_lines(footer, hints) {
            self.key_label_hits
                .extend(super::destructive_confirm::key_label_hits(
                    &text, x, y, labels,
                ));
        }
    }

    fn draw_agent_breadcrumb(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(view) = self.agent_view.as_ref() else {
            return;
        };
        let crumb = Line::from(vec![
            Span::styled(" root › ", self.theme.chrome_dim),
            Span::styled(view.label.clone(), self.theme.title),
        ]);
        frame.render_widget(Paragraph::new(crumb), area);
    }

    /// Forgets every hit rectangle of the composer and its shelves, for a
    /// frame that does not draw them.
    fn clear_composer_hits(&mut self) {
        self.composer_inner = None;
        self.queue_area = None;
        self.queue_button_hits.clear();
        self.prompt_history_hits.clear();
        self.capacity_recovery_action = None;
        self.status_agents_hit = None;
        self.status_processes_hit = None;
        self.jump_to_live_hit = None;
        self.composer_chrome.profile_hit = None;
    }

    /// A frame under full-screen Activity draws neither main view.
    pub(super) fn clear_main_view_hits(&mut self) {
        self.clear_composer_hits();
        self.agent_bar_hit = None;
    }

    /// One unboxed row: the agent as the Activity > Agents list paints it.
    fn draw_agent_bar(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.agent_bar_hit = None;
        if area.height == 0 || area.width < 3 {
            return;
        }
        self.agent_bar_hit = Some(area);
        let Some(view) = self.agent_view.as_ref() else {
            return;
        };
        let node = self.agent_tree.as_ref().and_then(|tree| {
            tree.nodes
                .iter()
                .find(|node| node.agent_id == view.agent_id)
        });
        let inner = Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1);
        let line = match node {
            Some(node) => {
                let spinner = (!self.render_prefs.reduced_motion).then(|| self.counter_spinner(0));
                let processes = self.running_processes_by_owner();
                let clock = AgentRowClock {
                    now_unix: self.agents_now(),
                    spinner,
                    processes: &processes,
                    profile: self.composer_chrome.profile.as_ref(),
                };
                let cells = agent_row(
                    node,
                    "",
                    agent_list::attention(node).is_some(),
                    &clock,
                    usize::from(inner.width),
                );
                agent_row_line(&self.theme, &cells, false, false)
            }
            None => Line::from(Span::styled(view.label.clone(), self.theme.slash_item_help)),
        };
        frame.render_widget(Paragraph::new(line), inner);
    }

    fn viewed_agent_needs_recovery(&self) -> bool {
        self.viewed_agent_id()
            .and_then(|agent_id| {
                self.agent_tree
                    .as_ref()?
                    .nodes
                    .iter()
                    .find(|node| node.agent_id == agent_id)
            })
            .and_then(agent_list::attention)
            == Some(agent_list::Attention::RecoveryRequired)
    }

    /// The recovery remedy, in the body above the agent bar.
    fn draw_agent_remedy(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.height == 0 || area.width < 3 {
            return;
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                RECOVERY_REMEDY,
                self.theme.warning,
            ))),
            Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1),
        );
    }

    /// What the root waits on that only the root view can answer.
    pub(super) fn root_needs_operator(&self) -> Option<&'static str> {
        if self.open_approval.is_some() {
            Some("root needs approval")
        } else if self.capacity_recovery.is_some() {
            Some("root needs a context decision")
        } else {
            None
        }
    }

    /// Keys while the agent view is on screen and no modal owns input.
    /// Printable keys do nothing: the agent view has no composer. `Ctrl`
    /// chords are routed before this, with the promptless set.
    pub(super) fn handle_agent_view_key(&mut self, key: KeyEvent) -> Option<UserAction> {
        match key.code {
            // Above the stop confirmation: going back never stops the root
            // turn or pauses auto-continue.
            KeyCode::Esc => return self.agent_view_back(),
            KeyCode::Char(key @ ('i' | 'c')) => {
                let agent_id = self.viewed_agent_id()?.to_owned();
                return self.confirm_destructive(if key == 'i' {
                    UserAction::InterruptTopologyNode { agent_id }
                } else {
                    UserAction::CloseTopologyNode { agent_id }
                });
            }
            KeyCode::Char('x') => return self.viewed_agent_acknowledgement(),
            KeyCode::Char('y') => {
                if let Some(agent_id) = self.viewed_agent_id() {
                    copy_text_osc52(agent_id);
                }
            }
            KeyCode::Char('[') => self.show_neighbour_agent(-1),
            KeyCode::Char(']') => self.show_neighbour_agent(1),
            KeyCode::Up => self.scroll_agent_view(1, true),
            KeyCode::Down => self.scroll_agent_view(1, false),
            KeyCode::PageUp => self.scroll_agent_view(10, true),
            KeyCode::PageDown => self.scroll_agent_view(10, false),
            KeyCode::Enter => self.toggle_last_tool_expand(),
            _ => {}
        }
        None
    }

    /// `Esc` (and `Ctrl+C`) in the agent transcript: back to the Agents list
    /// with this agent selected. The close glyph and `Ctrl+T` leave Activity.
    pub(super) fn agent_view_back(&mut self) -> Option<UserAction> {
        self.esc_clear_arm = None;
        self.mark_dirty();
        if self.quit_arm.take().is_some() {
            return None;
        }
        let agent_id = self.viewed_agent_id().map(str::to_owned);
        self.close_agent_view();
        agent_id.and_then(|agent_id| self.open_agents_at(&agent_id))
    }

    /// The acknowledgement `x` sends for the agent on screen, when it needs
    /// attention not yet acknowledged.
    fn viewed_agent_acknowledgement(&self) -> Option<UserAction> {
        let agent_id = self.viewed_agent_id()?;
        let tree = self.agent_tree.as_ref()?;
        let node = tree.nodes.iter().find(|node| node.agent_id == agent_id)?;
        acknowledge_intent(&tree.tree_id, node, &self.agent_acks.attempts)
    }

    fn scroll_agent_view(&mut self, lines: usize, up: bool) {
        self.pane.scroll_from_bottom = if up {
            self.pane.scroll_from_bottom.saturating_add(lines)
        } else {
            self.pane.scroll_from_bottom.saturating_sub(lines)
        };
        self.begin_transcript_camera();
        self.mark_dirty();
    }

    /// `[`/`]`: the previous or next agent in the Agents list's visible order
    /// (the root is never one); the loop's view sync closes and opens.
    fn show_neighbour_agent(&mut self, step: isize) {
        let (Some(view), Some(tree)) = (self.agent_view.as_ref(), self.agent_tree.as_ref()) else {
            return;
        };
        let projection = agent_list::project(
            &tree.nodes,
            &self.agent_acks.attempts,
            self.agents_now(),
            AgentScope::Live,
        );
        let agents: Vec<&str> = projection
            .rows
            .iter()
            .map(|row| row.id.as_str())
            .filter(|id| {
                tree.nodes
                    .iter()
                    .any(|node| node.agent_id == *id && node.parent_agent_id.is_some())
            })
            .collect();
        let Some(at) = agents.iter().position(|id| *id == view.agent_id) else {
            return;
        };
        let Some(next) = at
            .checked_add_signed(step)
            .and_then(|next| agents.get(next))
        else {
            return;
        };
        let Some(node) = tree.nodes.iter().find(|node| node.agent_id == *next) else {
            return;
        };
        let (agent_id, label) = (node.agent_id.clone(), agent_view_label(node));
        self.open_agent_view(agent_id, label);
    }

    /// A left click on the agent bar.
    pub(super) fn is_agent_bar_click(&self, mouse: MouseEvent) -> bool {
        let point = (mouse.column, mouse.row).into();
        matches!(mouse.kind, MouseEventKind::Down(event::MouseButton::Left))
            && self.viewed_agent_id().is_some()
            && self.agent_bar_hit.is_some_and(|hit| hit.contains(point))
    }

    /// Closes the agent transcript and leaves the Agents list focused on it,
    /// without changing the Activity tab. Leaving Agents, and a click on the
    /// Agents tab, both use this so coming back shows the list.
    pub(super) fn close_agent_transcript_onto_list(&mut self) {
        let Some(agent_id) = self.viewed_agent_id().map(str::to_owned) else {
            return;
        };
        self.close_agent_view();
        self.focus_agents_list_on(&agent_id);
    }

    /// Focuses the Agents list on `agent_id`, installing the snapshot the list
    /// already holds when the navigator has not applied one.
    fn focus_agents_list_on(&mut self, agent_id: &str) {
        if self.tree_navigator.is_none() {
            self.tree_navigator = Some(crate::tui::TreeNavigator::awaiting());
        }
        let Some(nav) = self.tree_navigator.as_mut() else {
            return;
        };
        if nav.snapshot.is_none()
            && let Some(tree) = self.agent_tree.clone()
        {
            let tree_id = tree.tree_id.clone();
            nav.apply_snapshot(tree);
            nav.set_acknowledged(&tree_id, &self.agent_acks.attempts);
        }
        nav.focus_on(agent_id);
    }

    /// Opens Activity > Agents with `agent_id` selected.
    fn open_agents_at(&mut self, agent_id: &str) -> Option<UserAction> {
        let action = self.open_tree_navigator();
        self.focus_agents_list_on(agent_id);
        action
    }

    /// Running background processes per owning agent, for Agents rows.
    pub(super) fn running_processes_by_owner(&self) -> BTreeMap<String, usize> {
        let mut owned = BTreeMap::new();
        for process in self
            .processes
            .iter()
            .filter(|process| process.status.is_live())
        {
            *owned.entry(process.owner.clone()).or_default() += 1;
        }
        owned
    }
}
