use super::*;

fn fired(plan: &FaultPlan, host: &str, path: &str) -> Option<&'static str> {
    plan.select(host, path).map(|selected| match selected {
        Selected::Status { .. } => "status",
        Selected::Connect => "connect",
        Selected::WriteReset => "write-reset",
        Selected::RecordRejected => "record-rejected",
        Selected::Invalid => "invalid",
        Selected::Stream(_) => "stream",
    })
}

#[test]
fn ordinals_count_per_rule_over_matching_requests_only() -> Result<(), FaultSpecError> {
    let plan = FaultPlan::parse(
        "host=api.x.ai,path=/v1/responses,nth=2,fault=429,retry-after=3; host=openrouter.ai,fault=connect",
    )?;
    assert_eq!(fired(&plan, "api.x.ai", "/v1/models"), None);
    assert_eq!(fired(&plan, "api.x.ai", "/v1/responses"), None);
    assert_eq!(
        fired(&plan, "API.X.AI", "/v1/responses?x=1"),
        Some("status")
    );
    assert_eq!(fired(&plan, "api.x.ai", "/v1/responses"), None);
    assert_eq!(
        fired(&plan, "openrouter.ai", "/api/v1/chat"),
        Some("connect")
    );
    assert_eq!(
        fired(&plan, "openrouter.ai", "/api/v1/chat"),
        Some("connect")
    );
    Ok(())
}

#[test]
fn clones_share_counters() -> Result<(), FaultSpecError> {
    let plan = FaultPlan::parse("nth=2,fault=502")?;
    let clone = plan.clone();
    assert_eq!(fired(&plan, "h", "/"), None);
    assert_eq!(fired(&clone, "h", "/"), Some("status"));
    Ok(())
}

#[test]
fn malformed_rules_name_the_rule_and_the_problem() {
    for (spec, reason) in [
        ("fault=418", "unknown fault `418`"),
        ("nth=0,fault=500", "`nth` counts from 1"),
        ("fault=stall", "`stall` needs `seconds`"),
        (
            "fault=reset,retry-after=1",
            "`retry-after` applies to status faults only",
        ),
        (
            "fault=429,after-events=2",
            "`after-events`/`after-bytes` apply to stream faults only",
        ),
        ("host", "`host` is not key=value"),
        ("colour=red,fault=500", "unknown field `colour`"),
    ] {
        assert_eq!(
            FaultPlan::parse(&format!("fault=500;{spec}")).err(),
            Some(FaultSpecError {
                rule: 2,
                reason: reason.to_owned()
            }),
            "{spec}"
        );
    }
}

#[test]
fn an_unparsable_environment_plan_fails_every_exchange() {
    assert!(plan_from(None).is_none());
    let plan = plan_from(Some("fault=teapot"));
    let plan = plan.as_ref();
    assert_eq!(plan.and_then(|plan| fired(plan, "a", "/")), Some("invalid"));
    assert_eq!(
        plan.and_then(|plan| fired(plan, "b", "/x")),
        Some("invalid")
    );
}

#[test]
fn event_thresholds_fall_after_the_blank_line_across_chunks() {
    let mut fault = StreamFault::new(StreamAction::Reset, Threshold::Events(2));
    assert_eq!(fault.cut_point(b"data: a\r\n\r\ndata: b\r"), None);
    assert_eq!(fault.due(), None);
    assert_eq!(fault.cut_point(b"\n\r\ndata: c\n\n"), Some(3));
    assert_eq!(fault.due(), Some(StreamAction::Reset));
    assert_eq!(fault.due(), None);
}

#[test]
fn comment_only_blocks_before_data_are_not_events() {
    let mut fault = StreamFault::new(StreamAction::Reset, Threshold::Events(1));
    let keep_alive = b": OPENROUTER PROCESSING\n\n";
    for _ in 0..3 {
        assert_eq!(fault.cut_point(keep_alive), None);
    }
    assert_eq!(fault.cut_point(b"\n\n"), None);
    assert_eq!(fault.due(), None);
    assert_eq!(fault.cut_point(b"data: a\n\ndata: b\n\n"), Some(9));
    assert_eq!(fault.due(), Some(StreamAction::Reset));
}

#[test]
fn comments_within_and_between_events_do_not_add_events() {
    let body: &[u8] =
        b": ping\n\nevent: delta\n: inside\ndata: a\n\n: between\n:\n\nid: 7\nretry: 5\n\ndata\n\n";
    for (events, end) in [
        (1, b"data: a\n\n".as_slice()),
        (2, b"retry: 5\n\n"),
        (3, b"data\n\n"),
    ] {
        let mut fault = StreamFault::new(StreamAction::Eof, Threshold::Events(events));
        let cut = fault.cut_point(body);
        assert!(body[..cut.unwrap_or(0)].ends_with(end), "{events}: {cut:?}");
    }
    let mut short = StreamFault::new(StreamAction::Eof, Threshold::Events(4));
    assert_eq!(short.cut_point(body), None);
}

#[test]
fn crlf_framing_counts_the_same_as_lf() {
    let mut fault = StreamFault::new(StreamAction::Reset, Threshold::Events(2));
    let body = b": keep-alive\r\n\r\ndata: a\r\n\r\n: x\r\ndata: b\r\n\r\ndata: c\r\n\r\n";
    let cut = fault.cut_point(body);
    assert_eq!(
        cut.map(|cut| &body[..cut]),
        Some(&body[..body.len() - b"data: c\r\n\r\n".len()])
    );
}

#[test]
fn a_block_with_a_comment_and_a_data_line_counts_once() {
    let mut fault = StreamFault::new(StreamAction::Reset, Threshold::Events(2));
    assert_eq!(fault.cut_point(b": note\ndata: a\n\n"), None);
    assert_eq!(fault.cut_point(b"data: b\n: note\n\n: tail\n\n"), Some(16));
}

#[test]
fn the_cut_lands_exactly_after_the_nth_data_event() {
    let body = b": OPENROUTER PROCESSING\n\ndata: one\n\n: OPENROUTER PROCESSING\n\ndata: two\n\ndata: three\n\n";
    let mut fault = StreamFault::new(StreamAction::Reset, Threshold::Events(2));
    let cut = fault.cut_point(body).unwrap_or(0);
    assert_eq!(
        &body[..cut],
        b": OPENROUTER PROCESSING\n\ndata: one\n\n: OPENROUTER PROCESSING\n\ndata: two\n\n"
    );
    assert_eq!(fault.due(), Some(StreamAction::Reset));
}

#[test]
fn byte_thresholds_cut_exactly_and_zero_fires_at_once() {
    let mut fault = StreamFault::new(StreamAction::Eof, Threshold::Bytes(5));
    assert_eq!(fault.cut_point(b"abc"), None);
    assert_eq!(fault.cut_point(b"defg"), Some(2));
    let mut immediate = StreamFault::new(StreamAction::Eof, Threshold::Events(0));
    assert_eq!(immediate.due(), Some(StreamAction::Eof));
}
