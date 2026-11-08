//! Activity groups over the durable tool-row projection (spec 02 §5.4):
//! summary plus tip while live, one line once settled, every item when open.

use super::blocks::{
    DisplayMode, RenderPrefs, RowPlan, SUBMILLISECOND_ELAPSED, ScrollBlock, ToolVerb,
    format_group_elapsed, humanize_tool, process_subject_verb, recorded_tool_duration, row_indent,
    subject_tool_verb, tool_is_failed, tool_is_interrupted, tool_is_running, tool_state_detail,
};
use super::presentation::ActivityKind;
use plakous_host_protocol::views::transcript::WaitFacts;

#[cfg(test)]
thread_local! {
    pub(crate) static PLAN_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn hud_verb(name: &str, kind: ActivityKind, subject: Option<&str>) -> &'static str {
    if name == "wait" {
        return "waiting";
    }
    hud_verb_for(subject_tool_verb(name, kind, subject))
}

pub(super) fn hud_verb_for(verb: ToolVerb) -> &'static str {
    match verb {
        ToolVerb::Run => "running command",
        ToolVerb::WaitProcess => "waiting",
        ToolVerb::ReadProcess => "reading",
        ToolVerb::InputProcess => "sending input",
        ToolVerb::InterruptProcess => "sending interrupt",
        ToolVerb::StopProcess => "stopping process",
        ToolVerb::Edit | ToolVerb::Write => "editing",
        ToolVerb::Read | ToolVerb::List | ToolVerb::Fetch => "reading",
        ToolVerb::Search | ToolVerb::WebSearch | ToolVerb::XSearch => "searching",
        ToolVerb::WaitAgent | ToolVerb::Agent => "waiting on agents",
        ToolVerb::Compact => "compacting",
        _ => "working",
    }
}

/// The label of a Code Mode cell (`exec`/`wait`, `exec_js`, `exec_py`).
const CODE_MODE: &str = "CodeMode";

fn noun(count: usize, singular: &str) -> String {
    format!(
        "{count} {singular}{}",
        if count == 1 {
            ""
        } else if singular.ends_with('s') || singular.ends_with("ch") {
            "es"
        } else {
            "s"
        }
    )
}

fn verb(block: &ScrollBlock) -> Option<ToolVerb> {
    match block {
        ScrollBlock::Tool {
            name,
            kind,
            subject,
            view,
            ..
        } => Some(
            view.operation
                .map(super::blocks::operation_verb)
                .unwrap_or_else(|| subject_tool_verb(name, *kind, subject.as_deref())),
        ),
        _ => None,
    }
}

fn exploration(block: &ScrollBlock) -> bool {
    matches!(verb(block), Some(ToolVerb::Read | ToolVerb::Search))
}

fn same_parent(left: &ScrollBlock, right: &ScrollBlock) -> bool {
    matches!((left, right), (ScrollBlock::Tool { parent_call_id: a, .. }, ScrollBlock::Tool { parent_call_id: b, .. }) if a == b)
}

fn edit_counts(subject: &str) -> Option<(usize, usize)> {
    subject
        .rsplit_once(" +")
        .and_then(|(_, suffix)| suffix.split_once("/-"))
        .filter(|(a, r)| {
            !a.is_empty()
                && !r.is_empty()
                && a.bytes().all(|c| c.is_ascii_digit())
                && r.bytes().all(|c| c.is_ascii_digit())
        })
        .and_then(|(a, r)| Some((a.parse().ok()?, r.parse().ok()?)))
}

fn exits_label(exits: &[String]) -> String {
    let mut unique = Vec::new();
    for exit in exits {
        if !unique.contains(&exit.as_str()) {
            unique.push(exit.as_str());
        }
    }
    let mut label = unique
        .iter()
        .take(3)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if unique.len() > 3 {
        label.push_str(&format!(", +{}", unique.len() - 3));
    }
    label
}

fn script_calls(subject: &str) -> usize {
    subject
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn script_failures(subject: &str) -> usize {
    subject
        .split(" · ")
        .find_map(|part| part.strip_suffix(" failed").and_then(|n| n.parse().ok()))
        .unwrap_or(0)
}

fn failure_exit(state: &str) -> Option<&str> {
    tool_state_detail(state)?
        .split(" · ")
        .next()
        .filter(|detail| detail.starts_with("exit "))
}

fn append_problems(label: &mut String, failed: usize, interrupted: usize) {
    let mut problems = Vec::new();
    if failed > 0 {
        problems.push(format!("{failed} failed"));
    }
    if interrupted > 0 {
        problems.push(format!("{interrupted} interrupted"));
    }
    if !problems.is_empty() {
        label.push_str(&format!(" ({})", problems.join(", ")));
    }
}

fn agent_count(name: &str, subject: &str) -> usize {
    if name == "wait_agent" {
        subject
            .split(" · ")
            .next()
            .unwrap_or_default()
            .split(',')
            .filter(|id| !id.trim().is_empty())
            .count()
            .max(1)
    } else {
        1
    }
}

/// Milliseconds of a label [`super::blocks::format_tool_elapsed`] produced (`12ms`, `1.2s`,
/// `<1ms`).
fn elapsed_ms(label: &str) -> Option<u64> {
    if label == SUBMILLISECOND_ELAPSED {
        return Some(0);
    }
    if let Some(ms) = label.strip_suffix("ms") {
        return ms.parse().ok();
    }
    let seconds = label.strip_suffix('s')?;
    let (whole, tenths) = seconds.split_once('.').unwrap_or((seconds, "0"));
    let whole: u64 = whole.parse().ok()?;
    let tenths: u64 = tenths.get(..1).unwrap_or("0").parse().ok()?;
    Some(whole * 1000 + tenths * 100)
}

/// A recorded tool elapsed as a row shows it: past a minute in the group
/// meta's long format (`21m40s`, `1h05m`), else as recorded (`12.3s`).
fn long_elapsed(label: &str) -> String {
    match elapsed_ms(label) {
        Some(ms) if ms >= 60_000 => format_group_elapsed(ms),
        _ => label.to_owned(),
    }
}

/// The elapsed a live tip paints beside its spinner, in whole seconds so a
/// repaint never reflows the row; nothing under one second.
pub(super) fn tip_elapsed(elapsed: std::time::Duration) -> Option<String> {
    match elapsed.as_secs() {
        0 => None,
        secs => Some(super::blocks::format_duration_secs(secs)),
    }
}

#[derive(Default)]
struct Counts {
    reads: usize,
    searches: usize,
    edits: usize,
    writes: usize,
    commands: usize,
    scripts: usize,
    script_calls: usize,
    process_checks: usize,
    process_inputs: usize,
    process_interrupts: usize,
    process_stops: usize,
    spawned_agents: usize,
    failed_spawns: usize,
    finished_agents: std::collections::HashSet<String>,
    messaged_agents: usize,
    listed_agents: usize,
    closed_agents: usize,
    interrupted_agents: usize,
    recovered_agents: usize,
    adopted: usize,
    discarded_workspaces: usize,
    stop_all: usize,
    submitted_results: usize,
    plan_updates: usize,
    /// Sealed thoughts inside the group.
    thoughts: usize,
    /// Every other tool by its own label, in first-seen order.
    other: Vec<(String, usize)>,
    added: usize,
    removed: usize,
    unknown_edits: usize,
    failures: usize,
    interruptions: usize,
    exits: Vec<String>,
    /// Summed call durations: the meta when the span is unknown.
    elapsed_ms: u64,
    /// Earliest host start and latest host finish among the stamped calls.
    span: Option<(u64, u64)>,
    /// A call reported a duration without the host's stamps (a carried row,
    /// or a live call the host held no start for), so the span is partial.
    unstamped: bool,
}

/// One group summary line: label, failure flag and right-aligned meta.
#[derive(Debug, Eq, PartialEq)]
struct Summary {
    label: String,
    failed: bool,
    meta: Option<String>,
}

impl Counts {
    fn record_finished(&mut self, name: &str, subject: &str) {
        let identities: Vec<_> = if name == "wait_agent" {
            subject
                .split(" · ")
                .next()
                .unwrap_or_default()
                .split(',')
                .filter(|id| !id.trim().is_empty())
                .map(str::trim)
                .collect()
        } else if name == "child_result" {
            subject
                .split_once(" · ")
                .and_then(|(_, result)| {
                    let (identity, status) = result.split_once(' ')?;
                    matches!(
                        status,
                        "completed"
                            | "completed_without_submission"
                            | "failed"
                            | "closed"
                            | "cancelled"
                            | "recovery_required"
                            | "blocked"
                            | "adopted"
                    )
                    .then_some(identity)
                })
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        self.finished_agents
            .extend(identities.into_iter().map(str::to_owned));
    }

    fn record_other(&mut self, label: String) {
        if let Some(entry) = self.other.iter_mut().find(|(have, _)| *have == label) {
            entry.1 += 1;
        } else {
            self.other.push((label, 1));
        }
    }

    fn summary(&self) -> Summary {
        let mut parts = Vec::new();
        let counted = |parts: &mut Vec<String>, count: usize, verb: &str, singular: &str| {
            if count > 0 {
                parts.push(format!("{verb} {}", noun(count, singular)));
            }
        };
        counted(&mut parts, self.reads, "read", "file");
        counted(&mut parts, self.searches, "ran", "search");
        if self.edits > 0 {
            let mut label = format!("edited {}", noun(self.edits, "file"));
            if self.unknown_edits == 0 && (self.added > 0 || self.removed > 0) {
                label.push_str(&format!(" (+{} −{})", self.added, self.removed));
            }
            parts.push(label);
        }
        counted(&mut parts, self.writes, "wrote", "file");
        counted(&mut parts, self.commands, "ran", "command");
        if self.scripts > 0 {
            let mut label = if self.scripts == 1 {
                CODE_MODE.to_owned()
            } else {
                format!("{CODE_MODE} ×{}", self.scripts)
            };
            if self.script_calls > 0 {
                label.push_str(&format!(" · {} calls", self.script_calls));
            }
            parts.push(label);
        }
        counted(&mut parts, self.process_checks, "checked", "process");
        counted(&mut parts, self.process_inputs, "sent input to", "process");
        counted(
            &mut parts,
            self.process_interrupts,
            "interrupted",
            "process",
        );
        counted(&mut parts, self.process_stops, "stopped", "process");
        counted(&mut parts, self.spawned_agents, "spawned", "agent");
        counted(&mut parts, self.failed_spawns, "attempted", "spawn");
        let finished = self.finished_agents.len();
        if finished > 0 {
            parts.push(format!("{} finished", noun(finished, "agent")));
        }
        counted(&mut parts, self.messaged_agents, "messaged", "agent");
        if self.listed_agents > 0 {
            parts.push("listed agents".to_owned());
        }
        counted(&mut parts, self.closed_agents, "closed", "agent");
        counted(&mut parts, self.interrupted_agents, "interrupted", "agent");
        counted(&mut parts, self.recovered_agents, "recovered", "agent");
        counted(&mut parts, self.adopted, "adopted", "candidate");
        counted(
            &mut parts,
            self.discarded_workspaces,
            "discarded",
            "workspace",
        );
        counted(&mut parts, self.stop_all, "requested stop-all", "time");
        counted(&mut parts, self.submitted_results, "submitted", "result");
        if self.plan_updates > 0 {
            parts.push("updated plan".to_owned());
        }
        for (label, count) in &self.other {
            parts.push(if *count == 1 {
                label.clone()
            } else {
                format!("{label} ×{count}")
            });
        }
        if self.thoughts > 0 {
            parts.push(noun(self.thoughts, "thought"));
        }
        let mut label = parts.join(", ");
        if let Some(first) = label.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        append_problems(&mut label, self.failures, self.interruptions);
        let mut meta = Vec::new();
        if !self.exits.is_empty() {
            meta.push(exits_label(&self.exits));
        }
        let elapsed_ms = match self.span {
            Some((start, finish)) if !self.unstamped => finish.saturating_sub(start),
            _ => self.elapsed_ms,
        };
        if elapsed_ms > 0 {
            meta.push(format_group_elapsed(elapsed_ms));
        }
        Summary {
            label,
            failed: self.failures > 0 || self.interruptions > 0,
            meta: (!meta.is_empty()).then(|| meta.join(" · ")),
        }
    }
}

/// Summarize the top-level calls of `blocks` that `counted` admits (indices
/// into `blocks`). A cell's nested calls count through their cell.
#[allow(
    clippy::too_many_lines,
    reason = "Activity summary folds every transcript block kind while maintaining the same counted and uncounted aggregates."
)]
fn summary(blocks: &[ScrollBlock], counted: &dyn Fn(usize) -> bool) -> Summary {
    let cells: std::collections::HashMap<&str, usize> = blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| match block {
            ScrollBlock::Tool {
                kind: ActivityKind::Compose,
                call_id: Some(id),
                ..
            } => Some((id.as_str(), index)),
            _ => None,
        })
        .collect();
    let mut counts = Counts::default();
    let mut child_failures = std::collections::HashMap::<&String, usize>::new();
    let mut child_interruptions = std::collections::HashMap::<&String, usize>::new();
    for block in blocks {
        if let ScrollBlock::Tool {
            parent_call_id: Some(parent),
            state,
            ..
        } = block
            && tool_is_failed(state)
        {
            if tool_is_interrupted(state) {
                *child_interruptions.entry(parent).or_default() += 1;
            } else {
                *child_failures.entry(parent).or_default() += 1;
            }
        }
    }
    for (index, block) in blocks.iter().enumerate() {
        if is_grouped_thought(block) {
            counts.thoughts += usize::from(counted(index));
            continue;
        }
        let ScrollBlock::Tool {
            call_id,
            name,
            kind,
            state,
            subject,
            parent_call_id,
            started_at_ms,
            finished_at_ms,
            ..
        } = block
        else {
            continue;
        };
        if *kind == ActivityKind::Compact {
            continue;
        }
        let owner = parent_call_id
            .as_deref()
            .and_then(|parent| cells.get(parent))
            .copied();
        if !counted(owner.unwrap_or(index)) {
            continue;
        }
        if tool_is_failed(state)
            && let Some(detail) = failure_exit(state)
        {
            counts.exits.push(detail.to_owned());
        }
        if owner.is_some() {
            continue;
        }
        let duration = recorded_tool_duration(state).and_then(elapsed_ms);
        if let Some(ms) = duration {
            counts.elapsed_ms = counts.elapsed_ms.saturating_add(ms);
        }
        match (*started_at_ms, *finished_at_ms) {
            (Some(start), Some(finish)) => {
                counts.span = Some(counts.span.map_or((start, finish), |(first, last)| {
                    (first.min(start), last.max(finish))
                }));
            }
            _ if duration.is_some() => counts.unstamped = true,
            _ => {}
        }
        let interrupted = usize::from(tool_is_interrupted(state));
        let failed = usize::from(tool_is_failed(state) && interrupted == 0);
        let child_interrupted = call_id
            .as_ref()
            .and_then(|id| child_interruptions.get(id))
            .copied()
            .unwrap_or(0);
        counts.interruptions += if *kind == ActivityKind::Compose {
            child_interrupted.max(interrupted)
        } else {
            interrupted
        };
        counts.failures += if *kind == ActivityKind::Compose {
            script_failures(subject.as_deref().unwrap_or_default())
                .saturating_sub(child_interrupted)
                .max(
                    call_id
                        .as_ref()
                        .and_then(|id| child_failures.get(id))
                        .copied()
                        .unwrap_or(0),
                )
                .max(failed)
        } else {
            failed
        };
        match verb(block).unwrap_or(ToolVerb::Generic) {
            ToolVerb::Read | ToolVerb::List | ToolVerb::Fetch => counts.reads += 1,
            ToolVerb::Search => counts.searches += 1,
            ToolVerb::Edit => {
                counts.edits += subject
                    .as_deref()
                    .unwrap_or_default()
                    .split_once(" files ")
                    .and_then(|(count, _)| count.parse().ok())
                    .unwrap_or(1);
                if let Some((a, b)) = edit_counts(subject.as_deref().unwrap_or_default()) {
                    counts.added += a;
                    counts.removed += b;
                } else {
                    counts.unknown_edits += 1;
                }
            }
            ToolVerb::Write => counts.writes += 1,
            ToolVerb::Run => counts.commands += 1,
            ToolVerb::Compose => {
                counts.scripts += 1;
                counts.script_calls += script_calls(subject.as_deref().unwrap_or_default());
            }
            ToolVerb::WaitProcess | ToolVerb::ReadProcess => counts.process_checks += 1,
            ToolVerb::InputProcess => counts.process_inputs += 1,
            ToolVerb::InterruptProcess => counts.process_interrupts += 1,
            ToolVerb::StopProcess => counts.process_stops += 1,
            ToolVerb::SpawnAgent => {
                if tool_is_failed(state) {
                    counts.failed_spawns += 1;
                } else {
                    counts.spawned_agents +=
                        agent_count(name, subject.as_deref().unwrap_or_default());
                }
            }
            ToolVerb::WaitAgent | ToolVerb::Agent => {
                if !tool_is_failed(state) && !tool_is_running(state) {
                    counts.record_finished(name, subject.as_deref().unwrap_or_default());
                }
                if tool_is_failed(state)
                    || (*kind == ActivityKind::Agent
                        && name != "wait_agent"
                        && name != "child_result")
                {
                    counts.record_other(humanize_tool(name));
                }
            }
            ToolVerb::MessageAgent => counts.messaged_agents += 1,
            ToolVerb::ListAgents => counts.listed_agents += 1,
            ToolVerb::CloseAgent => counts.closed_agents += 1,
            ToolVerb::InterruptAgent => counts.interrupted_agents += 1,
            ToolVerb::RecoverAgent => counts.recovered_agents += 1,
            ToolVerb::Adopt => counts.adopted += 1,
            ToolVerb::DiscardWorkspace => counts.discarded_workspaces += 1,
            ToolVerb::StopAll => counts.stop_all += 1,
            ToolVerb::SubmitResult => counts.submitted_results += 1,
            ToolVerb::Plan => counts.plan_updates += 1,
            ToolVerb::WebSearch => counts.record_other("web search".to_owned()),
            ToolVerb::XSearch => counts.record_other("X search".to_owned()),
            ToolVerb::GenerateImage => counts.record_other("image generation".to_owned()),
            ToolVerb::Mcp => counts.record_other(super::blocks::display_tool_title_with_verb(
                name,
                ToolVerb::Mcp,
            )),
            ToolVerb::Compact | ToolVerb::Generic => counts.record_other(humanize_tool(name)),
        }
    }
    counts.summary()
}

fn is_activity_tool(block: &ScrollBlock) -> bool {
    matches!(block, ScrollBlock::Tool { kind, .. } if *kind != ActivityKind::Compact)
}

/// Rows that end the current group (spec 02 §5.4). Hidden thinking neither
/// renders nor ends one.
fn seals_activity(block: &ScrollBlock, prefs: RenderPrefs) -> bool {
    match block {
        ScrollBlock::Assistant(_)
        | ScrollBlock::User { .. }
        | ScrollBlock::Steer { .. }
        | ScrollBlock::Header(_)
        | ScrollBlock::Milestone { .. }
        | ScrollBlock::Mail { .. }
        | ScrollBlock::System(_)
        | ScrollBlock::Error(_) => true,
        ScrollBlock::Tool { kind, .. } => *kind == ActivityKind::Compact,
        ScrollBlock::Thinking { text, sealed, .. } => {
            prefs.show_thinking && (!*sealed || !text.trim().is_empty())
        }
        ScrollBlock::Item { .. } | ScrollBlock::Discarded { .. } | ScrollBlock::Approval { .. } => {
            false
        }
    }
}

/// A sealed thought leading into a call joins its group in every family.
fn ends_group(blocks: &[ScrollBlock], index: usize, prefs: RenderPrefs) -> bool {
    if !seals_activity(&blocks[index], prefs) {
        return false;
    }
    if !is_grouped_thought(&blocks[index]) {
        return true;
    }
    blocks[index + 1..]
        .iter()
        .find(|block| {
            is_activity_tool(block) || (seals_activity(block, prefs) && !is_grouped_thought(block))
        })
        .is_none_or(|block| !is_activity_tool(block))
}

fn is_grouped_thought(block: &ScrollBlock) -> bool {
    matches!(block, ScrollBlock::Thinking { sealed: true, .. })
}

/// Keep the original block indices: selection, paging and wheel anchors are
/// block-space identities, not indices into a second grouped transcript.
pub(super) fn plan(blocks: &[ScrollBlock], prefs: RenderPrefs) -> Vec<RowPlan> {
    #[cfg(test)]
    PLAN_CALLS.set(PLAN_CALLS.get() + 1);
    let mut plans = vec![RowPlan::default(); blocks.len()];
    for (plan, block) in plans.iter_mut().zip(blocks) {
        if let ScrollBlock::Tool { view, .. } = block {
            plan.operation = view.operation;
        }
    }
    let mut start = 0;
    while start < blocks.len() {
        let starts_group = is_activity_tool(&blocks[start])
            || (prefs.show_thinking
                && is_grouped_thought(&blocks[start])
                && !ends_group(blocks, start, prefs));
        if !starts_group {
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < blocks.len() && !ends_group(blocks, end, prefs) {
            end += 1;
        }
        if !(start..end).any(|i| is_activity_tool(&blocks[i])) {
            start = end;
            continue;
        }
        // A group ends at a row that seals it, or runs to the live tail.
        let sealed = end < blocks.len();
        plan_group(blocks, &mut plans, start, end, sealed, prefs);
        start = end;
    }
    // Consecutive milestone rows stack; the last one closes with the blank.
    for index in 1..blocks.len() {
        if matches!(blocks[index - 1], ScrollBlock::Milestone { .. })
            && matches!(blocks[index], ScrollBlock::Milestone { .. })
        {
            plans[index - 1].activity_continues = true;
        }
    }
    let mut start = 0;
    while start < blocks.len() {
        if !matches!(blocks[start], ScrollBlock::Mail { .. }) {
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < blocks.len() && matches!(blocks[end], ScrollBlock::Mail { .. }) {
            end += 1;
        }
        if end - start > 1 {
            let open = matches!(
                blocks[start],
                ScrollBlock::Mail {
                    group_open: true,
                    ..
                }
            );
            let mut agents = Vec::new();
            for block in &blocks[start..end] {
                if let ScrollBlock::Mail { text, .. } = block
                    && let Some((_, agent, _)) = super::blocks::mail_parts(text)
                    && !agents.contains(&agent)
                {
                    agents.push(agent);
                }
            }
            plans[start].mail_summary =
                Some(format!("{} messages · {}", end - start, agents.join(" ")));
            plans[start].group_folded = !open;
            for (index, plan) in plans.iter_mut().enumerate().take(end).skip(start) {
                plan.group_owner = Some(start);
                plan.activity_continues = index + 1 < end;
                if index > start {
                    plan.hidden = !open;
                }
            }
        }
        start = end;
    }
    plans
}

/// One group `start..end`: summary plus tip while a call runs, one summary
/// line (or the single call's own item line) once none does, every item when
/// opened or when grouping is off.
#[allow(
    clippy::too_many_lines,
    reason = "Plan grouping preserves active, completed, failed, and folded step presentation as one contiguous activity group."
)]
fn plan_group(
    blocks: &[ScrollBlock],
    plans: &mut [RowPlan],
    start: usize,
    end: usize,
    sealed: bool,
    prefs: RenderPrefs,
) {
    let cells: std::collections::HashMap<&str, usize> = (start..end)
        .filter_map(|index| match &blocks[index] {
            ScrollBlock::Tool {
                kind: ActivityKind::Compose,
                call_id: Some(id),
                ..
            } => Some((id.as_str(), index)),
            _ => None,
        })
        .collect();
    let mut owners: Vec<Option<usize>> = (start..end)
        .map(|index| match &blocks[index] {
            ScrollBlock::Tool {
                parent_call_id: Some(parent),
                ..
            } => cells.get(parent.as_str()).copied(),
            _ => None,
        })
        .collect();
    for index in start..end {
        let mut visited = std::collections::HashSet::new();
        let mut current = index;
        while let Some(parent) = owners[current - start] {
            if !visited.insert(current) {
                owners[index - start] = None;
                break;
            }
            current = parent;
        }
    }
    let owner = |index: usize| owners[index - start];
    let top: Vec<usize> = (start..end)
        .filter(|index| is_activity_tool(&blocks[*index]) && owner(*index).is_none())
        .collect();
    // Only thoughts leading into a call belong to the summary; trailing empty
    // thoughts neither end the group nor contribute to it.
    let last_call = (start..end)
        .rfind(|index| is_activity_tool(&blocks[*index]))
        .unwrap_or(start);
    let thoughts: Vec<usize> = (start..end)
        .filter(|index| {
            *index < last_call && prefs.show_thinking && is_grouped_thought(&blocks[*index])
        })
        .collect();
    let running = |index: usize| matches!(&blocks[index], ScrollBlock::Tool { state, .. } if tool_is_running(state));
    let Some(&first) = top.first() else {
        return;
    };
    let open = matches!(
        blocks[first],
        ScrollBlock::Tool {
            group_open: true,
            ..
        }
    );
    plans[start].group_owner = Some(first);
    plan_cells(blocks, plans, start, end, &owner);
    for index in start..end {
        if matches!(&blocks[index], ScrollBlock::Thinking { text, sealed: true, .. } if text.trim().is_empty())
        {
            plans[index].hidden = true;
        }
    }
    let legacy_single = thoughts.is_empty()
        && top.len() == 1
        && !matches!(
            blocks[first],
            ScrollBlock::Tool {
                kind: ActivityKind::Compose,
                ..
            }
        );
    if !prefs.group_tool_calls || open {
        if prefs.group_tool_calls && (top.len() > 1 || legacy_single) {
            let found = summary(&blocks[start..end], &|i| {
                top.contains(&(start + i)) || thoughts.contains(&(start + i))
            });
            set_header(&mut plans[start], found, top.len());
            plans[start].group_head_open = true;
        }
        merge_exploration(blocks, plans, start, end);
        merge_agents(blocks, plans, start, end, !sealed);
        rail(
            blocks,
            plans,
            start,
            end,
            open && prefs.group_tool_calls && (top.len() > 1 || legacy_single),
            &owner,
        );
        return;
    }
    let finished: Vec<usize> = top.iter().copied().filter(|i| !running(*i)).collect();
    let summarized = top.len() > 1 && !finished.is_empty();
    if summarized {
        let found = summary(&blocks[start..end], &|i| {
            finished.contains(&(start + i)) || thoughts.contains(&(start + i))
        });
        set_header(&mut plans[start], found, top.len());
        plans[start].group_folded = !running(start);
    }
    for index in start + 1..end {
        let hide = match &blocks[index] {
            ScrollBlock::Tool { .. } => {
                let mut item = index;
                while let Some(parent) = owner(item) {
                    item = parent;
                }
                summarized && finished.contains(&item)
            }
            ScrollBlock::Thinking { .. } if thoughts.contains(&index) => summarized,
            // A resolved approval is a durable outcome a fold must not hide.
            ScrollBlock::Approval {
                resolution: Some(_),
                ..
            } => false,
            _ => sealed,
        };
        plans[index].hidden |= hide;
    }
    if summarized && !running(start) {
        for (index, plan) in plans.iter_mut().enumerate().take(end).skip(start + 1) {
            if owner(index) == Some(start) {
                plan.hidden = true;
            }
        }
    }
    rail(blocks, plans, start, end, false, &owner);
}

fn set_header(plan: &mut RowPlan, found: Summary, count: usize) {
    plan.hidden = false;
    plan.activity_summary = Some(found.label);
    plan.activity_failed = found.failed;
    plan.summary_meta = found.meta;
    plan.group_count = count;
}

/// Spacing and, for an open group, the rails: one per top-level member, and a
/// cell's nested calls one level deeper under their cell's rail.
fn rail(
    blocks: &[ScrollBlock],
    plans: &mut [RowPlan],
    start: usize,
    end: usize,
    open: bool,
    owner: &dyn Fn(usize) -> Option<usize>,
) {
    let visible: Vec<_> = super::blocks::row_order_range(plans, start, end)
        .into_iter()
        .filter(|i| *i >= start && *i < end && !plans[*i].hidden)
        .collect();
    for (position, index) in visible.iter().copied().enumerate() {
        plans[index].activity_continues = position + 1 < visible.len();
    }
    let last_member = visible.iter().copied().rfind(|index| {
        owner(*index).is_none() && !matches!(&blocks[*index], ScrollBlock::Thinking { .. })
    });
    for &index in &visible {
        let member = owner(index).unwrap_or(index);
        if open {
            plans[index].group_child = Some(Some(member) == last_member);
        }
    }
    for index in visible {
        let rails: Vec<_> = plans[index]
            .cell_tails
            .iter()
            .map(|tail| row_indent(&plans[tail.owner]))
            .collect();
        for (tail, rail) in plans[index].cell_tails.iter_mut().zip(rails) {
            tail.rail = rail;
        }
    }
}

/// A cell keeps a bounded outline in call order, with active and unsuccessful
/// calls taking priority. Its source/output footer follows the outline.
fn plan_cells(
    blocks: &[ScrollBlock],
    plans: &mut [RowPlan],
    start: usize,
    end: usize,
    parent_of: &dyn Fn(usize) -> Option<usize>,
) {
    let mut children = std::collections::HashMap::<usize, Vec<usize>>::new();
    for index in start..end {
        if let Some(parent) = parent_of(index) {
            children.entry(parent).or_default().push(index);
        }
    }
    for index in start..end {
        let ScrollBlock::Tool {
            kind: ActivityKind::Compose,
            subject,
            state,
            ..
        } = &blocks[index]
        else {
            continue;
        };
        let owned = children.get(&index).map(Vec::as_slice).unwrap_or_default();
        let mut selected = owned.to_vec();
        selected.sort_by_key(|child| match &blocks[*child] {
            ScrollBlock::Tool { state, .. } if tool_is_running(state) => 0,
            ScrollBlock::Tool { state, .. } if tool_is_failed(state) => 1,
            _ => 2,
        });
        selected.truncate(10);
        selected.sort_unstable();
        let mut failed = 0;
        let mut interrupted = 0;
        let mut exits = Vec::new();
        let mut child_running = false;
        for &child in owned {
            plans[child].hidden = !selected.contains(&child);
            plans[child].compose_child =
                Some(selected.last() == Some(&child) && owned.len() == selected.len());
            plans[child].compose_owner = Some(index);
            if let ScrollBlock::Tool { state, .. } = &blocks[child] {
                child_running |= tool_is_running(state);
                interrupted += usize::from(tool_is_interrupted(state));
                failed += usize::from(tool_is_failed(state) && !tool_is_interrupted(state));
                if let Some(exit) = failure_exit(state) {
                    exits.push(exit.to_owned());
                }
            }
        }
        let subject = subject.as_deref().unwrap_or_default();
        failed = failed.max(script_failures(subject).saturating_sub(interrupted));
        interrupted = interrupted.max(usize::from(tool_is_interrupted(state)));
        let mut label = CODE_MODE.to_owned();
        let calls = script_calls(subject);
        if calls > 0 {
            label.push_str(&format!(" · {}", noun(calls, "call")));
        }
        append_problems(&mut label, failed, interrupted);
        plans[index].activity_action = Some(label);
        plans[index].activity_meta = (!exits.is_empty()).then(|| exits_label(&exits));
        plans[index].cell_failed = failed > 0 || interrupted > 0;
        plans[index].activity_running = child_running;
        plans[*selected.last().unwrap_or(&index)]
            .cell_tails
            .push(crate::tui::blocks::CellTail {
                owner: index,
                hidden_calls: owned.len().saturating_sub(selected.len()),
                rail: "  ",
            });
    }
    let mut tails = Vec::new();
    for (index, plan) in plans.iter_mut().enumerate().take(end).skip(start) {
        for tail in std::mem::take(&mut plan.cell_tails) {
            tails.push((index, tail));
        }
    }
    tails.sort_by_key(|(_, tail)| {
        let mut depth = 0;
        let mut current = tail.owner;
        while let Some(parent) = parent_of(current) {
            depth += 1;
            current = parent;
        }
        std::cmp::Reverse(depth)
    });
    for (mut last, tail) in tails {
        let mut visited = std::collections::HashSet::new();
        while visited.insert(last) {
            let next = (start..end)
                .rev()
                .find(|child| plans[*child].compose_owner == Some(last) && !plans[*child].hidden);
            let Some(next) = next else {
                break;
            };
            last = next;
        }
        plans[last].cell_tails.push(tail);
    }
}

fn merge_exploration(blocks: &[ScrollBlock], plans: &mut [RowPlan], start: usize, end: usize) {
    let mut index = start;
    while index < end {
        if plans[index].hidden
            || plans[index].compose_child.is_some()
            || !exploration(&blocks[index])
        {
            index += 1;
            continue;
        }
        let mut next = index;
        let (mut reads, mut searches, mut running, mut failed) = (0, 0, false, false);
        while next < end && exploration(&blocks[next]) && same_parent(&blocks[index], &blocks[next])
        {
            if verb(&blocks[next]) == Some(ToolVerb::Read) {
                reads += 1;
            } else {
                searches += 1;
            }
            if let ScrollBlock::Tool { state, .. } = &blocks[next] {
                running |= tool_is_running(state);
                failed |= tool_is_failed(state);
            }
            next += 1;
        }
        // Failed calls keep their identity and exit status; a lone read or
        // search keeps its own item line.
        if !failed && next - index > 1 {
            let parts: Vec<_> = [(reads, "file"), (searches, "search")]
                .into_iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, label)| noun(n, label))
                .collect();
            plans[index].activity_action = Some(format!("Explored {}", parts.join(", ")));
            plans[index].activity_running = running;
            open_members(blocks, plans, index, next);
        }
        index = next;
    }
}

/// The verb a block merges under in an agent run; a wait that carries its
/// facts is its own item.
fn agent_verb(block: &ScrollBlock) -> Option<ToolVerb> {
    match block {
        ScrollBlock::Tool { wait: Some(_), .. } => None,
        _ => verb(block),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "Agent activity merging reconciles tool results, child lifecycle, and visible summaries before emitting ordered rows."
)]
fn merge_agents(
    blocks: &[ScrollBlock],
    plans: &mut [RowPlan],
    start: usize,
    end: usize,
    live: bool,
) {
    // Spawn subjects carry briefs, not child ids. Match the completion count
    // within the same host-supplied parent scope instead of guessing identities.
    let mut finished_by_parent = std::collections::HashMap::new();
    for block in &blocks[start..end] {
        if let ScrollBlock::Tool {
            name,
            state,
            subject,
            parent_call_id,
            ..
        } = block
            && matches!(
                agent_verb(block),
                Some(ToolVerb::WaitAgent | ToolVerb::Agent)
            )
            && !tool_is_running(state)
            && !tool_is_failed(state)
        {
            finished_by_parent
                .entry(parent_call_id.as_deref())
                .or_insert_with(Counts::default)
                .record_finished(name, subject.as_deref().unwrap_or_default());
        }
    }
    let mut remaining: std::collections::HashMap<_, _> = finished_by_parent
        .into_iter()
        .map(|(parent, counts)| (parent, counts.finished_agents.len()))
        .collect();
    let mut index = start;
    while index < end {
        let activity = agent_verb(&blocks[index]);
        if plans[index].compose_child.is_some()
            || !matches!(
                activity,
                Some(ToolVerb::SpawnAgent | ToolVerb::WaitAgent | ToolVerb::Agent)
            )
        {
            index += 1;
            continue;
        }
        let spawn = activity == Some(ToolVerb::SpawnAgent);
        let mut next = index;
        let (mut running, mut failed) = (false, false);
        let mut count = 0;
        let mut finished = Counts::default();
        while next < end
            && if spawn {
                agent_verb(&blocks[next]) == activity
            } else {
                matches!(
                    agent_verb(&blocks[next]),
                    Some(ToolVerb::WaitAgent | ToolVerb::Agent)
                )
            }
            && same_parent(&blocks[index], &blocks[next])
        {
            if let ScrollBlock::Tool {
                name,
                subject,
                state,
                ..
            } = &blocks[next]
            {
                count += agent_count(name, subject.as_deref().unwrap_or_default());
                if !spawn {
                    running |= tool_is_running(state);
                    finished.record_finished(name, subject.as_deref().unwrap_or_default());
                }
                failed |= tool_is_failed(state);
            }
            next += 1;
        }
        if spawn {
            let parent = match &blocks[index] {
                ScrollBlock::Tool { parent_call_id, .. } => parent_call_id.as_deref(),
                _ => None,
            };
            let completed = remaining.entry(parent).or_default();
            let matched = count.min(*completed);
            *completed -= matched;
            running = live && matched < count;
            if running {
                count -= matched;
            }
        } else {
            count = finished.finished_agents.len();
        }
        if !failed && count > 0 {
            plans[index].activity_action = Some(format!(
                "{} {}",
                noun(count, "agent"),
                if running {
                    "working"
                } else if spawn {
                    "spawned"
                } else {
                    "finished"
                }
            ));
            plans[index].activity_running = running;
            open_members(blocks, plans, index, next);
        }
        index = next;
    }
}

/// A merged row `head..end` hides its calls until the operator opens it;
/// then each call is its own item one rail deeper, the head's own call first.
fn open_members(blocks: &[ScrollBlock], plans: &mut [RowPlan], head: usize, end: usize) {
    let open = matches!(
        &blocks[head],
        ScrollBlock::Tool { view, .. } if view.members_open
    );
    let nested = plans[head].compose_child.is_none();
    plans[head].members_open = open;
    for (index, plan) in plans.iter_mut().enumerate().take(end).skip(head + 1) {
        plan.hidden = !open;
        if open && nested {
            plan.compose_child = Some(index + 1 == end);
        }
    }
}

/// One item line's verb and subject, single-line, without rail or glyph.
pub(super) fn item_label(
    name: &str,
    state: &str,
    kind: ActivityKind,
    subject: Option<&str>,
    wait: Option<&WaitFacts>,
    plan: &RowPlan,
) -> String {
    one_line(&item_label_raw(name, state, kind, subject, wait, plan))
}

fn process_subject_is_label(name: &str, verb: ToolVerb, subject: &str) -> bool {
    (verb != ToolVerb::Run || name == "exec_command")
        && matches!(
            verb,
            ToolVerb::Run
                | ToolVerb::WaitProcess
                | ToolVerb::ReadProcess
                | ToolVerb::InputProcess
                | ToolVerb::InterruptProcess
                | ToolVerb::StopProcess
        )
        && process_subject_verb(subject).is_some()
}

/// [`item_label`] with the subject's own newlines and tabs, which a copy of
/// the row carries.
pub(super) fn item_label_raw(
    name: &str,
    state: &str,
    kind: ActivityKind,
    subject: Option<&str>,
    wait: Option<&WaitFacts>,
    plan: &RowPlan,
) -> String {
    let running = tool_is_running(state) || plan.activity_running;
    let subject = subject.unwrap_or_default();
    let verb = plan
        .operation
        .map(super::blocks::operation_verb)
        .unwrap_or_else(|| subject_tool_verb(name, kind, Some(subject)));
    let mut label = if let Some(label) = &plan.activity_action {
        label.clone()
    } else if let Some(facts) = wait {
        wait_item_label(name, state, kind, subject, facts, plan)
    } else {
        let or_unknown = if subject.is_empty() {
            "(unknown)"
        } else {
            subject
        };
        match verb {
            verb if process_subject_is_label(name, verb, subject) => {
                if !running && let Some(subject) = subject.strip_prefix("Waiting on process ") {
                    format!("Waited on process {subject}")
                } else {
                    subject.to_owned()
                }
            }
            ToolVerb::Run if subject.trim().is_empty() => "Run process".to_owned(),
            ToolVerb::Run => format!("$ {subject}"),
            ToolVerb::WaitProcess => format!(
                "{} {or_unknown}",
                if running {
                    "Waiting on process"
                } else {
                    "Waited on process"
                },
            ),
            ToolVerb::ReadProcess
            | ToolVerb::InputProcess
            | ToolVerb::InterruptProcess
            | ToolVerb::StopProcess => {
                format!(
                    "{} {or_unknown}",
                    super::blocks::display_tool_title_with_verb(name, verb)
                )
            }
            ToolVerb::Edit => match edit_counts(subject) {
                Some((a, b)) => {
                    let path = subject.rsplit_once(" +").map_or(subject, |(path, _)| path);
                    if a == 0 && b == 0 {
                        format!("Edited {path}")
                    } else {
                        format!("Edited {path} +{a} −{b}")
                    }
                }
                None => format!("Edited {subject}"),
            },
            ToolVerb::Write => {
                let path = edit_counts(subject)
                    .and_then(|_| subject.rsplit_once(" +"))
                    .map_or(subject, |(path, _)| path);
                format!("Wrote {path}")
            }
            ToolVerb::Compose if running => CODE_MODE.to_owned(),
            ToolVerb::Compose => format!("{CODE_MODE} · {}", noun(script_calls(subject), "call")),
            ToolVerb::Read => format!("Read {subject}"),
            ToolVerb::Search => format!("Searched {subject}"),
            ToolVerb::List => format!("Listed {subject}"),
            ToolVerb::Fetch if running => format!("Fetching {subject}"),
            ToolVerb::Fetch => format!("Fetched {subject}"),
            ToolVerb::WebSearch if running => format!("Searching the web {subject}"),
            ToolVerb::WebSearch => format!("Searched the web {subject}"),
            ToolVerb::XSearch if running => format!("Searching X {subject}"),
            ToolVerb::XSearch => format!("Searched X {subject}"),
            ToolVerb::GenerateImage if running => format!("Generating image {subject}"),
            ToolVerb::GenerateImage => format!("Generated image {subject}"),
            ToolVerb::Plan if subject.is_empty() => "Updated plan".to_owned(),
            ToolVerb::Plan => format!("Updated plan · {subject}"),
            ToolVerb::MessageAgent => format!("Messaged {subject}"),
            ToolVerb::ListAgents if subject.is_empty() => "Listed agents".to_owned(),
            ToolVerb::ListAgents => format!("Listed agents · {subject}"),
            ToolVerb::SpawnAgent => format!("Spawn agent {subject}"),
            ToolVerb::WaitAgent => format!("Wait on agents {subject}"),
            ToolVerb::Agent => format!("Agent {subject}"),
            _ => format!(
                "{} {subject}",
                super::blocks::display_tool_title_with_verb(name, verb)
            ),
        }
    };
    if tool_is_interrupted(state) && plan.activity_action.is_none() {
        label.push_str(" · interrupted");
    }
    label.truncate(label.trim_end().len());
    label
}

fn wait_item_label(
    name: &str,
    state: &str,
    kind: ActivityKind,
    subject: &str,
    facts: &WaitFacts,
    plan: &RowPlan,
) -> String {
    let running = tool_is_running(state) || plan.activity_running;
    let verb = plan
        .operation
        .map(super::blocks::operation_verb)
        .unwrap_or_else(|| subject_tool_verb(name, kind, Some(subject)));
    let head = match verb {
        ToolVerb::WaitAgent | ToolVerb::Agent => if running {
            "Waiting for agents"
        } else {
            "Waited for agents"
        }
        .to_owned(),
        _ => item_label(name, state, kind, Some(subject), None, plan),
    };
    let mut label = crate::tui::blocks::wait_label(head, running, facts);
    if tool_is_failed(state)
        && !tool_is_interrupted(state)
        && let Some(reason) = tool_state_detail(state)
    {
        label.push_str(" · ");
        label.push_str(reason);
    }
    label
}

/// Embedded newlines show as `↵`, so an item is exactly one terminal row.
pub(super) fn one_line(text: &str) -> String {
    text.replace("\r\n", "↵")
        .replace(['\n', '\r'], "↵")
        .replace('\t', " ")
}

/// The status glyph of an item line; `⠼` is the spinner the paint animates.
pub(super) fn item_glyph(state: &str, kind: ActivityKind, plan: &RowPlan) -> &'static str {
    if tool_is_running(state) || plan.activity_running {
        "⠼"
    } else if tool_is_failed(state)
        || (kind == ActivityKind::Compose && plan.cell_failed)
        || (kind != ActivityKind::Compose && plan.activity_failed)
    {
        "✗"
    } else {
        "✓"
    }
}

/// Right-aligned meta of an item line: exit code, then duration.
pub(super) fn item_meta(
    name: &str,
    state: &str,
    kind: ActivityKind,
    subject: Option<&str>,
    plan: &RowPlan,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(meta) = &plan.activity_meta {
        parts.push(meta.clone());
    } else if tool_is_failed(state)
        // A process wait already names its exit in the subject.
        && !(plan.operation.map(super::blocks::operation_verb)
            .unwrap_or_else(|| subject_tool_verb(name, kind, subject)) == ToolVerb::WaitProcess
            && subject.is_some_and(|subject| subject.contains(" · exited ")))
        && let Some(exit) = failure_exit(state)
    {
        parts.push(exit.to_owned());
    }
    // A merged line stands for several calls; one call's time would mislead.
    let merged = plan.activity_action.is_some() && kind != ActivityKind::Compose;
    if !merged
        && !tool_is_running(state)
        && let Some(duration) = recorded_tool_duration(state)
    {
        parts.push(long_elapsed(duration));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The `▸`/`▾` an item line ends with: `▾` while its detail is open.
pub(super) fn item_caret(mode: DisplayMode) -> &'static str {
    if mode == DisplayMode::Expanded {
        "▾"
    } else {
        "▸"
    }
}

/// The left part of one item line: rail, status glyph, verb and subject.
pub(super) fn action(
    name: &str,
    state: &str,
    kind: ActivityKind,
    subject: Option<&str>,
    plan: &RowPlan,
) -> String {
    format!(
        "{}{} {}",
        row_indent(plan),
        item_glyph(state, kind, plan),
        item_label(name, state, kind, subject, None, plan)
    )
}

/// A summary line ends `▾` while its group is open, else `▸`.
pub(super) const fn header_caret(plan: &RowPlan) -> &'static str {
    if plan.group_head_open { "▾" } else { "▸" }
}

#[cfg(test)]
mod tests;
