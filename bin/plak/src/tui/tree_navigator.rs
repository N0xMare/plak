//! Inert agent-tree navigator DTOs and local overlay state.
//!
//! The TUI never opens an Agent, store, or Plakous session. Host events replace
//! exact-ID snapshots; this module only selects and emits typed intents. Stale revisions are the host's to resnapshot. Which
//! agents the Agents tab lists is [`crate::tui::agent_list`]'s projection.

use std::collections::{BTreeMap, BTreeSet};

pub(super) use crate::tui::agent_list;
use crate::tui::agent_list::{AgentProjection, AgentScope, RowCells, truncate_display};
pub(super) use crate::tui::blocks::format_duration_secs;
#[cfg(test)]
pub(super) use crate::tui::status_glyph;
use crate::tui::{AgentAcknowledgement, CanvasNode, CanvasTree};

mod rows;
pub use rows::{AgentRowClock, agent_row, agent_view_label, attempt_label};
use rows::{
    ROOT_TURN_OVERWRITABLE, agent_name, blank_as_unknown, dormancy_inspector, elapsed_label,
    header_line, model_label, process_count_label, row_status, turns_label, until_label, work_line,
};
pub(crate) use rows::{acknowledge_intent, agent_role, recover_intent, short_id, wrap_footer_hint};

/// Rows the docked inspector takes when the overlay is tall enough for it.
const INSPECTOR_ROWS: u16 = 6;
/// Shortest overlay that still carries an inspector.
const INSPECTOR_MIN_HEIGHT: u16 = 12;

/// What the Agents footer names besides the keys.
#[derive(Clone, Copy, Debug)]
pub struct FooterView<'a> {
    pub scope: AgentScope,
    pub hidden: usize,
    pub history: bool,
    pub failures_only: bool,
    pub query: &'a str,
    /// `u recover` is named only when the selection is eligible.
    pub offer_recover: bool,
    /// `r resume` is named only while the selection holds a continuation
    /// after a lost host.
    pub offer_resume: bool,
}

/// Footer for the Agents tab: why agents are missing, then the keys.
#[must_use]
pub fn agents_footer_hint(view: FooterView<'_>) -> String {
    if view.history {
        let filter = if view.failures_only {
            "failures only · "
        } else {
            ""
        };
        let typed = view.query.trim();
        let query = if typed.is_empty() {
            String::new()
        } else {
            format!("/ {typed} · ")
        };
        let recover = if view.offer_recover {
            " · u recover"
        } else {
            ""
        };
        return format!(
            "{filter}{query}t tree · / search · ! failures · ↑/↓ select · Enter transcript · x acknowledge{recover} · R refresh · Esc back"
        );
    }
    let shown = match view.scope {
        AgentScope::Live if view.hidden == 0 => "active + last 5m · f all".to_owned(),
        AgentScope::Live => {
            format!("active + last 5m · {} older hidden · f all", view.hidden)
        }
        AgentScope::All => "all agents · f live".to_owned(),
    };
    let recover = if view.offer_recover {
        " · u recover"
    } else {
        ""
    };
    let resume = if view.offer_resume {
        " · r resume"
    } else {
        ""
    };
    format!(
        "{shown} · ↑/↓ select · Enter transcript · x acknowledge · i interrupt · c close · s stop all · y copy{recover}{resume} · R refresh · t history · Esc back"
    )
}

/// The Agents footer at `width` in `max_rows`: the long form when it fits,
/// otherwise every key in short form, so a narrow list still names (and a
/// click still presses) `i`, `c` and `s` rather than clipping them away.
#[must_use]
pub fn fitted_agents_footer(view: FooterView<'_>, width: u16, max_rows: usize) -> String {
    let long = agents_footer_hint(view);
    let budget = usize::from(width).saturating_sub(1).max(1);
    if rows::wrap_footer_segments(long.trim(), budget).len() <= max_rows.max(1) {
        return long;
    }
    if view.history {
        let filter = if view.failures_only {
            "failures · "
        } else {
            ""
        };
        let recover = if view.offer_recover { " · u" } else { "" };
        return format!("{filter}t · / · ! · ↑/↓ · Enter · x{recover} · R · Esc");
    }
    let shown = match view.scope {
        AgentScope::Live => "f all",
        AgentScope::All => "f live",
    };
    let recover = if view.offer_recover { " · u" } else { "" };
    let resume = if view.offer_resume { " · r" } else { "" };
    format!("↑/↓ · Enter · x · i · c · s · y{recover}{resume} · R · t · {shown} · Esc")
}

/// Rows the list footer may take before it gives up and clips.
///
/// The footer wraps (§5.5) but never eats the list: on a short overlay it
/// keeps to two rows and clips the last with a visible `…`.
#[must_use]
pub(crate) const fn footer_max_rows(height: u16) -> usize {
    if height >= 12 { 3 } else { 2 }
}

fn footer_row_count(height: u16, footer: &str, width: u16) -> u16 {
    if height < 4 {
        return 0;
    }
    u16::try_from(wrap_footer_hint(footer, width, footer_max_rows(height)).len())
        .unwrap_or(1)
        .min(height.saturating_sub(3))
        .max(1)
}

/// Renders a host state token as prose: `tree_terminal` is a wire value, not
/// something an operator should have to read off the chrome (§5.10).
#[must_use]
pub(crate) fn humanize_state(state: &str) -> String {
    state.replace('_', " ")
}

/// What a recovery-required agent needs from this list.
pub const RECOVERY_REMEDY: &str = "u recovers";

/// The inspector's remedy for a child held after a lost host.
pub const LOSS_HELD_REMEDY: &str = "held after the lost host · r resumes";

/// Result of one overlay key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TopologyNavigatorOutcome {
    /// Key consumed; overlay stays open.
    Consumed,
    /// Operator dismissed the overlay.
    Dismiss,
    /// Forward a typed intent to the host.
    Action(crate::tui::UserAction),
    /// A destructive intent: the TUI sends it only from its confirmation
    /// pop-up.
    Confirm(crate::tui::UserAction),
    /// Enter on the root row: close Activity and show the root transcript.
    ShowRoot,
    /// Enter on an agent row: show its transcript as the Agents tab's body.
    ShowAgent {
        /// Exact agent id from the tree.
        agent_id: String,
        /// `#<id> · <description>`, for the breadcrumb.
        label: String,
    },
}

/// Local overlay state. Snapshots, pages and acknowledgements are
/// host-authoritative; the clock is the one the last frame painted with, so
/// keys act on the list the operator is looking at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeNavigator {
    /// `None` while awaiting the first host snapshot.
    pub snapshot: Option<CanvasTree>,
    /// Selected agent id. It survives rows appearing and disappearing; while
    /// it is not listed, the nearest listed ancestor stands in for it.
    selected: Option<String>,
    /// Live working set or every agent. The host never drops terminal nodes;
    /// only this projection hides them.
    pub scope: AgentScope,
    /// Acknowledged occurrences of this snapshot's tree.
    acknowledged: BTreeSet<AgentAcknowledgement>,
    /// Unix seconds the projection is evaluated at.
    now_unix: u64,
    /// Next unix second a shown recent row dims or leaves. `None` when nothing
    /// shown will age. Armed from the frame's projection, not from a poll.
    pub(crate) age_wake_at: Option<u64>,
    /// First list row in view.
    list_scroll: usize,
    /// Sticky descent: the child last descended into, per parent id.
    remembered_child: BTreeMap<String, String>,
    /// Flat history over this snapshot, instead of the live/all tree.
    history: bool,
    /// Tree scope to restore when `t` leaves history.
    saved_scope: AgentScope,
    /// In-memory history query. Letters edit it only while `searching`.
    search: String,
    searching: bool,
    /// History shows only failed, blocked, unfinished, and recovery-required rows.
    failures_only: bool,
    /// Operator zone for Today/Earlier. UTC until the frame sets it.
    zone: &'static jiff::tz::TimeZone,
    /// Selectable rows the last frame could show, for paging.
    list_rows: usize,
    /// Profile the last frame used to name roles in the history query.
    last_profile: Option<crate::tui::ProfileRow>,
    /// History lines for one snapshot, query, failures flag, and local day.
    history_cache: Option<crate::tui::agent_history::HistoryCache>,
}

/// One painted list row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentListRow {
    pub id: String,
    pub cells: RowCells,
    pub dim: bool,
    pub selected: bool,
    /// False for a history header or an empty-state notice.
    pub selectable: bool,
}

/// One painted Agents body: header counts, rows in view, inspector, footer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentListFrame {
    pub header: String,
    pub rows: Vec<AgentListRow>,
    pub inspector: Vec<String>,
    pub footer: String,
}

impl TreeNavigator {
    /// Open while the host snapshot is in flight.
    #[must_use]
    pub fn awaiting() -> Self {
        Self {
            snapshot: None,
            selected: None,
            scope: AgentScope::Live,
            acknowledged: BTreeSet::new(),
            now_unix: 0,
            age_wake_at: None,
            list_scroll: 0,
            remembered_child: BTreeMap::new(),
            history: false,
            saved_scope: AgentScope::Live,
            search: String::new(),
            searching: false,
            failures_only: false,
            zone: &crate::tui::UTC_ZONE,
            list_rows: 1,
            last_profile: None,
            history_cache: None,
        }
    }

    /// The operator's zone for history's local calendar day.
    pub fn set_zone(&mut self, zone: &'static jiff::tz::TimeZone) {
        self.zone = zone;
    }

    /// History search owns typed input.
    #[must_use]
    pub const fn is_searching(&self) -> bool {
        self.searching
    }

    /// Append one accepted character to the history query.
    pub fn push_search(&mut self, ch: char) {
        if !self.searching || !crate::tui::text_input::is_accepted_single_line_char(ch) {
            return;
        }
        if self.search.chars().count() >= crate::tui::agent_history::SEARCH_LIMIT {
            return;
        }
        self.search.push(ch);
    }

    /// Delete the last history-query character.
    pub fn pop_search(&mut self) {
        if self.searching {
            self.search.pop();
        }
    }

    /// Single-line paste into the history query, bounded like other filters.
    pub fn paste_search(&mut self, text: &str) {
        if !self.searching {
            return;
        }
        let clean = crate::tui::text_input::sanitize_single_line(text);
        for ch in clean.chars() {
            self.push_search(ch);
        }
    }

    /// Open over an exact host snapshot.
    #[must_use]
    #[cfg(test)]
    pub fn from_snapshot(snapshot: CanvasTree) -> Self {
        let mut navigator = Self::awaiting();
        navigator.apply_snapshot(snapshot);
        navigator
    }

    /// Sets the clock the projection is evaluated at.
    pub const fn set_now(&mut self, now_unix: u64) {
        self.now_unix = now_unix;
    }

    /// Replaces the acknowledged occurrences of `tree_id`. A set for another
    /// tree is not this list's and is ignored.
    pub fn set_acknowledged(&mut self, tree_id: &str, attempts: &BTreeSet<AgentAcknowledgement>) {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|snap| snap.tree_id == tree_id)
        {
            attempts.clone_into(&mut self.acknowledged);
        }
    }

    /// Paints the root row from the session's turn phase. The tree never
    /// tracks a root turn (the root has no attempt), so the durable status is
    /// `running` forever; the header knows better. Returns whether the label
    /// changed.
    ///
    /// The turn phase only knows "a turn is bound" or "no turn is bound", so it
    /// may only repaint the labels that mean exactly that
    /// ([`ROOT_TURN_OVERWRITABLE`]). A root that is `sealing`, `adopting`,
    /// `blocked`, `paused`, or failed carries a durable fact the phase cannot
    /// see, and painting `running`/`idle` over it would erase the observation
    /// the operator needs to see.
    pub fn project_root_turn(&mut self, active: bool) -> bool {
        let label = if active { "running" } else { "idle" };
        let Some(root) = self.snapshot.as_mut().and_then(|snap| {
            snap.nodes
                .iter_mut()
                .find(|node| node.parent_agent_id.is_none())
        }) else {
            return false;
        };
        if root.finished
            || root.status == label
            || !ROOT_TURN_OVERWRITABLE.contains(&root.status.as_str())
        {
            return false;
        }
        label.clone_into(&mut root.status);
        true
    }

    /// Replace the tree snapshot. Selection is an agent id, so it needs no
    /// remapping.
    ///
    /// Older revisions and unchanged equal revisions are dropped (§5.10);
    /// progress-only equal revisions replace observations. The host dedups
    /// by revision too, but an explicit `R` refresh re-pushes the same one.
    pub fn apply_snapshot(&mut self, snapshot: CanvasTree) {
        if self.snapshot.as_ref().is_some_and(|previous| {
            snapshot.revision < previous.revision
                || (snapshot.revision == previous.revision
                    && (!snapshot
                        .nodes
                        .iter()
                        .map(|node| &node.agent_id)
                        .eq(previous.nodes.iter().map(|node| &node.agent_id))
                        || snapshot
                            .nodes
                            .iter()
                            .map(|node| (&node.agent_id, &node.live_progress))
                            .eq(previous
                                .nodes
                                .iter()
                                .map(|node| (&node.agent_id, &node.live_progress)))))
        }) {
            return;
        }
        if self
            .snapshot
            .as_ref()
            .is_some_and(|previous| previous.tree_id != snapshot.tree_id)
        {
            self.acknowledged.clear();
            self.reset_history();
        }
        self.snapshot = Some(snapshot);
        self.history_cache = None;
    }

    fn reset_history(&mut self) {
        self.history = false;
        self.saved_scope = AgentScope::Live;
        self.scope = AgentScope::Live;
        self.search.clear();
        self.searching = false;
        self.failures_only = false;
    }

    fn footer_view(&self, hidden: usize) -> FooterView<'_> {
        FooterView {
            scope: self.scope,
            hidden,
            history: self.history,
            failures_only: self.failures_only,
            query: &self.search,
            offer_recover: self.offer_recover(),
            offer_resume: !self.history && self.resume_action().is_some(),
        }
    }

    /// The list at the navigator's clock.
    #[must_use]
    pub fn projection(&self) -> AgentProjection {
        self.snapshot
            .as_ref()
            .map_or_else(AgentProjection::default, |snap| {
                agent_list::project(&snap.nodes, &self.acknowledged, self.now_unix, self.scope)
            })
    }

    /// Arms [`Self::age_wake_at`] from one projection of the current clock.
    pub fn refresh_age_wake(&mut self) {
        let projection = self.projection();
        let now = self.now_unix;
        let wake = self
            .snapshot
            .as_ref()
            .and_then(|snap| agent_list::next_age_boundary(&snap.nodes, &projection, now));
        self.age_wake_at = wake;
    }

    fn node(&self, agent_id: &str) -> Option<&CanvasNode> {
        self.snapshot
            .as_ref()?
            .nodes
            .iter()
            .find(|node| node.agent_id == agent_id)
    }

    /// The listed row selection resolves to: the selected agent, else its
    /// nearest listed ancestor, else the first row.
    fn resolve_selection(&self, projection: &AgentProjection) -> Option<String> {
        let mut cursor = self.selected.as_deref();
        let mut seen = BTreeSet::new();
        while let Some(id) = cursor {
            if !seen.insert(id) {
                break;
            }
            if projection.position(id).is_some() {
                return Some(id.to_owned());
            }
            cursor = self
                .node(id)
                .and_then(|node| node.parent_agent_id.as_deref());
        }
        projection.rows.first().map(|row| row.id.clone())
    }

    /// Selected node, if the snapshot has rows.
    ///
    /// History does not substitute another agent when the selection is hidden
    /// by the query or the failures filter.
    #[must_use]
    pub fn selected_node(&self) -> Option<&CanvasNode> {
        if self.history {
            let id = self.selected.as_deref()?;
            let node = self.node(id)?;
            return crate::tui::agent_history::history_member(node).then_some(node);
        }
        let id = self.resolve_selection(&self.projection())?;
        self.node(&id)
    }

    /// Selected agent id.
    #[must_use]
    pub fn selected_agent_id(&self) -> Option<&str> {
        self.selected_node().map(|node| node.agent_id.as_str())
    }

    /// Current snapshot revision.
    #[must_use]
    #[cfg(test)]
    pub fn revision(&self) -> Option<u64> {
        self.snapshot.as_ref().map(|snap| snap.revision)
    }

    /// Move the selection one listed row forward or backward.
    pub fn move_selection(&mut self, delta: i8) {
        if self.history {
            self.move_by(isize::from(delta));
            return;
        }
        let projection = self.projection();
        if projection.rows.is_empty() {
            return;
        }
        let current = self
            .resolve_selection(&projection)
            .and_then(|id| projection.position(&id))
            .unwrap_or(0);
        let next = if delta < 0 {
            current.saturating_sub(1)
        } else {
            current.saturating_add(1).min(projection.rows.len() - 1)
        };
        let id = projection.rows[next].id.clone();
        self.focus_on(&id);
    }

    /// `u` is offered only for a visible eligible selection.
    fn offer_recover(&self) -> bool {
        let Some(tree) = self.snapshot.as_ref() else {
            return false;
        };
        let Some(node) = self.actionable_node() else {
            return false;
        };
        plakous_host_protocol::topology::operator_child_recover_eligible(
            tree.state.as_str(),
            node.status.as_str(),
            node.parent_agent_id.is_none(),
            node.attempt_id.is_some(),
        )
    }

    /// A history selection the filter currently shows. The stored id stays
    /// for restoration; it is not an action target while hidden.
    fn history_selection_visible(&self) -> bool {
        let Some(id) = self.selected.as_deref() else {
            return false;
        };
        self.history_agent_ids().iter().any(|row| row == id)
    }

    fn actionable_node(&self) -> Option<&CanvasNode> {
        if self.history && !self.history_selection_visible() {
            return None;
        }
        self.selected_node()
    }

    /// `Left`: select the listed parent; a root stays selected.
    pub fn select_parent(&mut self) {
        let projection = self.projection();
        let Some(id) = self.resolve_selection(&projection) else {
            return;
        };
        if let Some(parent) = projection.layout.parent(&id).map(str::to_owned) {
            self.remembered_child.insert(parent.clone(), id);
            self.focus_on(&parent);
        }
    }

    /// `Right`: select a listed child, preferring the one last descended into.
    pub fn select_child(&mut self) {
        let projection = self.projection();
        let Some(id) = self.resolve_selection(&projection) else {
            return;
        };
        let children = projection.layout.children(&id);
        let target = self
            .remembered_child
            .get(&id)
            .filter(|child| children.contains(child))
            .or_else(|| children.first())
            .cloned();
        if let Some(target) = target {
            self.remembered_child.insert(id, target.clone());
            self.focus_on(&target);
        }
    }

    /// Jump to the first root of the forest.
    pub fn select_root(&mut self) {
        if let Some(root) = self.projection().layout.roots().first().cloned() {
            self.focus_on(&root);
        }
    }

    /// Toggle the live/all scope; selection keeps its agent.
    pub fn toggle_scope(&mut self) {
        self.scope = self.scope.toggled();
    }

    fn toggle_history(&mut self) {
        if self.history {
            self.history = false;
            self.searching = false;
            self.scope = self.saved_scope;
        } else {
            self.saved_scope = self.scope;
            self.history = true;
        }
    }

    fn page(&self) -> isize {
        isize::try_from(self.list_rows.max(1)).unwrap_or(1)
    }

    fn clear_or_dismiss(&mut self) -> TopologyNavigatorOutcome {
        let filtered = self.failures_only || !self.search.trim().is_empty();
        let hidden = self
            .selected
            .as_deref()
            .is_some_and(|id| !self.history_agent_ids().iter().any(|row| row == id));
        if self.history && filtered && (self.history_agent_ids().is_empty() || hidden) {
            self.search.clear();
            self.failures_only = false;
            self.history_cache = None;
            return TopologyNavigatorOutcome::Consumed;
        }
        TopologyNavigatorOutcome::Dismiss
    }

    fn navigate(&mut self, key: TopologyNavigatorKey) -> TopologyNavigatorOutcome {
        match key {
            TopologyNavigatorKey::Up => self.move_selection(-1),
            TopologyNavigatorKey::Down => self.move_selection(1),
            TopologyNavigatorKey::PageUp => self.move_by(-self.page()),
            TopologyNavigatorKey::PageDown => self.move_by(self.page()),
            TopologyNavigatorKey::Home => self.move_by(isize::MIN / 4),
            TopologyNavigatorKey::End => self.move_by(isize::MAX / 4),
            _ => {}
        }
        TopologyNavigatorOutcome::Consumed
    }

    fn history_selectable_ids(&self) -> Vec<String> {
        self.history_agent_ids()
    }

    fn history_lines_now(
        &mut self,
        profile: Option<&crate::tui::ProfileRow>,
    ) -> Vec<crate::tui::agent_history::HistoryLine> {
        let day = crate::tui::resume_pane::calendar_date(self.now_unix, self.zone);
        if let Some(lines) = self.history_cache.as_ref().and_then(|cache| {
            let snapshot = self.snapshot.as_ref()?;
            (cache.revision == snapshot.revision
                && cache.tree_id == snapshot.tree_id
                && cache.query == self.search
                && cache.failures_only == self.failures_only
                && cache.day == day)
                .then(|| cache.lines.clone())
        }) {
            return lines;
        }
        let Some(snapshot) = self.snapshot.as_ref() else {
            return Vec::new();
        };
        let lines = crate::tui::agent_history::history_lines(
            &snapshot.nodes,
            self.now_unix,
            self.zone,
            &self.search,
            self.failures_only,
            profile,
        );
        self.history_cache = Some(crate::tui::agent_history::HistoryCache {
            revision: snapshot.revision,
            tree_id: snapshot.tree_id.clone(),
            query: self.search.clone(),
            failures_only: self.failures_only,
            day,
            lines: lines.clone(),
        });
        lines
    }

    fn history_agent_ids(&self) -> Vec<String> {
        let day = crate::tui::resume_pane::calendar_date(self.now_unix, self.zone);
        let lines = if let Some(cache) = self.history_cache.as_ref().filter(|cache| {
            self.snapshot.as_ref().is_some_and(|snapshot| {
                cache.revision == snapshot.revision
                    && cache.tree_id == snapshot.tree_id
                    && cache.query == self.search
                    && cache.failures_only == self.failures_only
                    && cache.day == day
            })
        }) {
            cache.lines.clone()
        } else {
            let Some(snapshot) = &self.snapshot else {
                return Vec::new();
            };
            crate::tui::agent_history::history_lines(
                &snapshot.nodes,
                self.now_unix,
                self.zone,
                &self.search,
                self.failures_only,
                self.last_profile.as_ref(),
            )
        };
        lines
            .into_iter()
            .filter_map(|line| match line {
                crate::tui::agent_history::HistoryLine::Agent(id) => Some(id),
                _ => None,
            })
            .collect()
    }

    fn move_by(&mut self, delta: isize) {
        if self.history {
            let ids = self.history_selectable_ids();
            if ids.is_empty() {
                return;
            }
            let current = self
                .selected
                .as_ref()
                .and_then(|id| ids.iter().position(|row| row == id));
            let next = match current {
                Some(index) => {
                    let last = isize::try_from(ids.len() - 1).unwrap_or(0);
                    let at = isize::try_from(index).unwrap_or(0).saturating_add(delta);
                    usize::try_from(at.clamp(0, last)).unwrap_or(0)
                }
                None if delta < 0 => ids.len() - 1,
                None => 0,
            };
            self.focus_on(&ids[next]);
            return;
        }
        let projection = self.projection();
        if projection.rows.is_empty() {
            return;
        }
        let current = self
            .resolve_selection(&projection)
            .and_then(|id| projection.position(&id))
            .unwrap_or(0);
        let last = isize::try_from(projection.rows.len() - 1).unwrap_or(0);
        let at = isize::try_from(current).unwrap_or(0).saturating_add(delta);
        let next = usize::try_from(at.clamp(0, last)).unwrap_or(0);
        let id = projection.rows[next].id.clone();
        self.focus_on(&id);
    }

    pub(crate) fn focus_on(&mut self, agent_id: &str) {
        if self.node(agent_id).is_some() {
            self.selected = Some(agent_id.to_owned());
        }
    }
    /// Route one overlay key. The TUI maps Crossterm codes into this method.
    #[must_use]
    pub fn handle_key(&mut self, key: TopologyNavigatorKey) -> TopologyNavigatorOutcome {
        if self.snapshot.is_none() {
            return match key {
                TopologyNavigatorKey::Escape => TopologyNavigatorOutcome::Dismiss,
                _ => TopologyNavigatorOutcome::Consumed,
            };
        }
        self.handle_tree_key(key)
    }

    fn handle_search_key(&mut self, key: TopologyNavigatorKey) -> TopologyNavigatorOutcome {
        match key {
            TopologyNavigatorKey::Escape => {
                self.search.clear();
                self.searching = false;
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Enter => {
                self.searching = false;
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Backspace => {
                self.pop_search();
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Up
            | TopologyNavigatorKey::Down
            | TopologyNavigatorKey::PageUp
            | TopologyNavigatorKey::PageDown
            | TopologyNavigatorKey::Home
            | TopologyNavigatorKey::End => self.navigate(key),
            _ => TopologyNavigatorOutcome::Consumed,
        }
    }

    fn handle_motion(&mut self, key: TopologyNavigatorKey) -> TopologyNavigatorOutcome {
        match key {
            TopologyNavigatorKey::Up => self.move_selection(-1),
            TopologyNavigatorKey::Down => self.move_selection(1),
            TopologyNavigatorKey::PageUp => self.move_by(-self.page()),
            TopologyNavigatorKey::PageDown => self.move_by(self.page()),
            TopologyNavigatorKey::End => self.move_by(isize::MAX / 4),
            TopologyNavigatorKey::Left => {
                if !self.history {
                    self.select_parent();
                }
            }
            TopologyNavigatorKey::Right => {
                if !self.history {
                    self.select_child();
                }
            }
            TopologyNavigatorKey::Home => {
                if self.history {
                    self.move_by(isize::MIN / 4);
                } else {
                    self.select_root();
                }
            }
            _ => {}
        }
        TopologyNavigatorOutcome::Consumed
    }

    fn handle_tree_key(&mut self, key: TopologyNavigatorKey) -> TopologyNavigatorOutcome {
        if self.searching {
            return self.handle_search_key(key);
        }
        match key {
            TopologyNavigatorKey::Escape => self.clear_or_dismiss(),
            TopologyNavigatorKey::Up
            | TopologyNavigatorKey::Down
            | TopologyNavigatorKey::PageUp
            | TopologyNavigatorKey::PageDown
            | TopologyNavigatorKey::End
            | TopologyNavigatorKey::Left
            | TopologyNavigatorKey::Right
            | TopologyNavigatorKey::Home => self.handle_motion(key),
            TopologyNavigatorKey::Filter => {
                if self.history {
                    self.history = false;
                    self.scope = AgentScope::All;
                } else {
                    self.toggle_scope();
                }
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::History => {
                self.toggle_history();
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Search => {
                self.saved_scope = self.scope;
                self.history = true;
                self.searching = true;
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Failures => {
                if !self.history {
                    self.saved_scope = self.scope;
                    self.history = true;
                }
                self.failures_only = !self.failures_only;
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Backspace => TopologyNavigatorOutcome::Consumed,
            TopologyNavigatorKey::Acknowledge => self.acknowledge_action().map_or(
                TopologyNavigatorOutcome::Consumed,
                TopologyNavigatorOutcome::Action,
            ),
            TopologyNavigatorKey::Refresh => {
                TopologyNavigatorOutcome::Action(crate::tui::UserAction::RefreshTopologyTree)
            }
            TopologyNavigatorKey::Enter => match self.actionable_node() {
                None => TopologyNavigatorOutcome::Consumed,
                Some(node) if node.parent_agent_id.is_none() => TopologyNavigatorOutcome::ShowRoot,
                Some(node) => TopologyNavigatorOutcome::ShowAgent {
                    agent_id: node.agent_id.clone(),
                    label: agent_view_label(node),
                },
            },
            TopologyNavigatorKey::Interrupt => self.interrupt_action().map_or(
                TopologyNavigatorOutcome::Consumed,
                TopologyNavigatorOutcome::Confirm,
            ),
            TopologyNavigatorKey::Close => self.close_action().map_or(
                TopologyNavigatorOutcome::Consumed,
                TopologyNavigatorOutcome::Confirm,
            ),
            TopologyNavigatorKey::StopAll => {
                TopologyNavigatorOutcome::Confirm(crate::tui::UserAction::StopAllTopology)
            }
            TopologyNavigatorKey::Recover => self.recover_action().map_or(
                TopologyNavigatorOutcome::Consumed,
                TopologyNavigatorOutcome::Confirm,
            ),
            TopologyNavigatorKey::Resume => self.resume_action().map_or(
                TopologyNavigatorOutcome::Consumed,
                TopologyNavigatorOutcome::Action,
            ),
            TopologyNavigatorKey::Copy => TopologyNavigatorOutcome::Consumed,
        }
    }

    /// What `y` copies: the selected agent id.
    #[must_use]
    pub fn copy_text(&self) -> String {
        self.actionable_node()
            .map(|node| node.agent_id.clone())
            .unwrap_or_default()
    }

    /// Paint the Agents body at `width` x `height`. `spinner` is the frame for
    /// running and submitted rows; `None` under reduced motion, where they
    /// paint the static glyph. `processes` counts running processes per owner.
    pub fn frame(
        &mut self,
        width: u16,
        height: u16,
        spinner: Option<&str>,
        processes: &BTreeMap<String, usize>,
        profile: Option<&crate::tui::ProfileRow>,
    ) -> AgentListFrame {
        self.last_profile = profile.cloned();
        if self.history {
            return self.history_frame(width, height, spinner, processes, profile);
        }
        let projection = self.projection();
        let now = self.now_unix;
        let wake = self
            .snapshot
            .as_ref()
            .and_then(|snap| agent_list::next_age_boundary(&snap.nodes, &projection, now));
        self.age_wake_at = wake;
        let footer = fitted_agents_footer(
            self.footer_view(projection.hidden),
            width,
            footer_max_rows(height),
        );
        let header = header_line(&projection, self.snapshot.as_ref(), width);
        // The footer owns however many rows its own text needs at this width.
        let footer_rows = footer_row_count(height, &footer, width);
        let inspector_rows = if height >= INSPECTOR_MIN_HEIGHT {
            INSPECTOR_ROWS
        } else {
            0
        };
        let list_rows = usize::from(
            height
                .saturating_sub(1)
                .saturating_sub(footer_rows)
                .saturating_sub(inspector_rows),
        );
        self.list_rows = list_rows.max(1);
        let selected = self.resolve_selection(&projection);
        self.scroll_to(
            selected.as_deref().and_then(|id| projection.position(id)),
            projection.rows.len(),
            list_rows,
        );
        let rows = self.visible_rows(
            &projection,
            selected.as_deref(),
            list_rows,
            width,
            spinner,
            profile,
        );
        let inspector = if inspector_rows == 0 {
            Vec::new()
        } else {
            selected
                .as_deref()
                .map(|id| self.inspector_lines(id, &projection, width, processes))
                .unwrap_or_default()
        };
        AgentListFrame {
            header: truncate_display(&header, usize::from(width)),
            rows,
            inspector,
            footer,
        }
    }

    fn visible_rows(
        &self,
        projection: &AgentProjection,
        selected: Option<&str>,
        agent_slots: usize,
        width: u16,
        spinner: Option<&str>,
        profile: Option<&crate::tui::ProfileRow>,
    ) -> Vec<AgentListRow> {
        projection
            .rows
            .iter()
            .skip(self.list_scroll)
            .take(agent_slots)
            .filter_map(|row| {
                let node = self.node(&row.id)?;
                Some(AgentListRow {
                    id: row.id.clone(),
                    cells: agent_row(
                        node,
                        &row.prefix,
                        row.attention.is_some(),
                        &AgentRowClock {
                            now_unix: self.now_unix,
                            spinner,
                            profile,
                        },
                        usize::from(width),
                    ),
                    dim: row.dim,
                    selected: selected == Some(row.id.as_str()),
                    selectable: true,
                })
            })
            .collect()
    }

    fn history_frame(
        &mut self,
        width: u16,
        height: u16,
        spinner: Option<&str>,
        processes: &BTreeMap<String, usize>,
        profile: Option<&crate::tui::ProfileRow>,
    ) -> AgentListFrame {
        let lines = self.history_lines_now(profile);
        self.age_wake_at = crate::tui::agent_history::next_local_midnight(self.now_unix, self.zone);
        let footer = fitted_agents_footer(self.footer_view(0), width, footer_max_rows(height));
        let header = header_line(&self.projection(), self.snapshot.as_ref(), width);
        let header = truncate_display(&format!("History · {header}"), usize::from(width));
        let footer_rows = footer_row_count(height, &footer, width);
        let inspector_rows = if height >= INSPECTOR_MIN_HEIGHT {
            INSPECTOR_ROWS
        } else {
            0
        };
        let list_rows = usize::from(
            height
                .saturating_sub(1)
                .saturating_sub(footer_rows)
                .saturating_sub(inspector_rows),
        );
        self.list_rows = list_rows.max(1);
        let selected = self.selected_agent_id().map(str::to_owned);
        let at = selected.as_ref().and_then(|id| {
            lines.iter().position(|line| {
                matches!(line, crate::tui::agent_history::HistoryLine::Agent(row) if row == id)
            })
        });
        self.scroll_to(at, lines.len(), list_rows);
        let rows = lines
            .iter()
            .skip(self.list_scroll)
            .take(list_rows)
            .filter_map(|line| self.history_row(line, selected.as_deref(), width, spinner, profile))
            .collect();
        let inspector = if inspector_rows == 0 {
            Vec::new()
        } else {
            self.history_inspector(
                &lines,
                selected.as_deref(),
                width,
                processes,
                inspector_rows,
            )
        };
        AgentListFrame {
            header,
            rows,
            inspector,
            footer,
        }
    }

    fn history_inspector(
        &self,
        lines: &[crate::tui::agent_history::HistoryLine],
        selected: Option<&str>,
        width: u16,
        processes: &BTreeMap<String, usize>,
        inspector_rows: u16,
    ) -> Vec<String> {
        if inspector_rows == 0 {
            return Vec::new();
        }
        let visible = selected.is_some_and(|id| {
            lines.iter().any(|line| {
                matches!(line, crate::tui::agent_history::HistoryLine::Agent(row) if row == id)
            })
        });
        let filtered = self.failures_only || !self.search.trim().is_empty();
        let no_rows = lines
            .iter()
            .all(|line| !matches!(line, crate::tui::agent_history::HistoryLine::Agent(_)));
        let selected_is_history = selected.is_some_and(|id| {
            self.node(id)
                .is_some_and(crate::tui::agent_history::history_member)
        });
        if filtered && !visible && (selected_is_history || no_rows) {
            return vec!["Hidden by the filter. Esc clears it.".to_owned()];
        }
        selected.map_or_else(Vec::new, |id| {
            self.inspector_lines(id, &self.projection(), width, processes)
        })
    }

    fn scroll_to(&mut self, at: Option<usize>, len: usize, list_rows: usize) {
        let at = at.unwrap_or(0);
        if at < self.list_scroll {
            self.list_scroll = at;
        } else if list_rows > 0 && at >= self.list_scroll + list_rows {
            self.list_scroll = at + 1 - list_rows;
        }
        self.list_scroll = self.list_scroll.min(len.saturating_sub(list_rows));
    }

    fn history_row(
        &self,
        line: &crate::tui::agent_history::HistoryLine,
        selected: Option<&str>,
        width: u16,
        spinner: Option<&str>,
        profile: Option<&crate::tui::ProfileRow>,
    ) -> Option<AgentListRow> {
        match line {
            crate::tui::agent_history::HistoryLine::Header(title)
            | crate::tui::agent_history::HistoryLine::Notice(title) => Some(AgentListRow {
                id: String::new(),
                cells: RowCells {
                    rails: String::new(),
                    body: format!(" {title}"),
                    mark: "   ".to_owned(),
                },
                dim: true,
                selected: false,
                selectable: false,
            }),
            crate::tui::agent_history::HistoryLine::Agent(id) => {
                let node = self.node(id)?;
                let attention = agent_list::attention(node)
                    .is_some_and(|_| !agent_list::acknowledged(node, &self.acknowledged));
                Some(AgentListRow {
                    id: id.clone(),
                    cells: agent_row(
                        node,
                        "",
                        attention,
                        &AgentRowClock {
                            now_unix: self.now_unix,
                            spinner,
                            profile,
                        },
                        usize::from(width),
                    ),
                    dim: true,
                    selected: selected == Some(id.as_str()),
                    selectable: true,
                })
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "The agent inspector renders lifecycle, binding, workspace, and recovery facts from the same selected node."
    )]
    fn inspector_lines(
        &self,
        id: &str,
        projection: &AgentProjection,
        width: u16,
        processes: &BTreeMap<String, usize>,
    ) -> Vec<String> {
        let Some(node) = self.node(id) else {
            return Vec::new();
        };
        let now_unix = self.now_unix;
        let cells = usize::from(width).saturating_sub(2).max(8);
        let children = projection.layout.children(id).len();
        let parent = node.parent_agent_id.as_deref().map_or_else(
            || "tree root".to_owned(),
            |id| format!("parent {}", short_id(id)),
        );
        let attention = agent_list::attention(node);
        let acknowledged = agent_list::acknowledged(node, &self.acknowledged);
        let status = attention.map_or_else(
            || humanize_state(row_status(node)),
            |attention| attention.label().to_owned(),
        );
        let tokens = node.usage.map_or_else(
            || "tokens —".to_owned(),
            |usage| {
                format!(
                    "{} in · {} out tokens",
                    usage.input_tokens, usage.output_tokens
                )
            },
        );
        let times = match (node.started_at_unix, node.ended_at_unix) {
            (Some(started), Some(ended)) => format!(
                "started {} ago · ran {} · ended {} ago",
                until_label(now_unix, started),
                elapsed_label(Some(started), ended),
                until_label(now_unix, ended)
            ),
            (Some(started), None) => format!("started {} ago", until_label(now_unix, started)),
            (None, _) => "start unknown".to_owned(),
        };
        let can_ack = attention.is_some() && !acknowledged && node.attempt_id.is_some();
        let remedy = match (attention, can_ack, acknowledged) {
            (Some(agent_list::Attention::RecoveryRequired), true, _) => {
                format!(" · {RECOVERY_REMEDY} · x acknowledges")
            }
            (Some(agent_list::Attention::RecoveryRequired), false, true) => {
                format!(" · {RECOVERY_REMEDY} · acknowledged")
            }
            (Some(agent_list::Attention::RecoveryRequired), _, _) => {
                format!(" · {RECOVERY_REMEDY}")
            }
            (Some(_), true, _) => " · needs attention · x acknowledges".to_owned(),
            (Some(_), false, true) => " · acknowledged".to_owned(),
            _ => String::new(),
        };
        let remedy = match node.continuation {
            Some(plakous_host_protocol::views::LossContinuationView::Held) => {
                format!("{remedy} · {LOSS_HELD_REMEDY}")
            }
            Some(plakous_host_protocol::views::LossContinuationView::Pending) => {
                format!("{remedy} · continuation pending")
            }
            Some(plakous_host_protocol::views::LossContinuationView::Running) => {
                format!("{remedy} · continuing after the lost host")
            }
            None => remedy,
        };
        let name = agent_name(node);
        let description = node.description.trim();
        let described = if description.is_empty() || description == name {
            String::new()
        } else {
            format!(" · {description}")
        };
        let mut usage = format!(
            "Usage   {} · {tokens}{} · {times}",
            turns_label(node.turns),
            process_count_label(processes.get(id).copied().unwrap_or(0)),
        );
        if let Some(compaction) = node.usage.and_then(|usage| usage.compaction) {
            let measured = compaction
                .tokens_before
                .zip(compaction.tokens_after)
                .map_or_else(String::new, |(before, after)| {
                    format!(" · {before} -> {after} input")
                });
            let coverage = if compaction.complete {
                ""
            } else {
                " · retained only"
            };
            usage.push_str(&format!(
                " · Compact {} committed · {} failed · {} no-eligible{measured}{coverage}",
                compaction.committed, compaction.failed, compaction.no_eligible
            ));
        }
        let lines = [
            format!(
                "{} {name}{described} · {status}{remedy}{}",
                short_id(&node.agent_id),
                dormancy_inspector(node),
            ),
            node.fault.as_deref().map_or_else(
                || format!("Task    {}", blank_as_unknown(&node.prompt)),
                |fault| format!("Fault   {fault}"),
            ),
            format!(
                "Model   {} · {} · {} {}",
                model_label(node),
                attempt_label(node),
                node.workspace_mode,
                node.workspace_path
            ),
            usage,
            format!(
                "Place   {parent} · {children} {} · id {}",
                if children == 1 { "child" } else { "children" },
                node.agent_id
            ),
            format!("Work    {}", work_line(node, now_unix)),
        ];
        lines
            .into_iter()
            .map(|line| truncate_display(&line, cells))
            .collect()
    }

    fn acknowledge_action(&self) -> Option<crate::tui::UserAction> {
        let snap = self.snapshot.as_ref()?;
        let node = self.actionable_node()?;
        acknowledge_intent(&snap.tree_id, node, &self.acknowledged)
    }

    /// The selected agent, unless it is the root: the root's turn stops
    /// from the root view, and the root is never closed from the list.
    fn selected_child_id(&self) -> Option<&str> {
        self.actionable_node()
            .filter(|node| node.parent_agent_id.is_some())
            .map(|node| node.agent_id.as_str())
    }

    fn interrupt_action(&self) -> Option<crate::tui::UserAction> {
        Some(crate::tui::UserAction::InterruptTopologyNode {
            agent_id: self.selected_child_id()?.to_owned(),
        })
    }

    fn close_action(&self) -> Option<crate::tui::UserAction> {
        Some(crate::tui::UserAction::CloseTopologyNode {
            agent_id: self.selected_child_id()?.to_owned(),
        })
    }

    fn recover_action(&self) -> Option<crate::tui::UserAction> {
        recover_intent(
            self.snapshot.as_ref()?,
            self.actionable_node()?.agent_id.as_str(),
        )
    }

    /// `r` on a child a lost host interrupted whose continuation is held
    /// (or recorded and not started): the host's Resume of that one node.
    fn resume_action(&self) -> Option<crate::tui::UserAction> {
        let node = self.actionable_node()?;
        let held = matches!(
            node.continuation,
            Some(
                plakous_host_protocol::views::LossContinuationView::Held
                    | plakous_host_protocol::views::LossContinuationView::Pending
            )
        );
        (held && node.parent_agent_id.is_some()).then(|| {
            crate::tui::UserAction::ResumeLossContinuation {
                agent_id: Some(node.agent_id.clone()),
            }
        })
    }
}

/// Overlay keys after Crossterm translation in `ui.rs`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopologyNavigatorKey {
    /// Up / k.
    Up,
    /// Down / j.
    Down,
    /// Left / h — parent.
    Left,
    /// Right / l — first child.
    Right,
    /// Enter.
    Enter,
    /// Esc.
    Escape,
    /// i — ask to interrupt the selected agent.
    Interrupt,
    /// c — ask to close the selected agent.
    Close,
    /// s — ask to stop every non-root agent (root host command).
    StopAll,
    /// y — copy the focused node id.
    Copy,
    /// Home — jump to the root, or the first history row.
    Home,
    /// End — last listed row.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Backspace — edit the history query.
    Backspace,
    /// t — toggle history.
    History,
    /// / — history search.
    Search,
    /// ! — history failures.
    Failures,
    /// u — recover the selected child's recorded session.
    Recover,
    /// r — resume the selected child a lost host interrupted.
    Resume,
    /// f — toggle the live/all scope.
    Filter,
    /// x — acknowledge the selected agent's attention.
    Acknowledge,
    /// R — ask the host for a fresh snapshot.
    Refresh,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;

#[cfg(test)]
mod footer_copy_tests;
