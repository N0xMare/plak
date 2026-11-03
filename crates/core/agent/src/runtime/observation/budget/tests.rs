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
        let content = page["content"].as_str().ok_or("content lost")?;
        let shown = content.lines().count();
        assert!(content.is_empty() || content.ends_with('\n'), "{budget}");
        assert_eq!(page["lines"], shown);
        assert_eq!(page["next_offset"], 1 + shown);
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
        // One unterminated line cannot be shown whole: nothing is, and the
        // page continues where it began.
        assert_eq!(clipped["offset"], 12);
        assert_eq!(clipped["lines"], 0);
        assert_eq!(clipped["eof"], false);
        assert_eq!(clipped["next_offset"], 12);
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

/// Splits a plain read into its text and its metadata fields.
fn plain_read_parts(raised: &str) -> Option<(&str, Vec<(&str, &str)>)> {
    let body = raised.strip_suffix('\n')?;
    let start = body.rfind('\n').map_or(0, |index| index + 1);
    let (text, metadata) = body.split_at(start);
    let fields = metadata
        .split(' ')
        .map(|field| field.split_once('='))
        .collect::<Option<Vec<_>>>()?;
    Some((text, fields))
}

fn plain(body: &str, budget: usize) -> String {
    super::clip_observation_shaped(body, budget, &super::ObservationShape::PlainFileRead)
}

#[test]
fn a_cut_plain_read_stays_head_first_and_continues_after_the_lines_shown()
-> Result<(), Box<dyn std::error::Error>> {
    use super::{CUT_FLOOR_BYTES, OBSERVATION_EXHAUSTED};
    let mut content = String::new();
    for index in 0..20_000 {
        writeln!(content, "line {index} \"quoted\" λ")?;
    }
    let raised = format!(
        "{content}offset=7 lines=20000 eof=false next_offset=20007 clipped_lines=3 workspace_generation=9\n"
    );
    let withheld = format!("{OBSERVATION_EXHAUSTED}\n");
    for wrapped in [false, true] {
        let body = if wrapped {
            serde_json::json!({ "result": raised }).to_string()
        } else {
            raised.clone()
        };
        for budget in [0, 160, 512, 4_096, 65_536] {
            let clipped = plain(&body, budget);
            // A withheld page keeps its guidance and metadata line, which may
            // pass the cut floor.
            assert!(
                clipped.len() <= budget.max(CUT_FLOOR_BYTES + withheld.len() + 32),
                "{budget}: {} bytes",
                clipped.len()
            );
            let inner = if wrapped {
                let value: serde_json::Value = serde_json::from_str(&clipped)?;
                value["result"].as_str().ok_or("wrapper lost")?.to_owned()
            } else {
                clipped
            };
            let (text, fields) = plain_read_parts(&inner).ok_or("metadata line lost")?;
            let shown = if text == withheld {
                0
            } else {
                assert!(content.starts_with(text), "{budget}: not a head cut");
                assert!(text.ends_with('\n'), "{budget}: partial line");
                text.lines().count()
            };
            let next = 7 + shown;
            assert_eq!(
                fields,
                [
                    ("offset", "7"),
                    ("lines", shown.to_string().as_str()),
                    ("eof", "false"),
                    ("next_offset", next.to_string().as_str()),
                    ("clipped_lines", "3"),
                    ("workspace_generation", "9"),
                    ("observation_truncated", "true"),
                ],
                "{budget}"
            );
            assert!(!inner.contains("omitted"), "{budget}: head and tail cut");
            assert_eq!(budget == 0, shown == 0, "{budget}");
        }
    }
    Ok(())
}

#[test]
fn a_cut_plain_read_at_end_of_file_drops_the_final_newline_marker() {
    let content = "a\r\n".repeat(2_000) + "tail";
    let raised =
        format!("{content}\noffset=1 lines=2001 eof=true next_offset=null final_newline=false\n");
    let clipped = plain(&raised, 1_024);
    assert!(clipped.len() <= 1_024);
    assert!(clipped.contains(" eof=false next_offset="));
    assert!(!clipped.contains("final_newline"));
    assert!(!clipped.contains("tail"));
    assert!(clipped.ends_with(" observation_truncated=true\n"));
}

#[test]
fn a_full_output_line_inside_read_content_is_never_kept_as_a_locator() {
    let content = format!(
        "{}full output: /state/jobs/session/branch-p1-service.log\n{}",
        "before\n".repeat(5_000),
        "after\n".repeat(5_000)
    );
    let raised = format!("{content}offset=1 lines=10001 eof=true next_offset=null\n");
    let wrapped = serde_json::json!({ "result": raised }).to_string();
    for body in [raised.as_str(), wrapped.as_str()] {
        for budget in [0, 160, 512] {
            let clipped = plain(body, budget);
            assert!(!clipped.contains("full output:"), "{budget}: {clipped}");
            assert!(clipped.contains("observation_truncated=true"), "{budget}");
        }
    }
}

/// Only a result its raising program declared a plain read is cut as one: a
/// process result whose last line happens to match the read metadata grammar
/// keeps its head, tail and full-output locator.
#[test]
fn undeclared_text_ending_in_read_metadata_keeps_its_tail_and_locator() {
    use super::clip_observation_text;
    let path = "full output: /state/jobs/session/branch-p1-service.log";
    let body = format!(
        "{}{path}\noffset=1 lines=2 eof=true next_offset=null\n",
        "build step ok\n".repeat(20_000)
    );
    for wrapped in [false, true] {
        let body = if wrapped {
            serde_json::json!({ "result": body }).to_string()
        } else {
            body.clone()
        };
        for budget in [0, 512, 4_096] {
            let clipped = clip_observation_text(&body, budget);
            assert!(clipped.contains(path), "{budget}: locator lost: {clipped}");
            assert!(!clipped.contains("observation_truncated"), "{budget}");
            if budget != 0 {
                assert!(clipped.contains("omitted"), "{budget}: not head and tail");
                assert!(
                    clipped.contains("offset=1 lines=2 eof=true next_offset=null"),
                    "{budget}: tail lost"
                );
            }
        }
    }
}

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// One canonical read raised through `presentation`'s Direct read: the raised
/// outcome and its `{"result": string}` text.
fn raise_read(
    presentation: plakous_tools::CodingPresentation,
    canonical: &serde_json::Value,
) -> TestResult<(plakous_tools::ToolOutcome, String)> {
    use plakous_compose::{CodingPresentationExt as _, standard_coding_packs_in_memory};
    use plakous_model::{
        AttemptId, CallId, Digest, DigestDomain, IntegrationClass, RunId, ToolCorrelation,
        ToolName, ToolResultPart, TurnId,
    };
    use plakous_tools::{
        CodingPresentation, PROCESS_TOOL_PROJECTION_REVISION, ProviderModelProjectionIdentityV2,
        ToolInvocationRequest, ToolOutcome, ToolRegistryBuilder, register_packs,
    };
    let mut builder = ToolRegistryBuilder::default();
    register_packs(&standard_coding_packs_in_memory()?, &mut builder)?;
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"read-page-cut-test"],
    )?)?)?;
    let target = ProviderModelProjectionIdentityV2::new(
        IntegrationClass::Standard,
        "test.responses",
        "test.oauth",
        "test.read-page-cut.v1",
        "test-family",
        "test-model",
        "test-model",
        effective,
        PROCESS_TOOL_PROJECTION_REVISION,
    )?;
    let map = presentation.projection_map(
        plakous_tools::JobConfig::default(),
        builder.freeze(),
        target,
    )?;
    let (name, arguments) = match presentation {
        CodingPresentation::Claude => ("Read", r#"{"file_path":"a.txt"}"#),
        CodingPresentation::Grok => ("read_file", r#"{"target_file":"a.txt"}"#),
        _ => ("read", r#"{"path":"a.txt"}"#),
    };
    let request = ToolInvocationRequest::new(
        "00000000-0000-7000-8000-000000000a01".parse::<RunId>()?,
        "00000000-0000-7000-8000-000000000a02".parse::<TurnId>()?,
        "00000000-0000-7000-8000-000000000a03".parse::<AttemptId>()?,
        "00000000-0000-7000-8000-000000000a04".parse::<CallId>()?,
        ToolName::new(name)?,
        ToolCorrelation::default(),
        arguments,
    );
    let (_, receipt) = map.prepare(request)?.into_parts();
    let raised = map.raise(
        &receipt.ok_or("read omitted its receipt")?,
        ToolOutcome::success_text(canonical.to_string())?,
    )?;
    let [ToolResultPart::Text(text)] = raised.parts() else {
        return Err("raised read was not one text part".into());
    };
    let text = text.text().to_owned();
    Ok((raised, text))
}

/// The shape a raised read's mark declares.
fn raised_shape(raised: &plakous_tools::ToolOutcome) -> super::ObservationShape {
    use plakous_tools::internal::{RaisedFileRead, raised_file_read};
    match raised_file_read(raised) {
        Some(RaisedFileRead::Page(page)) => {
            super::ObservationShape::ReadPage(Box::new(page.clone()))
        }
        Some(RaisedFileRead::PlainWithMetadata) => super::ObservationShape::PlainFileRead,
        None => super::ObservationShape::Text,
    }
}

/// A raised page as the cut sees it, wrapped in its `{"result": string}`
/// presentation or bare: the mark describes the wrapped string, which is
/// also what an unwrapped (identity) raising renders.
struct RaisedPage {
    wrapped: String,
    bare: String,
    shape: super::ObservationShape,
}

impl RaisedPage {
    fn raise(
        presentation: plakous_tools::CodingPresentation,
        canonical: &serde_json::Value,
    ) -> TestResult<Self> {
        let (raised, wrapped) = raise_read(presentation, canonical)?;
        let shape = raised_shape(&raised);
        if !matches!(shape, super::ObservationShape::ReadPage(_)) {
            return Err("a Direct read page carries no page mark".into());
        }
        let value: serde_json::Value = serde_json::from_str(&wrapped)?;
        let bare = value["result"].as_str().ok_or("wrapper lost")?.to_owned();
        Ok(Self {
            wrapped,
            bare,
            shape,
        })
    }

    /// The cut at `budget`, as the wrapped string it shows, either way.
    fn cut(&self, wrapped: bool, budget: usize) -> TestResult<(String, usize)> {
        let body = if wrapped { &self.wrapped } else { &self.bare };
        let clipped = super::clip_observation_shaped(body, budget, &self.shape);
        let size = clipped.len();
        let inner = if wrapped {
            let value: serde_json::Value = serde_json::from_str(&clipped)?;
            value["result"].as_str().ok_or("wrapper lost")?.to_owned()
        } else {
            clipped
        };
        Ok((inner, size))
    }
}

fn canonical_page(content: &str, lines: usize, eof: bool) -> serde_json::Value {
    serde_json::json!({
        "content": content,
        "offset": 7,
        "lines": lines,
        "eof": eof,
        "next_offset": if eof { serde_json::Value::Null } else { (7 + lines).into() },
        "clipped_lines": 3,
        "lossy": true
    })
}

const CAT_N_NOTES: &str = "... 3 lines longer than 2000 characters were clipped\n... invalid UTF-8 was replaced with U+FFFD\n... page truncated to fit the context budget\n";
const PI_NOTES: &str = "\n[3 lines longer than 2000 characters were clipped]\n\n[invalid UTF-8 was replaced with U+FFFD]\n\n";

/// What a cut of `bare` showing `shown` lines must be, given the lines kept.
fn expected_cut(
    presentation: plakous_tools::CodingPresentation,
    head: &str,
    shown: usize,
) -> String {
    let next = 7 + shown;
    match presentation {
        plakous_tools::CodingPresentation::Claude => {
            format!("{head}{CAT_N_NOTES}... more lines; next_offset={next}")
        }
        _ if shown == 0 => format!("{head}{PI_NOTES}[Use offset={next} to continue.]"),
        _ => format!(
            "{head}{PI_NOTES}[Showing lines 7-{}. Use offset={next} to continue.]",
            next - 1
        ),
    }
}

/// The lines a cut kept from the head of `bare`, and how many.
fn kept_head<'a>(bare: &str, inner: &'a str) -> (&'a str, usize) {
    let mut end = 0;
    let mut shown = 0;
    for line in bare.split_inclusive('\n') {
        if !line.ends_with('\n') || !inner[end..].starts_with(line) {
            break;
        }
        end += line.len();
        shown += 1;
    }
    (&inner[..end], shown)
}

const PAGE_PRESENTATIONS: [plakous_tools::CodingPresentation; 2] = [
    plakous_tools::CodingPresentation::Claude,
    plakous_tools::CodingPresentation::Pi,
];

/// Claude's `cat -n` read and pi's read are cut head-first to whole lines, by
/// their raising program's mark: the page's notes stay, and each says in its
/// own words where to continue after the last line shown.
#[test]
fn a_cut_read_page_stays_head_first_and_continues_after_the_lines_shown() -> TestResult {
    use super::{CUT_FLOOR_BYTES, OBSERVATION_EXHAUSTED};
    let mut content = String::new();
    for index in 0..2_000 {
        writeln!(content, "line {index} \"quoted\" λ")?;
    }
    for presentation in PAGE_PRESENTATIONS {
        let page = RaisedPage::raise(presentation, &canonical_page(&content, 2_000, false))?;
        for wrapped in [false, true] {
            for budget in [0, 160, 512, 4_096, 32_768] {
                let (inner, size) = page.cut(wrapped, budget)?;
                assert!(
                    size <= budget.max(CUT_FLOOR_BYTES + OBSERVATION_EXHAUSTED.len() + 64),
                    "{presentation:?} {wrapped} {budget}: {size} bytes"
                );
                let (head, shown) = kept_head(&page.bare, &inner);
                let head = if shown == 0 {
                    format!("{OBSERVATION_EXHAUSTED}\n")
                } else {
                    head.to_owned()
                };
                assert_eq!(
                    inner,
                    expected_cut(presentation, &head, shown),
                    "{presentation:?} {wrapped} {budget}"
                );
                assert!(!inner.contains("omitted"), "{budget}: head and tail cut");
                assert_eq!(
                    budget < 512,
                    shown == 0,
                    "{presentation:?} {wrapped} {budget}"
                );
            }
        }
    }
    Ok(())
}

/// CRLF lines survive whole, and an unterminated last line is never shown
/// part-way: the continuation points at it.
#[test]
fn a_cut_crlf_read_page_keeps_whole_lines_and_drops_the_unterminated_tail() -> TestResult {
    let content = "a\r\n".repeat(2_000) + "tail";
    for presentation in PAGE_PRESENTATIONS {
        let page = RaisedPage::raise(presentation, &canonical_page(&content, 2_001, true))?;
        for wrapped in [false, true] {
            let (inner, size) = page.cut(wrapped, 1_024)?;
            assert!(size <= 1_024, "{presentation:?} {wrapped}: {size} bytes");
            let (head, shown) = kept_head(&page.bare, &inner);
            assert!(shown > 0, "{presentation:?} {wrapped}");
            assert!(head.ends_with("a\r\n"), "{presentation:?} {wrapped}");
            assert_eq!(inner, expected_cut(presentation, head, shown));
            assert!(!inner.contains("tail"));
        }
    }
    Ok(())
}

/// When no line fits, a single over-long line or a budget that fits nothing,
/// the capacity guidance comes first and the continuation names the page's
/// first line.
#[test]
fn a_read_page_that_fits_no_line_leads_with_the_capacity_guidance() -> TestResult {
    use super::OBSERVATION_EXHAUSTED;
    let long = format!("{}\nshort\n", "λ".repeat(1_999));
    let many = "short line\n".repeat(500);
    for presentation in PAGE_PRESENTATIONS {
        for (content, lines, budget) in [(&long, 2, 1_024), (&long, 2, 0), (&many, 500, 0)] {
            let page = RaisedPage::raise(presentation, &canonical_page(content, lines, true))?;
            for wrapped in [false, true] {
                let (inner, _) = page.cut(wrapped, budget)?;
                assert_eq!(
                    inner,
                    expected_cut(presentation, &format!("{OBSERVATION_EXHAUSTED}\n"), 0),
                    "{presentation:?} {wrapped} {budget}"
                );
            }
        }
    }
    Ok(())
}

/// Text is never sniffed as a read page: a process result that ends in a line
/// like either presentation's continuation keeps its head, tail and locator,
/// and so does a body its page mark does not describe.
#[test]
fn unmarked_text_ending_like_a_read_page_keeps_its_tail_and_locator() -> TestResult {
    use super::clip_observation_text;
    let path = "full output: /state/jobs/session/branch-p1-service.log";
    let page = RaisedPage::raise(
        plakous_tools::CodingPresentation::Claude,
        &canonical_page("one\ntwo\n", 2, false),
    )?;
    for tail in [
        "     1\tfoo\n... page truncated to fit the context budget\n... more lines; next_offset=12",
        "foo\n\n[Showing lines 1-2. Use offset=3 to continue.]",
    ] {
        let body = format!("{}{path}\n{tail}", "build step ok\n".repeat(20_000));
        for wrapped in [false, true] {
            let body = if wrapped {
                serde_json::json!({ "result": body }).to_string()
            } else {
                body.clone()
            };
            for budget in [0, 512, 4_096] {
                for clipped in [
                    clip_observation_text(&body, budget),
                    super::clip_observation_shaped(&body, budget, &page.shape),
                ] {
                    assert!(clipped.contains(path), "{budget}: locator lost: {clipped}");
                    if budget != 0 {
                        assert!(clipped.contains("omitted"), "{budget}: not head and tail");
                        let last = tail.lines().last().ok_or("tail")?;
                        assert!(clipped.contains(last), "{budget}: tail lost");
                    }
                }
            }
        }
    }
    Ok(())
}

/// The mark lives only in memory: a result reconciled from its journaled
/// evidence after a restart carries none and is cut by the text rules, while
/// the live result is cut head-first.
#[test]
fn a_restart_reconciled_read_page_is_cut_as_text() -> TestResult {
    use plakous_model::{
        CallId, TextBlock, ToolCorrelation, ToolExecutionOwner, ToolResultBlock, ToolResultKind,
        ToolResultPart,
    };
    let mut content = String::new();
    for index in 0..2_000 {
        writeln!(content, "line {index}")?;
    }
    for presentation in PAGE_PRESENTATIONS {
        let (raised, text) = raise_read(presentation, &canonical_page(&content, 2_000, false))?;
        let result = ToolResultBlock::new(
            "00000000-0000-7000-8000-000000000a05".parse::<CallId>()?,
            ToolExecutionOwner::Local,
            ToolResultKind::Success,
            vec![ToolResultPart::Text(TextBlock::new(text, None)?)],
            ToolCorrelation::default(),
        )?;
        let live =
            crate::runtime::ToolExecutionResult::new(result, true).with_raised_shape(&raised);
        assert!(matches!(
            live.observation_shape,
            super::ObservationShape::ReadPage(_)
        ));
        let evidence = serde_json::to_vec(&live)?;
        let reconciled: crate::runtime::ToolExecutionResult = serde_json::from_slice(&evidence)?;
        assert_eq!(reconciled.observation_shape, super::ObservationShape::Text);
        for (execution, head_first) in [(live, true), (reconciled, false)] {
            let mut budget = super::ObservationBudget::new(4_096, None);
            let (clipped, clip) = super::super::clip_observed_tool_result(
                execution.result,
                &mut budget,
                &execution.observation_shape,
            )?;
            assert!(clip.is_some());
            let [ToolResultPart::Text(text)] = clipped.parts() else {
                return Err("clipped read was not one text part".into());
            };
            let text = text.text();
            assert_eq!(
                text.contains("omitted"),
                !head_first,
                "{presentation:?}: {text}"
            );
            assert_eq!(
                text.contains("next_offset=2007") || text.contains("offset=2007 to continue"),
                !head_first,
                "{presentation:?}: {text}"
            );
        }
    }
    Ok(())
}
