//! Pure observation byte policy and structure-preserving text clipping.

use crate::runtime::prompts;
use plakous_model::ContextHeadroom;
use serde::{Deserialize, Serialize};
const OBSERVATION_BUDGET_RESERVE_TOKENS: u64 = 4_096;
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
    let input = match headroom {
        ContextHeadroom::ProviderReservedInput { input }
        | ContextHeadroom::Shared(input)
        | ContextHeadroom::Separate { input, .. } => input.value(),
        ContextHeadroom::SharedWithOutputCap { shared, .. } => shared.value(),
        _ => 0,
    };
    let usable = input.saturating_sub(OBSERVATION_BUDGET_RESERVE_TOKENS);
    usize::try_from(usable).unwrap_or(usize::MAX)
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
mod tests {
    use std::fmt::Write as _;

    use super::{OBSERVATION_BUDGET_RESERVE_TOKENS, observation_bytes_from_headroom};
    use plakous_model::{ContextHeadroom, QualifiedTokens};

    #[test]
    fn a_hosted_round_cap_is_aggregate_and_never_raises_headroom() {
        let mut budget = super::ObservationBudget::new(900_000, Some(82_688));
        let first = budget.clip_text(&"a".repeat(50_000));
        let second = budget.clip_text(&"b".repeat(50_000));
        assert_eq!(first.len(), 50_000);
        assert!(first.len() + second.len() <= 82_688);
        assert_eq!(budget.remaining, Some(0));
        assert!(!budget.capacity_exhausted());
        let mut tight = super::ObservationBudget::new(10_000, Some(82_688));
        assert!(tight.clip_text(&"c".repeat(50_000)).len() <= 10_000);
        assert!(tight.capacity_exhausted());
        let mut unanchored = super::ObservationBudget::new(0, Some(82_688));
        assert_eq!(
            unanchored.clip_text(&"c".repeat(50_000)),
            super::OBSERVATION_EXHAUSTED
        );
        assert!(unanchored.capacity_exhausted());
    }

    #[test]
    fn process_full_output_paths_survive_clipping_and_withholding()
    -> Result<(), Box<dyn std::error::Error>> {
        let path = "full output: /state/jobs/session/branch-p1-service.log";
        let output = format!("{}\n{path}", "log line\n".repeat(20_000));
        let process =
            serde_json::json!({"status":"exited", "exit_code":101, "output":output}).to_string();
        let tasks = serde_json::json!({"tasks":[
            {"task_id":"p1", "status":"exited", "output":output},
            {"task_id":"p2", "status":"running", "output":output},
            {"task_id":"p3", "status":"not_found"}
        ]})
        .to_string();
        for body in [output, process, tasks] {
            for wrapped in [false, true] {
                let body = if wrapped {
                    serde_json::json!({"result":body}).to_string()
                } else {
                    body.clone()
                };
                for budget in [0, 160, 512, 4096] {
                    let clipped = super::clip_observation_text(&body, budget);
                    let text = if wrapped {
                        let value: serde_json::Value = serde_json::from_str(&clipped)?;
                        value["result"].as_str().ok_or("wrapper lost")?.to_owned()
                    } else {
                        clipped
                    };
                    assert!(text.contains(path), "path lost at budget {budget}: {text}");
                    if body.contains("task_id") {
                        let value: serde_json::Value = serde_json::from_str(&text)?;
                        assert_eq!(value["tasks"][1]["task_id"], "p2");
                        assert_eq!(value["tasks"][2]["status"], "not_found");
                        assert!(
                            value["tasks"][0]["output"]
                                .as_str()
                                .ok_or("task output")?
                                .contains(path)
                        );
                        assert!(
                            value["tasks"][1]["output"]
                                .as_str()
                                .ok_or("task output")?
                                .contains(path)
                        );
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn reserved_input_headroom_charges_one_token_per_byte() {
        let remaining_tokens = OBSERVATION_BUDGET_RESERVE_TOKENS + 16_000;
        let bytes = observation_bytes_from_headroom(ContextHeadroom::ProviderReservedInput {
            input: QualifiedTokens::Exact(remaining_tokens),
        });
        assert_eq!(bytes, 16_000);
    }

    #[test]
    fn zero_capacity_keeps_a_body_no_larger_than_the_withheld_notice() {
        for body in [
            "",
            "a 35-byte result remains useful here!",
            super::OBSERVATION_EXHAUSTED,
        ] {
            let mut budget = super::ObservationBudget::new(0, None);
            assert_eq!(budget.clip_text(body), body);
            assert_eq!(budget.capacity_exhausted(), !body.is_empty());
        }
    }

    #[test]
    fn forty_kilobyte_code_result_is_clipped_at_conservative_headroom() {
        let bytes = observation_bytes_from_headroom(ContextHeadroom::Shared(
            QualifiedTokens::Exact(36_000),
        ));
        let mut budget = super::ObservationBudget::new(bytes, None);
        let body = "x".repeat(40_000);
        let clipped = budget.clip_text(&body);
        assert!(clipped.len() <= 36_000 - 4_096);
        assert!(clipped.contains("omitted"));
        assert!(budget.capacity_exhausted());
    }

    #[test]
    fn canonical_result_cap_keeps_exact_omitted_bytes_without_capacity_pressure() {
        let mut budget = super::ObservationBudget::new(2_000_000, None);
        let body = "x".repeat(500_000);
        let clipped = budget.clip_text(&body);
        assert!(clipped.len() <= super::CANONICAL_RESULT_BYTES);
        let retained = clipped
            .lines()
            .filter(|line| !line.starts_with('['))
            .map(str::len)
            .sum::<usize>();
        assert!(clipped.contains(&super::omitted_marker(body.len() - retained)));
        assert!(!budget.capacity_exhausted());
    }

    #[test]
    fn a_cut_raised_text_result_stays_one_valid_wrapper_with_its_nested_structure()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::runtime::observation::budget::clip_observation_text;
        let plain = serde_json::json!({ "result": "\"line\"\n".repeat(80_000) }).to_string();
        let read = serde_json::json!({
            "result": serde_json::json!({
                "content": "λ \"quoted\"\n".repeat(30_000),
                "offset": 1, "lines": 30_000, "eof": false, "next_offset": 30_001
            })
            .to_string()
        })
        .to_string();
        for budget in [0, 160, 4_096, super::CANONICAL_RESULT_BYTES] {
            let clipped = clip_observation_text(&plain, budget);
            let value: serde_json::Value = serde_json::from_str(&clipped)?;
            let inner = value["result"].as_str().ok_or("plain wrapper lost")?;
            assert!(budget < 4_096 || clipped.len() <= budget, "{budget}");
            assert!(inner.contains("omitted") || inner == super::OBSERVATION_EXHAUSTED);

            let clipped = clip_observation_text(&read, budget);
            let value: serde_json::Value = serde_json::from_str(&clipped)?;
            let page: serde_json::Value =
                serde_json::from_str(value["result"].as_str().ok_or("read wrapper lost")?)?;
            assert_eq!(page["next_offset"], 30_001);
            assert_eq!(page["observation_truncated"], true);
            assert!(budget < 4_096 || clipped.len() <= budget, "{budget}");
        }
        Ok(())
    }

    /// A wrapped JSON whose structured skeleton cannot fit is still cut under
    /// the per-result bound, so its history item satisfies `SemanticJsonV1`'s
    /// string rule.
    #[test]
    fn a_wrapped_structured_skeleton_cut_stays_under_the_per_result_bound() {
        use crate::runtime::observation::budget::{CANONICAL_RESULT_BYTES, clip_observation_text};
        let big = "y".repeat(300_000);
        for inner in [
            serde_json::json!({ "output": "x", "data": big }),
            serde_json::json!({ "content": "abc", "offset": 1, "next_offset": 2, "data": big }),
        ] {
            let body = serde_json::json!({ "result": inner.to_string() }).to_string();
            let clipped = clip_observation_text(&body, CANONICAL_RESULT_BYTES);
            let string_ok = plakous_model::SemanticJsonV1::from_value(serde_json::Value::String(
                clipped.clone(),
            ))
            .is_ok();
            assert!(
                clipped.len() <= CANONICAL_RESULT_BYTES && string_ok,
                "clipped wrapper is {} bytes (bound {CANONICAL_RESULT_BYTES}); SemanticJsonV1 string ok: {string_ok}",
                clipped.len()
            );
        }
    }

    /// Escape-dense wrapped text under an ample budget keeps a head and tail
    /// instead of being withheld for context capacity.
    #[test]
    fn escape_dense_wrapped_text_keeps_head_and_tail_under_an_ample_budget()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::runtime::observation::budget::{CANONICAL_RESULT_BYTES, clip_observation_text};
        for (label, text) in [
            ("quotes", "\"".repeat(400_000)),
            ("ansi", "\u{1b}[31mE\u{1b}[0m\n".repeat(40_000)),
        ] {
            let body = serde_json::json!({ "result": text }).to_string();
            let clipped = clip_observation_text(&body, CANONICAL_RESULT_BYTES);
            let value: serde_json::Value = serde_json::from_str(&clipped)?;
            let inner = value["result"].as_str().ok_or("wrapper lost")?;
            assert!(
                inner != super::OBSERVATION_EXHAUSTED
                    && inner.contains(super::OBSERVATION_OMITTED)
                    && clipped.len() > CANONICAL_RESULT_BYTES / 2
                    && clipped.len() <= CANONICAL_RESULT_BYTES,
                "{label}: {} bytes kept of an ample {CANONICAL_RESULT_BYTES}-byte bound: {:?}",
                clipped.len(),
                &inner[..inner.len().min(80)]
            );
        }
        Ok(())
    }

    /// Nested `{"result"}` wrappers are cut in bounded time and within bound.
    #[test]
    fn nested_wrappers_are_cut_in_bounded_time() {
        use crate::runtime::observation::budget::{CANONICAL_RESULT_BYTES, clip_observation_text};
        let mut text = "z ".repeat(150_000);
        for _ in 0..12 {
            text = serde_json::json!({ "result": text }).to_string();
        }
        let started = std::time::Instant::now();
        let clipped = clip_observation_text(&text, CANONICAL_RESULT_BYTES);
        let elapsed = started.elapsed();
        assert!(
            clipped.len() <= CANONICAL_RESULT_BYTES,
            "{} bytes",
            clipped.len()
        );
        assert!(serde_json::from_str::<serde_json::Value>(&clipped).is_ok());
        assert!(elapsed < std::time::Duration::from_secs(10), "{elapsed:?}");
    }

    /// Every cut with a budget of at least the floor fits that budget, for
    /// plain, process, read and wrapped bodies with fields beside the cut one.
    #[test]
    fn every_cut_at_or_above_the_floor_fits_its_budget() {
        use crate::runtime::observation::budget::{CUT_FLOOR_BYTES, clip_observation_text};
        let big = "\"y\\".repeat(20_000);
        let output = serde_json::json!({ "exit_code": 0, "output": big, "note": big }).to_string();
        let read = serde_json::json!({
            "content": big, "offset": 1, "lines": 9, "eof": false, "next_offset": 10, "data": big
        })
        .to_string();
        let bodies = [
            big.clone(),
            output.clone(),
            read.clone(),
            serde_json::json!({ "result": big }).to_string(),
            serde_json::json!({ "result": output }).to_string(),
            serde_json::json!({ "result": read }).to_string(),
        ];
        for body in &bodies {
            for budget in [CUT_FLOOR_BYTES, 300, 1_000, 4_096, 50_000, 100_000] {
                let clipped = clip_observation_text(body, budget);
                assert!(clipped.len() <= budget, "{} > {budget}", clipped.len());
                if body.starts_with("{\"result\"") {
                    assert!(serde_json::from_str::<serde_json::Value>(&clipped).is_ok());
                }
            }
            assert!(clip_observation_text(body, 0).len() <= CUT_FLOOR_BYTES);
        }
    }

    #[test]
    fn a_per_result_cap_is_not_capacity_pressure_and_pressure_survives_round_trip()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut capped = super::ObservationBudget::new(30_000, Some(2_000));
        assert!(!capped.capacity_limited());
        capped.record_result(25_000, 2_000, false);
        assert!(!capped.capacity_exhausted());
        let mut capacity = super::ObservationBudget::new(1_000, None);
        assert!(capacity.capacity_limited());
        capacity.record_result(25_000, 1_000, true);
        assert!(capacity.capacity_exhausted());
        let encoded = serde_json::to_vec(&capacity)?;
        assert_eq!(
            serde_json::from_slice::<super::ObservationBudget>(&encoded)?,
            capacity
        );
        Ok(())
    }
    #[test]
    fn truncated_read_observation_keeps_content_separate_from_budget_metadata()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::runtime::observation::budget::clip_observation_text;
        let body = serde_json::json!({"content": "λ".repeat(400), "offset":12, "lines":40, "eof":false,"next_offset":52}).to_string();
        for budget in [0, 160] {
            let clipped: serde_json::Value =
                serde_json::from_str(&clip_observation_text(&body, budget))?;
            assert_eq!(clipped["offset"], 12);
            assert_eq!(clipped["next_offset"], 52);
            assert_eq!(clipped["observation_truncated"], true);
            assert!(
                clipped["content"]
                    .as_str()
                    .ok_or("missing content")?
                    .chars()
                    .all(|c| c == 'λ')
            );
        }
        Ok(())
    }

    #[test]
    fn multibyte_clip_boundaries_keep_the_exact_omitted_byte_count() {
        let body = "\u{1f980}λ".repeat(300);
        for budget in 80..120 {
            let clipped = super::clip_head_tail(&body, budget);
            assert!(clipped.len() <= budget);
            let retained = clipped
                .lines()
                .filter(|line| !line.starts_with('['))
                .map(str::len)
                .sum::<usize>();
            assert!(clipped.contains(&super::omitted_marker(body.len() - retained)));
        }
    }

    fn build_log(lines: usize) -> String {
        let mut log = String::from("   Compiling plak v0.1.0\n");
        for index in 0..lines {
            assert!(writeln!(log, "test module_{index}::case ... ok").is_ok());
        }
        log.push_str("test parser::rejects_trailing_comma ... FAILED\n");
        log.push_str("error: test failed, to rerun pass `-p plak --lib`\n");
        log
    }

    #[test]
    fn a_clipped_build_log_keeps_its_first_and_final_lines_within_budget() {
        use super::clip_observation_text;
        let log = build_log(8_000);
        assert!(log.len() > 200_000);
        let clipped = clip_observation_text(&log, 4_096);
        assert!(clipped.len() <= 4_096, "{} bytes", clipped.len());
        assert!(clipped.starts_with("   Compiling plak v0.1.0\n"));
        assert!(clipped.contains("parser::rejects_trailing_comma ... FAILED"));
        assert!(clipped.ends_with("to rerun pass `-p plak --lib`\n"));
        assert!(clipped.contains("bytes omitted from the middle of this output"));
        // Both cuts land on line boundaries.
        for line in clipped.lines() {
            assert!(
                line.starts_with("test module_")
                    || line.starts_with('[')
                    || line.starts_with("   Compiling")
                    || line.starts_with("test parser")
                    || line.starts_with("error:"),
                "partial line {line:?}"
            );
        }
    }

    #[test]
    fn head_tail_clipping_never_splits_a_character_and_names_what_it_omitted() {
        use super::clip_head_tail;
        let body = "λ".repeat(5_000);
        for budget in [0, 1, 50, 97, 98, 99, 500, 4_001] {
            let clipped = clip_head_tail(&body, budget);
            let omitted = clipped
                .split('[')
                .nth(1)
                .and_then(|rest| rest.split(' ').next())
                .and_then(|digits| digits.parse::<usize>().ok());
            assert!(omitted.is_some(), "missing omitted count: {clipped}");
            let omitted = omitted.unwrap_or_default();
            let kept = clipped.chars().filter(|c| *c == 'λ').count() * 2;
            assert_eq!(kept + omitted, body.len(), "budget {budget}");
            if budget >= 200 {
                assert!(clipped.len() <= budget);
            }
        }
    }

    #[test]
    fn a_clipped_process_output_keeps_its_tail_and_stays_valid_json()
    -> Result<(), Box<dyn std::error::Error>> {
        use super::clip_observation_text;
        let log = build_log(3_000);
        let body =
            serde_json::json!({"exit_code": 101, "session_id": null, "output": log}).to_string();
        let clipped = clip_observation_text(&body, 2_048);
        assert!(clipped.len() <= 2_048);
        let value: serde_json::Value = serde_json::from_str(&clipped)?;
        assert_eq!(value["exit_code"], 101);
        let output = value["output"].as_str().ok_or("output")?;
        assert!(output.contains("rejects_trailing_comma ... FAILED"));
        assert!(output.starts_with("   Compiling"));
        Ok(())
    }
}
