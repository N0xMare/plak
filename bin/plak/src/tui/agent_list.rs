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
    /// The node's recovery is unresolved: the host recovers it, and `u`
    /// retries it.
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

/// A dormant body is dimmed at once. Parked live work stays listed.
fn dormant_dim(node: &CanvasNode) -> bool {
    !node.dormancy.is_empty()
}

/// `Some(dim)` when the node is shown on its own account.
fn shown_on_own(
    node: &CanvasNode,
    acks: &BTreeSet<AgentAcknowledgement>,
    now_unix: u64,
) -> Option<bool> {
    // Parked work is live and dimmed immediately, whatever its status reads.
    if node.dormancy.parked {
        return Some(true);
    }
    if is_active(node) {
        return Some(false);
    }
    if attention(node).is_some() && !acknowledged(node, acks) {
        // The attention mark stays bright; a dormant body still dims.
        return Some(dormant_dim(node));
    }
    if node.parent_agent_id.is_none() {
        return Some(false);
    }
    if attention(node).is_some() {
        return None;
    }
    ended_age(node, now_unix)
        .filter(|age| *age <= RECENT_SECS)
        .map(|age| dormant_dim(node) || age > DIM_AFTER_SECS)
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
            let body_dim = dim.get(placement.id.as_str()).copied().unwrap_or(false);
            // Attention stays marked; a dormant body still dims so the mark is
            // what stays bright.
            let dimmed = body_dim && (attention.is_none() || !node.dormancy.is_empty());
            Some(AgentRow {
                id: placement.id.clone(),
                prefix: outline_prefix(&layout, placement),
                dim: dimmed,
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
///
/// Model, turns, tokens and process count stay in the inspector.
pub struct RowFacts<'a> {
    pub glyph: &'a str,
    pub elapsed: &'a str,
    pub short_id: &'a str,
    /// The one live fact, right-aligned.
    pub fact: &'a str,
    pub role: &'a str,
}

/// Cells the live fact keeps before the role may take any.
const STATUS_RESERVE: usize = 12;
/// Cells the attention mark takes, reserved on every row.
const MARK_CELLS: usize = 3;

/// Lays one row out in `width` cells: glyph, right-aligned elapsed, `#id`,
/// role, then one live fact right-aligned at the end. The role is truncated
/// before the fact, and the fact before the identity. Fewer than three cells
/// left for the role omits it rather than painting a lone `…`. Glyph,
/// elapsed, `#id` and the mark are not truncated to reserve the fact. A
/// narrow `waiting on p3, c2` keeps its ids as `on p3, c2`. The identity
/// does not move when the fact changes length; the role yields cells to the
/// fact's reserve, which is `min(fact width, STATUS_RESERVE) + 2`.
#[must_use]
pub fn compose_row(rails: &str, facts: &RowFacts<'_>, attention: bool, width: usize) -> RowCells {
    let mark = if attention { "  \u{26a0}" } else { "   " };
    let budget = width
        .saturating_sub(UnicodeWidthStr::width(rails))
        .saturating_sub(MARK_CELLS);
    // Elapsed is right-aligned in six cells (`59m59s`) so ids line up.
    let head = format!("{} {:>6}  {}", facts.glyph, facts.elapsed, facts.short_id);
    let head_width = UnicodeWidthStr::width(head.as_str());
    let fact_width = UnicodeWidthStr::width(facts.fact).min(STATUS_RESERVE);
    let fact_gap = usize::from(!facts.fact.is_empty()) * 2;
    // The reserve never takes cells the identity needs.
    let reserved = (fact_width + fact_gap).min(budget.saturating_sub(head_width));
    let left_budget = budget.saturating_sub(reserved);
    let mut left = head.clone();
    if !facts.role.is_empty() && left_budget > head_width + 2 {
        let room = left_budget - head_width - 2;
        let role = if UnicodeWidthStr::width(facts.role) <= room {
            facts.role.to_owned()
        } else if room < 3 {
            String::new()
        } else {
            truncate_display(facts.role, room)
        };
        if !role.is_empty() {
            left = format!("{left}  {role}");
        }
    }
    // Only a budget smaller than the identity itself may cut it, and only
    // after the role and the fact have already yielded.
    let left = if head_width <= budget {
        left
    } else {
        truncate_display(&head, budget)
    };
    let separator =
        usize::from(!facts.fact.is_empty() && budget > UnicodeWidthStr::width(left.as_str())) * 2;
    let room = budget.saturating_sub(UnicodeWidthStr::width(left.as_str()) + separator);
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
mod tests;
