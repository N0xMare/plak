//! Row labels, recovery intents and the inspector lines the navigator paints.

use std::collections::BTreeSet;

use unicode_width::UnicodeWidthStr;

use super::agent_list::{self, AgentProjection, RowCells, RowFacts, compose_row, truncate_display};
use super::{format_duration_secs, humanize_state};
use crate::tui::{AgentAcknowledgement, CanvasNode, CanvasTree, SurfaceIntent, status_glyph};

/// Wraps a list footer onto the rows `width` needs, up to `max_rows`.
///
/// Segments are the ` · ` pieces, so a key label such as `t history` stays
/// on one row instead of splitting across the wrap.
#[must_use]
pub(crate) fn wrap_footer_hint(hint: &str, width: u16, max_rows: usize) -> Vec<String> {
    let budget = usize::from(width).saturating_sub(1).max(1);
    let max_rows = max_rows.max(1);
    let mut rows = wrap_footer_segments(hint, budget);
    if rows.len() > max_rows {
        let tail = rows.split_off(max_rows - 1).join(" · ");
        rows.push(crate::tui::picker::clip(&tail, budget));
    }
    rows.into_iter().map(|line| format!(" {line}")).collect()
}

/// Footer segments that fit in `budget` cells, keeping each ` · ` piece whole.
pub(super) fn wrap_footer_segments(hint: &str, budget: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current = String::new();
    for part in hint.trim().split(" · ").filter(|part| !part.is_empty()) {
        let part_width = UnicodeWidthStr::width(part);
        let next = if current.is_empty() {
            part_width
        } else {
            UnicodeWidthStr::width(current.as_str()) + 3 + part_width
        };
        if !current.is_empty() && next > budget {
            rows.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str(" · ");
        }
        current.push_str(part);
    }
    if !current.is_empty() {
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// `u` on a child the host would recover, bound to this snapshot's revision
/// and attempt. The host fences the revision; the pop-up does not.
pub(crate) fn recover_intent(tree: &CanvasTree, agent_id: &str) -> Option<crate::tui::UserAction> {
    let node = tree.nodes.iter().find(|node| node.agent_id == agent_id)?;
    let attempt = node.attempt_id.clone()?;
    if !plakous_host_protocol::topology::operator_child_recover_eligible(
        tree.state.as_str(),
        node.status.as_str(),
        node.parent_agent_id.is_none(),
        true,
    ) {
        return None;
    }
    Some(crate::tui::UserAction::RecoverTopologyNode {
        tree_id: tree.tree_id.clone(),
        revision: tree.revision,
        agent_id: node.agent_id.clone(),
        expected_attempt_id: attempt,
    })
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
pub(super) const ROOT_TURN_OVERWRITABLE: &[&str] = &["running", "idle", "pending", "completed"];

/// The bound model tuple as one label. Unknown stays unknown.
pub(super) fn model_label(node: &CanvasNode) -> String {
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
pub(super) fn turns_label(turns: Option<u32>) -> String {
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
pub(super) fn row_status(node: &CanvasNode) -> &str {
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
pub(super) fn attempts_strip(node: &CanvasNode) -> String {
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
pub(super) fn short_candidate(candidate_id: &str) -> String {
    let tail = candidate_id.rsplit('-').next().unwrap_or(candidate_id);
    let tail = if tail.is_empty() { candidate_id } else { tail };
    format!("#{}", truncate_display(tail, 8))
}

/// `sealed · #abc123` once the node's attempt has sealed a candidate.
/// Only `execute` and `mutate` copies seal, so a `read_only`
/// row never carries one and this stays `None` for it.
#[must_use]
pub(super) fn sealed_line(node: &CanvasNode) -> Option<String> {
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
pub(super) fn blocked_line(node: &CanvasNode, now_unix: u64) -> Option<String> {
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
pub(super) fn until_label(at: u64, now_unix: u64) -> String {
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
pub(super) fn work_line(node: &CanvasNode, now_unix: u64) -> String {
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
pub(super) fn elapsed_label(started_at_unix: Option<u64>, now_unix: u64) -> String {
    let Some(started) = started_at_unix else {
        return "\u{2014}".to_owned();
    };
    format_duration_secs(now_unix.saturating_sub(started))
}

/// Trailing segment of a durable id, which is what the boxes have room for.
pub(crate) fn short_id(agent_id: &str) -> String {
    let tail = agent_id.rsplit('-').next().unwrap_or(agent_id);
    let tail = if tail.is_empty() { agent_id } else { tail };
    format!("#{}", truncate_display(tail, 10))
}

pub(super) fn blank_as_unknown(text: &str) -> String {
    if text.trim().is_empty() {
        "\u{2014}".to_owned()
    } else {
        text.trim().to_owned()
    }
}

/// `● 3 active   ✓ 21 completed   ⚠ 1 attention`, then the tree state when it
/// is not plainly live. A narrow body keeps the glyphs and counts.
pub(super) fn header_line(
    projection: &AgentProjection,
    snapshot: Option<&CanvasTree>,
    width: u16,
) -> String {
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

/// What an agent row reads besides its node: the frame's clock and spinner
/// frame (`None` under reduced motion). Process counts stay in the inspector.
pub struct AgentRowClock<'a> {
    pub now_unix: u64,
    pub spinner: Option<&'a str>,
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
    let elapsed = row_elapsed(node, clock.now_unix);
    let facts = RowFacts {
        glyph: &glyph,
        elapsed: &elapsed,
        short_id: &short,
        fact: &fact,
        role: &role,
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
pub(crate) fn agent_role(node: &CanvasNode, profile: Option<&crate::tui::ProfileRow>) -> String {
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

/// ` · N processes` for the inspector; empty when the agent owns none.
pub(super) fn process_count_label(owned: usize) -> String {
    match owned {
        0 => String::new(),
        1 => " · 1 process".to_owned(),
        count => format!(" · {count} processes"),
    }
}

/// The row's name: the template, else the description; the root is `root`.
pub(super) fn agent_name(node: &CanvasNode) -> String {
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
pub(super) fn agent_glyph(node: &CanvasNode, attention: bool, spinner: Option<&str>) -> String {
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

/// One live fact: in-flight work, else the dormancy reason, else the status.
pub(super) fn live_fact(node: &CanvasNode, now_unix: u64) -> String {
    if agent_list::is_active(node)
        && let Some(activity) = node
            .live_progress
            .as_ref()
            .and_then(|progress| progress.activity.as_deref())
            .filter(|activity| !activity.trim().is_empty())
    {
        return activity.trim().to_owned();
    }
    if let Some(reason) = dormancy_fact(node) {
        return reason;
    }
    if let Some(attention) = agent_list::attention(node) {
        return match attention {
            agent_list::Attention::Blocked => {
                blocked_line(node, now_unix).unwrap_or_else(|| attention.label().to_owned())
            }
            _ => attention.label().to_owned(),
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

/// The row's dormancy reason. Parked names its waits; several facts keep the
/// closed → retired → reclaimed → parked order.
pub(super) fn dormancy_fact(node: &CanvasNode) -> Option<String> {
    let dormancy = node.dormancy;
    if dormancy.closed {
        return Some("closed".to_owned());
    }
    if dormancy.conversation_retired {
        return Some("retired conversation".to_owned());
    }
    if dormancy.workspace_reclaimed {
        return Some("reclaimed checkout".to_owned());
    }
    if dormancy.parked {
        return Some(match node.waiting_on.as_deref() {
            Some(waiting) if !waiting.is_empty() => format!("waiting on {}", waiting.join(", ")),
            _ => "parked".to_owned(),
        });
    }
    None
}

/// Inspector clause for every dormancy fact, and whether followup revives it.
pub(super) fn dormancy_inspector(node: &CanvasNode) -> String {
    if node.dormancy.is_empty() {
        return String::new();
    }
    let mut facts = Vec::new();
    if node.dormancy.closed {
        facts.push("closed");
    }
    if node.dormancy.conversation_retired {
        facts.push("retired conversation");
    }
    if node.dormancy.workspace_reclaimed {
        facts.push("reclaimed checkout");
    }
    if node.dormancy.parked {
        facts.push("parked");
    }
    let revive = match node.dormancy.followup_revives() {
        Some(true) => " · followup can revive it",
        Some(false) => " · followup cannot revive a retired conversation",
        None => "",
    };
    format!(" · {}{revive}", facts.join(", "))
}

/// Running agents count up from their start; finished ones show how long
/// they ran.
pub(super) fn row_elapsed(node: &CanvasNode, now_unix: u64) -> String {
    match node.ended_at_unix {
        Some(ended) if !agent_list::is_active(node) => elapsed_label(node.started_at_unix, ended),
        _ => elapsed_label(node.started_at_unix, now_unix),
    }
}
