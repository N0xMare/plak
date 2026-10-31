//! History presentation over one agent snapshot.
//!
//! Not a store and not a host query. The list is the snapshot's settled
//! non-root agents, grouped by the operator's local calendar day.

use crate::tui::agent_list;
use crate::tui::resume_pane::calendar_date;
use crate::tui::{CanvasNode, ProfileRow};

use super::tree_navigator::{agent_role, short_id};

/// Characters a history query may hold. Paste beyond this is dropped.
pub(crate) const SEARCH_LIMIT: usize = 128;

/// History lines for one snapshot, query, failures flag, and local day.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HistoryCache {
    pub revision: u64,
    pub tree_id: String,
    pub query: String,
    pub failures_only: bool,
    pub day: String,
    pub lines: Vec<HistoryLine>,
}

/// One history line. Headers and notices are not selectable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HistoryLine {
    /// `Today`, `Earlier`, or `Unknown time`.
    Header(&'static str),
    /// One settled agent, newest first within its group.
    Agent(String),
    /// Empty filter or empty history.
    Notice(&'static str),
}

/// Local-day group. Unknown is a missing or unreadable stamp, never a guess.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum HistoryGroup {
    Today,
    Earlier,
    Unknown,
}

impl HistoryGroup {
    const fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Earlier => "Earlier",
            Self::Unknown => "Unknown time",
        }
    }
}

/// Settled or recovery-required, and not the root, live work, or a park.
pub(crate) fn history_member(node: &CanvasNode) -> bool {
    node.parent_agent_id.is_some() && !node.dormancy.parked && !agent_list::is_active(node)
}

/// Failed, blocked, unfinished, or recovery-required. Acknowledged failures
/// stay. Cancelled, closed, and resolved attempts do not.
pub(crate) fn history_failure(node: &CanvasNode) -> bool {
    if !history_member(node) || node.attempt_id.is_none() {
        return false;
    }
    if node.status == "closed"
        || matches!(node.attempt_status.as_deref(), Some("closed" | "cancelled"))
    {
        return false;
    }
    node.status == "failed"
        || node.status == "recovery_required"
        || node.blocked.is_some()
        || matches!(
            node.attempt_status.as_deref(),
            Some("failed" | "blocked" | "completed_without_submission" | "recovery_required")
        )
}

fn stamp(node: &CanvasNode) -> Option<u64> {
    node.ended_at_unix.or(node.started_at_unix)
}

fn group_of(node: &CanvasNode, now_unix: u64, zone: &jiff::tz::TimeZone) -> HistoryGroup {
    let Some(stamp) = stamp(node) else {
        return HistoryGroup::Unknown;
    };
    let day = calendar_date(stamp, zone);
    if day == "unknown date" {
        return HistoryGroup::Unknown;
    }
    if day == calendar_date(now_unix, zone) {
        HistoryGroup::Today
    } else {
        HistoryGroup::Earlier
    }
}

/// The next local midnight after `now_unix`, so a history list regroups then.
pub(crate) fn next_local_midnight(now_unix: u64, zone: &jiff::tz::TimeZone) -> Option<u64> {
    let seconds = i64::try_from(now_unix).ok()?;
    let zoned = jiff::Timestamp::from_second(seconds)
        .ok()?
        .to_zoned(zone.clone());
    let start = zoned
        .date()
        .tomorrow()
        .ok()?
        .to_datetime(jiff::civil::Time::midnight())
        .to_zoned(zone.clone())
        .ok()?;
    u64::try_from(start.timestamp().as_second()).ok()
}

fn query_hit(node: &CanvasNode, query: &str, profile: Option<&ProfileRow>) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    let short = short_id(&node.agent_id);
    let role = agent_role(node, profile);
    let template = node.template.as_deref().unwrap_or("");
    [
        node.agent_id.as_str(),
        short.as_str(),
        short.trim_start_matches('#'),
        role.as_str(),
        template,
        node.description.as_str(),
    ]
    .into_iter()
    .any(|field| field.to_lowercase().contains(&query))
}

/// Flat history for the snapshot. Active, interrupted, parked, and the root
/// stay in the tree view. One row per agent.
pub(crate) fn history_lines(
    nodes: &[CanvasNode],
    now_unix: u64,
    zone: &jiff::tz::TimeZone,
    query: &str,
    failures_only: bool,
    profile: Option<&ProfileRow>,
) -> Vec<HistoryLine> {
    let mut rows: Vec<&CanvasNode> = nodes
        .iter()
        .filter(|node| history_member(node))
        .filter(|node| !failures_only || history_failure(node))
        .filter(|node| query_hit(node, query, profile))
        .collect();
    let unfiltered = nodes.iter().any(history_member);
    let today = calendar_date(now_unix, zone);
    rows.sort_by_cached_key(|node| {
        let group = match stamp(node) {
            Some(stamp) => {
                let day = calendar_date(stamp, zone);
                if day == "unknown date" {
                    HistoryGroup::Unknown
                } else if day == today {
                    HistoryGroup::Today
                } else {
                    HistoryGroup::Earlier
                }
            }
            None => HistoryGroup::Unknown,
        };
        (group, std::cmp::Reverse(stamp(node)), node.agent_id.clone())
    });
    if rows.is_empty() {
        return vec![HistoryLine::Notice(if unfiltered {
            "No matches · Esc clears"
        } else {
            "No settled agents"
        })];
    }
    let mut lines = Vec::new();
    let mut previous = None;
    for node in rows {
        let group = group_of(node, now_unix, zone);
        if previous != Some(group) {
            lines.push(HistoryLine::Header(group.label()));
            previous = Some(group);
        }
        lines.push(HistoryLine::Agent(node.agent_id.clone()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::tui::tree_view::CanvasNode;
    use plakous_host_protocol::views::NodeDormancy;

    fn node(id: &str, ended: Option<u64>) -> CanvasNode {
        CanvasNode {
            agent_id: id.into(),
            parent_agent_id: Some("root".into()),
            depth: 1,
            status: "completed".into(),
            attempt_status: Some("completed".into()),
            attempt_seq: Some(1),
            attempt_id: Some(format!("{id}-a1")),
            attempts: vec![1],
            tier: None,
            description: "writes the notes".into(),
            prompt: "prompt".into(),
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
            template: Some("my-profile-worker".into()),
            model_account: "a".into(),
            model_tuple: "m".into(),
            model_effort: None,
            turns: Some(1),
            started_at_unix: ended,
            ended_at_unix: ended,
            finished: true,
            fault: None,
            waiting_on: None,
            dormancy: NodeDormancy::default(),
        }
    }

    #[test]
    fn groups_by_local_day_and_sorts_newest_first() {
        let zone = jiff::tz::TimeZone::fixed(jiff::tz::offset(-4));
        let now = 1_700_000_000;
        let wake = next_local_midnight(now, &zone).unwrap();
        let mut today = node("b-today", Some(now));
        let mut earlier = node("a-old", Some(now.saturating_sub(3 * 86_400)));
        let mut unknown = node("c-unknown", None);
        unknown.started_at_unix = None;
        let mut same_day = node("a-later", Some(now.saturating_sub(60)));
        today.description = "alpha notes".into();
        earlier.attempt_status = Some("failed".into());
        earlier.status = "failed".into();
        same_day.template = Some("reviewer".into());
        let running = {
            let mut node = node("run", Some(now));
            node.status = "running".into();
            node.finished = false;
            node.ended_at_unix = None;
            node
        };
        let parked = {
            let mut node = node("park", Some(now));
            node.dormancy.parked = true;
            node.status = "running".into();
            node.finished = false;
            node
        };
        let lines = history_lines(
            &[today, earlier.clone(), unknown, same_day, running, parked],
            now,
            &zone,
            "",
            false,
            None,
        );
        let ids: Vec<&str> = lines
            .iter()
            .filter_map(|line| match line {
                HistoryLine::Agent(id) => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["b-today", "a-later", "a-old", "c-unknown"]);
        assert!(matches!(lines[0], HistoryLine::Header("Today")));
        assert!(
            lines
                .iter()
                .any(|line| matches!(line, HistoryLine::Header("Earlier")))
        );
        assert!(
            lines
                .iter()
                .any(|line| matches!(line, HistoryLine::Header("Unknown time")))
        );

        let before = wake - 1;
        let mut boundary = node("edge", Some(before));
        assert_eq!(group_of(&boundary, before, &zone), HistoryGroup::Today);
        boundary.ended_at_unix = Some(before);
        assert_eq!(group_of(&boundary, wake, &zone), HistoryGroup::Earlier);
    }

    #[test]
    fn search_and_failures_filter_without_dropping_an_acknowledgement() {
        let zone = jiff::tz::TimeZone::UTC;
        let mut failed = node("c-107", Some(50));
        failed.attempt_status = Some("failed".into());
        failed.status = "failed".into();
        failed.description = "quota notes".into();
        let mut acked = failed.clone();
        acked.agent_id = "c-acked".into();
        acked.attempt_id = Some("c-acked-a1".into());
        let mut cancelled = node("c-cancel", Some(40));
        cancelled.attempt_status = Some("cancelled".into());
        cancelled.status = "completed".into();
        let mut unfinished = node("c-open", Some(45));
        unfinished.attempt_status = Some("completed_without_submission".into());
        unfinished.status = "completed".into();
        unfinished.finished = true;
        let done = node("c-done", Some(30));
        let nodes = [failed, acked.clone(), cancelled, unfinished, done];
        let failures = history_lines(&nodes, 100, &zone, "", true, None);
        let ids: Vec<&str> = failures
            .iter()
            .filter_map(|line| match line {
                HistoryLine::Agent(id) => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["c-107", "c-acked", "c-open"]);
        let acknowledged = agent_list::acknowledged(
            &acked,
            &BTreeSet::from([crate::tui::AgentAcknowledgement {
                agent_id: acked.agent_id.clone(),
                attempt_id: acked.attempt_id.clone().unwrap(),
                attention: "failed".into(),
                ended_at: acked.ended_at_unix,
            }]),
        );
        assert!(acknowledged, "the failure occurrence is acknowledged");
        assert!(
            ids.contains(&"c-acked"),
            "an acknowledgement does not drop the failure"
        );
        let found = history_lines(&nodes, 100, &zone, "QUOTA", false, None);
        assert!(
            found
                .iter()
                .any(|line| matches!(line, HistoryLine::Agent(id) if id == "c-107"))
        );
        let by_role = history_lines(&nodes, 100, &zone, "worker", false, None);
        assert!(by_role.len() > 1);
        let none = history_lines(&nodes, 100, &zone, "missing-role", false, None);
        assert_eq!(none, vec![HistoryLine::Notice("No matches · Esc clears")]);
    }
}
