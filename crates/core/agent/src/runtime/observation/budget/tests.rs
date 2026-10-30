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
    let mut exhausted = super::ObservationBudget::new(0, Some(82_688));
    assert_eq!(
        exhausted.clip_text(&"c".repeat(50_000)),
        super::OBSERVATION_EXHAUSTED
    );
    assert!(exhausted.capacity_exhausted());
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
fn an_unanchored_estimate_grants_half_its_estimated_headroom() {
    let headroom = ContextHeadroom::Shared(QualifiedTokens::Exact(
        OBSERVATION_BUDGET_RESERVE_TOKENS + 64_000,
    ));
    assert_eq!(
        super::observation_bytes_for_sent_status(headroom, false),
        64_000
    );
    assert_eq!(
        super::observation_bytes_for_sent_status(headroom, true),
        32_000
    );
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
    let bytes =
        observation_bytes_from_headroom(ContextHeadroom::Shared(QualifiedTokens::Exact(36_000)));
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
        let string_ok =
            plakous_model::SemanticJsonV1::from_value(serde_json::Value::String(clipped.clone()))
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
    let body = serde_json::json!({"exit_code": 101, "session_id": null, "output": log}).to_string();
    let clipped = clip_observation_text(&body, 2_048);
    assert!(clipped.len() <= 2_048);
    let value: serde_json::Value = serde_json::from_str(&clipped)?;
    assert_eq!(value["exit_code"], 101);
    let output = value["output"].as_str().ok_or("output")?;
    assert!(output.contains("rejects_trailing_comma ... FAILED"));
    assert!(output.starts_with("   Compiling"));
    Ok(())
}
