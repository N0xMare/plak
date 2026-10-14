//! Inert agent-tree navigator DTOs and local overlay state.
//!
//! The TUI never opens an Agent, store, or Plakous session. Host events replace
//! exact-ID snapshots; this module only selects and emits typed intents. Stale revisions are the host's to resnapshot. Which
//! agents the Agents tab lists is [`crate::tui::agent_list`]'s projection.

use std::collections::{BTreeMap, BTreeSet};

use crate::tui::agent_list::{
    self, AgentProjection, AgentScope, RowCells, RowFacts, compose_row, truncate_display,
};
use crate::tui::{AgentAcknowledgement, CanvasNode, CanvasTree, SurfaceIntent, status_glyph};

/// Rows the docked inspector takes when the overlay is tall enough for it.
const INSPECTOR_ROWS: u16 = 6;
/// Shortest overlay that still carries an inspector.
const INSPECTOR_MIN_HEIGHT: u16 = 12;

/// Compact chrome when a `restored_paused` snapshot arrives with the overlay closed.
pub const RESTORED_PAUSED_NAVIGATOR_HINT: &str = "the agent tree is paused · /tree then r resumes";

/// Footer for the Agents tab: why agents are missing, then the keys.
#[must_use]
pub fn agents_footer_hint(restored_paused: bool, scope: AgentScope, hidden: usize) -> String {
    let shown = match scope {
        AgentScope::Live if hidden == 0 => "active + last 5m · f all".to_owned(),
        AgentScope::Live => format!("active + last 5m · {hidden} older hidden · f all"),
        AgentScope::All => "all agents · f live".to_owned(),
    };
    let head = if restored_paused {
        // `restored_paused` is a wire value and `ResumeTree` is the
        // name of a request type. Neither is something an operator reads.
        "Tree paused · r resumes · "
    } else {
        ""
    };
    format!(
        "{shown} · {head}↑/↓ select · Enter transcript · x acknowledge · i interrupt · c close · s stop all · y copy · R refresh · Esc back"
    )
}

/// The Agents footer at `width` in `max_rows`: the long form when it fits,
/// otherwise every key in short form, so a narrow list still names (and a
/// click still presses) `i`, `c` and `s` rather than clipping them away.
#[must_use]
pub fn fitted_agents_footer(
    restored_paused: bool,
    scope: AgentScope,
    hidden: usize,
    width: u16,
    max_rows: usize,
) -> String {
    let long = agents_footer_hint(restored_paused, scope, hidden);
    let budget = usize::from(width).saturating_sub(1).max(1);
    if crate::tui::picker::wrap(long.trim(), budget).len() <= max_rows.max(1) {
        return long;
    }
    let shown = match scope {
        AgentScope::Live => "f all",
        AgentScope::All => "f live",
    };
    let head = if restored_paused {
        "paused · r resume · "
    } else {
        ""
    };
    format!("{head}↑/↓ · Enter · x · i · c · s · y · R · {shown} · Esc")
}

/// Rows the list footer may take before it gives up and clips.
///
/// The footer wraps (§5.5) but never eats the list: on a short overlay it
/// keeps to two rows and clips the last with a visible `…`.
#[must_use]
pub(crate) const fn footer_max_rows(height: u16) -> usize {
    if height >= 12 { 3 } else { 2 }
}

/// Wraps a list footer onto the rows `width` needs, up to `max_rows`.
#[must_use]
pub(crate) fn wrap_footer_hint(hint: &str, width: u16, max_rows: usize) -> Vec<String> {
    let budget = usize::from(width).saturating_sub(1).max(1);
    let max_rows = max_rows.max(1);
    let mut rows = crate::tui::picker::wrap(hint.trim(), budget);
    if rows.len() > max_rows {
        let tail = rows.split_off(max_rows - 1).join(" ");
        rows.push(crate::tui::picker::clip(&tail, budget));
    }
    rows.into_iter().map(|line| format!(" {line}")).collect()
}

/// Renders a host state token as prose: `tree_terminal` is a wire value, not
/// something an operator should have to read off the chrome (§5.10).
#[must_use]
pub(crate) fn humanize_state(state: &str) -> String {
    state.replace('_', " ")
}

/// What a recovery-required agent needs; recovery is the root agent's.
pub const RECOVERY_REMEDY: &str = "remedy: ask the root to run recover_agent";

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
}

/// One painted list row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentListRow {
    pub id: String,
    pub cells: RowCells,
    pub dim: bool,
    pub selected: bool,
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
        }
        self.snapshot = Some(snapshot);
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
    #[must_use]
    pub fn selected_node(&self) -> Option<&CanvasNode> {
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

    fn handle_tree_key(&mut self, key: TopologyNavigatorKey) -> TopologyNavigatorOutcome {
        match key {
            TopologyNavigatorKey::Escape => TopologyNavigatorOutcome::Dismiss,
            TopologyNavigatorKey::Up => {
                self.move_selection(-1);
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Down => {
                self.move_selection(1);
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Left => {
                self.select_parent();
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Right => {
                self.select_child();
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Home => {
                self.select_root();
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Filter => {
                self.toggle_scope();
                TopologyNavigatorOutcome::Consumed
            }
            TopologyNavigatorKey::Acknowledge => self.acknowledge_action().map_or(
                TopologyNavigatorOutcome::Consumed,
                TopologyNavigatorOutcome::Action,
            ),
            TopologyNavigatorKey::Refresh => {
                TopologyNavigatorOutcome::Action(crate::tui::UserAction::RefreshTopologyTree)
            }
            TopologyNavigatorKey::Enter => match self.selected_node() {
                None => TopologyNavigatorOutcome::Consumed,
                Some(node) if node.parent_agent_id.is_none() => TopologyNavigatorOutcome::ShowRoot,
                Some(node) => TopologyNavigatorOutcome::ShowAgent {
                    agent_id: node.agent_id.clone(),
                    label: agent_view_label(node),
                },
            },
            TopologyNavigatorKey::Resume => self.resume_action().map_or(
                TopologyNavigatorOutcome::Consumed,
                TopologyNavigatorOutcome::Confirm,
            ),
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
            TopologyNavigatorKey::Copy => TopologyNavigatorOutcome::Consumed,
        }
    }

    /// What `y` copies: the selected agent id.
    #[must_use]
    pub fn copy_text(&self) -> String {
        self.selected_agent_id().unwrap_or_default().to_owned()
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
        let projection = self.projection();
        let now = self.now_unix;
        let wake = self
            .snapshot
            .as_ref()
            .and_then(|snap| agent_list::next_age_boundary(&snap.nodes, &projection, now));
        self.age_wake_at = wake;
        let restored_paused = self
            .snapshot
            .as_ref()
            .is_some_and(|snap| snap.restored_paused);
        let footer = fitted_agents_footer(
            restored_paused,
            self.scope,
            projection.hidden,
            width,
            footer_max_rows(height),
        );
        let header = header_line(&projection, self.snapshot.as_ref(), width);
        // The footer owns however many rows its own text needs at this width.
        let footer_rows = if height >= 4 {
            u16::try_from(wrap_footer_hint(&footer, width, footer_max_rows(height)).len())
                .unwrap_or(1)
                .min(height.saturating_sub(3))
                .max(1)
        } else {
            0
        };
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
        let selected = self.resolve_selection(&projection);
        let at = selected
            .as_deref()
            .and_then(|id| projection.position(id))
            .unwrap_or(0);
        if at < self.list_scroll {
            self.list_scroll = at;
        } else if list_rows > 0 && at >= self.list_scroll + list_rows {
            self.list_scroll = at + 1 - list_rows;
        }
        self.list_scroll = self
            .list_scroll
            .min(projection.rows.len().saturating_sub(list_rows));
        let rows = projection
            .rows
            .iter()
            .skip(self.list_scroll)
            .take(list_rows)
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
                            processes,
                            profile,
                        },
                        usize::from(width),
                    ),
                    dim: row.dim,
                    selected: selected.as_deref() == Some(row.id.as_str()),
                })
            })
            .collect();
        let inspector = if inspector_rows == 0 {
            Vec::new()
        } else {
            selected
                .as_deref()
                .map(|id| self.inspector_lines(id, &projection, width))
                .unwrap_or_default()
        };
        AgentListFrame {
            header: truncate_display(&header, usize::from(width)),
            rows,
            inspector,
            footer,
        }
    }

    fn inspector_lines(&self, id: &str, projection: &AgentProjection, width: u16) -> Vec<String> {
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
        let name = agent_name(node);
        let description = node.description.trim();
        let described = if description.is_empty() || description == name {
            String::new()
        } else {
            format!(" · {description}")
        };
        let mut lines = vec![
            format!(
                "{} {name}{described} · {status}{remedy}",
                short_id(&node.agent_id),
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
            format!("Usage   {} · {tokens} · {times}", turns_label(node.turns)),
            format!(
                "Place   {parent} · {children} {} · id {}",
                if children == 1 { "child" } else { "children" },
                node.agent_id
            ),
            format!("Work    {}", work_line(node, now_unix)),
        ];
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
            lines[3].push_str(&format!(
                " · Compact {} committed · {} failed · {} no-eligible{measured}{coverage}",
                compaction.committed, compaction.failed, compaction.no_eligible
            ));
        }
        lines
            .into_iter()
            .map(|line| truncate_display(&line, cells))
            .collect()
    }

    fn acknowledge_action(&self) -> Option<crate::tui::UserAction> {
        let snap = self.snapshot.as_ref()?;
        let projection = self.projection();
        let node = self.node(self.resolve_selection(&projection)?.as_str())?;
        acknowledge_intent(&snap.tree_id, node, &self.acknowledged)
    }

    fn resume_action(&self) -> Option<crate::tui::UserAction> {
        let snap = self.snapshot.as_ref()?;
        snap.restored_paused
            .then_some(crate::tui::UserAction::ResumeTopologyTree)
    }

    /// The selected agent, unless it is the root: the root's turn stops
    /// from the root view, and the root is never closed from the list.
    fn selected_child_id(&self) -> Option<&str> {
        self.selected_node()
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
}

/// `x` on an agent (the Agents tab or the agent view): acknowledge the
/// attention it needs now, unless this occurrence already is.
pub(crate) fn acknowledge_intent(
    tree_id: &str,
    node: &CanvasNode,
    acknowledged: &BTreeSet<AgentAcknowledgement>,
) -> Option<crate::tui::UserAction> {
    let attention = agent_list::attention(node)?;
    if agent_list::acknowledged(node, acknowledged) {
        return None;
    }
    Some(crate::tui::UserAction::Extension(
        SurfaceIntent::AcknowledgeAgent {
            tree_id: tree_id.to_owned(),
            agent_id: node.agent_id.clone(),
            attempt_id: node.attempt_id.clone()?,
            attention: attention.label().to_owned(),
            ended_at: node.ended_at_unix,
        },
    ))
}

/// Labels the root box may be repainted with by [`TreeNavigator::project_root_turn`].
///
/// Anything else on the root row is a durable observation the session's turn
/// phase does not know about — `sealing` and `adopting` above all — and
/// overwriting it would erase the operator-visible candidate state.
const ROOT_TURN_OVERWRITABLE: &[&str] = &["running", "idle", "pending", "completed"];

/// The bound model tuple as one label. Unknown stays unknown.
fn model_label(node: &CanvasNode) -> String {
    let tuple = node.model_tuple.trim();
    let account = node.model_account.trim();
    let head = match (tuple.is_empty(), account.is_empty()) {
        (false, _) => tuple.to_owned(),
        (true, false) => account.to_owned(),
        (true, true) => return "\u{2014}".to_owned(),
    };
    node.model_effort
        .as_deref()
        .filter(|effort| !effort.is_empty())
        .map_or(head.clone(), |effort| format!("{head} · {effort}"))
}

/// Admitted turns. `None` is unknown, never zero.
fn turns_label(turns: Option<u32>) -> String {
    match turns {
        Some(1) => "1 turn".to_owned(),
        Some(count) => format!("{count} turns"),
        None => "turns \u{2014}".to_owned(),
    }
}

/// The status a row paints: the node's own, unless its copy is mid-adopt.
///
/// Each row carries one in-flight status — a node seals, a workspace
/// adopts — and the canvas draws one box per node, so the box has to choose.
/// It shows `adopting`, because a node whose candidate is being landed reads
/// `completed` on its own row while the thing the operator is waiting on is the
/// adoption. Every other workspace status is history the `Work` line carries.
#[must_use]
fn row_status(node: &CanvasNode) -> &str {
    match node.workspace_status.as_deref() {
        Some("adopting") => "adopting",
        Some("discarding") if node.status != "recovery_required" => "discarding",
        _ => node.status.as_str(),
    }
}

/// `a2 · running` for the node's current attempt. A node with no attempt row
/// says so; the root never has one.
///
/// Only the current attempt is projected, so the label is always the current
/// one — a `followup` re-task opens `a2` on the same node and this label moves
/// with it.
#[must_use]
pub fn attempt_label(node: &CanvasNode) -> String {
    match (node.attempt_seq, node.attempt_status.as_deref()) {
        (Some(seq), Some(status)) => format!("a{seq} \u{b7} {status}"),
        (Some(seq), None) => format!("a{seq}"),
        (None, Some(status)) => status.to_owned(),
        (None, None) => "no attempt".to_owned(),
    }
}

/// `a1 ‹a2›` — every attempt this node has had, the current one bracketed.
///
/// Empty when there is at most one attempt: `Bind` already names it, and a
/// strip of one is noise. Plain text carries the highlight because the
/// inspector is a `Vec<String>` the overlay paints in one style.
#[must_use]
fn attempts_strip(node: &CanvasNode) -> String {
    if node.attempts.len() < 2 {
        return String::new();
    }
    node.attempts
        .iter()
        .map(|seq| {
            if Some(*seq) == node.attempt_seq {
                format!("\u{2039}a{seq}\u{203a}")
            } else {
                format!("a{seq}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Trailing segment of a candidate id, which is what a row has room for.
#[must_use]
fn short_candidate(candidate_id: &str) -> String {
    let tail = candidate_id.rsplit('-').next().unwrap_or(candidate_id);
    let tail = if tail.is_empty() { candidate_id } else { tail };
    format!("#{}", truncate_display(tail, 8))
}

/// `sealed · #abc123` once the node's attempt has sealed a candidate.
/// Only `execute` and `mutate` copies seal, so a `read_only`
/// row never carries one and this stays `None` for it.
#[must_use]
fn sealed_line(node: &CanvasNode) -> Option<String> {
    node.candidate_id
        .as_deref()
        .filter(|candidate| !candidate.trim().is_empty())
        .map(|candidate| format!("sealed \u{b7} {}", short_candidate(candidate)))
}

/// `blocked · resets in 12m` from the attempt's `resets_at`, or plain `blocked`
/// when the provider named no reset. The quota and account are
/// spelled in the inspector, not in the box: the box has room for the fact and
/// the clock, nothing else.
#[must_use]
fn blocked_line(node: &CanvasNode, now_unix: u64) -> Option<String> {
    let blocked = node.blocked.as_ref()?;
    Some(match blocked.resets_at_unix {
        Some(resets_at) => format!(
            "blocked \u{b7} resets in {}",
            until_label(resets_at, now_unix)
        ),
        None => "blocked".to_owned(),
    })
}

/// Time remaining until `at`. A reset already in the past reads `now`, never a
/// negative or a wrapped duration.
#[must_use]
fn until_label(at: u64, now_unix: u64) -> String {
    let seconds = at.saturating_sub(now_unix);
    if seconds == 0 {
        "now".to_owned()
    } else if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    } else {
        format!("{}d{:02}h", seconds / 86_400, (seconds % 86_400) / 3600)
    }
}

/// The inspector's `Work` line: what this node is authorised to do, what its
/// copy is doing, and what it produced. Every segment is omitted when the host
/// does not know it, so the line never invents a tier or a workspace state.
#[must_use]
fn work_line(node: &CanvasNode, now_unix: u64) -> String {
    let mut parts: Vec<String> = Vec::new();
    let attempts = attempts_strip(node);
    if !attempts.is_empty() {
        parts.push(attempts);
    }
    parts.push(
        node.tier
            .as_deref()
            .filter(|tier| !tier.trim().is_empty())
            .map_or_else(|| "tier —".to_owned(), |tier| format!("tier {tier}")),
    );
    if let Some(status) = node
        .workspace_status
        .as_deref()
        .filter(|status| !status.trim().is_empty())
    {
        parts.push(format!("copy {status}"));
    }
    if let Some(sealed) = sealed_line(node) {
        parts.push(sealed);
    }
    if let Some(blocked) = node.blocked.as_ref() {
        let reset = blocked.resets_at_unix.map_or_else(String::new, |at| {
            format!(" · resets in {}", until_label(at, now_unix))
        });
        parts.push(format!(
            "blocked {} on {}{reset}",
            blank_as_unknown(&blocked.quota),
            blank_as_unknown(&blocked.account)
        ));
    }
    parts.join(" · ")
}

/// Elapsed wall time. Without a durable start stamp this stays unknown.
fn elapsed_label(started_at_unix: Option<u64>, now_unix: u64) -> String {
    let Some(started) = started_at_unix else {
        return "\u{2014}".to_owned();
    };
    super::blocks::format_duration_secs(now_unix.saturating_sub(started))
}

/// Trailing segment of a durable id, which is what the boxes have room for.
pub(crate) fn short_id(agent_id: &str) -> String {
    let tail = agent_id.rsplit('-').next().unwrap_or(agent_id);
    let tail = if tail.is_empty() { agent_id } else { tail };
    format!("#{}", truncate_display(tail, 10))
}

fn blank_as_unknown(text: &str) -> String {
    if text.trim().is_empty() {
        "\u{2014}".to_owned()
    } else {
        text.trim().to_owned()
    }
}

/// `● 3 active   ✓ 21 completed   ⚠ 1 attention`, then the tree state when it
/// is not plainly live. A narrow body keeps the glyphs and counts.
fn header_line(projection: &AgentProjection, snapshot: Option<&CanvasTree>, width: u16) -> String {
    let counts = projection.counts;
    let state = snapshot
        .map(|snap| snap.state.as_str())
        .filter(|state| *state != "live")
        .map_or_else(String::new, |state| {
            format!("   tree {}", humanize_state(state))
        });
    let full = format!(
        "\u{25cf} {} active   \u{2713} {} completed   \u{26a0} {} attention{state}",
        counts.active, counts.completed, counts.attention
    );
    if full.chars().count() <= usize::from(width) {
        return full;
    }
    format!(
        "\u{25cf} {}  \u{2713} {}  \u{26a0} {}{state}",
        counts.active, counts.completed, counts.attention
    )
}

/// What an agent row reads besides its node: the frame's clock, spinner
/// frame (`None` under reduced motion) and running processes per owner.
pub struct AgentRowClock<'a> {
    pub now_unix: u64,
    pub spinner: Option<&'a str>,
    pub processes: &'a BTreeMap<String, usize>,
    /// The session's profile, whose card labels name each template's role.
    pub profile: Option<&'a crate::tui::ProfileRow>,
}

/// One Agents row for `node` in `width` cells: the one row format the
/// Activity list and the agent view's bar both paint.
#[must_use]
pub fn agent_row(
    node: &CanvasNode,
    rails: &str,
    attention: bool,
    clock: &AgentRowClock<'_>,
    width: usize,
) -> RowCells {
    let glyph = agent_glyph(node, attention, clock.spinner);
    let short = short_id(&node.agent_id);
    let fact = live_fact(node, clock.now_unix);
    let role = agent_role(node, clock.profile);
    let model = node.model_tuple.trim().to_owned();
    let turns = if node.parent_agent_id.is_some() {
        turns_label(node.turns)
    } else {
        String::new()
    };
    let tokens = node.usage.map_or_else(String::new, |usage| {
        let mut tokens = format!(
            "{} tok",
            compact_count(usage.input_tokens.saturating_add(usage.output_tokens))
        );
        if let Some(compaction) = usage.compaction {
            tokens.push_str(&format!(
                " · {} compact{}",
                compaction.committed,
                if compaction.complete { "" } else { "+" }
            ));
        }
        tokens
    });
    let owned = clock
        .processes
        .get(&node.agent_id)
        .copied()
        .unwrap_or_default();
    let processes = match owned {
        0 => String::new(),
        1 => "1 process".to_owned(),
        count => format!("{count} processes"),
    };
    let elapsed = row_elapsed(node, clock.now_unix);
    let facts = RowFacts {
        glyph: &glyph,
        elapsed: &elapsed,
        short_id: &short,
        fact: &fact,
        role: &role,
        model: &model,
        turns: &turns,
        tokens: &tokens,
        processes: &processes,
    };
    compose_row(rails, &facts, attention, width)
}

/// The agent view's breadcrumb label: `#<id> · <description>`, the
/// description falling back to the row's name.
#[must_use]
pub fn agent_view_label(node: &CanvasNode) -> String {
    let description = node.description.trim();
    let description = if description.is_empty() {
        agent_name(node)
    } else {
        description.to_owned()
    };
    format!("{} \u{b7} {description}", short_id(&node.agent_id))
}

/// The template's role within the session's profile: a template the profile
/// carries reads as its card label (`my-profile-worker` is
/// `worker` in `my-profile`); any other template reads in
/// full. The root is `root`; a node without a template has no role.
fn agent_role(node: &CanvasNode, profile: Option<&crate::tui::ProfileRow>) -> String {
    if node.parent_agent_id.is_none() {
        return "root".to_owned();
    }
    let Some(template) = node
        .template
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return String::new();
    };
    profile
        .and_then(|profile| {
            let label = template.strip_prefix(&format!("{}-", profile.id))?;
            profile
                .cards
                .iter()
                .any(|card| card.label == label)
                .then(|| label.to_owned())
        })
        .unwrap_or_else(|| template.to_owned())
}

/// `950`, `12.4k`, `3.1M`.
fn compact_count(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{}.{}k", count / 1_000, count % 1_000 / 100),
        _ => format!("{}.{}M", count / 1_000_000, count % 1_000_000 / 100_000),
    }
}

/// The row's name: the template, else the description; the root is `root`.
fn agent_name(node: &CanvasNode) -> String {
    if node.parent_agent_id.is_none() {
        return "root".to_owned();
    }
    node.template
        .as_deref()
        .filter(|template| !template.trim().is_empty())
        .map_or_else(|| blank_as_unknown(&node.description), str::to_owned)
}

/// The status glyph (spec 02 §5.7 vocabulary): running and submitted animate,
/// waiting is `◐`, done `✓`, failed `✕`, no result `!`, cancelled `○`.
fn agent_glyph(node: &CanvasNode, attention: bool, spinner: Option<&str>) -> String {
    let status = row_status(node);
    let running = matches!(status, "running")
        || node.attempt_status.as_deref() == Some("submitted") && agent_list::is_active(node);
    if running {
        return spinner.unwrap_or("\u{25cf}").to_owned();
    }
    let glyph = match status {
        "pending" | "paused" | "interrupted" => '\u{25d0}',
        "idle" => '\u{25cc}',
        "closed" | "discarding" => '\u{25cb}',
        _ if agent_list::is_active(node) => status_glyph(status),
        _ => match node.attempt_status.as_deref() {
            Some("completed_without_submission") => '!',
            Some("blocked") => '\u{2298}',
            Some("cancelled" | "closed") => '\u{25cb}',
            _ if attention || status == "failed" || status == "recovery_required" => '\u{2715}',
            _ => '\u{2713}',
        },
    };
    glyph.to_string()
}

/// One live fact: the current tool verb while it runs, else the status.
fn live_fact(node: &CanvasNode, now_unix: u64) -> String {
    if agent_list::is_active(node)
        && let Some(activity) = node
            .live_progress
            .as_ref()
            .and_then(|progress| progress.activity.as_deref())
            .filter(|activity| !activity.trim().is_empty())
    {
        return activity.trim().to_owned();
    }
    if let Some(attention) = agent_list::attention(node) {
        return match attention {
            agent_list::Attention::Blocked => {
                blocked_line(node, now_unix).unwrap_or_else(|| attention.label().to_owned())
            }
            _ => attention.label().to_owned(),
        };
    }
    if !node.active_turn
        && let Some(waiting_on) = &node.waiting_on
    {
        return if waiting_on.is_empty() {
            "parked".to_owned()
        } else {
            format!("waiting on {}", waiting_on.join(", "))
        };
    }
    match row_status(node) {
        "interrupted" => "waiting".to_owned(),
        "adopting" => sealed_line(node).map_or_else(
            || "adopting".to_owned(),
            |sealed| format!("adopting · {}", sealed.trim_start_matches("sealed · ")),
        ),
        "completed" => {
            let done = if node.attempt_status.as_deref() == Some("adopted") {
                "adopted"
            } else {
                "done"
            };
            sealed_line(node).map_or_else(
                || done.to_owned(),
                |sealed| format!("{done} · {}", sealed.trim_start_matches("sealed · ")),
            )
        }
        status => humanize_state(status),
    }
}

/// Running agents count up from their start; finished ones show how long
/// they ran.
fn row_elapsed(node: &CanvasNode, now_unix: u64) -> String {
    match node.ended_at_unix {
        Some(ended) if !agent_list::is_active(node) => elapsed_label(node.started_at_unix, ended),
        _ => elapsed_label(node.started_at_unix, now_unix),
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
    /// r — `ResumeTree` when paused.
    Resume,
    /// i — ask to interrupt the selected agent.
    Interrupt,
    /// c — ask to close the selected agent.
    Close,
    /// s — ask to stop every non-root agent (root host command).
    StopAll,
    /// y — copy the focused node id.
    Copy,
    /// Home — jump to the root.
    Home,
    /// f — toggle the live/all scope.
    Filter,
    /// x — acknowledge the selected agent's attention.
    Acknowledge,
    /// R — ask the host for a fresh snapshot.
    Refresh,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;
    use plakous_host_protocol::views::{
        BlockedObservation, PROJECTED_STATUS_LABELS, UNKNOWN_STATUS_GLYPH,
    };

    fn node(id: &str, parent: Option<&str>, depth: u8) -> CanvasNode {
        CanvasNode {
            agent_id: id.to_owned(),
            parent_agent_id: parent.map(str::to_owned),
            depth,
            status: "running".into(),
            attempt_status: None,
            attempt_seq: None,
            attempt_id: None,
            attempts: Vec::new(),
            tier: None,
            description: "description".into(),
            prompt: "prompt".into(),
            workspace_path: "/tmp".into(),
            workspace_mode: "inherit".into(),
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
            finished: false,
            fault: None,
            waiting_on: None,
        }
    }

    #[test]
    fn a_parked_child_reads_what_it_waits_on() {
        let mut parked = node("c1", Some("root"), 1);
        parked.status = "running".to_owned();
        parked.waiting_on = Some(vec!["p3".to_owned(), "c2".to_owned()]);
        assert_eq!(live_fact(&parked, 0), "waiting on p3, c2");
        parked.waiting_on = Some(Vec::new());
        assert_eq!(live_fact(&parked, 0), "parked");
        // The cue is the row's right-aligned status column.
        let processes = BTreeMap::new();
        let clock = AgentRowClock {
            now_unix: 0,
            spinner: None,
            processes: &processes,
            profile: None,
        };
        let row = agent_row(&parked, "", false, &clock, 100).body;
        assert!(row.ends_with(" parked"), "{row:?}");
    }

    /// Spec 02 §6.1: a template the session's profile carries reads as its
    /// role; any other template in full; the description is not on the row
    /// but heads the inspector.
    #[test]
    fn a_row_names_the_profile_role_and_leaves_the_description_to_the_inspector() {
        let role = |label: &str| crate::tui::ProfileRole {
            label: label.to_owned(),
            account: "claude".into(),
            model: "m".into(),
            effort: "default".into(),
            fast: false,
            depth: 0,
        };
        let profile = crate::tui::ProfileRow {
            id: "my-profile".into(),
            aliases: Vec::new(),
            root: role("root"),
            cards: vec![role("staff-worker")],
        };
        let mut staff = node("c23", Some("root"), 1);
        staff.template = Some("my-profile-staff-worker".into());
        staff.usage = Some(plakous_host_protocol::views::AttemptUsage {
            elapsed_ms: 0,
            turns: 3,
            tool_calls: 0,
            input_tokens: 12_000,
            output_tokens: 400,
            compaction: None,
        });
        let mut outside = node("c24", Some("root"), 1);
        outside.template = Some("my-profile-reviewer".into());
        let processes = BTreeMap::from([("c23".to_owned(), 2)]);
        let clock = AgentRowClock {
            now_unix: 100,
            spinner: None,
            processes: &processes,
            profile: Some(&profile),
        };
        let row = agent_row(&staff, "", false, &clock, 120).body;
        assert!(
            row.contains("#c23  staff-worker  grok-4.6  1 turn  12.4k tok · 2 processes"),
            "{row}"
        );
        assert!(row.trim_end().ends_with("  running"), "{row}");
        assert!(!row.contains("description"), "{row}");
        let row = agent_row(&outside, "", false, &clock, 120).body;
        assert!(row.contains("  my-profile-reviewer  "), "{row}");
        let without = AgentRowClock {
            profile: None,
            ..clock
        };
        let row = agent_row(&staff, "", false, &without, 120).body;
        assert!(row.contains("  my-profile-staff-worker  "), "{row}");

        let mut nav = TreeNavigator::awaiting();
        nav.apply_snapshot(snapshot(vec![node("root", None, 0), staff]));
        nav.focus_on("c23");
        let frame = nav.frame(120, 30, None, &BTreeMap::new(), Some(&profile));
        assert!(
            frame
                .inspector
                .iter()
                .any(|line| line.contains("· description ·")),
            "{:?}",
            frame.inspector
        );
    }

    fn snapshot(nodes: Vec<CanvasNode>) -> CanvasTree {
        CanvasTree {
            tree_id: "tree1".into(),
            revision: 3,
            state: "live".into(),
            restored_paused: false,
            solicited: false,
            nodes,
        }
    }

    #[test]
    fn visible_row_navigation_stops_and_left_right_follow_ancestry() {
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
            node("root", None, 0),
            node("a", Some("root"), 1),
            node("b", Some("root"), 1),
            node("a1", Some("a"), 2),
        ]));
        nav.move_selection(1);
        assert_eq!(nav.selected_agent_id(), Some("a"));
        nav.move_selection(1);
        assert_eq!(nav.selected_agent_id(), Some("a1"));
        nav.select_parent();
        assert_eq!(nav.selected_agent_id(), Some("a"));
        nav.select_child();
        assert_eq!(nav.selected_agent_id(), Some("a1"));
        nav.focus_on("root");
        nav.move_selection(-1);
        assert_eq!(nav.selected_agent_id(), Some("root"));
        nav.focus_on("b");
        nav.select_child();
        assert_eq!(
            nav.selected_agent_id(),
            Some("b"),
            "a leaf cannot descend into a sibling's child"
        );
        nav.focus_on("root");
        nav.select_parent();
        assert_eq!(nav.selected_agent_id(), Some("root"));
    }

    #[test]
    fn enter_shows_the_selected_transcript_and_esc_dismisses_tree() {
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
            node("root", None, 0),
            node("agent-c3", Some("root"), 1),
        ]));
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::Enter),
            TopologyNavigatorOutcome::ShowRoot
        );
        nav.focus_on("agent-c3");
        let TopologyNavigatorOutcome::ShowAgent { agent_id, label } =
            nav.handle_key(TopologyNavigatorKey::Enter)
        else {
            panic!("Enter on an agent row shows its transcript");
        };
        assert_eq!(agent_id, "agent-c3");
        assert!(label.starts_with("#c3 \u{b7} "), "{label}");
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::Escape),
            TopologyNavigatorOutcome::Dismiss
        );
    }

    #[test]
    fn footer_hint_explicit_when_restored_paused() {
        // The paused footer says what the key does, in words the
        // operator can act on, and never repeats a wire value or a request
        // type name.
        let paused = agents_footer_hint(true, AgentScope::All, 0);
        assert!(paused.contains("Tree paused"), "{paused}");
        assert!(paused.contains("r resumes"), "{paused}");
        assert!(!paused.contains("ResumeTree"), "{paused}");
        assert!(!paused.contains("restored_paused"), "{paused}");
        let live = agents_footer_hint(false, AgentScope::All, 0);
        assert!(live.contains("Enter transcript"), "{live}");
        assert!(live.contains("all agents · f live"), "{live}");
        assert!(live.contains("R refresh"), "{live}");
    }

    #[test]
    fn resume_only_when_restored_paused() {
        let mut live = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0)]));
        assert_eq!(
            live.handle_key(TopologyNavigatorKey::Resume),
            TopologyNavigatorOutcome::Consumed
        );
        let mut paused = snapshot(vec![node("root", None, 0)]);
        paused.restored_paused = true;
        paused.state = "restored_paused".into();
        let mut nav = TreeNavigator::from_snapshot(paused);
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::Resume),
            TopologyNavigatorOutcome::Confirm(crate::tui::UserAction::ResumeTopologyTree)
        );
    }

    #[test]
    fn destructive_keys_ask_for_confirmation_and_never_act() {
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
            node("root", None, 0),
            node("a", Some("root"), 1),
        ]));
        nav.focus_on("a");
        for (key, action) in [
            (
                TopologyNavigatorKey::Interrupt,
                crate::tui::UserAction::InterruptTopologyNode {
                    agent_id: "a".into(),
                },
            ),
            (
                TopologyNavigatorKey::Close,
                crate::tui::UserAction::CloseTopologyNode {
                    agent_id: "a".into(),
                },
            ),
            (
                TopologyNavigatorKey::StopAll,
                crate::tui::UserAction::StopAllTopology,
            ),
        ] {
            assert_eq!(
                nav.handle_key(key),
                TopologyNavigatorOutcome::Confirm(action)
            );
            // The navigator holds no arm: a following Enter opens the
            // transcript, as it always does.
            assert!(matches!(
                nav.handle_key(TopologyNavigatorKey::Enter),
                TopologyNavigatorOutcome::ShowAgent { .. }
            ));
        }
    }

    #[test]
    fn apply_snapshot_preserves_selection_by_id() {
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
            node("root", None, 0),
            node("a", Some("root"), 1),
        ]));
        nav.focus_on("a");
        let mut next = snapshot(vec![
            node("root", None, 0),
            node("a", Some("root"), 1),
            node("b", Some("root"), 1),
        ]);
        next.revision = 4;
        nav.apply_snapshot(next);
        assert_eq!(nav.selected_agent_id(), Some("a"));
        assert_eq!(nav.revision(), Some(4));
    }

    #[test]
    fn a_non_advancing_snapshot_is_dropped() {
        // §5.10: a snapshot whose revision does not advance is dropped. The
        // host dedups too, but an explicit `R` re-pushes the same revision.
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
            node("root", None, 0),
            node("a", Some("root"), 1),
        ]));
        nav.focus_on("a");

        // Same revision: dropped whole.
        let same = snapshot(vec![node("root", None, 0)]);
        assert_eq!(same.revision, 3);
        nav.apply_snapshot(same);
        assert_eq!(nav.revision(), Some(3));
        assert_eq!(nav.selected_agent_id(), Some("a"), "selection is untouched");

        // An older revision is dropped too.
        let mut older = snapshot(vec![node("root", None, 0)]);
        older.revision = 2;
        nav.apply_snapshot(older);
        assert_eq!(nav.revision(), Some(3));

        // An advancing revision is applied.
        let mut newer = snapshot(vec![node("root", None, 0), node("a", Some("root"), 1)]);
        newer.revision = 4;
        nav.apply_snapshot(newer);
        assert_eq!(nav.revision(), Some(4));
    }

    #[test]
    fn progress_only_snapshot_replaces_observations() {
        let mut row = node("root", None, 0);
        row.live_progress = Some(plakous_host_protocol::views::ChildProgress {
            turn_id: "turn-1".into(),
            ..Default::default()
        });
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![row.clone()]));
        row.live_progress.as_mut().unwrap().tool_calls = 2;
        nav.apply_snapshot(snapshot(vec![row]));
        assert_eq!(
            nav.selected_node()
                .unwrap()
                .live_progress
                .as_ref()
                .unwrap()
                .tool_calls,
            2
        );
    }

    #[test]
    fn awaiting_esc_dismisses() {
        let mut nav = TreeNavigator::awaiting();
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::Escape),
            TopologyNavigatorOutcome::Dismiss
        );
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::Enter),
            TopologyNavigatorOutcome::Consumed
        );
    }

    fn terminal_node(id: &str, parent: Option<&str>, depth: u8, status: &str) -> CanvasNode {
        let mut node = node(id, parent, depth);
        node.status = status.to_owned();
        node.finished = true;
        // Ended at the navigator's clock, so it is listed as just finished.
        node.ended_at_unix = Some(0);
        node
    }

    /// root
    /// ├── a ── a1, a2
    /// └── b ── b1
    fn family() -> CanvasTree {
        snapshot(vec![
            node("root", None, 0),
            node("a", Some("root"), 1),
            node("a1", Some("a"), 2),
            node("a2", Some("a"), 2),
            node("b", Some("root"), 1),
            node("b1", Some("b"), 2),
        ])
    }

    #[test]
    fn arrows_follow_visible_rows_and_outline_relations() {
        let mut nav = TreeNavigator::from_snapshot(family());
        assert_eq!(nav.selected_agent_id(), Some("root"));
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::Down),
            TopologyNavigatorOutcome::Consumed
        );
        assert_eq!(nav.selected_agent_id(), Some("a"));
        let _ = nav.handle_key(TopologyNavigatorKey::Down);
        assert_eq!(nav.selected_agent_id(), Some("a1"));
        let _ = nav.handle_key(TopologyNavigatorKey::Right);
        assert_eq!(nav.selected_agent_id(), Some("a1"));
        let _ = nav.handle_key(TopologyNavigatorKey::Left);
        assert_eq!(nav.selected_agent_id(), Some("a"));
        let _ = nav.handle_key(TopologyNavigatorKey::Right);
        assert_eq!(nav.selected_agent_id(), Some("a1"));
    }

    #[test]
    fn up_and_down_cross_subtrees_in_visible_preorder() {
        let mut nav = TreeNavigator::from_snapshot(family());
        nav.focus_on("a2");
        let _ = nav.handle_key(TopologyNavigatorKey::Down);
        assert_eq!(nav.selected_agent_id(), Some("b"));
        let _ = nav.handle_key(TopologyNavigatorKey::Down);
        assert_eq!(nav.selected_agent_id(), Some("b1"));
        let _ = nav.handle_key(TopologyNavigatorKey::Up);
        assert_eq!(nav.selected_agent_id(), Some("b"));
    }

    #[test]
    fn right_prefers_the_remembered_child_and_stops_at_a_leaf() {
        let mut nav = TreeNavigator::from_snapshot(family());
        nav.focus_on("b");
        let _ = nav.handle_key(TopologyNavigatorKey::Right);
        assert_eq!(nav.selected_agent_id(), Some("b1"));
        nav.focus_on("a1");
        let _ = nav.handle_key(TopologyNavigatorKey::Right);
        assert_eq!(nav.selected_agent_id(), Some("a1"));
    }

    #[test]
    fn left_climbs_by_parent_and_stops_at_a_root() {
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
            node("root", None, 0),
            node("a", Some("root"), 1),
            node("b", Some("root"), 1),
            node("a1", Some("a"), 2),
            // The parent id is not in the snapshot, so the layout severs the
            // edge and promotes this node to a root on depth 0.
            node("stray", Some("missing"), 0),
        ]));
        nav.focus_on("a1");
        let _ = nav.handle_key(TopologyNavigatorKey::Left);
        assert_eq!(nav.selected_agent_id(), Some("a"));
        let _ = nav.handle_key(TopologyNavigatorKey::Left);
        assert_eq!(nav.selected_agent_id(), Some("root"));
        nav.focus_on("stray");
        let _ = nav.handle_key(TopologyNavigatorKey::Left);
        assert_eq!(nav.selected_agent_id(), Some("stray"));
    }

    #[test]
    fn home_returns_to_the_root() {
        let mut nav = TreeNavigator::from_snapshot(family());
        nav.focus_on("b1");
        let _ = nav.handle_key(TopologyNavigatorKey::Home);
        assert_eq!(nav.selected_agent_id(), Some("root"));
    }

    #[test]
    fn selection_binds_to_the_agent_and_survives_it_leaving_the_list() {
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
            node("root", None, 0),
            terminal_node("done", Some("root"), 1, "completed"),
            node("live", Some("root"), 1),
        ]));
        assert_eq!(nav.scope, AgentScope::Live);
        nav.focus_on("done");
        assert_eq!(nav.selected_agent_id(), Some("done"));
        // Five minutes on, the finished agent leaves the live list: its
        // nearest listed ancestor stands in, never an unrelated row.
        nav.set_now(RECENT_PAST);
        assert_eq!(nav.selected_agent_id(), Some("root"));
        assert!(
            nav.snapshot
                .as_ref()
                .unwrap()
                .nodes
                .iter()
                .any(|node| node.agent_id == "done"),
            "hidden is a projection; the host snapshot still carries it"
        );
        let _ = nav.handle_key(TopologyNavigatorKey::Filter);
        assert_eq!(nav.scope, AgentScope::All);
        assert_eq!(nav.selected_agent_id(), Some("done"), "and it comes back");
        let _ = nav.handle_key(TopologyNavigatorKey::Filter);
        let mut next = nav.snapshot.clone().unwrap();
        next.revision = 9;
        next.nodes.push(node("new", Some("root"), 1));
        nav.apply_snapshot(next);
        nav.focus_on("live");
        let mut fewer = nav.snapshot.clone().unwrap();
        fewer.revision = 10;
        fewer.nodes.retain(|node| node.agent_id != "new");
        nav.apply_snapshot(fewer);
        assert_eq!(
            nav.selected_agent_id(),
            Some("live"),
            "rows leaving do not move it"
        );
    }

    const RECENT_PAST: u64 = agent_list::RECENT_SECS + 1;

    #[test]
    fn x_acknowledges_the_selected_attempt_and_waits_for_the_product() {
        let mut failed = terminal_node("tests", Some("root"), 1, "failed");
        failed.attempt_status = Some("failed".into());
        failed.attempt_id = Some("tests-a1".into());
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), failed]));
        nav.set_now(3_600);
        nav.focus_on("tests");
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::Acknowledge),
            TopologyNavigatorOutcome::Action(crate::tui::UserAction::Extension(
                SurfaceIntent::AcknowledgeAgent {
                    tree_id: "tree1".into(),
                    agent_id: "tests".into(),
                    attempt_id: "tests-a1".into(),
                    attention: "failed".into(),
                    ended_at: Some(0),
                }
            ))
        );
        assert_eq!(
            nav.selected_agent_id(),
            Some("tests"),
            "no optimistic acknowledgement: the row stays until the product answers"
        );
        let covered = BTreeSet::from([AgentAcknowledgement {
            agent_id: "tests".into(),
            attempt_id: "tests-a1".into(),
            attention: "failed".into(),
            ended_at: Some(0),
        }]);
        nav.set_acknowledged("other-tree", &covered);
        assert_eq!(
            nav.projection().counts.attention,
            1,
            "another tree's set is ignored"
        );
        nav.set_acknowledged("tree1", &covered);
        assert_eq!(nav.projection().counts.attention, 0);
        assert_eq!(nav.selected_agent_id(), Some("root"));
        nav.focus_on("tests");
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::Acknowledge),
            TopologyNavigatorOutcome::Consumed,
            "an acknowledged attempt is not acknowledged twice"
        );
    }

    #[test]
    fn the_inspector_offers_x_only_when_acknowledgement_can_be_recorded() {
        let mut root = node("root", None, 0);
        root.status = "failed".into();
        root.finished = true;
        let mut bare = node("bare", Some("root"), 1);
        bare.status = "failed".into();
        bare.finished = true;
        bare.ended_at_unix = Some(0);
        let mut failed = terminal_node("tests", Some("root"), 1, "failed");
        failed.attempt_status = Some("failed".into());
        failed.attempt_id = Some("tests-a1".into());
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![root, bare, failed]));
        nav.set_now(10);
        for id in ["root", "bare"] {
            nav.focus_on(id);
            let frame = nav.frame(80, 24, None, &BTreeMap::new(), None);
            assert!(
                frame
                    .inspector
                    .iter()
                    .all(|line| !line.contains("x acknowledges")),
                "{id} cannot be acknowledged: {:?}",
                frame.inspector
            );
        }
        nav.focus_on("tests");
        let frame = nav.frame(80, 24, None, &BTreeMap::new(), None);
        assert!(
            frame
                .inspector
                .iter()
                .any(|line| line.contains("x acknowledges")),
            "{:?}",
            frame.inspector
        );
    }

    #[test]
    fn refresh_emits_the_refresh_intent() {
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0)]));
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::Refresh),
            TopologyNavigatorOutcome::Action(crate::tui::UserAction::RefreshTopologyTree)
        );
    }

    #[test]
    fn copy_yields_the_focused_id_on_the_canvas() {
        let mut nav = TreeNavigator::from_snapshot(family());
        nav.focus_on("b1");
        assert_eq!(nav.copy_text(), "b1");
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::Copy),
            TopologyNavigatorOutcome::Consumed
        );
        assert_eq!(nav.selected_agent_id(), Some("b1"));
    }

    #[test]
    fn keys_paint_no_optimistic_transition() {
        let mut nav = TreeNavigator::from_snapshot(family());
        let before = nav.snapshot.clone();
        for key in [
            TopologyNavigatorKey::Interrupt,
            TopologyNavigatorKey::Close,
            TopologyNavigatorKey::StopAll,
            TopologyNavigatorKey::Refresh,
        ] {
            let _ = nav.handle_key(key);
        }
        assert_eq!(nav.snapshot, before);
    }

    #[test]
    fn restored_paused_never_auto_resumes() {
        let mut paused = family();
        paused.restored_paused = true;
        paused.state = "restored_paused".into();
        let mut nav = TreeNavigator::from_snapshot(paused);
        // Every navigation key is inert with respect to admission state.
        for key in [
            TopologyNavigatorKey::Down,
            TopologyNavigatorKey::Up,
            TopologyNavigatorKey::Home,
            TopologyNavigatorKey::Filter,
            TopologyNavigatorKey::Left,
            TopologyNavigatorKey::Right,
        ] {
            assert_eq!(nav.handle_key(key), TopologyNavigatorOutcome::Consumed);
        }
        assert!(nav.snapshot.as_ref().unwrap().restored_paused);
        // Only an explicit r, confirmed in the pop-up, leaves RestoredPaused.
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::Resume),
            TopologyNavigatorOutcome::Confirm(crate::tui::UserAction::ResumeTopologyTree)
        );
        assert!(nav.snapshot.as_ref().unwrap().restored_paused);
    }

    #[test]
    fn child_only_revision_reaches_the_canvas() {
        let mut nav = TreeNavigator::from_snapshot(family());
        nav.focus_on("b1");
        let mut next = family();
        next.revision = 4;
        for node in &mut next.nodes {
            if node.agent_id == "a1" {
                node.status = "completed".into();
                node.finished = true;
                node.ended_at_unix = Some(0);
            }
        }
        nav.apply_snapshot(next);
        assert_eq!(nav.revision(), Some(4));
        assert_eq!(nav.selected_agent_id(), Some("b1"));
        let frame = paint(&mut nav, 200, 44, 0);
        let painted = painted_text(&frame);
        let a1 = painted
            .lines()
            .find(|line| line.contains("#a1"))
            .expect("a1 is listed");
        assert!(a1.contains("✓      —  #a1  implement"), "{painted}");
        assert!(a1.trim_end().ends_with(" done"), "{painted}");
    }

    /// The root box follows the header's turn phase, not the tree's
    /// permanent `running`.
    #[test]
    fn root_box_paints_idle_when_no_turn_is_in_flight() {
        let mut nav = TreeNavigator::from_snapshot(family());
        assert!(nav.project_root_turn(false));
        assert!(!nav.project_root_turn(false), "no change, no repaint");
        let frame = paint(&mut nav, 200, 44, 0);
        assert!(frame.inspector[0].contains("idle"), "{:?}", frame.inspector);
        let root = &nav.snapshot.as_ref().unwrap().nodes[0];
        assert!(root.parent_agent_id.is_none());
        assert_eq!(root.status, "idle");
        assert_eq!(status_glyph("idle"), '\u{25cc}');
        assert!(nav.project_root_turn(true));
        assert_eq!(nav.snapshot.as_ref().unwrap().nodes[0].status, "running");
    }

    /// A faulted child says why in the inspector instead of a brief
    /// nobody is executing.
    #[test]
    fn inspector_shows_the_fault_reason_in_place_of_the_task() {
        let mut child = node("c1", Some("root"), 1);
        child.status = "recovery_required".into();
        child.finished = true;
        child.attempt_id = Some("c1-a1".into());
        child.fault = Some("child c1 could not start: no credential".into());
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
        nav.focus_on("c1");
        let frame = paint(&mut nav, 200, 44, 0);
        assert!(
            frame.inspector[1].starts_with("Fault   child c1 could not start"),
            "{:?}",
            frame.inspector
        );
    }

    #[test]
    fn agent_rows_and_inspector_show_derived_compaction_counts() {
        let mut child = node("c1", Some("root"), 1);
        child.usage = Some(plakous_host_protocol::views::AttemptUsage {
            compaction: Some(plakous_host_protocol::views::AttemptCompaction {
                committed: 2,
                failed: 1,
                no_eligible: 3,
                tokens_before: Some(900),
                tokens_after: Some(300),
                complete: false,
            }),
            ..Default::default()
        });
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
        nav.focus_on("c1");
        let frame = paint(&mut nav, 200, 44, 0);
        assert!(
            frame.inspector.iter().any(|line| line.contains(
                "Compact 2 committed · 1 failed · 3 no-eligible · 900 -> 300 input · retained only"
            )),
            "{:?}",
            frame.inspector
        );
        assert!(
            painted_text(&frame).contains("2 compact"),
            "{}",
            painted_text(&frame)
        );
    }

    #[test]
    fn canvas_frame_draws_outline_rails_status_inspector_and_footer() {
        let mut nav = TreeNavigator::from_snapshot(family());
        nav.focus_on("a1");
        let frame = paint(&mut nav, 200, 44, 0);
        let painted = painted_text(&frame);
        assert!(painted.contains("├─"), "{painted}");
        assert!(painted.contains("└─"), "{painted}");
        assert!(painted.contains("│ ├─"), "{painted}");
        assert!(painted.contains('\u{25cf}'), "{painted}");
        assert_eq!(frame.inspector.len(), usize::from(INSPECTOR_ROWS));
        assert!(frame.inspector[0].contains("#a1"));
        assert!(frame.inspector[2].contains("grok-4.6"));
        assert!(frame.footer.contains("Enter transcript"));
        assert_eq!(frame.rows.len(), 6);
        assert!(frame.rows.iter().any(|row| row.id == "root"));
        assert!(frame.header.contains("5 active"), "{}", frame.header);
    }

    #[test]
    fn short_overlays_drop_the_inspector() {
        let mut nav = TreeNavigator::from_snapshot(family());
        let frame = paint(&mut nav, 60, 8, 0);
        assert!(frame.inspector.is_empty());
        // The header takes a row and the long footer would need more than
        // its two rows at 60 columns, so the short one takes one (§5.5); the
        // list scrolls inside what is left.
        assert!(!frame.footer.contains("interrupt"), "{}", frame.footer);
        assert_eq!(frame.rows.len(), 6);
    }

    #[test]
    fn row_and_inspector_labels_never_invent_state() {
        let mut unknown = node("root", None, 0);
        unknown.turns = None;
        unknown.started_at_unix = None;
        unknown.model_tuple = String::new();
        unknown.model_account = String::new();
        unknown.model_effort = None;
        unknown.prompt = String::new();
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![unknown]));
        let frame = paint(&mut nav, 120, 30, 100);
        assert!(
            painted_text(&frame).contains("\u{25cf}      \u{2014}  #root"),
            "the unknown elapsed reads as a dash: {frame:?}"
        );
        assert!(frame.inspector[1].contains('\u{2014}'));
        assert!(frame.inspector[2].contains('\u{2014}'));
        assert!(frame.inspector[3].contains("turns \u{2014}"));
    }

    #[test]
    fn elapsed_and_turn_labels_format_known_values() {
        assert_eq!(elapsed_label(Some(100), 141), "41s");
        assert_eq!(elapsed_label(Some(100), 234), "2m14s");
        assert_eq!(elapsed_label(Some(100), 4000), "1h05m");
        assert_eq!(elapsed_label(None, 4000), "\u{2014}");
        assert_eq!(turns_label(Some(1)), "1 turn");
        assert_eq!(turns_label(Some(12)), "12 turns");
    }

    #[test]
    fn status_glyphs_follow_the_spec_table() {
        assert_eq!(status_glyph("pending"), '\u{25cb}');
        assert_eq!(status_glyph("running"), '\u{25d0}');
        assert_eq!(status_glyph("completed"), '\u{25cf}');
        assert_eq!(status_glyph("interrupted"), '\u{25a0}');
        assert_eq!(status_glyph("closed"), '\u{25a0}');
        assert_eq!(status_glyph("failed"), '\u{d7}');
    }

    /// Regression: `status_glyph` must render every lifecycle label
    /// and no test failed. Every label the host can project now has an arm, and
    /// a new one that does not is a failure here rather than a dot on the
    /// canvas nobody reports.
    #[test]
    fn every_projected_status_label_has_its_own_glyph() {
        for label in PROJECTED_STATUS_LABELS {
            assert_ne!(
                status_glyph(label),
                UNKNOWN_STATUS_GLYPH,
                "`{label}` falls through to the unknown glyph"
            );
        }
        assert_eq!(
            status_glyph("not_a_status"),
            UNKNOWN_STATUS_GLYPH,
            "an unprojected label must still be total, not a panic"
        );
    }

    /// sealing (a node) and adopting (a workspace row) are two
    /// different things in flight and must not read as one another, nor as the
    /// `·` both used to fall to.
    #[test]
    fn sealing_and_adopting_have_distinct_glyphs() {
        let sealing = status_glyph("sealing");
        let adopting = status_glyph("adopting");
        let adopted = status_glyph("adopted");
        let blocked = status_glyph("blocked");
        for (name, glyph) in [
            ("sealing", sealing),
            ("adopting", adopting),
            ("adopted", adopted),
            ("blocked", blocked),
        ] {
            assert_ne!(glyph, UNKNOWN_STATUS_GLYPH, "{name} is still the dot");
            assert_ne!(glyph, status_glyph("running"), "{name} reads as running");
            assert_ne!(
                glyph,
                status_glyph("completed"),
                "{name} reads as completed"
            );
        }
        assert_ne!(sealing, adopting);
        assert_ne!(adopting, adopted);
        assert_ne!(sealing, blocked);
    }

    /// Plan §F trap 2: the session's turn phase knew only `running`/`idle` and
    /// painted them over a root whose durable status the phase cannot see —
    /// erasing the lifecycle observation.
    #[test]
    fn a_sealing_or_adopting_root_is_not_overwritten_by_the_running_projection() {
        for durable in ["sealing", "adopting", "blocked", "paused", "failed"] {
            let mut root = node("root", None, 0);
            root.status = durable.to_owned();
            let mut nav = TreeNavigator::from_snapshot(snapshot(vec![root]));
            assert!(
                !nav.project_root_turn(false),
                "`{durable}` was repainted idle"
            );
            assert!(
                !nav.project_root_turn(true),
                "`{durable}` was repainted running"
            );
            assert_eq!(nav.snapshot.as_ref().unwrap().nodes[0].status, durable);
        }
        // The phase still owns the two labels that mean exactly what it knows.
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0)]));
        assert!(nav.project_root_turn(false));
        assert_eq!(nav.snapshot.as_ref().unwrap().nodes[0].status, "idle");
    }

    /// the attempt is the unit, and a `followup` re-task opens a
    /// fresh one on the same node. The row shows the current attempt.
    #[test]
    fn a_retasked_node_shows_its_new_attempt() {
        let mut child = node("c1", Some("root"), 1);
        child.attempt_seq = Some(1);
        child.attempt_status = Some("completed".into());
        let first = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
        assert_eq!(
            attempt_label(&first.snapshot.as_ref().unwrap().nodes[1]),
            "a1 · completed"
        );

        let mut retasked = node("c1", Some("root"), 1);
        retasked.attempt_seq = Some(2);
        retasked.attempts = vec![1, 2];
        retasked.attempt_status = Some("running".into());
        let mut nav = first;
        let mut next = snapshot(vec![node("root", None, 0), retasked]);
        next.revision = 4;
        nav.apply_snapshot(next);
        let row = &nav.snapshot.as_ref().unwrap().nodes[1];
        assert_eq!(attempt_label(row), "a2 · running");
        nav.focus_on("c1");
        let frame = paint(&mut nav, 200, 44, 0);
        assert!(
            frame.inspector.iter().any(|line| line.contains("a2")),
            "{:?}",
            frame.inspector
        );
        // Both attempts are listed and the current one is marked.
        let work = frame
            .inspector
            .iter()
            .find(|line| line.starts_with("Work"))
            .unwrap();
        assert!(work.contains("a1 \u{2039}a2\u{203a}"), "{work}");
    }

    /// One attempt is already named on the `Bind` line, so the strip stays out
    /// of the way until there is a history to show.
    #[test]
    fn a_single_attempt_draws_no_strip() {
        let mut child = node("c1", Some("root"), 1);
        child.attempt_seq = Some(1);
        child.attempts = vec![1];
        assert_eq!(attempts_strip(&child), "");
        child.attempts = vec![1, 2, 3];
        child.attempt_seq = Some(3);
        assert_eq!(attempts_strip(&child), "a1 a2 \u{2039}a3\u{203a}");
    }

    /// A node with no attempt row says so rather than borrowing `a1`.
    #[test]
    fn a_node_without_an_attempt_says_so() {
        assert_eq!(attempt_label(&node("root", None, 0)), "no attempt");
    }

    /// a finished mutate/execute child carries the candidate it
    /// sealed, and the box says which one instead of an elapsed clock.
    #[test]
    fn a_sealed_child_names_its_candidate_on_the_box_and_in_the_inspector() {
        let mut child = terminal_node("c1", Some("root"), 1, "completed");
        child.tier = Some("mutate".into());
        child.workspace_mode = "fork".into();
        child.workspace_status = Some("adopted".into());
        child.candidate_id = Some("cand-0f3a91b2".into());
        child.attempt_seq = Some(1);
        child.attempt_status = Some("adopted".into());
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
        nav.focus_on("c1");
        let frame = paint(&mut nav, 200, 44, 0);
        let painted = painted_text(&frame);
        assert!(painted.contains("adopted · #0f3a91b2"), "{painted}");
        let work = frame
            .inspector
            .iter()
            .find(|line| line.starts_with("Work"))
            .unwrap();
        assert!(work.contains("tier mutate"), "{work}");
        assert!(work.contains("copy adopted"), "{work}");
        assert!(work.contains("sealed · #0f3a91b2"), "{work}");
    }

    /// a quota refusal marks the *attempt* blocked and the node
    /// `Failed`, so the reset time is only readable from the attempt. The row
    /// shows how long the wait is, not just that there is one.
    #[test]
    fn a_blocked_attempt_shows_when_its_quota_resets() {
        let now = 1_000_000_u64;
        let mut child = terminal_node("c1", Some("root"), 1, "failed");
        child.attempt_seq = Some(2);
        child.attempt_id = Some("c1-a2".into());
        child.attempt_status = Some("blocked".into());
        child.blocked = Some(BlockedObservation {
            quota: "weekly".into(),
            account: "codex".into(),
            resets_at_unix: Some(now + 45 * 60),
        });
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
        nav.focus_on("c1");
        let frame = paint(&mut nav, 200, 44, now);
        let painted = painted_text(&frame);
        assert!(painted.contains("blocked · resets in 45m"), "{painted}");
        let work = frame
            .inspector
            .iter()
            .find(|line| line.starts_with("Work"))
            .unwrap();
        assert!(work.contains("blocked weekly on codex"), "{work}");
        assert!(work.contains("resets in 45m"), "{work}");
    }

    /// Without a `resets_at` the row says `blocked` and stops: the provider
    /// named no clock and the canvas does not invent one.
    #[test]
    fn a_blocked_attempt_without_a_reset_invents_no_clock() {
        let mut child = terminal_node("c1", Some("root"), 1, "failed");
        child.attempt_id = Some("c1-a1".into());
        child.blocked = Some(BlockedObservation {
            quota: "5h".into(),
            account: "grok".into(),
            resets_at_unix: None,
        });
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
        nav.focus_on("c1");
        let frame = paint(&mut nav, 200, 44, 0);
        let painted = painted_text(&frame);
        assert!(painted.contains("blocked"), "{painted}");
        assert!(!painted.contains("resets in"), "{painted}");
    }

    /// A reset that has already passed reads `now`, never a wrapped duration.
    #[test]
    fn a_reset_in_the_past_reads_now() {
        assert_eq!(until_label(10, 500), "now");
        assert_eq!(until_label(530, 500), "30s");
        assert_eq!(until_label(500 + 3600 + 120, 500), "1h02m");
        assert_eq!(until_label(500 + 86_400 * 2, 500), "2d00h");
    }

    /// the box says which template and which tier, never the
    /// retired class or write effect.
    #[test]
    fn the_row_carries_the_template_and_the_tier() {
        let mut child = node("c1", Some("root"), 1);
        child.template = Some("review".into());
        child.tier = Some("read_only".into());
        child.workspace_mode = "snapshot".into();
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
        nav.focus_on("c1");
        let frame = paint(&mut nav, 200, 44, 0);
        let painted = painted_text(&frame);
        assert!(painted.contains("review"), "{painted}");
        let bind = frame
            .inspector
            .iter()
            .find(|line| line.starts_with("Model"))
            .unwrap();
        assert!(bind.contains("snapshot"), "{bind}");
        let work = frame
            .inspector
            .iter()
            .find(|line| line.starts_with("Work"))
            .unwrap();
        assert!(work.contains("tier read_only"), "{work}");
    }

    /// A tier the host did not project stays unknown; the inspector never
    /// defaults a node to `read_only`, which would understate what it may do.
    #[test]
    fn an_unknown_tier_stays_unknown() {
        let node = node("c1", Some("root"), 1);
        assert!(work_line(&node, 0).contains("tier —"));
    }

    fn paint(nav: &mut TreeNavigator, width: u16, height: u16, now: u64) -> AgentListFrame {
        nav.set_now(now);
        nav.frame(width, height, None, &BTreeMap::new(), None)
    }

    fn painted_text(frame: &AgentListFrame) -> String {
        frame
            .rows
            .iter()
            .map(|row| format!("{}{}{}", row.cells.rails, row.cells.body, row.cells.mark))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn orphaned_children_survive_a_hidden_parent() {
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
            node("root", None, 0),
            terminal_node("mid", Some("root"), 1, "closed"),
            node("leaf", Some("mid"), 2),
        ]));
        nav.set_now(10 * 60);
        let frame = paint(&mut nav, 120, 30, 10 * 60);
        assert_eq!(
            frame
                .rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["root", "mid", "leaf"],
            "a long-closed parent stays, dimmed, to explain its running child"
        );
        assert!(frame.rows[1].dim);
    }
}

#[cfg(test)]
mod paused_copy_tests {
    use super::{AgentScope, RESTORED_PAUSED_NAVIGATOR_HINT, agents_footer_hint};

    /// The canvas footer read
    /// `restored_paused · r ResumeTree` — a wire state and a typed enum
    /// variant where the operator needs prose and a keystroke.
    #[test]
    fn paused_canvas_copy_is_prose_with_a_remedy() {
        let footer = agents_footer_hint(true, AgentScope::default(), 0);
        assert!(footer.contains("Tree paused"), "{footer}");
        assert!(footer.contains("r resumes"), "{footer}");
        for copy in [footer.as_str(), RESTORED_PAUSED_NAVIGATOR_HINT] {
            assert!(!copy.contains("ResumeTree"), "{copy}");
            assert!(!copy.contains("restored_paused"), "{copy}");
        }
        assert!(RESTORED_PAUSED_NAVIGATOR_HINT.contains("r resumes"));

        let live = agents_footer_hint(false, AgentScope::default(), 24);
        assert!(
            live.starts_with("active + last 5m · 24 older hidden · f all"),
            "{live}"
        );
        assert!(live.contains("↑/↓ select"), "{live}");
        assert!(live.ends_with("Esc back"), "{live}");
        assert!(live.contains("x acknowledge"), "{live}");
        assert!(!live.contains("ResumeTree"), "{live}");
    }
}
