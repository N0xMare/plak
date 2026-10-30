//! Typed facts the host reads from a wait or adopt call's own arguments and
//! result (spec 02 §5.4): a wait item's requested timeout, what woke it and
//! the actual wait, and the adopted and adopt-conflict milestones. A
//! `wait_agent` or `adopt_candidate` result is read whole from the facts the
//! topology tool left at its boundary, never from the bounded preview. The
//! same values are persisted on the transcript row and emitted live.

use plakous_host_protocol::topology::WaitAgentRequest;
use plakous_host_protocol::views::transcript::{AgentMilestone, MilestoneEvent, WaitFacts};
use plakous_recursive_subagents::call_facts::TopologyCallFacts;
use serde_json::Value;

/// Conflict paths a milestone keeps.
const MAX_CONFLICT_PATHS: usize = 16;

/// The facts a wait starts with: its requested ceiling. `None` for a call
/// that is not a wait (`wait_agent`, or a process wait).
pub(crate) fn wait_started(
    name: &str,
    process_label: Option<&str>,
    args: Option<&Value>,
) -> Option<WaitFacts> {
    let timeout_ms =
        if name == "wait_agent" && args.is_some_and(|args| args.get("task_ids").is_some()) {
            // Grok's output tool over children: an omitted timeout is a snapshot.
            Some(
                args.and_then(|args| args.get("timeout_ms"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(plakous_host_protocol::topology::MAX_WAIT_TIMEOUT_MS),
            )
        } else if name == "wait_agent" && args.is_some_and(|args| args.get("ids").is_some()) {
            // Pi's `wait` over children: an omitted timeout is 300000 ms.
            Some(
                args.and_then(|args| args.get("timeout_ms"))
                    .and_then(Value::as_u64)
                    .unwrap_or(plakous_host_protocol::topology::DEFAULT_WAIT_TIMEOUT_MS)
                    .min(plakous_host_protocol::topology::MAX_WAIT_TIMEOUT_MS),
            )
        } else if name == "wait_agent" {
            args.and_then(|args| serde_json::from_value::<WaitAgentRequest>(args.clone()).ok())
                .and_then(|request| request.validate().ok())
        } else if process_label == Some("process_session_wait") {
            args.and_then(|args| {
                ["yield_time_ms", "timeout_ms"]
                    .iter()
                    .find_map(|key| args.get(key).and_then(Value::as_u64))
            })
        } else {
            return None;
        };
    Some(WaitFacts {
        timeout_ms,
        ..WaitFacts::default()
    })
}

/// The facts a wait ends with, from the facts it started with and its own
/// whole result. A failed wait keeps its timeout and actual wait; its error is
/// the row's state.
pub(crate) fn wait_settled(
    name: &str,
    started: Option<WaitFacts>,
    result: Option<&Value>,
    succeeded: bool,
    waited_ms: Option<u64>,
    process_ids: Option<&[String]>,
) -> Option<WaitFacts> {
    let mut facts = started?;
    facts.waited_ms = waited_ms;
    if let Some(result) = result.filter(|_| succeeded) {
        if name == "wait_agent" {
            agent_wake(&await_receipt(result), &mut facts);
        } else if let Some([id]) = process_ids {
            facts.woke_by = super::tools::process_wait_outcome(result, id).map(|outcome| {
                if outcome == "still running" {
                    "timeout".to_owned()
                } else {
                    outcome
                }
            });
        }
    }
    Some(facts)
}

/// The facts a bare pi `wait` that ran the tree's await starts with, in
/// place of the process wait's: its ceiling, 300000 ms when omitted.
pub(super) fn bare_await_started(started: Option<WaitFacts>) -> WaitFacts {
    WaitFacts {
        timeout_ms: Some(
            started
                .and_then(|facts| facts.timeout_ms)
                .unwrap_or(plakous_host_protocol::topology::DEFAULT_WAIT_TIMEOUT_MS)
                .min(plakous_host_protocol::topology::MAX_WAIT_TIMEOUT_MS),
        ),
        ..WaitFacts::default()
    }
}

/// The children an await receipt lists, as a row subject (`c1,c2`).
pub(super) fn await_children(result: &Value) -> Option<String> {
    let receipt = await_receipt(result);
    let ids = receipt
        .get("agents")?
        .as_array()?
        .iter()
        .filter_map(|row| row.get("agent_id").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(",");
    (!ids.is_empty()).then_some(ids)
}

/// An await receipt, unwrapped from the topology envelope a Direct Grok or pi
/// call returns it in (`{"result": "<receipt JSON>"}`); a history preview
/// clipped inside the envelope yields no receipt.
fn await_receipt(result: &Value) -> std::borrow::Cow<'_, Value> {
    match result.get("result").and_then(Value::as_str) {
        Some(inner) => {
            std::borrow::Cow::Owned(serde_json::from_str::<Value>(inner).unwrap_or(Value::Null))
        }
        None => std::borrow::Cow::Borrowed(result),
    }
}

/// What woke a `wait_agent`, from its own result.
fn agent_wake(result: &Value, facts: &mut WaitFacts) {
    let rows = result
        .get("agents")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    match result.get("woke_by").and_then(Value::as_str) {
        Some("settled") => {
            // The result mail this wait delivered names the attempt that
            // settled; a row settled and delivered earlier is not it.
            let delivered = result
                .get("mail")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|mail| mail.get("purpose").and_then(Value::as_str) == Some("result"))
                .filter_map(|mail| mail.get("attempt_id"))
                .find_map(|attempt| {
                    rows.iter()
                        .find(|row| row.get("attempt_id") == Some(attempt))
                });
            let row = delivered.or_else(|| rows.iter().find(|row| settled_word(row).is_some()));
            let agent = row.and_then(|row| row.get("agent_id").and_then(Value::as_str));
            let word = row.and_then(settled_word).unwrap_or("settled");
            facts.settled_agent = agent.map(str::to_owned);
            facts.woke_by =
                Some(agent.map_or_else(|| word.to_owned(), |id| format!("{id} {word}")));
        }
        Some("mail") => {
            let from = result
                .get("mail")
                .and_then(Value::as_array)
                .and_then(|mail| mail.first())
                .and_then(|mail| mail.get("from"))
                .and_then(Value::as_str);
            facts.woke_by =
                Some(from.map_or_else(|| "mail".to_owned(), |from| format!("mail from {from}")));
        }
        Some("process") => facts.woke_by = Some("process exited".to_owned()),
        Some(reason) => facts.woke_by = Some(reason.to_owned()),
        None if result.get("timed_out").and_then(Value::as_bool) == Some(true) => {
            facts.woke_by = Some("timeout".to_owned());
        }
        None => {}
    }
}

/// How a settled wait row reads, or `None` for a row still in flight.
fn settled_word(row: &Value) -> Option<&'static str> {
    match row.get("status").and_then(Value::as_str)? {
        "completed" => Some("finished"),
        "failed" => Some("failed"),
        "interrupted" => Some("interrupted"),
        "closed" => Some("closed"),
        "recovery_required" => Some("needs recovery"),
        _ => None,
    }
}

/// The milestone an `adopt_candidate` call earns from the facts its tool
/// left: `adopted` with the landed file count, or `adopt_conflict` with the
/// conflicting paths (at most [`MAX_CONFLICT_PATHS`], each a whole path as
/// the VCS named it). Other refusals leave no facts and are the tool row's
/// own failure. `producer` resolves the agent that sealed the candidate and
/// that agent's template.
pub(super) fn adopt_milestone(
    candidate: &str,
    facts: TopologyCallFacts,
    producer: impl FnOnce(&str) -> Option<(String, Option<String>)>,
) -> Option<AgentMilestone> {
    let (event, files, paths, conflict_candidate) = match facts {
        TopologyCallFacts::Adopted { files_changed } => (
            MilestoneEvent::Adopted,
            Some(u64::from(files_changed)),
            Vec::new(),
            None,
        ),
        TopologyCallFacts::AdoptConflict {
            mut paths,
            candidate,
        } => {
            paths.truncate(MAX_CONFLICT_PATHS);
            (MilestoneEvent::AdoptConflict, None, paths, candidate)
        }
        TopologyCallFacts::Wait(_) => return None,
    };
    let (agent_id, template) = producer(candidate)
        .or_else(|| sealer_named_by(candidate).map(|agent| (agent, None)))
        .unwrap_or_default();
    Some(AgentMilestone {
        agent_id,
        event,
        template,
        text: None,
        duration_ms: None,
        candidate: Some(conflict_candidate.unwrap_or_else(|| candidate.to_owned())),
        files,
        paths,
    })
}

/// The agent a candidate id names when no tree is at hand: `cand-<agent>-a<n>`
/// for an attempt's candidate, `cand-<agent>` for a node without attempts.
fn sealer_named_by(candidate: &str) -> Option<String> {
    let named = candidate.strip_prefix("cand-")?;
    let agent = named
        .rsplit_once("-a")
        .filter(|(_, seq)| !seq.is_empty() && seq.bytes().all(|byte| byte.is_ascii_digit()))
        .map_or(named, |(agent, _)| agent);
    (!agent.is_empty()).then(|| agent.to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
