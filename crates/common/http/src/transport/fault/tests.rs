use super::*;

fn fired(plan: &FaultPlan, host: &str, path: &str) -> Option<&'static str> {
    fired_on(plan, FaultHop::Upstream, host, path)
}

fn fired_on(plan: &FaultPlan, hop: FaultHop, host: &str, path: &str) -> Option<&'static str> {
    plan.clone()
        .with_hop(hop)
        .select(host, path)
        .map(|selected| match selected {
            Selected::Status { .. } => "status",
            Selected::Connect => "connect",
            Selected::Hang(_) => "hang",
            Selected::WriteReset => "write-reset",
            Selected::RecordRejected => "record-rejected",
            Selected::Invalid => "invalid",
            Selected::RelayLost(_) => "relay-lost",
            Selected::GatewayRefusal(_) => "gateway-refusal",
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
fn hang_matches_host_path_and_shared_ordinal() -> Result<(), FaultSpecError> {
    let plan = FaultPlan::parse("host=example.invalid,path=/usage,nth=2,fault=hang,seconds=30")?;
    assert_eq!(fired(&plan, "other.invalid", "/usage"), None);
    assert_eq!(fired(&plan, "example.invalid", "/models"), None);
    assert_eq!(fired(&plan, "example.invalid", "/usage"), None);
    assert_eq!(
        fired(&plan.clone(), "EXAMPLE.INVALID", "/usage?x=1"),
        Some("hang")
    );
    assert_eq!(fired(&plan, "example.invalid", "/usage"), None);
    Ok(())
}

#[tokio::test]
async fn hang_times_out_without_opening_a_socket() -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let client = crate::Client::new(Limits::default())?
        .with_faults(Some(FaultPlan::parse("fault=hang,seconds=0.05")?));
    let request = crate::Request::new(
        crate::Origin::parse(&format!("https://{}", listener.local_addr()?))?,
        crate::Method::GET,
        "/usage",
        crate::HeaderMap::new(),
        Bytes::new(),
        client.limits(),
    )?;
    let started = std::time::Instant::now();
    let execute = client.execute(
        request,
        ExchangeControl::new(started + Duration::from_secs(5)),
    );
    let failure = tokio::select! {
        _ = listener.accept() => panic!("hang opened a socket"),
        result = execute => result.err().ok_or("hang succeeded")?,
    };
    assert!(started.elapsed() >= Duration::from_millis(50));
    assert_eq!(
        (failure.phase(), failure.transmission(), failure.kind()),
        (
            Phase::Connect,
            TransmissionFact::NotStarted,
            ErrorKind::DeadlineExceeded
        )
    );
    assert_eq!(failure.connection_use(), None);
    assert_eq!(failure.connect_attempts(), None);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn hang_cancellation_and_deadline_end_the_wait() -> Result<(), Box<dyn std::error::Error>> {
    let control = ExchangeControl::new(std::time::Instant::now() + Duration::from_secs(60));
    let mut wait = Box::pin(injected_hang(Duration::from_secs(60), &control));
    assert!(futures_util::poll!(&mut wait).is_pending());
    control.cancellation().cancel();
    let failure = tokio::time::timeout(Duration::from_secs(1), wait).await?;
    assert_eq!(failure.kind(), ErrorKind::Cancelled);

    let control = ExchangeControl::new(std::time::Instant::now() + Duration::from_millis(20));
    let failure = tokio::time::timeout(
        Duration::from_secs(1),
        injected_hang(Duration::from_secs(60), &control),
    )
    .await?;
    assert_eq!(failure.kind(), ErrorKind::DeadlineExceeded);
    Ok(())
}

#[test]
fn malformed_rules_name_the_rule_and_the_problem() {
    for (spec, reason) in [
        ("fault=418", "unknown fault `418`"),
        ("nth=0,fault=500", "`nth` counts from 1"),
        ("fault=stall", "`stall` needs `seconds`"),
        ("fault=hang", "`hang` needs `seconds`"),
        (
            "fault=hang,seconds=-1",
            "`seconds` needs a duration, not `-1`",
        ),
        (
            "fault=hang,seconds=NaN",
            "`seconds` needs a duration, not `NaN`",
        ),
        (
            "fault=hang,seconds=1,after-events=0",
            "`after-events`/`after-bytes` apply to stream faults only",
        ),
        (
            "fault=hang,seconds=1,retry-after=1",
            "`retry-after` applies to status faults only",
        ),
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

#[test]
fn hops_count_only_their_own_rules() -> Result<(), FaultSpecError> {
    let plan = FaultPlan::parse(
        "hop=gateway,nth=2,fault=connect;hop=upstream,nth=1,host=api.x.ai,fault=500",
    )?;
    assert_eq!(
        fired_on(&plan, FaultHop::Upstream, "api.x.ai", "/"),
        Some("status")
    );
    assert_eq!(fired_on(&plan, FaultHop::Upstream, "api.x.ai", "/"), None);
    assert_eq!(fired_on(&plan, FaultHop::Gateway, "api.x.ai", "/"), None);
    assert_eq!(
        fired_on(&plan, FaultHop::Gateway, "other.invalid", "/v1"),
        Some("connect")
    );
    assert_eq!(fired(&plan, "api.x.ai", "/"), None);
    Ok(())
}

#[test]
fn an_absent_hop_is_upstream() -> Result<(), FaultSpecError> {
    let plan = FaultPlan::parse("fault=429")?;
    assert_eq!(fired_on(&plan, FaultHop::Gateway, "h", "/"), None);
    assert_eq!(fired(&plan, "h", "/"), Some("status"));
    Ok(())
}

#[test]
fn an_external_gateway_refuses_upstream_rules_only() {
    let refused = FaultPlan::parse_for("fault=500", FaultPlacement::ExternalGateway)
        .err()
        .map(|error| error.to_string());
    assert_eq!(
        refused.as_deref(),
        Some(
            "PLAK_TEST_FAULTS rule 1: hop=upstream does not apply when an external gateway \
             address is configured"
        )
    );
    assert!(
        FaultPlan::parse_for(
            "hop=gateway,fault=relay-lost=before-head",
            FaultPlacement::ExternalGateway
        )
        .is_ok()
    );
    assert!(
        FaultPlan::parse_for("hop=upstream,fault=500", FaultPlacement::ExternalGateway).is_err()
    );
    let mixed = FaultPlan::parse_for(
        "hop=gateway,fault=connect;fault=500",
        FaultPlacement::ExternalGateway,
    )
    .err()
    .map(|error| error.to_string());
    assert_eq!(
        mixed.as_deref(),
        Some(
            "PLAK_TEST_FAULTS rule 2: hop=upstream does not apply when an external gateway \
             address is configured"
        )
    );
}

#[test]
fn a_mistyped_gateway_placement_fails_every_exchange() {
    let plan = plan_from_placement(Some("hop=gateway,fault=connect"), None);
    assert_eq!(
        plan.as_ref()
            .and_then(|plan| fired_on(plan, FaultHop::Gateway, "h", "/")),
        Some("invalid")
    );
    assert_eq!(
        plan.as_ref().and_then(|plan| fired(plan, "h", "/")),
        Some("invalid")
    );
    let local = plan_from_placement(Some("fault=500"), Some(FaultPlacement::Local));
    assert_eq!(
        local.as_ref().and_then(|plan| fired(plan, "h", "/")),
        Some("status")
    );
}

#[test]
fn the_provider_grammar_is_unchanged() {
    for spec in [
        "hop=upstream,fault=500",
        "fault=relay-lost=before-head",
        "fault=gateway-refusal=unauthenticated",
        "fault=500,retry-after-ms=1",
        "fault=500,credential-status=ready",
        "fault=500,supported=1",
    ] {
        let error = FaultPlan::parse_provider(spec)
            .err()
            .map(|error| error.to_string());
        assert!(
            error
                .as_deref()
                .is_some_and(|text| text.contains("unknown")),
            "{spec}: {error:?}"
        );
    }
    match FaultPlan::parse_provider("host=api.x.ai,nth=2,fault=429,retry-after=3") {
        Ok(plan) => {
            assert_eq!(fired(&plan, "api.x.ai", "/"), None);
            assert_eq!(fired(&plan, "api.x.ai", "/"), Some("status"));
            assert_eq!(fired_on(&plan, FaultHop::Gateway, "api.x.ai", "/"), None);
        }
        Err(error) => panic!("{error}"),
    }
}

#[test]
fn hop_one_faults_require_the_gateway_hop() {
    for spec in [
        "fault=relay-lost=before-head",
        "hop=upstream,fault=relay-lost=after-head",
        "fault=gateway-refusal=busy,retry-after-ms=5",
        "hop=gateway,fault=gateway-refusal=fence_moved",
        "hop=nope,fault=500",
        "hop=gateway,fault=gateway-refusal=credential_not_ready",
        "hop=gateway,fault=gateway-refusal=no_such",
    ] {
        assert!(FaultPlan::parse(spec).is_err(), "{spec}");
    }
}

#[test]
fn gateway_refusals_carry_the_protocol_status_and_body() -> Result<(), String> {
    let cases = [
        (
            "unauthenticated",
            401_u16,
            r#"{"gateway_error":{"code":"unauthenticated"}}"#,
        ),
        (
            "scope_denied",
            403,
            r#"{"gateway_error":{"code":"scope_denied"}}"#,
        ),
        (
            "account_unknown",
            404,
            r#"{"gateway_error":{"code":"account_unknown"}}"#,
        ),
        (
            "account_changed",
            409,
            r#"{"gateway_error":{"code":"account_changed"}}"#,
        ),
        (
            "upstream_not_pinned",
            422,
            r#"{"gateway_error":{"code":"upstream_not_pinned"}}"#,
        ),
        (
            "limit_exceeded",
            413,
            r#"{"gateway_error":{"code":"limit_exceeded"}}"#,
        ),
        ("internal", 500, r#"{"gateway_error":{"code":"internal"}}"#),
        (
            "gateway_disabled",
            503,
            r#"{"gateway_error":{"code":"gateway_disabled"}}"#,
        ),
        (
            "account_disabled",
            503,
            r#"{"gateway_error":{"code":"account_disabled"}}"#,
        ),
        (
            "version_unsupported",
            426,
            r#"{"gateway_error":{"code":"version_unsupported","supported":[1]}}"#,
        ),
    ];
    for (code, status, body) in cases {
        let refusal = take_refusal(&format!("hop=gateway,fault=gateway-refusal={code}"))?;
        assert_eq!(refusal.status, status, "{code}");
        assert_eq!(refusal.code, code);
        assert_eq!(refusal.body, body, "{code}");
        assert!(
            refusal
                .reply()
                .windows(body.len())
                .any(|window| window == body.as_bytes())
        );
    }
    let busy = take_refusal("hop=gateway,fault=gateway-refusal=busy,retry-after-ms=15")?;
    assert_eq!(busy.status, 503);
    assert_eq!(
        busy.body,
        r#"{"gateway_error":{"code":"busy","retry_after_ms":15}}"#
    );
    let waiting = take_refusal(
        "hop=gateway,fault=gateway-refusal=credential_not_ready,credential-status=quarantined",
    )?;
    assert_eq!(waiting.status, 409);
    assert_eq!(
        waiting.body,
        r#"{"gateway_error":{"code":"credential_not_ready","status":"quarantined"}}"#
    );
    let versions =
        take_refusal("hop=gateway,fault=gateway-refusal=version_unsupported,supported=1+2")?;
    assert_eq!(
        versions.body,
        r#"{"gateway_error":{"code":"version_unsupported","supported":[1,2]}}"#
    );
    Ok(())
}

#[cfg(feature = "gateway-server")]
#[test]
fn gateway_refusal_bodies_decode() -> Result<(), String> {
    use plakous_gateway_protocol::call;
    use plakous_gateway_protocol::facts::CredentialStatus;
    use plakous_gateway_protocol::refusal::{GatewayRefusal, RefusalBody};

    let refusal = take_refusal(
        "hop=gateway,fault=gateway-refusal=credential_not_ready,credential-status=quarantined",
    )?;
    let decoded: RefusalBody =
        call::decode(refusal.body.as_bytes()).map_err(|error| error.to_string())?;
    assert_eq!(
        decoded.gateway_error,
        GatewayRefusal::CredentialNotReady {
            status: CredentialStatus::Quarantined
        }
    );
    let busy = take_refusal("hop=gateway,fault=gateway-refusal=busy,retry-after-ms=8")?;
    let decoded: RefusalBody =
        call::decode(busy.body.as_bytes()).map_err(|error| error.to_string())?;
    assert_eq!(
        decoded.gateway_error,
        GatewayRefusal::Busy { retry_after_ms: 8 }
    );
    Ok(())
}

fn take_refusal(spec: &str) -> Result<GatewayRefusalFault, String> {
    let plan = FaultPlan::parse(spec).map_err(|error| error.to_string())?;
    match plan.with_hop(FaultHop::Gateway).select("h", "/") {
        Some(Selected::GatewayRefusal(refusal)) => Ok(refusal),
        _ => Err(format!("`{spec}` did not select a gateway refusal")),
    }
}
