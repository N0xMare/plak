//! Activity groups over the durable tool-row projection (spec 02 §5.4):
//! summary plus tip while live, one line once settled, every item when open.

use super::blocks::{
    DisplayMode, RenderPrefs, RowPlan, SUBMILLISECOND_ELAPSED, ScrollBlock, ToolVerb,
    display_tool_title_for, format_group_elapsed, humanize_tool, process_subject_verb,
    recorded_tool_duration, row_indent, subject_tool_verb, tool_is_failed, tool_is_interrupted,
    tool_is_running, tool_state_detail,
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
    match subject_tool_verb(name, kind, subject) {
        ToolVerb::Run => "running command",
        ToolVerb::WaitProcess => "waiting",
        ToolVerb::ReadProcess => "reading",
        ToolVerb::InputProcess => "sending input",
        ToolVerb::InterruptProcess => "sending interrupt",
        ToolVerb::StopProcess => "stopping process",
        ToolVerb::Edit | ToolVerb::Write => "editing",
        ToolVerb::Read | ToolVerb::List | ToolVerb::Fetch => "reading",
        ToolVerb::Search => "searching",
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
            ..
        } => Some(subject_tool_verb(name, *kind, subject.as_deref())),
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
        match subject_tool_verb(name, *kind, subject.as_deref()) {
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
    plans
}

/// One group `start..end`: summary plus tip while a call runs, one summary
/// line (or the single call's own item line) once none does, every item when
/// opened or when grouping is off.
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
    let mut label = if let Some(label) = &plan.activity_action {
        label.clone()
    } else if let Some(facts) = wait {
        let head = match subject_tool_verb(name, kind, Some(subject)) {
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
    } else {
        let or_unknown = if subject.is_empty() {
            "(unknown)"
        } else {
            subject
        };
        match subject_tool_verb(name, kind, Some(subject)) {
            verb if process_subject_verb(subject) == Some(verb) => {
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
                format!("{} {or_unknown}", display_tool_title_for(name, kind))
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
            ToolVerb::Fetch => format!("Fetched {subject}"),
            ToolVerb::Plan if subject.is_empty() => "Updated plan".to_owned(),
            ToolVerb::Plan => format!("Updated plan · {subject}"),
            ToolVerb::MessageAgent => format!("Messaged {subject}"),
            ToolVerb::ListAgents if subject.is_empty() => "Listed agents".to_owned(),
            ToolVerb::ListAgents => format!("Listed agents · {subject}"),
            ToolVerb::SpawnAgent => format!("Spawn agent {subject}"),
            ToolVerb::WaitAgent => format!("Wait on agents {subject}"),
            ToolVerb::Agent => format!("Agent {subject}"),
            _ => format!("{} {subject}", display_tool_title_for(name, kind)),
        }
    };
    if tool_is_interrupted(state) && plan.activity_action.is_none() {
        label.push_str(" · interrupted");
    }
    label.truncate(label.trim_end().len());
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
        && !(subject_tool_verb(name, kind, subject) == ToolVerb::WaitProcess
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
mod tests {
    use super::*;
    use crate::tui::blocks::INTERRUPTED_MARKER;

    fn all(_: usize) -> bool {
        true
    }

    #[test]
    fn process_actions_name_the_operation_and_outcome() {
        for (name, state, subject, expected) in [
            ("Shell", "running", "cargo test", "  ⠼ $ cargo test"),
            (
                "process_session_poll",
                "running",
                "7 · cargo test",
                "  ⠼ Waiting on process 7 · cargo test",
            ),
            (
                "process_session_poll",
                "done",
                "7 · cargo test · exited 0",
                "  ✓ Waited on process 7 · cargo test · exited 0",
            ),
            (
                "process_session_poll",
                "done",
                "7 · cargo test · still running",
                "  ✓ Waited on process 7 · cargo test · still running",
            ),
            (
                "process_session_poll",
                "failed:exit 7",
                "7 · cargo test · exited 7",
                "  ✗ Waited on process 7 · cargo test · exited 7",
            ),
            (
                "process_session_poll",
                "done",
                "Read output of process 7 · cargo test",
                "  ✓ Read output of process 7 · cargo test",
            ),
            (
                "process_session_write",
                "done",
                "7 · 4 bytes",
                "  ✓ Sent input to process 7 · 4 bytes",
            ),
            (
                "process_session_terminate",
                "done",
                "7 · cargo test",
                "  ✓ Stopped process 7 · cargo test",
            ),
            (
                "process_session_poll",
                "running",
                "7",
                "  ⠼ Waiting on process 7",
            ),
        ] {
            assert_eq!(
                action(
                    name,
                    state,
                    ActivityKind::Process,
                    Some(subject),
                    &RowPlan::default()
                ),
                expected
            );
        }
    }

    #[test]
    fn process_interrupt_keeps_its_action_label() {
        assert_eq!(
            action(
                "process_session_write",
                "done",
                ActivityKind::Process,
                Some("Sent interrupt to process 7 · cargo test"),
                &RowPlan::default()
            ),
            "  ✓ Sent interrupt to process 7 · cargo test"
        );
    }

    #[test]
    fn process_operations_have_their_own_summary_nouns() {
        let blocks = [
            tool(
                "process_session_poll",
                "done",
                "Waited on process 7 · cargo test · still running",
            ),
            tool(
                "process_session_write",
                "done",
                "Sent input to process 7 · 4 bytes",
            ),
            tool(
                "process_session_terminate",
                "done",
                "Stopped process 7 · cargo test",
            ),
        ];
        let found = summary(&blocks, &all);
        assert_eq!(
            found.label,
            "Checked 1 process, sent input to 1 process, stopped 1 process"
        );
        assert!(!found.failed);
    }

    #[test]
    fn process_rows_never_have_an_empty_command_label() {
        for name in [
            "Shell",
            "exec_command",
            "Bash",
            "BashOutput",
            "TaskStop",
            "KillShell",
            "write_stdin",
            "run_terminal_command",
            "get_command_or_subagent_output",
            "process_session_start",
            "process_session_write",
            "process_session_poll",
            "process_session_terminate",
        ] {
            for state in ["running", "done"] {
                let label = action(
                    name,
                    state,
                    ActivityKind::Process,
                    None,
                    &RowPlan::default(),
                );
                assert!(!label.trim().is_empty(), "{name}: {label}");
                assert!(!label.ends_with("$ "), "{name}: {label}");
            }
        }
    }

    fn tool(name: &str, state: &str, subject: &str) -> ScrollBlock {
        ScrollBlock::Tool {
            call_id: None,
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Tool,
            name: name.into(),
            state: state.into(),
            subject: Some(subject.into()),
            detail: None,
            mode: DisplayMode::Collapsed,
            manual: false,
            group_open: false,
            wait: None,
            started_at_ms: None,
            finished_at_ms: None,
            view: crate::tui::blocks::ItemView::default(),
        }
    }

    #[test]
    fn unknown_edit_totals_are_omitted() {
        let block = tool("Edit", "done", "foo+3.rs");
        let plans = plan(std::slice::from_ref(&block), RenderPrefs::default());
        assert_eq!(
            action(
                "Edit",
                "done",
                ActivityKind::Tool,
                Some("foo+3.rs"),
                &plans[0]
            ),
            "  ✓ Edited foo+3.rs"
        );
        assert_eq!(summary(&[block], &all).label, "Edited 1 file");
        assert_eq!(
            action(
                "Edit",
                "done",
                ActivityKind::Tool,
                Some("foo +0/-0"),
                &RowPlan::default()
            ),
            "  ✓ Edited foo"
        );
    }

    #[test]
    fn topology_operations_and_failed_spawns_never_claim_finished_agents() {
        for name in [
            "close_agent",
            "interrupt_agent",
            "recover_agent",
            "discard_workspace",
            "stop_all",
            "submit_result",
            "spawn_agent",
        ] {
            let mut block = tool(name, "failed: refused", "child");
            if let ScrollBlock::Tool { kind, .. } = &mut block {
                *kind = ActivityKind::Agent;
            }
            assert!(
                !summary(&[block], &all).label.contains("finished"),
                "{name}"
            );
            assert!(
                !action(
                    name,
                    "failed: refused",
                    ActivityKind::Agent,
                    Some("child"),
                    &RowPlan::default()
                )
                .contains("finished"),
                "{name}"
            );
        }
    }

    #[test]
    fn folded_groups_keep_resolved_approvals_and_errors_end_groups() {
        let blocks = [
            tool("read_file", "done", "a"),
            tool("grep", "done", "b"),
            ScrollBlock::Approval {
                approval_id: "a".into(),
                tool_name: "Edit".into(),
                subject: None,
                prompt: None,
                resolution: Some("approved".into()),
            },
            ScrollBlock::Assistant("answer".into()),
        ];
        let plans = plan(&blocks, RenderPrefs::default());
        assert!(plans[0].group_folded);
        assert!(plans[1].hidden);
        assert!(!plans[2].hidden);
        let blocks = [
            tool("read_file", "done", "a"),
            ScrollBlock::Error("failure".into()),
            tool("grep", "done", "b"),
            ScrollBlock::Assistant("answer".into()),
        ];
        let plans = plan(&blocks, RenderPrefs::default());
        assert!(plans.iter().all(|plan| !plan.hidden));
        assert!(plans.iter().all(|plan| plan.activity_summary.is_none()));
    }

    #[test]
    fn interrupted_marker_does_not_leave_a_dangling_group_rail() {
        let mut first = tool("read_file", "interrupted", "a");
        if let ScrollBlock::Tool { group_open, .. } = &mut first {
            *group_open = true;
        }
        let blocks = [
            first,
            ScrollBlock::System(INTERRUPTED_MARKER.into()),
            ScrollBlock::Assistant("answer".into()),
        ];
        let plans = plan(&blocks, RenderPrefs::default());
        assert_eq!(plans[0].group_child, Some(true));
        assert!(!plans[1].hidden);
    }

    #[test]
    fn first_expanded_member_has_a_rail() {
        let mut first = tool("read_file", "done", "a");
        if let ScrollBlock::Tool { group_open, .. } = &mut first {
            *group_open = true;
        }
        let blocks = [
            first,
            tool("Edit", "done", "b"),
            ScrollBlock::Assistant("answer".into()),
        ];
        assert_eq!(
            plan(&blocks, RenderPrefs::default())[0].group_child,
            Some(false)
        );
    }

    #[test]
    fn diffstat_requires_a_complete_trailing_suffix() {
        for subject in [
            "foo-2.rs",
            "foo+3.rs",
            "foo +3",
            "foo -2",
            "foo +3/-2 extra",
            "foo +3x/-2",
        ] {
            assert_eq!(edit_counts(subject), None, "{subject}");
        }
        assert_eq!(edit_counts("foo-2.rs +12/-3"), Some((12, 3)));
    }

    #[test]
    fn exploration_omits_zero_components() {
        let blocks = [
            tool("read_file", "done", "a.rs"),
            tool("read_file", "done", "b.rs"),
        ];
        let prefs = RenderPrefs {
            group_tool_calls: false,
            ..RenderPrefs::default()
        };
        assert_eq!(
            plan(&blocks, prefs)[0].activity_action.as_deref(),
            Some("Explored 2 files")
        );
    }

    #[test]
    fn historic_spawns_do_not_animate_and_finished_agents_are_unique() {
        let blocks = [
            tool("spawn_agent", "done", "child"),
            ScrollBlock::Assistant("Done".into()),
        ];
        let prefs = RenderPrefs {
            group_tool_calls: false,
            ..RenderPrefs::default()
        };
        let plans = plan(&blocks, prefs);
        assert!(!plans[0].activity_running);
        assert_eq!(plans[0].activity_action.as_deref(), Some("1 agent spawned"));
        let blocks = [
            tool("wait_agent", "done", "a,b"),
            tool("wait_agent", "done", "b"),
        ];
        assert_eq!(
            plan(&blocks, prefs)[0].activity_action.as_deref(),
            Some("2 agents finished")
        );
        let blocks = [
            tool("spawn_agent", "done", "child"),
            tool("wait_agent", "done", "a"),
        ];
        let plans = plan(&blocks, prefs);
        assert!(!plans[0].activity_running);
        assert_eq!(plans[0].activity_action.as_deref(), Some("1 agent spawned"));
    }

    #[test]
    fn large_group_keeps_one_head_after_front_eviction() {
        let mut blocks: Vec<_> = (0..600)
            .flat_map(|n| {
                [
                    tool("read_file", "done", &format!("file-{n}.rs")),
                    ScrollBlock::Discarded {
                        text: "thought".into(),
                        thinking: true,
                    },
                ]
            })
            .collect();
        blocks.push(ScrollBlock::Assistant("Answer".into()));
        let plans = plan(&blocks, RenderPrefs::default());
        assert_eq!(plans[0].activity_summary.as_deref(), Some("Read 600 files"));
        blocks.drain(..200);
        let plans = plan(&blocks, RenderPrefs::default());
        assert_eq!(plans[0].activity_summary.as_deref(), Some("Read 500 files"));
        assert_eq!(
            plans
                .iter()
                .filter(|p| p.activity_summary.is_some())
                .count(),
            1
        );
    }

    #[test]
    fn failed_exits_are_unique_and_bounded() {
        let blocks: Vec<_> = [1, 2, 3, 4, 1, 5]
            .map(|n| tool("exec_command", &format!("failed:exit {n}"), "false"))
            .into();
        let found = summary(&blocks, &all);
        assert!(found.failed);
        assert_eq!(found.label, "Ran 6 commands (6 failed)");
        assert_eq!(found.meta.as_deref(), Some("exit 1, exit 2, exit 3, +2"));
    }

    fn running(name: &str, id: &str, subject: &str) -> ScrollBlock {
        let mut block = tool(name, "running", subject);
        if let ScrollBlock::Tool { call_id, .. } = &mut block {
            *call_id = Some(id.into());
        }
        block
    }

    #[test]
    fn live_group_is_a_summary_of_finished_calls_plus_one_tip_per_running_call() {
        let tips_only = [
            running("exec_command", "a", "cargo test"),
            running("exec_command", "b", "cargo build"),
        ];
        let plans = plan(&tips_only, RenderPrefs::default());
        assert!(plans.iter().all(|plan| plan.activity_summary.is_none()));
        assert!(plans.iter().all(|plan| !plan.hidden));

        let blocks = [
            tool("read_file", "done:1.0s", "a.rs"),
            running("exec_command", "a", "cargo test"),
            tool("grep", "done:0.5s", "needle"),
            running("exec_command", "b", "cargo build"),
        ];
        let plans = plan(&blocks, RenderPrefs::default());
        assert_eq!(
            plans[0].activity_summary.as_deref(),
            Some("Read 1 file, ran 1 search")
        );
        assert_eq!(plans[0].summary_meta.as_deref(), Some("1.5s"));
        assert!(
            plans[0].group_folded,
            "a finished head draws only the summary"
        );
        assert!(!plans[0].hidden);
        assert!(
            !plans[1].hidden && !plans[3].hidden,
            "one tip per running call"
        );
        assert!(plans[2].hidden, "a finished call folds into the summary");
    }

    #[test]
    fn settled_group_folds_to_one_line_and_a_single_call_keeps_its_item() {
        let one = [
            tool("exec_command", "done:2.0s", "cargo test"),
            ScrollBlock::Assistant("ok".into()),
        ];
        let plans = plan(&one, RenderPrefs::default());
        assert!(plans[0].activity_summary.is_none());
        assert!(!plans[0].group_folded);

        let many = [
            tool("read_file", "done:30.0s", "a.rs"),
            tool("exec_command", "failed:exit 2 · 32.0s", "cargo test"),
            ScrollBlock::item("retry 2/4 · rate limited"),
            ScrollBlock::Assistant("ok".into()),
        ];
        let plans = plan(&many, RenderPrefs::default());
        assert_eq!(
            plans[0].activity_summary.as_deref(),
            Some("Read 1 file, ran 1 command (1 failed)")
        );
        assert_eq!(plans[0].summary_meta.as_deref(), Some("exit 2 · 1m02s"));
        assert!(plans[0].group_folded);
        assert!(plans[1].hidden && plans[2].hidden);

        let ungrouped = plan(
            &many,
            RenderPrefs {
                group_tool_calls: false,
                ..RenderPrefs::default()
            },
        );
        assert!(ungrouped.iter().all(|plan| !plan.hidden));
        assert!(ungrouped.iter().all(|plan| plan.activity_summary.is_none()));
    }

    #[test]
    fn summary_nouns_name_every_operation_and_never_say_actions() {
        let blocks = [
            tool("read_file", "done", "a"),
            tool("grep", "done", "b"),
            tool("apply_patch", "done", "c +1/-2"),
            tool("write_file", "done", "d"),
            tool("exec_command", "done", "ls"),
            tool("process_session_poll", "done", "Waited on process 7 · x"),
            tool("process_session_terminate", "done", "Stopped process 7 · x"),
            tool("send_agent_message", "done", "c23 · hi"),
            tool("list_agents", "done", ""),
            tool("close_agent", "done", "c1"),
            tool("update_plan", "done", "3/9 done"),
            tool("web_search", "done", "q"),
            tool("web_search", "done", "r"),
        ];
        let label = summary(&blocks, &all).label;
        assert_eq!(
            label,
            "Read 1 file, ran 1 search, edited 1 file (+1 −2), wrote 1 file, ran 1 command, \
             checked 1 process, stopped 1 process, messaged 1 agent, listed agents, \
             closed 1 agent, updated plan, web search ×2"
        );
        assert!(!label.contains("action"));
        let checks = [
            tool("process_session_poll", "done", "Waited on process 7 · x"),
            tool(
                "process_session_poll",
                "done",
                "Read output of process 7 · x",
            ),
        ];
        assert_eq!(summary(&checks, &all).label, "Checked 2 processes");
    }

    #[test]
    fn shown_thinking_counts_in_a_group_and_hidden_thinking_does_not() {
        let thinking = ScrollBlock::Thinking {
            location_key: "r".into(),
            text: "hmm".into(),
            sealed: true,
            elapsed_ms: None,
            mode: DisplayMode::Collapsed,
            manual: false,
        };
        let blocks = [
            tool("read_file", "done", "a"),
            tool("read_file", "done", "b"),
            thinking,
            tool("read_file", "done", "c"),
            tool("read_file", "done", "d"),
            ScrollBlock::Assistant("ok".into()),
        ];
        let shown = plan(&blocks, RenderPrefs::default());
        assert_eq!(
            shown[0].activity_summary.as_deref(),
            Some("Read 4 files, 1 thought")
        );
        assert!(shown[3].activity_summary.is_none());
        let hidden = plan(
            &blocks,
            RenderPrefs {
                show_thinking: false,
                ..RenderPrefs::default()
            },
        );
        assert_eq!(hidden[0].activity_summary.as_deref(), Some("Read 4 files"));
        assert!(hidden[3].activity_summary.is_none());
    }

    fn sealed_thought(key: &str) -> ScrollBlock {
        ScrollBlock::Thinking {
            location_key: key.into(),
            text: "hmm".into(),
            sealed: true,
            elapsed_ms: Some(1_200),
            mode: DisplayMode::Collapsed,
            manual: false,
        }
    }

    fn grouped() -> RenderPrefs {
        RenderPrefs::default()
    }

    #[test]
    fn thinking_between_calls_stays_inside_the_group_and_is_counted() {
        let blocks = [
            tool("read_file", "done", "a"),
            sealed_thought("r1"),
            tool("read_file", "done", "b"),
            sealed_thought("r2"),
            tool("exec_command", "done", "cargo test"),
            sealed_thought("r3"),
            ScrollBlock::Assistant("ok".into()),
        ];
        let plans = plan(&blocks, grouped());
        assert_eq!(
            plans[0].activity_summary.as_deref(),
            Some("Read 2 files, ran 1 command, 2 thoughts")
        );
        assert!(plans[0].group_folded);
        assert!(
            (1..5).all(|index| plans[index].hidden),
            "the fold covers the thoughts between its calls"
        );
        assert!(
            !plans[5].hidden && plans[5].group_child.is_none(),
            "a thought after the last call is not inside the group"
        );

        // Opening the group shows every member, thoughts included, in order.
        let mut open = blocks.clone();
        if let ScrollBlock::Tool { group_open, .. } = &mut open[0] {
            *group_open = true;
        }
        let plans = plan(&open, grouped());
        assert!((0..5).all(|index| !plans[index].hidden));
        assert_eq!(plans[1].group_child, Some(false));
        assert_eq!(plans[3].group_child, Some(false));
        assert_eq!(plans[4].group_child, Some(true));

        // Hidden thinking is never counted.
        let hidden = plan(
            &blocks,
            RenderPrefs {
                show_thinking: false,
                ..grouped()
            },
        );
        assert_eq!(
            hidden[0].activity_summary.as_deref(),
            Some("Read 2 files, ran 1 command")
        );
    }

    #[test]
    fn a_thought_joins_the_group_once_the_next_call_arrives() {
        let live = [tool("read_file", "done", "a"), sealed_thought("r1")];
        let plans = plan(&live, grouped());
        assert!(
            !plans[1].hidden,
            "a thought with no later call stays outside"
        );
        assert!(plans[0].activity_summary.is_none());
        let next = [
            tool("read_file", "done", "a"),
            sealed_thought("r1"),
            tool("read_file", "running", "b"),
        ];
        let plans = plan(&next, grouped());
        assert_eq!(
            plans[0].activity_summary.as_deref(),
            Some("Read 1 file, 1 thought")
        );
        assert!(plans[1].hidden);
    }

    #[test]
    fn a_thought_in_an_open_group_sits_on_the_group_rail() {
        let mut blocks = vec![
            tool("read_file", "done", "a"),
            sealed_thought("r1"),
            tool("read_file", "done", "b"),
            ScrollBlock::Assistant("ok".into()),
        ];
        if let ScrollBlock::Tool { group_open, .. } = &mut blocks[0] {
            *group_open = true;
        }
        let rows = crate::tui::blocks::layout_blocks_with(&blocks, 80, grouped()).plain;
        let at = rows
            .iter()
            .position(|row| row.starts_with("  ├─  ◆ hmm"))
            .unwrap_or_else(|| panic!("{rows:#?}"));
        assert!(rows[at + 1].starts_with("  └─  "), "{rows:#?}");
    }

    #[test]
    fn code_mode_cells_read_code_mode_in_a_group_summary() {
        let blocks = [
            tool("read_file", "done", "a"),
            cell("c1", "done:1.0s", DisplayMode::Collapsed),
            cell("c2", "done:2.0s", DisplayMode::Collapsed),
        ];
        assert_eq!(
            summary(&blocks, &all).label,
            "Read 1 file, CodeMode ×2 · 6 calls"
        );
        assert_eq!(summary(&blocks[1..2], &all).label, "CodeMode · 3 calls");
    }

    #[test]
    fn review_trailing_empty_thought_has_no_row_and_no_count() {
        let mut empty = sealed_thought("trailing");
        if let ScrollBlock::Thinking { text, .. } = &mut empty {
            text.clear();
        }
        let blocks = [
            tool("read_file", "done", "a"),
            tool("read_file", "done", "b"),
            empty,
            ScrollBlock::Assistant("Done".into()),
        ];
        let plans = plan(&blocks, RenderPrefs::default());
        assert_eq!(plans[0].activity_summary.as_deref(), Some("Read 2 files"));
        assert!(plans[2].hidden);
    }

    #[test]
    fn review_lone_calls_keep_thought_rows_without_a_summary() {
        for state in ["running", "done"] {
            for empty in [false, true] {
                let mut thought = sealed_thought("lead");
                if empty && let ScrollBlock::Thinking { text, .. } = &mut thought {
                    text.clear();
                }
                for call in [
                    tool("read_file", state, "a.rs"),
                    cell("c", state, DisplayMode::Collapsed),
                ] {
                    let mut blocks = vec![thought.clone(), call];
                    if matches!(
                        blocks[1],
                        ScrollBlock::Tool {
                            kind: ActivityKind::Compose,
                            ..
                        }
                    ) {
                        blocks.push(nested("c", "exec_command", state, "pwd"));
                    }
                    let plans = plan(&blocks, RenderPrefs::default());
                    assert!(plans.iter().all(|p| p.activity_summary.is_none()));
                    assert_eq!(plans[0].hidden, empty);
                    assert!(!plans[1].hidden);
                    if blocks.len() > 2 {
                        assert!(!plans[2].hidden);
                    }
                }
            }
        }
    }

    #[test]
    fn review_leading_thought_stays_at_tip_until_a_summary_exists() {
        let mut blocks = vec![sealed_thought("lead"), tool("read_file", "running", "a")];
        assert!(!plan(&blocks, RenderPrefs::default())[0].hidden);
        blocks.push(tool("exec_command", "running", "pwd"));
        assert!(!plan(&blocks, RenderPrefs::default())[0].hidden);
        if let ScrollBlock::Tool { state, .. } = &mut blocks[1] {
            *state = "done".into();
        }
        assert_eq!(
            plan(&blocks, RenderPrefs::default())[0]
                .activity_summary
                .as_deref(),
            Some("Read 1 file, 1 thought")
        );
    }

    #[test]
    fn review_nested_cells_and_invalid_lineage_keep_every_row() {
        for parents in [
            [None, Some("a"), Some("b")],
            [Some("a"), None, Some("b")],
            [Some("b"), Some("a"), Some("b")],
        ] {
            let mut blocks = [
                cell("a", "done", DisplayMode::Expanded),
                cell("b", "done", DisplayMode::Expanded),
                nested("b", "exec_command", "done", "grandchild"),
            ];
            for (block, parent) in blocks.iter_mut().zip(parents) {
                if let ScrollBlock::Tool { parent_call_id, .. } = block {
                    *parent_call_id = parent.map(str::to_owned);
                }
            }
            let prefs = RenderPrefs {
                group_tool_calls: false,
                ..RenderPrefs::default()
            };
            let plans = plan(&blocks, prefs);
            let order = crate::tui::blocks::row_order(&plans);
            assert_eq!(order.len(), 3);
            assert_eq!(
                order.iter().collect::<std::collections::HashSet<_>>().len(),
                3
            );
            assert!(
                crate::tui::blocks::layout_blocks_with(&blocks, 80, prefs)
                    .plain
                    .iter()
                    .any(|r| r.contains("grandchild"))
            );
        }
    }

    #[test]
    fn leading_thoughts_fold_and_empty_thoughts_never_split_groups() {
        let mut empty = sealed_thought("empty");
        if let ScrollBlock::Thinking { text, .. } = &mut empty {
            text.clear();
        }
        let blocks = [
            sealed_thought("lead"),
            tool("read_file", "done", "a"),
            empty,
            tool("exec_command", "done", "pwd"),
            ScrollBlock::Assistant("ok".into()),
        ];
        let plans = plan(&blocks, RenderPrefs::default());
        assert_eq!(
            plans[0].activity_summary.as_deref(),
            Some("Read 1 file, ran 1 command, 2 thoughts")
        );
        assert!((1..4).all(|i| plans[i].hidden));
        let mut alone = sealed_thought("alone");
        if let ScrollBlock::Thinking { text, .. } = &mut alone {
            text.clear();
        }
        assert_eq!(
            crate::tui::blocks::layout_blocks(&[alone], 80).plain,
            [] as [String; 0]
        );
    }

    #[test]
    fn collapsed_cell_outline_is_capped_and_prioritizes_running_and_failed_calls() {
        for count in [0, 1, 10, 11, 50] {
            let mut head = cell("c", "done:4.2s", DisplayMode::Collapsed);
            if let ScrollBlock::Tool { subject, .. } = &mut head {
                *subject = Some(format!("{count} tools"));
            }
            let mut blocks = vec![head];
            for i in 0..count {
                let state = if i == 48 {
                    "running"
                } else if i == 49 {
                    "failed:exit 2"
                } else {
                    "done"
                };
                blocks.push(nested("c", "exec_command", state, &format!("command-{i}")));
            }
            let plans = plan(&blocks, RenderPrefs::default());
            assert_eq!(
                plans.iter().skip(1).filter(|p| !p.hidden).count(),
                count.min(10)
            );
            let rows = crate::tui::blocks::layout_blocks(&blocks, 100)
                .plain
                .join("\n");
            if count == 0 {
                assert!(
                    rows.contains("CodeMode") && !rows.contains("0 calls"),
                    "{rows}"
                );
            }
            if count > 10 {
                assert!(rows.contains(&format!("… {} more", count - 10)), "{rows}");
            }
            if count == 50 {
                assert!(
                    rows.contains("command-48") && rows.contains("command-49"),
                    "{rows}"
                );
                assert!(rows.contains("(1 failed)"), "{rows}");
                assert!(rows.starts_with("  ⠼"), "{rows}");
            }
        }
    }

    fn cell(id: &str, state: &str, mode: DisplayMode) -> ScrollBlock {
        ScrollBlock::Tool {
            call_id: Some(id.into()),
            parent_call_id: None,
            cell_id: Some(id.into()),
            kind: ActivityKind::Compose,
            name: "exec".into(),
            state: state.into(),
            subject: Some("3 tools".into()),
            detail: Some("await tools.exec_command()".into()),
            mode,
            manual: false,
            group_open: false,
            wait: None,
            started_at_ms: None,
            finished_at_ms: None,
            view: crate::tui::blocks::ItemView::default(),
        }
    }

    fn nested(parent: &str, name: &str, state: &str, subject: &str) -> ScrollBlock {
        let mut block = tool(name, state, subject);
        if let ScrollBlock::Tool { parent_call_id, .. } = &mut block {
            *parent_call_id = Some(parent.into());
        }
        block
    }

    #[test]
    fn a_running_cell_tip_keeps_its_tally_and_outline() {
        let blocks = [
            cell("c", "running", DisplayMode::Collapsed),
            nested("c", "read_file", "done", "a.rs"),
            nested("c", "exec_command", "running", "cargo test"),
        ];
        let plans = plan(&blocks, RenderPrefs::default());
        assert_eq!(
            plans[0].activity_action.as_deref(),
            Some("CodeMode · 3 calls")
        );
        assert!(!plans[1].hidden && !plans[2].hidden);
        let done = [
            cell("c", "done:4.2s", DisplayMode::Collapsed),
            nested("c", "read_file", "done", "a.rs"),
            ScrollBlock::Assistant("ok".into()),
        ];
        let plans = plan(&done, RenderPrefs::default());
        assert_eq!(
            plans[0].activity_action.as_deref(),
            Some("CodeMode · 3 calls")
        );
        assert_eq!(
            item_meta(
                "exec",
                "done:4.2s",
                ActivityKind::Compose,
                Some("3 tools"),
                &plans[0]
            )
            .as_deref(),
            Some("4.2s")
        );
    }

    #[test]
    fn opening_a_group_does_not_open_its_cells_and_nested_calls_sit_one_rail_deeper() {
        let mut head = tool("read_file", "done", "a");
        if let ScrollBlock::Tool { group_open, .. } = &mut head {
            *group_open = true;
        }
        let blocks = [
            head.clone(),
            cell("c", "done", DisplayMode::Collapsed),
            nested("c", "read_file", "done", "b"),
            ScrollBlock::Assistant("ok".into()),
        ];
        let plans = plan(&blocks, RenderPrefs::default());
        assert!(!plans[2].hidden, "a collapsed cell keeps its outline");
        let blocks = [
            head,
            cell("c", "done", DisplayMode::Expanded),
            nested("c", "read_file", "done", "b"),
            tool("grep", "done", "x"),
            ScrollBlock::Assistant("ok".into()),
        ];
        let plans = plan(&blocks, RenderPrefs::default());
        assert!(!plans[2].hidden);
        assert_eq!(row_indent(&plans[1]), "  ├─  ");
        assert_eq!(row_indent(&plans[2]), "  │   └─  ");
        assert_eq!(row_indent(&plans[3]), "  └─  ");
    }

    #[test]
    fn opened_cell_group_keeps_each_cells_own_failure_glyph() {
        let mut first = cell("a", "done", DisplayMode::Collapsed);
        if let ScrollBlock::Tool { group_open, .. } = &mut first {
            *group_open = true;
        }
        let blocks = [
            first,
            cell("b", "done", DisplayMode::Collapsed),
            nested("b", "exec_command", "failed:exit 2", "false"),
        ];
        let plans = plan(&blocks, RenderPrefs::default());
        assert_eq!(item_glyph("done", ActivityKind::Compose, &plans[0]), "✓");
        assert_eq!(item_glyph("done", ActivityKind::Compose, &plans[1]), "✗");
        assert!(
            plans[0].activity_failed,
            "group summary still reports the failure"
        );
    }

    #[test]
    fn interleaved_cell_children_render_in_each_outline_in_call_order() {
        let blocks = [
            cell("a", "done", DisplayMode::Collapsed),
            cell("b", "done", DisplayMode::Collapsed),
            nested("a", "exec_command", "done", "a-first"),
            nested("b", "exec_command", "done", "b-first"),
            nested("a", "exec_command", "done", "a-last"),
        ];
        let prefs = RenderPrefs {
            group_tool_calls: false,
            ..RenderPrefs::default()
        };
        let rows = crate::tui::blocks::layout_blocks_with(&blocks, 80, prefs).plain;
        assert!(
            rows[1].contains("a-first") && rows[2].contains("a-last"),
            "{rows:?}"
        );
        assert!(rows[4].contains("b-first"), "{rows:?}");
    }

    #[test]
    fn two_cells_summary_totals_host_calls_and_live_outline_folds_when_done() {
        let mut second = cell("b", "running", DisplayMode::Collapsed);
        if let ScrollBlock::Tool { subject, .. } = &mut second {
            *subject = Some("4 tools".into());
        }
        let mut blocks = [
            cell("a", "done", DisplayMode::Collapsed),
            nested("a", "read_file", "done", "a.rs"),
            second,
            nested("b", "exec_command", "running", "pwd"),
        ];
        let live = plan(&blocks, RenderPrefs::default());
        assert!(live[1].hidden && !live[2].hidden && !live[3].hidden);
        if let ScrollBlock::Tool { state, .. } = &mut blocks[2] {
            *state = "done".into();
        }
        if let ScrollBlock::Tool { state, .. } = &mut blocks[3] {
            *state = "done".into();
        }
        let done = plan(&blocks, RenderPrefs::default());
        assert_eq!(
            done[0].activity_summary.as_deref(),
            Some("CodeMode ×2 · 7 calls")
        );
        assert!((1..4).all(|i| done[i].hidden));
    }

    #[test]
    fn streaming_leading_thought_is_pulled_in_only_after_sealing() {
        let mut thought = sealed_thought("t");
        if let ScrollBlock::Thinking { sealed, .. } = &mut thought {
            *sealed = false;
        }
        let mut blocks = [
            tool("read_file", "done", "a"),
            thought,
            tool("read_file", "done", "b"),
        ];
        let streaming = plan(&blocks, RenderPrefs::default());
        assert!(!streaming[1].hidden && streaming[0].activity_summary.is_none());
        if let ScrollBlock::Thinking { sealed, .. } = &mut blocks[1] {
            *sealed = true;
        }
        let sealed = plan(&blocks, RenderPrefs::default());
        assert_eq!(
            sealed[0].activity_summary.as_deref(),
            Some("Read 2 files, 1 thought")
        );
        assert!(sealed[1].hidden);
    }

    #[test]
    fn item_text_is_one_line_with_exit_and_duration_as_meta() {
        let plan = RowPlan::default();
        assert_eq!(
            action(
                "exec_command",
                "failed:exit 2 · 1.5s",
                ActivityKind::Process,
                Some("echo a\necho b"),
                &plan
            ),
            "  ✗ $ echo a↵echo b"
        );
        assert_eq!(
            item_meta(
                "exec_command",
                "failed:exit 2 · 1.5s",
                ActivityKind::Process,
                Some("echo a"),
                &plan
            )
            .as_deref(),
            Some("exit 2 · 1.5s")
        );
        assert_eq!(
            action(
                "exec_command",
                "done:1.0s",
                ActivityKind::Process,
                Some("ls"),
                &plan
            ),
            "  ✓ $ ls"
        );
    }

    /// Parallel calls overlap: the meta is the span from the first start to
    /// the last finish, not the sum; one call without host stamps falls the
    /// whole group back to the sum.
    #[test]
    fn group_meta_is_the_stamped_span_and_falls_back_to_the_sum() {
        let stamped = |subject: &str, state: &str, stamps: Option<(u64, u64)>| {
            let mut block = tool("exec_command", state, subject);
            if let ScrollBlock::Tool {
                started_at_ms,
                finished_at_ms,
                ..
            } = &mut block
            {
                *started_at_ms = stamps.map(|(start, _)| start);
                *finished_at_ms = stamps.map(|(_, finish)| finish);
            }
            block
        };
        let all = |_| true;
        let parallel = [
            stamped("a", "done:5.0s", Some((10_000, 15_000))),
            stamped("b", "done:3.0s", Some((11_000, 14_000))),
        ];
        assert_eq!(summary(&parallel, &all).meta.as_deref(), Some("5.0s"));
        let carried = [
            stamped("a", "done:5.0s", Some((10_000, 15_000))),
            stamped("b", "done:3.0s", None),
        ];
        assert_eq!(summary(&carried, &all).meta.as_deref(), Some("8.0s"));
    }

    #[test]
    fn group_and_tip_durations_format_for_the_reader() {
        assert_eq!(format_group_elapsed(62_000), "1m02s");
        assert_eq!(format_group_elapsed(3_900_000), "1h05m");
        assert_eq!(format_group_elapsed(1_500), "1.5s");
        assert_eq!(elapsed_ms("12ms"), Some(12));
        assert_eq!(elapsed_ms("<1ms"), Some(0));
        assert_eq!(elapsed_ms("2.5s"), Some(2_500));
        let tip = |secs| tip_elapsed(std::time::Duration::from_secs(secs));
        assert_eq!(tip(0), None);
        assert_eq!(tip(9).as_deref(), Some("9s"));
        assert_eq!(tip(75).as_deref(), Some("1m15s"));
        assert_eq!(tip(65).as_deref(), Some("1m05s"));
        assert_eq!(tip(3_900).as_deref(), Some("1h05m"));
    }
}
