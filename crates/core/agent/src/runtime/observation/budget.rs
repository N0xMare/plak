//! Pure observation byte policy and structure-preserving text clipping.

use crate::OBSERVATION_HEADROOM_RESERVE_TOKENS as OBSERVATION_BUDGET_RESERVE_TOKENS;
use crate::compaction_verdict::input_headroom_tokens;
use crate::runtime::prompts;
use plakous_model::ContextHeadroom;
use serde::{Deserialize, Serialize};
pub(in crate::runtime) const CANONICAL_RESULT_BYTES: usize =
    plakous_model::SemanticJsonV1::maximum_string_bytes() - 4_096;
pub(super) const OBSERVATION_EXHAUSTED: &str = prompts::PLAKOUS_RUNTIME_OBSERVATION_EXHAUSTED;

/// Capacity pressure survives clipping and durable tool-batch reconciliation.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::runtime) struct ObservationBudget {
    pub(super) remaining: Option<usize>,
    capacity_remaining: Option<usize>,
    capacity_exhausted: bool,
}

impl ObservationBudget {
    pub(in crate::runtime) const fn new(capacity: usize, cap: Option<usize>) -> Self {
        Self {
            remaining: Some(match cap {
                Some(cap) if cap < capacity => cap,
                _ => capacity,
            }),
            capacity_remaining: Some(capacity),
            capacity_exhausted: false,
        }
    }

    pub(super) const fn capped(cap: Option<usize>) -> Self {
        Self {
            remaining: cap,
            capacity_remaining: None,
            capacity_exhausted: false,
        }
    }

    pub(super) fn record_result(&mut self, original: usize, retained: usize, clipped: bool) {
        if let Some(remaining) = self.capacity_remaining.as_mut() {
            self.capacity_exhausted |= clipped && original > *remaining;
            *remaining = remaining.saturating_sub(retained);
        }
    }

    pub(super) const fn capacity_limited(self) -> bool {
        matches!((self.remaining, self.capacity_remaining), (Some(limit), Some(capacity)) if limit >= capacity)
    }

    pub(in crate::runtime) const fn capacity_exhausted(self) -> bool {
        self.capacity_exhausted
    }

    pub(in crate::runtime) fn mark_capacity_exhausted(&mut self) {
        self.capacity_exhausted = true;
    }

    pub(in crate::runtime) fn clip_text(&mut self, body: &str) -> String {
        let capacity_limited = self.capacity_limited();
        if capacity_limited
            && self
                .remaining
                .is_some_and(|remaining| body.len() > remaining)
            && body.len() <= OBSERVATION_EXHAUSTED.len()
        {
            // A capacity notice must not cost more than the output it withholds.
            self.remaining = Some(0);
            self.record_result(body.len(), body.len(), true);
            return body.to_owned();
        }
        let text = if let Some(remaining) = self.remaining.as_mut() {
            if body.is_empty() {
                return String::new();
            }
            let allowance = (*remaining).min(CANONICAL_RESULT_BYTES);
            let (text, next) = budgeted_observation_text(body, allowance);
            *remaining = remaining.saturating_sub(allowance.saturating_sub(next));
            text
        } else {
            clip_observation_text(body, CANONICAL_RESULT_BYTES)
        };
        self.record_result(body.len(), text.len(), capacity_limited && text != body);
        text
    }
}

pub(in crate::runtime) fn observation_bytes_from_headroom(headroom: ContextHeadroom) -> usize {
    let usable = input_headroom_tokens(headroom).saturating_sub(OBSERVATION_BUDGET_RESERVE_TOKENS);
    usize::try_from(usable).unwrap_or(usize::MAX)
}

/// The allowance a prepared call grants. An unanchored estimate delegated the
/// fit to the provider, so it grants only half of its estimated headroom until
/// a completed response's usage refreshes the allowance.
pub(in crate::runtime) fn observation_bytes_for_sent_status(
    headroom: ContextHeadroom,
    unanchored_estimate: bool,
) -> usize {
    let bytes = observation_bytes_from_headroom(headroom);
    if unanchored_estimate {
        bytes / 2
    } else {
        bytes
    }
}

const OBSERVATION_OMITTED: &str = prompts::PLAKOUS_RUNTIME_OBSERVATION_OMITTED;

/// Share of a clipped body kept from its head; the rest is kept from its tail.
/// Builds and test runs print their failures last, so the tail gets most.
const HEAD_SHARE_PERCENT: usize = 30;

pub(in crate::runtime) fn budgeted_observation_text(
    body: &str,
    remaining: usize,
) -> (String, usize) {
    let clipped = clip_observation_text(body, remaining);
    let next_remaining = remaining.saturating_sub(clipped.len());
    (clipped, next_remaining)
}

/// The least a cut is allowed to take, whatever its budget: room for the bare
/// omitted-bytes marker, the wrapped withheld notice, or a small structured
/// skeleton (a read's `next_offset`, a process result's exit code). A full
/// output locator can raise this floor to its minimal process notice.
pub(super) const CUT_FLOOR_BYTES: usize = 256;

pub(super) fn clip_observation_text(body: &str, remaining: usize) -> String {
    clip_observation(body, remaining, true)
}

/// `wrappers` selects whether a raised `{"result": string}` presentation is
/// cut structurally. Only the outermost one is, so a cut costs a bounded number
/// of passes however deeply wrappers nest; wrapped text is cut as text.
fn clip_observation(body: &str, remaining: usize, wrappers: bool) -> String {
    if remaining != 0 && body.len() <= remaining {
        return body.to_owned();
    }
    // A structured cut that cannot fit (a large field beside the one it cuts)
    // gives way to a plain cut, which always does.
    let notice =
        full_output_notice(body, wrappers).filter(|notice| notice.len() <= CANONICAL_RESULT_BYTES);
    let limit = remaining
        .max(CUT_FLOOR_BYTES)
        .max(notice.as_ref().map_or(0, String::len));
    if let Some(clipped) = clip_structured_observation(body, remaining, wrappers)
        .filter(|clipped| clipped.len() <= limit)
    {
        return clipped;
    }
    if remaining == 0 {
        return notice.unwrap_or_else(|| OBSERVATION_EXHAUSTED.to_owned());
    }
    if let Some(notice) = notice.filter(|_| serde_json::from_str::<serde_json::Value>(body).is_ok())
    {
        return notice;
    }
    clip_head_tail(body, remaining)
}

fn full_output_line(body: &str) -> Option<&str> {
    body.lines().rev().find(|line| {
        line.len() <= CANONICAL_RESULT_BYTES - CUT_FLOOR_BYTES
            && line
                .strip_prefix("full output: ")
                .is_some_and(|path| !path.is_empty())
    })
}

fn withheld_output(body: &str) -> String {
    full_output_line(body).map_or_else(
        || OBSERVATION_EXHAUSTED.to_owned(),
        |line| format!("{OBSERVATION_EXHAUSTED}\n{line}"),
    )
}

// A path is a read locator, not dispensable log content. Even a
// zero-byte allowance retains the smallest notice that names it.
fn full_output_notice(body: &str, wrappers: bool) -> Option<String> {
    if let Some(line) = full_output_line(body) {
        return Some(format!("{OBSERVATION_EXHAUSTED}\n{line}"));
    }
    let mut value: serde_json::Value = serde_json::from_str(body).ok()?;
    if let Some(inner) = text_result_object(&value).filter(|_| wrappers) {
        return serde_json::to_string(&serde_json::json!({
            "result": full_output_notice(inner, false)?
        }))
        .ok();
    }
    if let Some(tasks) = value
        .get_mut("tasks")
        .and_then(serde_json::Value::as_array_mut)
    {
        let mut has_path = false;
        for task in tasks.iter_mut() {
            let Some(output) = task.get("output").and_then(serde_json::Value::as_str) else {
                continue;
            };
            has_path |= full_output_line(output).is_some();
            task["output"] = serde_json::Value::String(withheld_output(output));
            retain_process_fields(task);
        }
        value.as_object_mut()?.retain(|key, _| key == "tasks");
        return has_path
            .then(|| serde_json::to_string(&value).ok())
            .flatten();
    }
    let output = value.get("output")?.as_str()?;
    full_output_line(output)?;
    value["output"] = serde_json::Value::String(withheld_output(output));
    retain_process_fields(&mut value);
    serde_json::to_string(&value).ok()
}

fn retain_process_fields(value: &mut serde_json::Value) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| {
            matches!(
                key.as_str(),
                "output" | "status" | "exit_code" | "session_id" | "cursor" | "task_id"
            )
        });
    }
}

/// The next, smaller budget for text whose encoding at `budget` came to
/// `encoded` bytes against `target`: scaled by the escaping ratio so dense
/// text keeps a head and tail, and always at least a 1/64 step smaller.
fn shrunk_budget(budget: usize, encoded: usize, target: usize) -> usize {
    let scaled = budget.saturating_mul(target) / encoded.max(1);
    scaled.min(budget.saturating_sub((budget / 64).max(1)))
}

/// Keeps the head and tail of `body` within `budget` bytes, replacing the
/// middle with one omitted-bytes line. Cuts land on line boundaries when one
/// is near and always on UTF-8 boundaries. A budget smaller than the marker
/// keeps only the marker, so the model still learns output existed.
pub(super) fn clip_head_tail(body: &str, budget: usize) -> String {
    if body.len() <= budget {
        return body.to_owned();
    }
    if let Some(line) = full_output_line(body) {
        let mut log = body.to_owned();
        if let Some(start) = log.rfind(line) {
            log.replace_range(start..start + line.len(), "");
        }
        let clipped = clip_head_tail_log(&log, budget.saturating_sub(line.len() + 1));
        return format!("{clipped}\n{line}");
    }
    clip_head_tail_log(body, budget)
}

fn clip_head_tail_log(body: &str, budget: usize) -> String {
    // The marker is sized for the whole body, which bounds its digits, plus
    // the two newlines that set it apart.
    let marker_len = omitted_marker(body.len()).len() + 2;
    let available = budget.saturating_sub(marker_len);
    let head_budget = available * HEAD_SHARE_PERCENT / 100;
    let tail_budget = available - head_budget;
    let mut head_end = floor_char_boundary(body, head_budget);
    if let Some(newline) = body[..head_end].rfind('\n')
        && newline + 1 >= head_end / 2
    {
        head_end = newline + 1;
    }
    let mut tail_start = ceil_char_boundary(body, body.len() - tail_budget);
    if let Some(newline) = body[tail_start..].find('\n')
        && newline < tail_budget / 2
    {
        tail_start += newline + 1;
    }
    let tail_start = tail_start.max(head_end);
    let head = &body[..head_end];
    let tail = &body[tail_start..];
    let marker = omitted_marker(tail_start - head_end);
    let mut clipped = String::with_capacity(head.len() + marker.len() + tail.len() + 2);
    clipped.push_str(head);
    if !head.is_empty() && !head.ends_with('\n') {
        clipped.push('\n');
    }
    clipped.push_str(&marker);
    if !tail.is_empty() {
        clipped.push('\n');
        clipped.push_str(tail);
    }
    clipped
}

fn omitted_marker(omitted: usize) -> String {
    format!("[{omitted} {OBSERVATION_OMITTED}]")
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn clip_structured_observation(body: &str, remaining: usize, wrappers: bool) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(body).ok()?;
    if value
        .get("content")
        .and_then(serde_json::Value::as_str)
        .is_some()
        && value.get("offset").is_some()
        && value.get("next_offset").is_some()
    {
        // A file read stays head-first: the model continues from `next_offset`.
        // An observation cut is not file content and must never be copied into a file.
        value["observation_truncated"] = serde_json::Value::Bool(true);
        let original = value["content"].as_str()?.to_owned();
        let mut take = remaining.min(original.len());
        loop {
            take = floor_char_boundary(&original, take);
            value["content"] = serde_json::Value::String(original[..take].to_owned());
            let encoded = serde_json::to_string(&value).ok()?;
            if take == 0 || encoded.len() <= remaining {
                return Some(encoded);
            }
            take /= 2;
        }
    }
    if let Some(inner) = text_result_object(&value).filter(|_| wrappers) {
        return Some(clip_text_result_object(inner, remaining));
    }
    if let Some(tasks) = value
        .get_mut("tasks")
        .and_then(serde_json::Value::as_array_mut)
    {
        let originals: Vec<_> = tasks
            .iter()
            .enumerate()
            .filter_map(|(index, task)| {
                task.get("output")
                    .and_then(serde_json::Value::as_str)
                    .map(|output| (index, output.to_owned()))
            })
            .collect();
        for (index, output) in &originals {
            tasks[*index]["output"] = serde_json::Value::String(withheld_output(output));
        }
        let skeleton = serde_json::to_string(&value).ok()?;
        if remaining == 0 || skeleton.len() > remaining {
            return Some(skeleton);
        }
        let mut share = remaining / originals.len().max(1);
        loop {
            for (index, output) in &originals {
                value["tasks"][*index]["output"] =
                    serde_json::Value::String(clip_head_tail(output, share));
            }
            let encoded = serde_json::to_string(&value).ok()?;
            if encoded.len() <= remaining {
                return Some(encoded);
            }
            share = shrunk_budget(share, encoded.len(), remaining);
            if share == 0 {
                return Some(skeleton);
            }
        }
    }
    if !value.as_object()?.contains_key("output") {
        return None;
    }
    let original = value.get("output")?.as_str()?.to_owned();
    value["output"] = serde_json::Value::String(withheld_output(&original));
    let skeleton = serde_json::to_string(&value).ok()?;
    if remaining == 0 || skeleton.len() > remaining {
        return serde_json::to_string(&value).ok();
    }
    // JSON escaping can grow the kept text, so shrink until the encoding fits.
    let mut budget = remaining - skeleton.len() + withheld_output(&original).len();
    loop {
        value["output"] = serde_json::Value::String(clip_head_tail(&original, budget));
        let encoded = serde_json::to_string(&value).ok()?;
        if encoded.len() <= remaining {
            return Some(encoded);
        }
        budget = shrunk_budget(budget, encoded.len(), remaining);
        if budget == 0 {
            return Some(skeleton);
        }
    }
}

/// The wrapped text of a raised `{"result": string}` presentation.
fn text_result_object(value: &serde_json::Value) -> Option<&str> {
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    object.get("result")?.as_str()
}

/// Cuts a raised `{"result": string}` presentation so it stays one valid
/// object within `remaining` (or [`CUT_FLOOR_BYTES`]): the wrapped text is
/// clipped as an observation of its own (a nested read or process result keeps
/// its structure), shrinking until its encoding fits, since the wrapper escapes
/// it once more. When nothing fits, the wrapper carries the withheld notice.
fn clip_text_result_object(inner: &str, remaining: usize) -> String {
    let wrap = |text: &str| {
        serde_json::to_string(&serde_json::json!({ "result": text }))
            .unwrap_or_else(|_| OBSERVATION_EXHAUSTED.to_owned())
    };
    let notice = full_output_notice(inner, false)
        .filter(|notice| wrap(notice).len() <= CANONICAL_RESULT_BYTES);
    let limit = remaining
        .max(CUT_FLOOR_BYTES)
        .max(notice.as_ref().map_or(0, |notice| wrap(notice).len()));
    let mut budget = remaining;
    loop {
        let encoded = wrap(&clip_observation(inner, budget, false));
        if encoded.len() <= limit {
            return encoded;
        }
        budget = shrunk_budget(budget, encoded.len(), limit);
        if budget == 0 {
            return wrap(notice.as_deref().unwrap_or(OBSERVATION_EXHAUSTED));
        }
    }
}

#[cfg(test)]
mod tests;
