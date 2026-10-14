//! The Agents tab's live list: which agents are shown, which need attention,
//! and the one-line row each paints.
//!
//! Presentation only. The full tree arrives as a host snapshot and is never
//! mutated here; visibility is a projection over it (active, ancestors of what
//! is shown, recently finished, unacknowledged attention), so changing the
//! policy never touches session data. Parent links are hardened into a forest
//! and siblings keep spawn order, so rows do not reorder under the operator.

use std::collections::{BTreeMap, BTreeSet};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::AgentAcknowledgement;
use crate::tui::CanvasNode;

/// A successfully finished agent stays in the live list this long.
pub const RECENT_SECS: u64 = 5 * 60;
/// After this long a recently finished agent paints dimmed.
pub const DIM_AFTER_SECS: u64 = 2 * 60;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutNode {
    pub id: String,
    pub parent: Option<String>,
    pub started_at_unix: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Placement {
    pub id: String,
    pub depth: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreeLayout {
    placements: Vec<Placement>,
    children: BTreeMap<String, Vec<String>>,
    parent: BTreeMap<String, String>,
    roots: Vec<String>,
}

impl TreeLayout {
    /// Build a forest in visible preorder. Siblings sort by start time then id.
    pub fn new(nodes: &[LayoutNode]) -> Self {
        let mut ids = Vec::new();
        let mut seen = BTreeSet::new();
        for node in nodes {
            if seen.insert(node.id.as_str()) {
                ids.push(node.id.clone());
            }
        }
        let known: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        let mut parent = BTreeMap::new();
        for node in nodes {
            if let Some(up) = node.parent.as_deref()
                && up != node.id
                && known.contains(up)
            {
                parent
                    .entry(node.id.clone())
                    .or_insert_with(|| up.to_owned());
            }
        }
        break_cycles(&ids, &mut parent);

        let started: BTreeMap<&str, Option<u64>> = nodes
            .iter()
            .map(|node| (node.id.as_str(), node.started_at_unix))
            .collect();
        let mut children: BTreeMap<String, Vec<String>> =
            ids.iter().map(|id| (id.clone(), Vec::new())).collect();
        for id in &ids {
            if let Some(up) = parent.get(id) {
                children.entry(up.clone()).or_default().push(id.clone());
            }
        }
        for siblings in children.values_mut() {
            siblings.sort_by(|a, b| {
                let a_stamp = started.get(a.as_str()).copied().flatten();
                let b_stamp = started.get(b.as_str()).copied().flatten();
                a_stamp
                    .is_none()
                    .cmp(&b_stamp.is_none())
                    .then(a_stamp.cmp(&b_stamp))
                    .then_with(|| a.cmp(b))
            });
        }
        let mut roots: Vec<String> = ids
            .iter()
            .filter(|id| !parent.contains_key(*id))
            .cloned()
            .collect();
        roots.sort_unstable();

        let mut placements = Vec::with_capacity(ids.len());
        let mut reached = BTreeSet::new();
        for root in &roots {
            preorder(root, 0, &children, &mut reached, &mut placements);
        }
        // Defensive fallback: malformed input still renders every unique id.
        for id in ids {
            if !reached.contains(&id) {
                parent.remove(&id);
                roots.push(id.clone());
                preorder(&id, 0, &children, &mut reached, &mut placements);
            }
        }
        Self {
            placements,
            children,
            parent,
            roots,
        }
    }

    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }
    pub fn children(&self, id: &str) -> &[String] {
        self.children.get(id).map_or(&[], Vec::as_slice)
    }
    pub fn parent(&self, id: &str) -> Option<&str> {
        self.parent.get(id).map(String::as_str)
    }
    pub fn roots(&self) -> &[String] {
        &self.roots
    }
}

fn break_cycles(ids: &[String], parent: &mut BTreeMap<String, String>) {
    for id in ids {
        let mut seen = BTreeSet::from([id.clone()]);
        let mut cursor = id.clone();
        while let Some(next) = parent.get(&cursor).cloned() {
            if !seen.insert(next.clone()) {
                parent.remove(&cursor);
                break;
            }
            cursor = next;
        }
    }
}

fn preorder(
    id: &str,
    depth: usize,
    children: &BTreeMap<String, Vec<String>>,
    reached: &mut BTreeSet<String>,
    out: &mut Vec<Placement>,
) {
    if !reached.insert(id.to_owned()) {
        return;
    }
    out.push(Placement {
        id: id.to_owned(),
        depth,
    });
    for child in children.get(id).map_or(&[][..], Vec::as_slice) {
        preorder(child, depth.saturating_add(1), children, reached, out);
    }
}

/// Why a stopped agent needs the operator. Read from the node's *current*
/// attempt only, so a `followup` that opens a new attempt supersedes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Attention {
    /// The attempt failed, including a provider cancel and a refused recovery.
    Failed,
    /// A provider quota refused the attempt.
    Blocked,
    /// The turn ended without `submit_result`.
    NoSubmission,
    /// The node needs `recover_agent` before anything else.
    RecoveryRequired,
}

impl Attention {
    /// The row's status fact.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::NoSubmission => "no result submitted",
            Self::RecoveryRequired => "needs recovery",
        }
    }
}

/// Running, waiting, sealing or adopting: anything that is not settled.
/// Interrupted counts: it waits on a resume and paints as waiting.
#[must_use]
pub fn is_active(node: &CanvasNode) -> bool {
    node.workspace_status.as_deref() == Some("adopting")
        || matches!(
            node.status.as_str(),
            "pending" | "running" | "paused" | "sealing" | "interrupted"
        )
}

/// What the current attempt asks of the operator, if anything.
///
/// The root and a node with no attempt never qualify: nothing can clear that
/// mark. Closing the node, or an attempt that was closed or cancelled, is a
/// resolution rather than attention.
#[must_use]
pub fn attention(node: &CanvasNode) -> Option<Attention> {
    if node.parent_agent_id.is_none() || node.attempt_id.is_none() || is_active(node) {
        return None;
    }
    if node.status == "closed"
        || matches!(node.attempt_status.as_deref(), Some("closed" | "cancelled"))
    {
        return None;
    }
    if node.status == "recovery_required" {
        return Some(Attention::RecoveryRequired);
    }
    if node.blocked.is_some() {
        return Some(Attention::Blocked);
    }
    match node.attempt_status.as_deref() {
        Some("failed") => Some(Attention::Failed),
        Some("blocked") => Some(Attention::Blocked),
        Some("completed_without_submission") => Some(Attention::NoSubmission),
        Some("recovery_required") => Some(Attention::RecoveryRequired),
        None if node.status == "failed" => Some(Attention::Failed),
        _ => None,
    }
}

/// The operator acknowledged this occurrence: attempt, attention label and end stamp.
#[must_use]
pub fn acknowledged(node: &CanvasNode, acks: &BTreeSet<AgentAcknowledgement>) -> bool {
    let (Some(attempt), Some(attention)) = (node.attempt_id.as_deref(), attention(node)) else {
        return false;
    };
    acks.contains(&AgentAcknowledgement {
        agent_id: node.agent_id.clone(),
        attempt_id: attempt.to_owned(),
        attention: attention.label().to_owned(),
        ended_at: node.ended_at_unix,
    })
}

/// Which agents the list shows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AgentScope {
    /// Active, recent, attention, and the ancestors that explain them.
    #[default]
    Live,
    /// Every agent in the tree.
    All,
}

impl AgentScope {
    /// Stable label for the test pilot's observers.
    #[cfg(any(test, feature = "test-hooks"))]
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::All => "all",
        }
    }

    /// The other scope.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Live => Self::All,
            Self::All => Self::Live,
        }
    }
}

/// One visible row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRow {
    pub id: String,
    /// Connector rails before the glyph.
    pub prefix: String,
    /// Settled context: an ancestor, an agent past two minutes, or history.
    pub dim: bool,
    /// Unacknowledged attention.
    pub attention: Option<Attention>,
}

/// Header counts. The root is context, not work, and is in none of them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AgentCounts {
    pub active: usize,
    pub completed: usize,
    pub attention: usize,
}

/// The projected list.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentProjection {
    pub rows: Vec<AgentRow>,
    pub counts: AgentCounts,
    /// Agents the scope leaves out.
    pub hidden: usize,
    /// Forest over the visible rows, for parent/child moves.
    pub layout: TreeLayout,
}

impl AgentProjection {
    /// Row index of `id`, when it is shown.
    #[must_use]
    pub fn position(&self, id: &str) -> Option<usize> {
        self.rows.iter().position(|row| row.id == id)
    }
}

/// `completed` or `adopted` only. Closed and cancelled are neither completed
/// nor attention.
fn is_completed(node: &CanvasNode) -> bool {
    matches!(
        node.attempt_status.as_deref(),
        Some("completed" | "adopted")
    )
}

/// Soonest unix second a shown recent row dims or leaves the live list.
///
/// Each such row contributes `ended + 120` and `ended + 300`. The result is
/// never less than one second ahead, so age repaints stay at or below 1 Hz.
/// Attention rows and running rows are not included: attention does not age
/// out, and running rows keep the spinner cadence.
#[must_use]
pub fn next_age_boundary(
    nodes: &[CanvasNode],
    shown: &AgentProjection,
    now_unix: u64,
) -> Option<u64> {
    let mut next: Option<u64> = None;
    for row in &shown.rows {
        if row.attention.is_some() {
            continue;
        }
        let Some(node) = nodes.iter().find(|node| node.agent_id == row.id) else {
            continue;
        };
        if is_active(node) {
            continue;
        }
        let Some(ended) = node.ended_at_unix else {
            continue;
        };
        if now_unix.saturating_sub(ended) > RECENT_SECS {
            continue;
        }
        for mark in [DIM_AFTER_SECS, RECENT_SECS] {
            let at = ended.saturating_add(mark);
            if at <= now_unix {
                continue;
            }
            let at = at.max(now_unix.saturating_add(1));
            next = Some(next.map_or(at, |best| best.min(at)));
        }
    }
    next
}

/// Age since the current attempt ended; `None` without both stamps.
fn ended_age(node: &CanvasNode, now_unix: u64) -> Option<u64> {
    node.ended_at_unix
        .map(|ended| now_unix.saturating_sub(ended))
}

/// `Some(dim)` when the node is shown on its own account.
fn shown_on_own(
    node: &CanvasNode,
    acks: &BTreeSet<AgentAcknowledgement>,
    now_unix: u64,
) -> Option<bool> {
    if is_active(node) {
        return Some(false);
    }
    if attention(node).is_some() && !acknowledged(node, acks) {
        return Some(false);
    }
    if node.parent_agent_id.is_none() {
        return Some(false);
    }
    if attention(node).is_some() {
        return None;
    }
    ended_age(node, now_unix)
        .filter(|age| *age <= RECENT_SECS)
        .map(|age| age > DIM_AFTER_SECS)
}

/// Projects the snapshot's nodes onto the list `scope` shows at `now_unix`.
#[must_use]
pub fn project(
    nodes: &[CanvasNode],
    acks: &BTreeSet<AgentAcknowledgement>,
    now_unix: u64,
    scope: AgentScope,
) -> AgentProjection {
    let by_id: BTreeMap<&str, &CanvasNode> = nodes
        .iter()
        .map(|node| (node.agent_id.as_str(), node))
        .collect();
    let mut dim: BTreeMap<&str, bool> = BTreeMap::new();
    for node in nodes {
        let own = match scope {
            AgentScope::Live => shown_on_own(node, acks, now_unix),
            AgentScope::All => Some(shown_on_own(node, acks, now_unix).is_none_or(|dimmed| dimmed)),
        };
        if let Some(dimmed) = own {
            dim.insert(node.agent_id.as_str(), dimmed);
        }
    }
    // Ancestors explain what is shown; a settled one is dimmed.
    let shown: Vec<&str> = dim.keys().copied().collect();
    for id in shown {
        let mut seen = BTreeSet::from([id]);
        let mut cursor = by_id
            .get(id)
            .and_then(|node| node.parent_agent_id.as_deref());
        while let Some(parent) = cursor {
            if !seen.insert(parent) {
                break;
            }
            let Some(node) = by_id.get(parent) else {
                break;
            };
            dim.entry(parent).or_insert_with(|| !is_active(node));
            cursor = node.parent_agent_id.as_deref();
        }
    }
    let layout_nodes: Vec<LayoutNode> = nodes
        .iter()
        .filter(|node| dim.contains_key(node.agent_id.as_str()))
        .map(|node| LayoutNode {
            id: node.agent_id.clone(),
            parent: node.parent_agent_id.clone(),
            started_at_unix: node.started_at_unix,
        })
        .collect();
    let layout = TreeLayout::new(&layout_nodes);
    let rows = layout
        .placements()
        .iter()
        .filter_map(|placement| {
            let node = by_id.get(placement.id.as_str())?;
            let attention = attention(node).filter(|_| !acknowledged(node, acks));
            Some(AgentRow {
                id: placement.id.clone(),
                prefix: outline_prefix(&layout, placement),
                dim: dim.get(placement.id.as_str()).copied().unwrap_or(false)
                    && attention.is_none(),
                attention,
            })
        })
        .collect::<Vec<_>>();
    let mut counts = AgentCounts::default();
    for node in nodes.iter().filter(|node| node.parent_agent_id.is_some()) {
        if is_active(node) {
            counts.active += 1;
        } else if is_completed(node) {
            counts.completed += 1;
        } else if attention(node).is_some() && !acknowledged(node, acks) {
            counts.attention += 1;
        }
    }
    let hidden = nodes.len().saturating_sub(rows.len());
    AgentProjection {
        rows,
        counts,
        hidden,
        layout,
    }
}

/// One painted row, split so the renderer can style each part.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RowCells {
    pub rails: String,
    pub body: String,
    /// `  ⚠` on an unacknowledged attention row, else three blanks, so the
    /// status column ends in the same cell on every row.
    pub mark: String,
}

/// The facts one row shows, in display order (spec 02 §5.7).
pub struct RowFacts<'a> {
    pub glyph: &'a str,
    pub elapsed: &'a str,
    pub short_id: &'a str,
    /// The live fact: the current tool verb while it runs, else the status.
    pub fact: &'a str,
    pub role: &'a str,
    pub model: &'a str,
    pub turns: &'a str,
    pub tokens: &'a str,
    /// `N processes`, painted as `· N processes`; empty when none run.
    pub processes: &'a str,
}

/// Cells the left facts always leave for the status at the row's end.
const STATUS_RESERVE: usize = 12;
/// Cells the attention mark takes, reserved on every row.
const MARK_CELLS: usize = 3;

/// Lays one row out in `width` cells. Glyph, elapsed and id lead; role,
/// model, turns, tokens and processes follow while they fit in the row less
/// [`STATUS_RESERVE`], dropping from the right. The live status is
/// right-aligned at the end and takes whatever the facts leave (cut at its
/// own end when longer; a narrow `waiting on p3, c2` keeps its ids as `on p3,
/// c2`). The facts never depend on the status, so a status change never moves
/// the rest of the row. The attention mark always stays.
#[must_use]
pub fn compose_row(rails: &str, facts: &RowFacts<'_>, attention: bool, width: usize) -> RowCells {
    let mark = if attention { "  \u{26a0}" } else { "   " };
    let budget = width
        .saturating_sub(UnicodeWidthStr::width(rails))
        .saturating_sub(MARK_CELLS);
    // Elapsed is right-aligned in six cells (`59m59s`) so ids line up.
    let head = format!("{} {:>6}  {}", facts.glyph, facts.elapsed, facts.short_id);
    // A narrow row still gives the facts two thirds of its cells.
    let left_budget = budget
        .saturating_sub(STATUS_RESERVE + 2)
        .max(budget * 2 / 3);
    let mut left = head;
    let optional = [
        ("  ", facts.role),
        ("  ", facts.model),
        ("  ", facts.turns),
        ("  ", facts.tokens),
        (" \u{b7} ", facts.processes),
    ];
    for (separator, part) in optional.into_iter().filter(|(_, part)| !part.is_empty()) {
        let next = format!("{left}{separator}{part}");
        if UnicodeWidthStr::width(next.as_str()) > left_budget {
            break;
        }
        left = next;
    }
    let left = truncate_display(&left, left_budget);
    let room = budget.saturating_sub(UnicodeWidthStr::width(left.as_str()) + 2);
    let fact = match facts.fact.strip_prefix("waiting ") {
        Some(on) if UnicodeWidthStr::width(facts.fact) > room && on.starts_with("on ") => on,
        _ => facts.fact,
    };
    let status = truncate_display(fact, room);
    let gap = budget.saturating_sub(
        UnicodeWidthStr::width(left.as_str()) + UnicodeWidthStr::width(status.as_str()),
    );
    RowCells {
        rails: rails.to_owned(),
        body: format!("{left}{}{status}", " ".repeat(gap)),
        mark: mark.to_owned(),
    }
}

pub fn outline_prefix(layout: &TreeLayout, placement: &Placement) -> String {
    if placement.depth == 0 {
        return "◆ ".to_owned();
    }
    let mut chain = Vec::new();
    let mut cursor = placement.id.as_str();
    while let Some(parent) = layout.parent(cursor) {
        chain.push((parent, cursor));
        cursor = parent;
    }
    chain.reverse();
    let mut prefix = String::from("  ");
    for (level, (parent, child)) in chain.iter().enumerate() {
        let siblings = layout.children(parent);
        let last = siblings.last().is_some_and(|id| id == child);
        if level + 1 == chain.len() {
            prefix.push_str(if last { "└─" } else { "├─" });
        } else {
            prefix.push_str(if last { "  " } else { "│ " });
        }
    }
    prefix.push(' ');
    prefix
}

pub fn truncate_display(text: &str, max_cells: usize) -> String {
    if max_cells == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_cells {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let cells = UnicodeWidthStr::width(grapheme).max(1);
        if used + cells > max_cells.saturating_sub(1) {
            break;
        }
        out.push_str(grapheme);
        used += cells;
    }
    out.push('…');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    const NOW: u64 = 10_000;

    fn layout_node(id: &str, parent: Option<&str>) -> LayoutNode {
        LayoutNode {
            id: id.into(),
            parent: parent.map(str::to_owned),
            started_at_unix: None,
        }
    }

    fn node(id: &str, parent: Option<&str>, status: &str) -> CanvasNode {
        CanvasNode {
            agent_id: id.into(),
            parent_agent_id: parent.map(str::to_owned),
            depth: u8::from(parent.is_some()),
            status: status.into(),
            attempt_status: None,
            attempt_seq: parent.map(|_| 1),
            attempt_id: parent.map(|_| format!("{id}-a1")),
            attempts: Vec::new(),
            tier: None,
            description: "d".into(),
            prompt: "p".into(),
            workspace_path: "/w".into(),
            workspace_mode: "inherit".into(),
            workspace_status: None,
            candidate_id: None,
            blocked: None,
            usage: None,
            active_turn: false,
            live_progress: None,
            unread_mail: 0,
            has_session: true,
            template: Some("t".into()),
            model_account: "a".into(),
            model_tuple: "m".into(),
            model_effort: None,
            turns: Some(1),
            started_at_unix: Some(NOW - 600),
            ended_at_unix: None,
            finished: !matches!(status, "pending" | "running" | "paused" | "sealing"),
            fault: None,
            waiting_on: None,
        }
    }

    fn finished(id: &str, parent: &str, attempt: &str, ago: u64) -> CanvasNode {
        let mut node = node(id, Some(parent), "completed");
        node.attempt_status = Some(attempt.into());
        node.ended_at_unix = Some(NOW - ago);
        if attempt == "failed" {
            "failed".clone_into(&mut node.status);
        }
        node
    }

    fn shown(projection: &AgentProjection) -> Vec<(&str, bool)> {
        projection
            .rows
            .iter()
            .map(|row| (row.id.as_str(), row.dim))
            .collect()
    }

    #[test]
    fn live_scope_shows_active_recent_and_the_ancestors_that_explain_them() {
        let nodes = vec![
            node("root", None, "idle"),
            finished("planner", "root", "completed", 9 * 60),
            node("verifier", Some("planner"), "running"),
            finished("web", "root", "completed", 60),
            finished("tests", "root", "completed", 3 * 60),
            finished("old", "root", "completed", 6 * 60),
            node("waiting", Some("root"), "interrupted"),
        ];
        let projection = project(&nodes, &BTreeSet::new(), NOW, AgentScope::Live);
        assert_eq!(
            shown(&projection),
            vec![
                ("root", false),
                ("planner", true),
                ("verifier", false),
                ("tests", true),
                ("waiting", false),
                ("web", false),
            ],
            "a terminal ancestor is dimmed, 2-5 minutes is dimmed, past 5 is hidden; \
             siblings with one start stamp order by id"
        );
        assert_eq!(projection.hidden, 1);
        assert_eq!(
            projection.counts,
            AgentCounts {
                active: 2,
                completed: 4,
                attention: 0
            }
        );
        let all = project(&nodes, &BTreeSet::new(), NOW, AgentScope::All);
        assert_eq!(all.rows.len(), nodes.len());
        assert_eq!(all.hidden, 0);
        assert!(all.rows.iter().find(|row| row.id == "old").unwrap().dim);
    }

    #[test]
    fn attention_outlives_the_ttl_until_its_attempt_is_acknowledged() {
        let mut recovery = node("stuck", Some("root"), "recovery_required");
        recovery.ended_at_unix = Some(NOW - 3_600);
        let nodes = vec![
            node("root", None, "idle"),
            finished("tests", "root", "failed", 3_600),
            finished("quiet", "root", "completed_without_submission", 3_600),
            recovery,
        ];
        let projection = project(&nodes, &BTreeSet::new(), NOW, AgentScope::Live);
        assert_eq!(
            projection
                .rows
                .iter()
                .map(|row| (row.id.as_str(), row.attention))
                .collect::<Vec<_>>(),
            vec![
                ("root", None),
                ("quiet", Some(Attention::NoSubmission)),
                ("stuck", Some(Attention::RecoveryRequired)),
                ("tests", Some(Attention::Failed)),
            ]
        );
        assert_eq!(projection.counts.attention, 3);
        let acks = BTreeSet::from([occurrence("tests", "failed", NOW - 3_600)]);
        let acked = project(&nodes, &acks, NOW, AgentScope::Live);
        assert_eq!(
            acked.position("tests"),
            None,
            "acknowledged and old: hidden"
        );
        assert_eq!(acked.counts.attention, 2);
    }

    fn occurrence(id: &str, attention: &str, ended_at: u64) -> AgentAcknowledgement {
        AgentAcknowledgement {
            agent_id: id.to_owned(),
            attempt_id: format!("{id}-a1"),
            attention: attention.to_owned(),
            ended_at: Some(ended_at),
        }
    }

    /// Acknowledging "needs recovery" does not cover a later refusal of that
    /// same attempt: the end stamp stays and the label changes, so it counts
    /// again.
    #[test]
    fn an_acknowledgement_covers_one_occurrence_not_the_whole_attempt() {
        let mut recovery = node("stuck", Some("root"), "recovery_required");
        recovery.attempt_status = Some("recovery_required".into());
        recovery.ended_at_unix = Some(NOW - 100);
        let acks = BTreeSet::from([AgentAcknowledgement {
            agent_id: "stuck".into(),
            attempt_id: "stuck-a1".into(),
            attention: Attention::RecoveryRequired.label().into(),
            ended_at: recovery.ended_at_unix,
        }]);
        let acked = project(
            &[node("root", None, "idle"), recovery.clone()],
            &acks,
            NOW,
            AgentScope::Live,
        );
        assert_eq!(
            acked.counts.attention, 0,
            "the recovery occurrence is covered"
        );
        recovery.status = "failed".into();
        recovery.attempt_status = Some("failed".into());
        let refused = project(
            &[node("root", None, "idle"), recovery],
            &acks,
            NOW,
            AgentScope::Live,
        );
        assert_eq!(
            refused
                .rows
                .iter()
                .find(|row| row.id == "stuck")
                .and_then(|row| row.attention),
            Some(Attention::Failed)
        );
        assert_eq!(
            refused.counts.attention, 1,
            "a refused recovery is a new occurrence and counts"
        );
        let mut again = node("stuck", Some("root"), "failed");
        again.attempt_status = Some("failed".into());
        again.ended_at_unix = Some(NOW - 5);
        let previous_failure = BTreeSet::from([occurrence("stuck", "failed", NOW - 100)]);
        let restamped = project(
            &[node("root", None, "idle"), again],
            &previous_failure,
            NOW,
            AgentScope::Live,
        );
        assert_eq!(
            restamped.counts.attention, 1,
            "a new end stamp is not covered"
        );
    }

    #[test]
    fn a_followup_supersedes_attention_and_an_old_ack_does_not_cover_the_new_attempt() {
        let acks = BTreeSet::from([occurrence("tests", "failed", NOW - 3_600)]);
        let mut retask = node("tests", Some("root"), "running");
        retask.attempt_id = Some("tests-a2".into());
        retask.attempt_status = Some("running".into());
        let running = project(
            &[node("root", None, "idle"), retask.clone()],
            &BTreeSet::new(),
            NOW,
            AgentScope::Live,
        );
        assert_eq!(
            running.rows[1].attention, None,
            "no ack needed once superseded"
        );
        assert_eq!(running.counts.active, 1);
        retask.status = "failed".into();
        retask.attempt_status = Some("failed".into());
        retask.ended_at_unix = Some(NOW - 3_600);
        let failed_again = project(
            &[node("root", None, "idle"), retask],
            &acks,
            NOW,
            AgentScope::Live,
        );
        assert_eq!(failed_again.rows[1].attention, Some(Attention::Failed));
    }

    /// Spec 02 §5.7: glyph, elapsed, id, role, model, turns, tokens,
    /// `· N processes`, then the status right-aligned in its own column; a
    /// narrow row drops from the right of the left part and keeps glyph,
    /// elapsed, id and status, then the attention mark.
    #[test]
    fn a_row_reads_left_to_right_and_drops_from_the_right_when_narrow() {
        let facts = RowFacts {
            glyph: "\u{2715}",
            elapsed: "3m04s",
            short_id: "#107",
            fact: "failed",
            role: "staff-worker",
            model: "grok-4.6",
            turns: "5 turns",
            tokens: "12.4k tok",
            processes: "2 processes",
        };
        let wide = compose_row("  ├─ ", &facts, true, 110);
        assert!(
            wide.body.starts_with(
                "\u{2715}  3m04s  #107  staff-worker  grok-4.6  5 turns  12.4k tok · 2 processes"
            ),
            "{wide:?}"
        );
        assert!(wide.body.ends_with("  failed"), "{wide:?}");
        assert_eq!(wide.mark, "  \u{26a0}");
        let narrow = compose_row("  ├─ ", &facts, true, 60);
        assert!(
            narrow
                .body
                .starts_with("\u{2715}  3m04s  #107  staff-worker"),
            "{narrow:?}"
        );
        assert!(!narrow.body.contains("12.4k"), "{narrow:?}");
        assert!(narrow.body.ends_with("failed"), "{narrow:?}");
        let status_only = compose_row("  ├─ ", &facts, true, 36);
        assert!(
            status_only.body.starts_with("\u{2715}  3m04s  #107 "),
            "{status_only:?}"
        );
        assert!(status_only.body.ends_with(" failed"), "{status_only:?}");
        assert!(!status_only.body.contains("staff"), "{status_only:?}");
        let tiny = compose_row("  ├─ ", &facts, true, 20);
        assert!(tiny.body.contains('…'), "{tiny:?}");
        for row in [&wide, &narrow, &status_only, &tiny] {
            assert!(!row.body.contains("tests"), "the description left the row");
        }
        for (row, width) in [(&wide, 110), (&narrow, 60), (&status_only, 36), (&tiny, 20)] {
            assert!(
                UnicodeWidthStr::width(format!("{}{}{}", row.rails, row.body, row.mark).as_str())
                    <= width
            );
        }
    }

    /// The status changes length as an agent waits,
    /// thinks and runs commands; the rest of the row must not move, and the
    /// status column ends in the same cell on every row, marked or not.
    #[test]
    fn the_status_column_is_stable_across_status_changes_and_rows() {
        for width in [40, 60, 80, 120] {
            let facts = |fact| RowFacts {
                glyph: "\u{25cf}",
                elapsed: "1m02s",
                short_id: "#c3",
                fact,
                role: "staff-worker",
                model: "grok-4.6",
                turns: "3 turns",
                tokens: "8.1k tok",
                processes: "",
            };
            // The row with no status is the stable part every status follows.
            let base = compose_row("  ├─ ", &facts(""), false, width);
            let base = base.body.trim_end();
            let mut ends = Vec::new();
            for (fact, attention) in [
                ("waiting", false),
                ("thinking", false),
                (
                    "running cargo nextest run --workspace --all-features",
                    false,
                ),
                ("waiting on p3, c2", false),
                ("failed", true),
                ("", false),
            ] {
                let row = compose_row("  ├─ ", &facts(fact), attention, width);
                let body_width = UnicodeWidthStr::width(row.body.as_str());
                assert!(
                    row.body.starts_with(base),
                    "{width}: the left part moved for {fact:?}: {row:?}"
                );
                ends.push(UnicodeWidthStr::width(row.rails.as_str()) + body_width);
                assert_eq!(UnicodeWidthStr::width(row.mark.as_str()), MARK_CELLS);
                let status = row.body[base.len()..].trim();
                if !fact.is_empty() {
                    let first = fact.split(' ').next().unwrap_or_default();
                    assert!(
                        status.starts_with(first) || fact.contains(status.trim_end_matches('…')),
                        "{width}: the status {fact:?} shows: {row:?}"
                    );
                    // The facts leave the status at least its reserve.
                    let shown = fact.strip_prefix("waiting ").unwrap_or(fact);
                    assert!(
                        UnicodeWidthStr::width(status) >= shown.len().min(STATUS_RESERVE),
                        "{width}: {fact:?} got {status:?}"
                    );
                }
                if fact.starts_with("waiting on") {
                    assert!(status.contains("p3"), "{width}: the ids were cut: {row:?}");
                }
            }
            assert!(
                ends.windows(2).all(|pair| pair[0] == pair[1]),
                "{width}: the status column moved: {ends:?}"
            );
        }
    }

    #[test]
    fn nested_children_keep_ancestor_rails() {
        let layout = TreeLayout::new(&[
            layout_node("root", None),
            layout_node("a", Some("root")),
            layout_node("a1", Some("a")),
            layout_node("b", Some("root")),
        ]);
        let prefixes: Vec<String> = layout
            .placements()
            .iter()
            .map(|placement| outline_prefix(&layout, placement))
            .collect();
        assert_eq!(prefixes, vec!["◆ ", "  ├─ ", "  │ └─ ", "  └─ "]);
    }

    #[test]
    fn malformed_links_degrade_to_a_forest() {
        let layout = TreeLayout::new(&[
            layout_node("a", Some("b")),
            layout_node("b", Some("a")),
            layout_node("c", Some("missing")),
        ]);
        assert_eq!(layout.placements().len(), 3);
    }

    #[test]
    fn truncation_is_unicode_safe() {
        assert_eq!(truncate_display("日本語", 5), "日本…");
        assert_eq!(truncate_display("hello", 3), "he…");
    }

    /// Header `completed` is a successful attempt, not every settled node.
    /// Closing is a resolution: it is neither completed nor attention.
    #[test]
    fn completed_counts_only_completed_or_adopted_and_closing_drops_attention() {
        let mut adopted = finished("adopted", "root", "adopted", 30);
        adopted.status = "completed".into();
        let mut closed = finished("closed", "root", "closed", 30);
        closed.status = "closed".into();
        let mut cancelled = finished("cancelled", "root", "cancelled", 30);
        cancelled.status = "failed".into();
        let mut operator_closed = finished("shut", "root", "failed", 3_600);
        operator_closed.status = "closed".into();
        let projection = project(
            &[
                node("root", None, "idle"),
                finished("done", "root", "completed", 30),
                adopted,
                closed,
                cancelled,
                finished("failed", "root", "failed", 30),
                operator_closed,
            ],
            &BTreeSet::new(),
            NOW,
            AgentScope::Live,
        );
        assert_eq!(
            projection.counts.completed, 2,
            "only completed and adopted attempts"
        );
        assert_eq!(
            projection.counts.attention, 1,
            "closed and cancelled are not attention, and closing a failed node resolves it"
        );
        assert!(
            projection
                .rows
                .iter()
                .find(|row| row.id == "shut")
                .is_none_or(|row| row.attention.is_none())
        );
    }

    /// The root and a node with no attempt cannot be acknowledged, so they
    /// must not show an attention mark that `x` cannot clear.
    #[test]
    fn the_root_and_attemptless_nodes_never_carry_attention() {
        let mut root = node("root", None, "failed");
        root.finished = true;
        let mut bare = node("bare", Some("root"), "failed");
        bare.attempt_id = None;
        bare.attempt_status = None;
        bare.finished = true;
        bare.ended_at_unix = Some(NOW - 10);
        let projection = project(
            &[root, bare, finished("tests", "root", "failed", 10)],
            &BTreeSet::new(),
            NOW,
            AgentScope::Live,
        );
        assert_eq!(
            projection
                .rows
                .iter()
                .filter(|row| row.attention.is_some())
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tests"]
        );
    }
}
