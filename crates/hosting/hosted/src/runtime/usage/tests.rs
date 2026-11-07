#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;
type ContentEvent = crate::surface_protocol::ContentEvent<TestExtension>;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;
use crate::runtime::test_kit::ProbeIntent;
use crate::runtime::test_kit::ProbeObservation;

use super::*;
use crate::runtime::*;

use crate::runtime::test_support::*;
use crate::sessions::SessionRecord;

struct NoCredentialReads;

fn default_ready_usage_fixture(root: &std::path::Path) -> (NativeSubscriptions, std::fs::File) {
    use plakous::auth::*;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let state = root.join("state");
    let layout = state.join("plak/subscriptions");
    for path in [
        &state,
        &state.join("plak"),
        &layout,
        &layout.join("compatibility"),
        &layout.join("codex"),
    ] {
        std::fs::create_dir(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let profile = SubscriptionIntegrationProfileId::new(
        NativeSubscriptions::notice(NativeProvider::Codex)
            .unwrap()
            .profile_id(),
    )
    .unwrap();
    let store = PrivateCredentialStore::open_slot(
        layout.join("codex/credential.json"),
        profile.clone(),
        CredentialSlotId::new("primary").unwrap(),
    )
    .unwrap();
    store
        .initialize_selected(
            &CredentialRecord::new(
                profile,
                AccountBinding::new("default-upstream").unwrap(),
                CredentialGeneration::new(1).unwrap(),
                CredentialStatus::Ready,
                SecretBytes::new(b"synthetic-ready".to_vec()),
            )
            .unwrap(),
        )
        .unwrap();
    // A default read must be observably Busy, never reach real provider HTTP.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(layout.join("compatibility/.codex.lock"))
        .unwrap();
    lock.lock().unwrap();
    (
        NativeSubscriptions::new(
            plakous::subscriptions::SubscriptionPaths::from_xdg_state_home(state).unwrap(),
        ),
        lock,
    )
}

fn receive_usage(
    events: &crate::surface_protocol::HostEventReceiver<TestExtension>,
) -> plakous_host_protocol::views::presentation::UsageLimitsView {
    loop {
        if let HostEvent::UsageLimits(view) = events.recv_timeout(Duration::from_secs(30)).unwrap()
        {
            return view;
        }
    }
}

#[test]
fn named_shell_usage_never_reads_the_ready_default_store() {
    let root = tempfile::tempdir().unwrap();
    let (subscriptions, _lock) = default_ready_usage_fixture(root.path());
    let mut session = SessionRecord::new(root.path().to_path_buf());
    session.account = Some("codex/work".into());
    let mut fixture = test_kit::shell_host_on_doubles::<TestExtension>(root.path(), session, None);
    fixture.config.subscriptions = Some(subscriptions);
    let (actions, events, worker) = test_kit::spawn_shell_host_on_doubles(fixture.config);
    actions
        .send(UserAction::Extension(ProbeIntent::InspectUsage))
        .unwrap();
    let view = receive_usage(&events);
    assert_eq!(view.verticals.len(), 1);
    assert_eq!(view.verticals[0].vertical.account(), "codex/work");
    assert!(!format!("{:?}", view.verticals[0].state).contains("Busy"));
    assert!(
        !format!("{:?}", view.verticals[0].state)
            .contains(crate::subscription_usage::USAGE_BUSY_REASON)
    );
    drop(actions);
    worker.join().unwrap();
}

#[test]
fn usage_refresh_follows_default_named_and_default_rebind_requests() {
    let root = tempfile::tempdir().unwrap();
    let (subscriptions, _lock) = default_ready_usage_fixture(root.path());
    let (config, _) = fresh_injected_attached_host(root.path());
    let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let refresh = SubscriptionUsageRefresh::new(
        &Sender::over(wire),
        config.state_roots.openrouter_credentials(),
        HeldUsage::default(),
        SessionLedger::default(),
    );
    for account in ["codex", "codex/work", "codex"] {
        refresh.request_fresh(Some(subscriptions.clone()), Some(account.parse().unwrap()));
        let view = receive_usage(&events);
        let row = view
            .verticals
            .iter()
            .find(|row| row.vertical.account() == account)
            .unwrap();
        let busy =
            format!("{:?}", row.state).contains(crate::subscription_usage::USAGE_BUSY_REASON);
        assert_eq!(busy, account == "codex", "{account}: {row:?}");
        assert_eq!(view.verticals.len(), if account == "codex" { 4 } else { 1 });
    }
}
impl crate::openrouter::OpenRouterCredentials for NoCredentialReads {
    fn preflight(&self, _: &crate::account::AccountRef) -> CliResult<()> {
        panic!("named native usage must not probe any gateway key")
    }
    fn key_label(&self, _: &crate::account::AccountRef) -> CliResult<String> {
        panic!("named native usage must not inspect a gateway key")
    }
    #[cfg(feature = "openrouter")]
    fn source(
        &self,
        _: &crate::account::AccountRef,
    ) -> CliResult<Arc<plakous::openrouter::OpenRouterKeySource>> {
        panic!("named native usage must not construct a gateway credential source")
    }
}

#[test]
fn named_host_usage_labels_only_its_own_observation_and_reads_no_gateway_key() {
    let native = NativeSubscriptionRefresh::Read(Box::new((
        Some(native_usage_fixture("codex", 1.0)),
        Some(native_usage_fixture("grok", 2.0)),
        Some(native_usage_fixture("claude", 12.5)),
    )));
    let view =
        project_named_usage_limits(native, &NoCredentialReads, &"claude/work".parse().unwrap());
    assert_eq!(view.verticals.len(), 1);
    assert_eq!(view.verticals[0].vertical.account(), "claude/work");
    assert_native_percent(&view, 0, "12.5");
}

#[test]
fn named_api_host_usage_never_reads_or_creates_native_default_slots() {
    let root = tempfile::tempdir().unwrap();
    let state_home = root.path().join("state");
    let subscriptions = NativeSubscriptions::new(
        plakous::subscriptions::SubscriptionPaths::from_xdg_state_home(state_home.clone()).unwrap(),
    );
    let executor = ApplicationExecutor::new().unwrap();
    for account in [
        "openai/work",
        "anthropic/work",
        "xai-api/work",
        "openrouter/work",
    ] {
        assert!(matches!(
            refresh_named_subscription_usage(
                &executor,
                Some(subscriptions.clone()),
                &account.parse().unwrap()
            ),
            NativeSubscriptionRefresh::NoSubscriptionStore
        ));
    }
    assert!(!state_home.exists());
}
use plakous::agent::TurnUsage;
use plakous::application::ApplicationExecutor;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn projected_usage_log_carries_its_authoritative_turn_id() {
    let root = tempfile::tempdir().unwrap();
    let home = SessionHome::new(root.path().join("sessions"));
    let session_id = uuid::Uuid::new_v4().to_string();
    let turn_id = plakous::model::TurnId::try_from(uuid::Uuid::new_v4()).unwrap();
    let log = SessionLog::open(&home, &session_id).unwrap();
    let (events, _receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
    emit_projected_turn_usage(
        &Sender::over(events),
        &session_id,
        turn_id,
        TurnUsage::default(),
        Some(&log),
        &SessionLedger::default(),
    );
    let rows = crate::session_log::read_tail(&home, &session_id, 10).unwrap();
    let row: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
    assert_eq!(row["kind"], "turn.usage");
    assert_eq!(row["turn_id"], turn_id.to_string());
}

/// A refresh that never ran is not reported as a timeout (spec 02 §7.2).
#[test]
fn a_missing_subscription_store_is_not_reported_as_a_timeout() {
    let no_store = NativeSubscriptionRefresh::NoSubscriptionStore;
    assert_eq!(
        no_store.unavailable_reason(),
        Some("no native subscription store is configured for this session")
    );
    let plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable { reason } =
        project_subscription_usage(NativeSubscriptionRefresh::NoSubscriptionStore)
    else {
        panic!("no store must project as unavailable");
    };
    assert!(!reason.contains("timed out"), "{reason}");

    let plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable { reason } =
        project_subscription_usage(NativeSubscriptionRefresh::TimedOut)
    else {
        panic!("a timed-out refresh must project as unavailable");
    };
    assert!(reason.contains("timed out"), "{reason}");
}

#[test]
fn turn_result_usage_projection_preserves_counts_and_coverage() {
    let usage = match TurnUsage::new(plakous::model::Usage::new(21, 8, 5, 13, 0), 3, 2, 1) {
        Ok(usage) => usage,
        Err(error) => panic!("valid partial usage coverage rejected: {error}"),
    };
    assert_eq!(
        project_turn_usage(usage, None),
        ContentEvent::TurnUsage {
            input_tokens: 21,
            output_tokens: 8,
            reasoning_tokens: 5,
            cached_input_tokens: 13,
            cache_write_tokens: 0,
            model_operations: Some(3),
            reporting_model_operations: Some(2),
            peak_prepared_input_tokens: None,
            terminal: true,
        }
    );
}

/// The peak is the largest single operation, never the running total.
///
/// Measuring how far a turn grew against the window cannot use
/// `input_tokens`: it sums over operations, so a many-round shell loop
/// inflates it without any one prompt growing. A tool-growth turn
/// recorded 534,462 summed tokens on a 400,000-token route
/// while no single prompt came near the ceiling, and the gate correctly
/// refused to read that as growth.
#[test]
fn the_prepared_input_peak_is_one_operations_prompt_not_the_running_sum() {
    let mut session = SessionRecord::new(std::env::temp_dir());
    let mut peak = 0_u64;
    for input in [12_000_u64, 31_500, 8_200, 27_000] {
        record_operation_prepared_input(
            &mut peak,
            &mut session,
            &plakous::model::UsageUpdate::new(plakous::model::Usage::new(input, 1, 0, 0, 0)),
            None,
        );
    }
    assert_eq!(peak, 31_500, "the peak is the largest single prompt");
    // A zero report is not an observation and must not move either.
    record_operation_prepared_input(
        &mut peak,
        &mut session,
        &plakous::model::UsageUpdate::new(plakous::model::Usage::new(0, 1, 0, 0, 0)),
        None,
    );
    assert_eq!(peak, 31_500);
}

/// The terminal observation carries the peak beside the provider sums.
#[test]
fn the_terminal_usage_projection_carries_the_growth_facts() {
    let usage = match TurnUsage::new(plakous::model::Usage::new(78_700, 8, 5, 13, 0), 4, 4, 1) {
        Ok(usage) => usage,
        Err(error) => panic!("valid usage coverage rejected: {error}"),
    };
    let ContentEvent::TurnUsage {
        input_tokens,
        model_operations,
        peak_prepared_input_tokens,
        terminal,
        ..
    }: ContentEvent = project_turn_usage(usage, Some(31_500))
    else {
        panic!("expected a turn-usage observation");
    };
    assert_eq!(input_tokens, 78_700, "the provider sum is unchanged");
    assert_eq!(model_operations, Some(4));
    assert_eq!(
        peak_prepared_input_tokens,
        Some(31_500),
        "one prompt's worth, which is what measures growth"
    );
    assert!(terminal);
}

/// The in-turn HUD applies the one Plakous formula: the held anchored
/// measure, plus `bytes/4` of what streams, re-anchored on each provider
/// report against the estimate of the request that report counted.
#[test]
fn live_occupancy_is_the_plakous_measure_advanced_through_the_turn() {
    let parts = plakous::model::ContextInputParts::measure_wire(br#"{"input":"x"}"#).unwrap();
    let projection = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(900_000),
        1_000_000,
        1_000_000,
        Some(parts),
    )
    .with_occupancy_measure(
        Some(851_715),
        plakous::model::ContextAnchor::new(298_468, 850_715),
    );
    assert_eq!(projection.occupied_tokens(), Some(299_468));
    let mut live = LiveOccupancy::from_projection(&projection);
    assert_eq!(live.tokens(), projection.occupied_tokens());
    live.appended(4_000);
    assert_eq!(live.tokens(), Some(300_468));
    live.round_started();
    assert_eq!(live.tokens(), Some(300_468));
    // The provider counted the request just sent; streamed text after
    // it is the only estimated suffix.
    live.provider_input(310_000);
    assert_eq!(live.tokens(), Some(310_000));
    live.appended(400);
    assert_eq!(live.tokens(), Some(310_100));
    live.provider_input(0);
    assert_eq!(live.tokens(), Some(310_100), "zero is not a report");
    live.round_started();
    live.compacted(Some(40_000), 90_000);
    assert_eq!(live.tokens(), Some(40_000));
    assert!(!live.anchored());
    live.provider_input(12_000);
    assert_eq!(live.tokens(), Some(12_000));
    assert!(live.anchored());

    let mut unavailable = LiveOccupancy::from_projection(&ContextProjection::unavailable());
    assert_eq!(unavailable.tokens(), None);
}

/// A long turn's rounds each publish a new request whose held projection
/// still carries the turn-start anchor. The meter must stay on the newest
/// provider count plus the estimated growth since, and never fall back to
/// the older count between a round's request and its usage report.
#[test]
fn live_occupancy_is_monotone_across_rounds_of_one_turn() {
    let parts = plakous::model::ContextInputParts::measure_wire(br#"{"input":"x"}"#).unwrap();
    let base = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(900_000),
        1_000_000,
        1_000_000,
        Some(parts),
    );
    let turn_start_anchor = plakous::model::ContextAnchor::new(700_000, 700_000);
    let mut live = LiveOccupancy::from_projection(
        &base.with_occupancy_measure(Some(700_000), turn_start_anchor),
    );
    live.round_started();
    assert_eq!(live.tokens(), Some(700_000));
    let mut readings = Vec::new();
    for round in 1..=4_u64 {
        // The provider counts 5k more than `bytes/4` saw for each round.
        live.provider_input(700_000 + round * 15_000);
        readings.push(live.tokens().unwrap());
        live.appended(4_000);
        readings.push(live.tokens().unwrap());
        live = live.next_request(
            &base.with_occupancy_measure(Some(700_000 + round * 10_000), turn_start_anchor),
        );
        readings.push(live.tokens().unwrap());
        live.round_started();
        readings.push(live.tokens().unwrap());
    }
    assert!(
        readings.windows(2).all(|pair| pair[0] <= pair[1]),
        "meter fell within the turn: {readings:?}"
    );
    // The last request reads the newest count plus its estimated growth.
    assert_eq!(readings.last().copied(), Some(760_000 + 10_000));
}

/// A bound is not on the parts scale, so a provider count never anchors
/// it: the HUD reads the bound plus the streamed suffix, unanchored, both
/// for a parts-less projection and for a compaction with no measured
/// successor.
#[test]
fn live_occupancy_never_anchors_a_bound() {
    let projection = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(900_000),
        1_000_000,
        1_000_000,
        None,
    )
    .with_occupied_tokens(Some(900_000));
    let mut live = LiveOccupancy::from_projection(&projection);
    live.round_started();
    live.provider_input(300_000);
    assert!(!live.anchored());
    assert_eq!(live.tokens(), Some(900_000));
    live.appended(400);
    assert_eq!(live.tokens(), Some(900_100));

    live.compacted(None, 50_000);
    live.round_started();
    live.provider_input(20_000);
    assert!(!live.anchored());
    assert_eq!(live.tokens(), Some(50_000));
}

#[test]
fn two_live_usage_updates_refresh_context_before_turn_terminal() {
    let projection = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(8_000),
        100_000,
        100_000,
        None,
    );
    let session = SessionRecord::new(std::env::temp_dir());
    let (sender, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(sender);
    let mut views = Vec::new();
    for input_tokens in [15_000, 35_000] {
        emit_live_context_projection(
            &events,
            &projection,
            session.context_counters(),
            0,
            false,
            Some(input_tokens),
            false,
            &[],
        );
        // Usage is a coalesced view: consume each refresh as the UI does.
        views.extend(receiver.try_iter().filter_map(|event| match event {
            HostEvent::Usage(view) => Some(view),
            _ => None,
        }));
    }
    emit_live_context_projection(
        &events,
        &projection,
        session.context_counters(),
        0,
        true,
        None,
        false,
        &[],
    );
    assert!(receiver.try_iter().next().is_none());
    emit_live_context_projection(
        &events,
        &projection,
        session.context_counters(),
        0,
        true,
        Some(5_000),
        false,
        &[],
    );
    views.extend(receiver.try_iter().filter_map(|event| match event {
        HostEvent::Usage(view) => Some(view),
        _ => None,
    }));
    assert_eq!(
        views.len(),
        3,
        "no zero reading after compact without usage"
    );
    assert_eq!(views[0].breakdown.as_ref().unwrap().used_tokens, 15_000);
    assert_eq!(views[1].breakdown.as_ref().unwrap().used_tokens, 35_000);
    assert_eq!(views[0].display_percent, Some(15));
    assert_eq!(views[1].display_percent, Some(35));
    assert_eq!(views[1].total_window, Some(100_000));
    assert_eq!(views[2].display_percent, Some(5));
    assert_eq!(views[2].breakdown.as_ref().unwrap().used_tokens, 5_000);
    assert_eq!(views[2].breakdown.as_ref().unwrap().categories.len(), 1);
    assert_eq!(views[2].prepared_input, None);
}

#[test]
fn context_breakdown_keeps_envelope_unattributed_under_an_anchored_measure() {
    let body = br#"{"model":"m","instructions":"core prompt\n\nrepo agents","tools":[],"input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
    let parts = plakous::model::ContextInputParts::measure_wire(body).unwrap();
    let enrolled = plakous::prompts::instructions::inlined_instruction_source_bytes(
        "core prompt\n\nrepo agents",
        &["repo agents"],
    );
    let projection = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(20_000),
        200_000,
        200_000,
        Some(parts),
    );
    let estimate = projection.input_estimate_with_enrolled(enrolled).unwrap();
    let occupancy = estimate.occupancy_tokens();
    let projection = projection.with_occupied_tokens(Some(5_000));
    let breakdown =
        context_breakdown(&projection, sessions::ContextCounters::default(), enrolled).unwrap();
    assert_eq!(breakdown.used_tokens, 5_000);
    let unattributed = breakdown
        .categories
        .iter()
        .find(|category| category.label == UNATTRIBUTED_CATEGORY)
        .expect("envelope must occupy as unattributed");
    assert_eq!(unattributed.tokens, estimate.envelope_tokens);
    let observed = breakdown
        .categories
        .iter()
        .find(|category| category.label == PROVIDER_OBSERVED_CATEGORY)
        .expect("provider excess must not fold into envelope");
    assert_eq!(observed.tokens, 5_000 - occupancy);
    let attributed: u64 = breakdown
        .categories
        .iter()
        .map(|category| category.tokens)
        .sum();
    assert_eq!(attributed, breakdown.used_tokens);
}

#[test]
fn prepared_request_updates_context_parts_and_splits_tool_names() {
    let old =
        plakous::model::ContextInputParts::measure_wire(br#"{"tools":[],"input":[]}"#).unwrap();
    let new = plakous::model::ContextInputParts::measure_wire(br#"{"tools":[{"name":"Bash"},{"name":"wait_agent"}],"input":[{"content":"a much newer request body"}]}"#).unwrap();
    let projection = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(20_000),
        200_000,
        200_000,
        Some(old),
    );
    let newer = projection.with_prepared_request(Some(new), Some(500), Some(700));
    let mut breakdown = context_breakdown(&newer, sessions::ContextCounters::default(), 0).unwrap();
    assert_eq!(breakdown.used_tokens, 500);
    assert!(
        breakdown
            .categories
            .iter()
            .any(|category| category.label == "2 tools on the last request; tokens est.")
    );
    append_request_tool_names(
        &mut breakdown,
        &["Bash".to_owned(), "wait_agent".to_owned()],
    );
    assert_eq!(
        breakdown
            .informational
            .iter()
            .find(|row| row.label.starts_with("Coding tools"))
            .unwrap()
            .detail
            .as_deref(),
        Some("Bash")
    );
    assert_eq!(
        breakdown
            .informational
            .iter()
            .find(|row| row.label.starts_with("Agent tools"))
            .unwrap()
            .detail
            .as_deref(),
        Some("wait_agent")
    );
}

#[test]
fn an_idle_context_projection_names_the_last_prepared_tools() {
    let body = br#"{"tools":[{"type":"function","name":"code"}],"input":[]}"#;
    let parts = plakous::model::ContextInputParts::measure_wire(body).unwrap();
    let projection = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(20_000),
        200_000,
        200_000,
        Some(parts),
    );
    let session = SessionRecord::new(std::env::temp_dir());
    let (sender, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(sender);
    events.remember_request_tools(&["Bash".to_owned(), "wait_agent".to_owned()]);
    emit_context_projection(&events, &projection, session.context_counters(), 0);
    let named = usage_breakdown(receiver.try_iter());
    assert_eq!(info_detail(&named, "Coding tools").as_deref(), Some("Bash"));
    assert_eq!(
        info_detail(&named, "Agent tools").as_deref(),
        Some("wait_agent")
    );

    assert!(events.send(ContentEvent::ClearView));
    emit_context_projection(&events, &projection, session.context_counters(), 0);
    let cleared = usage_breakdown(receiver.try_iter());
    assert!(
        cleared.informational.iter().all(|row| {
            !row.label.starts_with("Coding tools") && !row.label.starts_with("Agent tools")
        }),
        "{cleared:?}"
    );
}

fn usage_breakdown(
    events: impl IntoIterator<Item = HostEvent>,
) -> plakous_host_protocol::views::presentation::ContextBreakdownView {
    events
        .into_iter()
        .find_map(|event| match event {
            HostEvent::Usage(view) => view.breakdown,
            _ => None,
        })
        .expect("context usage")
}

fn info_detail(
    breakdown: &plakous_host_protocol::views::presentation::ContextBreakdownView,
    label: &str,
) -> Option<String> {
    breakdown
        .informational
        .iter()
        .find(|row| row.label.starts_with(label))
        .and_then(|row| row.detail.clone())
}

#[test]
fn tool_count_is_exact_and_only_tokens_are_estimated() {
    let body = br#"{"tools":[{"type":"function","name":"code"},{"type":"function","name":"wait_agent"}],"input":[]}"#;
    let parts = plakous::model::ContextInputParts::measure_wire(body).unwrap();
    let projection = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(20_000),
        200_000,
        200_000,
        Some(parts),
    );
    let breakdown =
        context_breakdown(&projection, sessions::ContextCounters::default(), 0).unwrap();
    assert!(
        breakdown
            .categories
            .iter()
            .any(|category| category.label == "2 tools on the last request; tokens est.")
    );
}

#[test]
fn grok_envelope_names_an_eighty_percent_compact_fence() {
    let projection = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(223_000),
        500_000,
        500_000,
        None,
    )
    .with_auto_compact_input(Some(400_000));
    let breakdown =
        context_breakdown(&projection, sessions::ContextCounters::default(), 0).unwrap();
    assert_eq!(breakdown.auto_compact_trigger_tokens, Some(400_000));
    assert!(!breakdown.occupancy_anchored);
    let anchored = context_breakdown(
        &projection.with_anchored_occupied_tokens(450_000),
        sessions::ContextCounters::default(),
        0,
    )
    .unwrap();
    assert!(anchored.occupancy_anchored);
}

#[test]
fn display_percent_is_the_compact_gauge_and_not_the_bound() {
    let body = br#"{"model":"m","instructions":"x","tools":[],"input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
    let parts = plakous::model::ContextInputParts::measure_wire(body).unwrap();
    let projection = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(20_000),
        200_000,
        200_000,
        Some(parts),
    );
    let usage = context_usage(&projection, sessions::ContextCounters::default(), 0);
    let gauge = projection.actionable_utilization().unwrap().gauge_percent();
    assert_eq!(
        usage.display_percent,
        Some(u64::from(gauge)),
        "the composer and the compact gauge read one occupancy"
    );
    assert_eq!(
        usage.breakdown.unwrap().used_tokens,
        parts.occupancy_tokens(),
        "occupancy is the request's bytes÷4, not the admission bound"
    );
}

#[test]
fn attached_one_shot_usage_reports_context_without_subscription_refresh() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.attached_one_shot = true;
    let events = run_attached_actions(
        config,
        vec![UserAction::Extension(ProbeIntent::InspectUsage)],
    );
    let statuses = usage_statuses(&events);
    assert!(
        !statuses.is_empty() && statuses.iter().all(|status| *status == "fits"),
        "attached usage must report fits, got {statuses:?}"
    );
    assert!(events.iter().all(|event| !matches!(
        event,
        HostEvent::SubscriptionUsage(_) | HostEvent::Extension(ProbeObservation::UsageObserved)
    )));
}

#[test]
fn subscription_refresh_coalesces_bursts_into_one_in_flight_read() {
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = Arc::clone(&calls);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
        let call = worker_calls.fetch_add(1, Ordering::AcqRel);
        if call == 0 {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }
        (
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable {
                reason: format!("test observation {call}"),
            },
            None,
        )
    });

    refresh.request(None, None);
    // A liveness bound only: generous, so load never trips it.
    started_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    for _ in 0..20 {
        refresh.request(None, None);
    }
    release_tx.send(()).unwrap();
    drop(refresh);

    assert_eq!(calls.load(Ordering::Acquire), 1);
    let terminal = rx
        .try_iter()
        .filter(|event| {
            matches!(
                event,
                HostEvent::SubscriptionUsage(
                    plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable { .. }
                )
            )
        })
        .count();
    assert_eq!(
        terminal, 1,
        "latest terminal observation replaces older state"
    );
}

#[test]
fn explicit_post_credential_read_is_queued_behind_an_older_read() {
    let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = Arc::clone(&calls);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
        if worker_calls.fetch_add(1, Ordering::AcqRel) == 0 {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }
        (
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
            None,
        )
    });
    refresh.request(None, None);
    // A liveness bound only: generous, so load never trips it.
    started_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    for _ in 0..20 {
        refresh.request_fresh(None, None);
    }
    release_tx.send(()).unwrap();
    drop(refresh);
    assert_eq!(calls.load(Ordering::Acquire), 2);
}

/// A surface reading subscription posture waits out the running read and
/// the explicit follow-up queued behind it, so it never holds a
/// compatibility lock the read only try-locks.
#[test]
fn usage_reads_settle_only_after_the_running_and_queued_reads() {
    let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(2);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(2);
    let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        (
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
            None,
        )
    });
    let reads = refresh.reads();
    assert!(reads.wait_settled(Duration::ZERO), "nothing was requested");
    refresh.request(None, None);
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    refresh.request_fresh(None, None);
    refresh.request(None, None);
    assert!(!reads.wait_settled(Duration::from_millis(20)));
    release_tx.send(()).unwrap();
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        !reads.wait_settled(Duration::from_millis(20)),
        "the queued follow-up read is still running"
    );
    release_tx.send(()).unwrap();
    assert!(reads.wait_settled(Duration::from_secs(5)));
}

#[test]
fn idle_usage_serves_a_fresh_held_projection_without_another_read() {
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = Arc::clone(&calls);
    let held = HeldUsage::default();
    let ledger = SessionLedger::default();
    let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
        worker_calls.fetch_add(1, Ordering::AcqRel);
        (
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
            Some(crate::subscription_usage::project_limits(
                [],
                plakous_host_protocol::views::presentation::SessionLedgerView::default(),
            )),
        )
    });
    let mut view = crate::subscription_usage::project_limits(
        [],
        plakous_host_protocol::views::presentation::SessionLedgerView::default(),
    );
    view.verticals[2].state =
        plakous_host_protocol::views::presentation::VerticalUsageState::Ready {
            meters: Vec::new(),
        };
    held.store(&view);

    request_subscription_usage_cached(&held, &ledger, &events, &refresh, None, None);
    let first: Vec<_> = rx.try_iter().collect();
    request_subscription_usage_cached(&held, &ledger, &events, &refresh, None, None);
    let second: Vec<_> = rx.try_iter().collect();
    thread::sleep(Duration::from_millis(50));
    drop(refresh);

    assert_eq!(
        calls.load(Ordering::Acquire),
        0,
        "a fresh held projection must not start another provider read"
    );
    for batch in [&first, &second] {
        let served = batch.iter().find_map(|event| match event {
            HostEvent::UsageLimits(view) => Some(view),
            _ => None,
        });
        let Some(served) = served else {
            panic!("cached projection is published as UsageLimits: {batch:?}");
        };
        assert_eq!(served.stale_age_seconds, None);
        assert!(matches!(
            served.verticals[2].state,
            plakous_host_protocol::views::presentation::VerticalUsageState::Ready { .. }
        ));
        assert!(batch.iter().all(|event| !matches!(
            event,
            HostEvent::SubscriptionUsage(
                plakous_host_protocol::views::presentation::SubscriptionUsageView::Loading
            )
        )));
        assert!(batch.iter().any(|event| matches!(
            event,
            HostEvent::SubscriptionUsage(
                plakous_host_protocol::views::presentation::SubscriptionUsageView::Read
            )
        )));
    }
}

#[test]
fn idle_usage_reads_when_nothing_is_held() {
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = Arc::clone(&calls);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let held = HeldUsage::default();
    let ledger = SessionLedger::default();
    let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
        worker_calls.fetch_add(1, Ordering::AcqRel);
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        (
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
            Some(crate::subscription_usage::project_limits(
                [],
                plakous_host_protocol::views::presentation::SessionLedgerView::default(),
            )),
        )
    });

    request_subscription_usage_cached(&held, &ledger, &events, &refresh, None, None);
    // A liveness bound only: generous, so load never trips it.
    started_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let during: Vec<_> = rx.try_iter().collect();
    release_tx.send(()).unwrap();
    drop(refresh);

    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert!(
        during.iter().any(|event| matches!(
            event,
            HostEvent::SubscriptionUsage(
                plakous_host_protocol::views::presentation::SubscriptionUsageView::Loading
            )
        )),
        "a cache miss must start a provider read: {during:?}"
    );
}

#[test]
fn a_rate_limited_claude_vertical_is_stored_as_the_last_ready_meters() {
    let held = HeldUsage::default();
    let mut ready = crate::subscription_usage::project_limits(
        [],
        plakous_host_protocol::views::presentation::SessionLedgerView::default(),
    );
    ready.verticals[2].observed_at = Some("2026-09-04 12:00".to_owned());
    ready.verticals[2].state =
        plakous_host_protocol::views::presentation::VerticalUsageState::Ready {
            meters: Vec::new(),
        };
    held.store(&ready);
    let observed = Instant::now()
        .checked_sub(Duration::from_secs(120))
        .unwrap();
    held.inner.lock().unwrap().as_mut().unwrap().1 = observed;

    let mut incoming = crate::subscription_usage::project_limits(
        [],
        plakous_host_protocol::views::presentation::SessionLedgerView::default(),
    );
    incoming.verticals[2].state =
        plakous_host_protocol::views::presentation::VerticalUsageState::Unavailable {
            reason: crate::subscription_usage::USAGE_RATE_LIMITED_REASON.to_owned(),
        };
    let merged = held.preserve_and_store(incoming);
    assert_eq!(merged.verticals[2], ready.verticals[2]);
    assert!(held.fresh().is_none(), "a 429 must not renew freshness");
    assert_eq!(held.inner.lock().unwrap().as_ref().unwrap().1, observed);
}

#[test]
fn a_held_busy_projection_starts_another_read() {
    let (tx, rx) = crate::surface_protocol::host_event_channel();
    let events = Sender::over(tx);
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = Arc::clone(&calls);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let held = HeldUsage::default();
    let ledger = SessionLedger::default();
    let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
        worker_calls.fetch_add(1, Ordering::AcqRel);
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        (
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
            Some(plakous_host_protocol::views::presentation::UsageLimitsView::default()),
        )
    });
    let mut view = crate::subscription_usage::project_limits(
        [],
        plakous_host_protocol::views::presentation::SessionLedgerView::default(),
    );
    for vertical in &mut view.verticals {
        vertical.state =
            plakous_host_protocol::views::presentation::VerticalUsageState::Unavailable {
                reason: crate::subscription_usage::USAGE_BUSY_REASON.to_owned(),
            };
    }
    held.store(&view);

    request_subscription_usage_cached(&held, &ledger, &events, &refresh, None, None);
    // A liveness bound only: generous, so load never trips it.
    started_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    release_tx.send(()).unwrap();
    drop(refresh);
    let _ = rx.try_iter().count();
    assert_eq!(
        calls.load(Ordering::Acquire),
        1,
        "a Busy-only cache must not block the next idle Usage open"
    );
}

#[test]
fn busy_usage_reads_are_retried_until_the_lock_clears() {
    let executor = ApplicationExecutor::new().unwrap();
    let attempts = AtomicUsize::new(0);
    let Ok(read) = executor.block_on(await_usage_not_busy(|| {
        let n = attempts.fetch_add(1, Ordering::SeqCst);
        async move {
            if n < 2 {
                plakous::model::SubscriptionUsageRead::Unavailable {
                    observed_at: plakous::model::Timestamp::new(0, 0)
                        .unwrap_or_else(|_| unreachable!("zero is a valid timestamp")),
                    reason: plakous::model::SubscriptionUsageUnavailable::Busy,
                }
            } else {
                plakous::model::SubscriptionUsageRead::Unavailable {
                    observed_at: plakous::model::Timestamp::new(0, 0)
                        .unwrap_or_else(|_| unreachable!("zero is a valid timestamp")),
                    reason: plakous::model::SubscriptionUsageUnavailable::LocalState,
                }
            }
        }
    })) else {
        panic!("executor is live");
    };
    assert!(matches!(
        read,
        plakous::model::SubscriptionUsageRead::Unavailable {
            reason: plakous::model::SubscriptionUsageUnavailable::LocalState,
            ..
        }
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[test]
fn busy_store_fence_read_is_retried_after_writer_lease_clears()
-> Result<(), Box<dyn std::error::Error>> {
    use plakous::auth::{
        AccountBinding, CredentialGeneration, CredentialRecord, CredentialSlotId, CredentialStatus,
        PrivateCredentialStore, SecretBytes, SubscriptionIntegrationProfileId,
    };
    use plakous::model::{SubscriptionUsageRead, SubscriptionUsageUnavailable, Timestamp};
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir()?;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    let profile = SubscriptionIntegrationProfileId::new("usage-test-profile")?;
    let account = AccountBinding::new("usage-test-account")?;
    let store = PrivateCredentialStore::open_slot(
        directory.path().join("credential.json"),
        profile.clone(),
        CredentialSlotId::new("usage-test-slot")?,
    )?;
    store.initialize_selected(&CredentialRecord::new(
        profile,
        account,
        CredentialGeneration::new(1)?,
        CredentialStatus::Ready,
        SecretBytes::new(b"offline-fixture".to_vec()),
    )?)?;
    assert!(store.try_load()?.is_some());
    let file = std::fs::File::open(store.lock_path())?;
    file.lock()?;
    let mut writer = Some(file);
    let mut attempts = 0;
    let executor = ApplicationExecutor::new()?;
    let read = executor.block_on(await_usage_not_busy(|| {
        attempts += 1;
        let reason = match store.try_load() {
            Err(_) if writer.is_some() => SubscriptionUsageUnavailable::Busy,
            Ok(Some(fence)) => {
                assert_eq!(fence.status(), CredentialStatus::Ready);
                // The next control-plane step, not a local-state refusal.
                SubscriptionUsageUnavailable::ProviderUnavailable
            }
            other => panic!("unexpected fence read: {other:?}"),
        };
        if attempts == 2 {
            drop(writer.take());
        }
        async move {
            SubscriptionUsageRead::Unavailable {
                observed_at: Timestamp::new(0, 0).unwrap(),
                reason,
            }
        }
    }))?;
    assert!(matches!(
        read,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::ProviderUnavailable,
            ..
        }
    ));
    assert_eq!(attempts, 3);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn subscription_refresh_timeout_has_a_terminal_outcome() {
    let started = tokio::time::Instant::now();
    let outcome = bounded_subscription_refresh(
        tokio::time::Instant::now() + Duration::from_millis(20),
        std::future::pending::<()>(),
    )
    .await;
    assert_eq!(outcome, None);
    assert_eq!(started.elapsed(), Duration::from_millis(20));
}

fn native_usage_fixture(meter: &str, used: f64) -> plakous::model::SubscriptionUsageRead {
    use plakous::model::{
        SubscriptionUsageMeter, SubscriptionUsagePercent, SubscriptionUsagePeriod,
        SubscriptionUsageRead, SubscriptionUsageSnapshot, SubscriptionUsageWindow, Timestamp,
    };
    SubscriptionUsageRead::Snapshot(
        SubscriptionUsageSnapshot::new(
            Timestamp::new(1_788_000_000, 0).unwrap(),
            None,
            vec![
                SubscriptionUsageMeter::new(
                    meter.to_owned(),
                    None,
                    vec![
                        SubscriptionUsageWindow::new(
                            Some("primary".to_owned()),
                            SubscriptionUsagePercent::new(used).unwrap(),
                            Some(SubscriptionUsagePeriod::Rolling {
                                duration_seconds: 18_000,
                            }),
                            None,
                        )
                        .unwrap(),
                    ],
                )
                .unwrap(),
            ],
        )
        .unwrap(),
    )
}

fn native_usage_limits(
    refresh: NativeSubscriptionRefresh,
) -> plakous_host_protocol::views::presentation::UsageLimitsView {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    project_usage_limits(
        refresh,
        config.state_roots.openrouter_credentials().as_ref(),
    )
}

fn assert_native_percent(
    view: &plakous_host_protocol::views::presentation::UsageLimitsView,
    index: usize,
    expected: &str,
) {
    use plakous_host_protocol::views::presentation::{UsageMeterReading, VerticalUsageState};
    let VerticalUsageState::Ready { meters, .. } = &view.verticals[index].state else {
        panic!("expected ready provider: {:?}", view.verticals[index]);
    };
    assert!(
        matches!(&meters[0].reading, UsageMeterReading::Percent { exact_percent, .. } if exact_percent == expected)
    );
}

#[test]
fn one_native_provider_timeout_keeps_both_siblings_fresh() {
    use plakous::model::SubscriptionUsageRead;
    use plakous_host_protocol::views::presentation::VerticalUsageState;
    let executor = ApplicationExecutor::new().unwrap();
    // Ready siblings are deterministic; the first provider never completes.
    let refresh = executor
        .block_on(concurrent_native_subscription_refresh(
            Duration::from_millis(20),
            std::future::pending::<SubscriptionUsageRead>(),
            std::future::ready(native_usage_fixture("included-credits", 46.4)),
            std::future::ready(native_usage_fixture("claude", 12.5)),
        ))
        .unwrap();
    let view = native_usage_limits(refresh);
    assert!(
        matches!(&view.verticals[0].state, VerticalUsageState::Unavailable { reason }
        if reason == SUBSCRIPTION_USAGE_TIMEOUT_REASON)
    );
    assert_native_percent(&view, 1, "46.4");
    assert_native_percent(&view, 2, "12.5");
    let text = crate::subscription_usage::render_limits_text(&view);
    assert_eq!(text.matches(SUBSCRIPTION_USAGE_TIMEOUT_REASON).count(), 1);
    assert!(text.contains("SuperGrok") && text.contains("Claude"));
}

#[test]
fn one_native_provider_busy_then_ready_is_retried_without_losing_siblings() {
    use plakous::model::{SubscriptionUsageRead, SubscriptionUsageUnavailable, Timestamp};
    let executor = ApplicationExecutor::new().unwrap();
    let mut attempts = 0;
    let read = await_usage_not_busy(|| {
        attempts += 1;
        std::future::ready(if attempts == 1 {
            SubscriptionUsageRead::Unavailable {
                observed_at: Timestamp::new(0, 0).unwrap(),
                reason: SubscriptionUsageUnavailable::Busy,
            }
        } else {
            native_usage_fixture("codex", 22.9)
        })
    });
    let refresh = executor
        .block_on(concurrent_native_subscription_refresh(
            Duration::from_secs(5),
            read,
            std::future::ready(native_usage_fixture("included-credits", 46.4)),
            std::future::ready(native_usage_fixture("claude", 12.5)),
        ))
        .unwrap();
    assert_eq!(attempts, 2);
    let view = native_usage_limits(refresh);
    for (index, percent) in ["22.9", "46.4", "12.5"].into_iter().enumerate() {
        assert_native_percent(&view, index, percent);
    }
}

#[test]
fn concurrent_native_success_preserves_the_existing_usage_projection() {
    use crate::subscription_usage::{
        VerticalObservation, claude_vertical, codex_vertical, openrouter_vertical,
        supergrok_vertical,
    };
    let executor = ApplicationExecutor::new().unwrap();
    let codex = native_usage_fixture("codex", 22.9);
    let grok = native_usage_fixture("included-credits", 46.4);
    let claude = native_usage_fixture("claude", 12.5);
    let refresh = executor
        .block_on(concurrent_native_subscription_refresh(
            Duration::from_secs(5),
            std::future::ready(codex.clone()),
            std::future::ready(grok.clone()),
            std::future::ready(claude.clone()),
        ))
        .unwrap();
    let expected = crate::subscription_usage::project_limits(
        [
            (codex_vertical(), VerticalObservation::Native(codex)),
            (supergrok_vertical(), VerticalObservation::Native(grok)),
            (claude_vertical(), VerticalObservation::Native(claude)),
            (openrouter_vertical(), VerticalObservation::NotConnected),
        ],
        plakous_host_protocol::views::presentation::SessionLedgerView::default(),
    );
    let view = native_usage_limits(refresh);
    assert_eq!(view, expected);
    assert_eq!(
        crate::subscription_usage::render_limits_text(&view),
        crate::subscription_usage::render_limits_text(&expected)
    );
}

/// Quit must never wait out an in-flight subscription-usage read: the
/// worker owns no durable state, so teardown is bounded and detaches.
#[test]
fn dropping_the_usage_refresh_is_bounded_even_while_a_read_is_running() {
    use std::sync::mpsc::channel;

    let (entered, running) = channel();
    let (release, held) = channel();
    let (finished, settled) = channel();
    let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
        entered.send(()).unwrap();
        let _ = held.recv();
        finished.send(()).unwrap();
        (
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Unavailable {
                reason: "stuck".to_owned(),
            },
            None,
        )
    });
    refresh.request(None, None);
    running.recv_timeout(Duration::from_secs(30)).unwrap();
    let (dropped, detached) = channel();
    let teardown = thread::spawn(move || {
        drop(refresh);
        dropped.send(()).unwrap();
    });
    // Drop must finish while the projector is still held, not after its read.
    let result = detached.recv_timeout(Duration::from_secs(30));
    release.send(()).unwrap();
    settled.recv_timeout(Duration::from_secs(30)).unwrap();
    teardown.join().unwrap();
    result.expect("teardown joined a read that had not been released");
}

#[test]
fn round_boundary_estimate_advances_without_becoming_provider_usage() {
    let projection = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(8_000),
        100_000,
        100_000,
        None,
    );
    let session = SessionRecord::new(std::env::temp_dir());
    let (sender, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
    emit_live_context_projection(
        &Sender::over(sender),
        &projection,
        session.context_counters(),
        0,
        false,
        Some(12_000),
        false,
        &[],
    );

    let HostEvent::Usage(view) = receiver.try_iter().next().unwrap() else {
        panic!("expected context usage");
    };
    assert_eq!(view.display_percent, Some(12));
    let breakdown = view.breakdown.unwrap();
    assert_eq!(breakdown.used_tokens, 12_000);
    assert!(breakdown.categories.iter().any(|category| {
        category.label == "Live turn growth (est.)" && category.tokens == 4_000
    }));
}

#[test]
fn post_compact_growth_does_not_freeze_at_an_earlier_provider_reading() {
    let projection = ContextProjection::test_fits(
        crate::context::PreparedInputCount::UpperBound(8_000),
        100_000,
        100_000,
        None,
    );
    let session = SessionRecord::new(std::env::temp_dir());
    let (sender, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
    emit_live_context_projection(
        &Sender::over(sender),
        &projection,
        session.context_counters(),
        0,
        true,
        Some(13_000),
        false,
        &[],
    );
    let HostEvent::Usage(view) = receiver.try_iter().next().unwrap() else {
        panic!("expected context usage");
    };
    assert_eq!(view.display_percent, Some(13));
    let breakdown = view.breakdown.unwrap();
    assert_eq!(breakdown.used_tokens, 13_000);
    assert_eq!(breakdown.categories[0].label, "Live turn growth (est.)");
}

/// Idle context after a completed turn still names the tools the turn prepared,
/// and a model rebind clears them. The idle projection reads the names the
/// turn drive remembered; the rebind reports context only after it drops them.
#[test]
fn idle_context_names_prepared_tools_until_a_model_rebind() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let workspace = config.cwd.clone();
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        root.path().join("rebind-tools-evidence.jsonl"),
    )
    .unwrap();
    let injection = test_kit::scripted_mut(&mut config);
    injection.rebind_target = plakous_offline_crash_harness::runtime_binding_for_account(
        &workspace,
        controller.clone(),
        b"successor-account",
    )
    .unwrap()
    .target_binding();
    let primary = injection.runtime.clone();
    injection.runtime = std::sync::Arc::new(move |role| {
        if role == RuntimeRole::Rebind {
            plakous_offline_crash_harness::runtime_binding_for_account(
                &workspace,
                controller.clone(),
                b"successor-account",
            )
            .map_err(|error| error.to_string())
        } else {
            primary(role)
        }
    });
    config.ready_catalog = plakous_host_protocol::views::ReadyCatalog::new(vec![
        plakous_host_protocol::views::ProviderSection {
            account: "codex".into(),
            label: "Codex".into(),
            ready: true,
            entries: vec![plakous_host_protocol::views::ModelEntry {
                id: "offline-loopback-v2".into(),
                tag: "offline".into(),
                reasoning: true,
                effort_options: vec!["medium".into()],
                extras: Vec::new(),
                unavailable_reason: None,
            }],
        },
    ]);
    let surface = Arc::new(IdleToolFork::default());
    config.surface = Some(Box::new(Arc::clone(&surface)));
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    send_attached_action(&actions, UserAction::Prompt("hello".into()));
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    push_until(&events, &mut seen, deadline, turn_returned_to_idle);
    let before_idle = seen.len();
    send_attached_action(&actions, UserAction::Extension(ProbeIntent::InspectUsage));
    push_until(&events, &mut seen, deadline, |events| {
        seen_extensions_since(events, before_idle) >= 1
    });
    let idle = last_tool_details(&seen[before_idle..]).unwrap_or_else(|| {
        panic!(
            "idle context projection missing: {:?}",
            &seen[before_idle..]
        )
    });
    assert!(
        !idle.is_empty(),
        "idle context should name the prepared tools: {idle:?}"
    );

    surface.fork.store(true, Ordering::SeqCst);
    let before_rebind = seen.len();
    send_attached_action(&actions, UserAction::Extension(ProbeIntent::InspectUsage));
    push_until(&events, &mut seen, deadline, |events| {
        let outcome = surface.outcome.lock().unwrap().clone();
        if let Some(outcome) = outcome.as_deref()
            && outcome != "forked"
        {
            panic!("model rebind did not switch: {outcome}");
        }
        let Some(at) = rebind_status_at(events, before_rebind) else {
            return false;
        };
        outcome.as_deref() == Some("forked") && first_tool_details(&events[at + 1..]).is_some()
    });
    let at = rebind_status_at(&seen, before_rebind)
        .unwrap_or_else(|| panic!("rebind status missing: {:?}", &seen[before_rebind..]));
    let cleared = first_tool_details(&seen[at + 1..])
        .unwrap_or_else(|| panic!("rebind context projection missing: {:?}", &seen[at + 1..]));
    assert!(
        cleared.is_empty(),
        "a model rebind drops the last request's tool names: {cleared:?}"
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

#[test]
fn usage_watch_is_due_only_after_the_freshness_interval() {
    let now = Instant::now();
    assert!(usage_watch_due(None, now));
    assert!(!usage_watch_due(Some(now), now));
    let started = now
        .checked_sub(crate::subscription_usage::USAGE_PROJECTION_FRESHNESS)
        .unwrap();
    assert!(usage_watch_due(Some(started), now));
    let earlier = now.checked_sub(Duration::from_secs(59)).unwrap();
    assert!(!usage_watch_due(Some(earlier), now));
}

#[test]
fn busy_watch_rereads_only_after_the_freshness_interval() {
    let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = Arc::clone(&calls);
    let refresh = SubscriptionUsageRefresh::new_with_projector(&events, move |_| {
        worker_calls.fetch_add(1, Ordering::AcqRel);
        (
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
            None,
        )
    });
    refresh.poll_while_watched(None, None);
    let deadline = Instant::now() + Duration::from_secs(2);
    while calls.load(Ordering::Acquire) < 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(30));
    refresh.poll_while_watched(None, None);
    thread::sleep(Duration::from_millis(30));
    assert_eq!(calls.load(Ordering::Acquire), 1);
    refresh.backdate_last_request(crate::subscription_usage::USAGE_PROJECTION_FRESHNESS);
    refresh.poll_while_watched(None, None);
    let deadline = Instant::now() + Duration::from_secs(2);
    while calls.load(Ordering::Acquire) < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(calls.load(Ordering::Acquire), 2);
    drop(refresh);
}

/// A busy re-read publishes the next observation without a loading notice, so
/// the figures already on screen stay up.
#[test]
fn busy_watch_does_not_announce_loading() {
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let refresh = SubscriptionUsageRefresh::new_with_projector(&events, |_| {
        (
            plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
            None,
        )
    });
    refresh.poll_while_watched(None, None);
    let mut saw_loading = false;
    let mut saw_read = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !saw_read {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(20)) {
            match event {
                HostEvent::SubscriptionUsage(
                    plakous_host_protocol::views::presentation::SubscriptionUsageView::Loading,
                ) => saw_loading = true,
                HostEvent::SubscriptionUsage(
                    plakous_host_protocol::views::presentation::SubscriptionUsageView::Read,
                ) => saw_read = true,
                _ => {}
            }
        }
    }
    assert!(saw_read);
    assert!(!saw_loading);
    drop(refresh);
}

/// The turn loop asks for allowances only while the surface says the Usage tab
/// is open. Several ticks with the watch closed read nothing; opening it reads
/// once. Replacing the gate with `if true` reads during the closed window.
#[test]
fn a_busy_turn_rereads_allowances_only_while_the_usage_tab_is_open() {
    use crate::runtime::recover_tests::{scripted_host_with, wait_for};
    use crate::runtime::test_support::send_attached_action;
    use crate::runtime::{SurfaceEffect, SurfaceFacts, SurfaceHandler, spawn_host_worker};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    struct WatchProbe {
        armed: Arc<AtomicBool>,
    }
    impl SurfaceHandler<TestExtension> for WatchProbe {
        fn usage_watch_armed(&self) -> bool {
            self.armed.load(Ordering::Acquire)
        }
        fn dispatch(&self, _: ProbeIntent, _: SurfaceFacts<'_>, _: &Sender) -> SurfaceEffect {
            SurfaceEffect::default()
        }
        fn publish_template_readiness(
            &self,
            _: &[plakous_host_protocol::views::ProviderSection],
            _: &Sender,
        ) {
        }
        fn readiness_read(&self) -> ReadinessRead {
            Box::new(|| Ok((Vec::new(), Vec::new())))
        }
    }

    let root = tempfile::tempdir().unwrap();
    let plan = serde_json::json!({
        "version": 1,
        "steps": [{"stream": [{"text": "busy prefix"}, {"stall": 30000}]}]
    });
    let plan: Arc<plakous_offline_crash_harness::script::ScriptPlan> =
        Arc::new(serde_json::from_value(plan).unwrap());
    let (mut config, _) = scripted_host_with(root.path(), plan);
    let armed = Arc::new(AtomicBool::new(false));
    config.surface = Some(Box::new(WatchProbe {
        armed: Arc::clone(&armed),
    }));
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("start".into()));
    wait_for(
        &events,
        &mut seen,
        "the stalled turn's prefix",
        |event| matches!(event, HostEvent::AssistantDelta(text) if text.contains("busy prefix")),
    );
    let before = seen.iter().filter(|event| is_allowance_read(event)).count();
    thread::sleep(Duration::from_millis(250));
    let quiet_until = Instant::now() + Duration::from_millis(40);
    while Instant::now() < quiet_until {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(10)) {
            seen.push(event);
        }
    }
    let closed = seen.iter().filter(|event| is_allowance_read(event)).count() - before;
    assert_eq!(closed, 0, "a closed Usage tab read allowances: {seen:#?}");

    armed.store(true, Ordering::Release);
    let open_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < open_deadline
        && seen.iter().filter(|event| is_allowance_read(event)).count() - before == 0
    {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(20)) {
            seen.push(event);
        }
    }
    let opened = seen.iter().filter(|event| is_allowance_read(event)).count() - before;
    assert_eq!(opened, 1, "opening Usage should read once: {seen:#?}");
    thread::sleep(Duration::from_millis(120));
    let settle = Instant::now() + Duration::from_millis(40);
    while Instant::now() < settle {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(10)) {
            seen.push(event);
        }
    }
    let afterwards = seen.iter().filter(|event| is_allowance_read(event)).count() - before;
    assert_eq!(afterwards, 1, "the fresh read was repeated: {seen:#?}");
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

fn rebind_status_at(events: &[HostEvent], from: usize) -> Option<usize> {
    events[from..]
        .iter()
        .position(|event| {
            matches!(
                event,
                HostEvent::Status {
                    phase: HostPhase::Rebind,
                    ..
                }
            )
        })
        .map(|index| from + index)
}

fn turn_returned_to_idle(events: &[HostEvent]) -> bool {
    let Some(completed_at) = events.iter().rposition(|event| {
        matches!(
            event,
            HostEvent::TurnLifecycle(lifecycle)
                if lifecycle.phase == plakous_host_protocol::TurnPhase::Completed
        )
    }) else {
        return false;
    };
    events[completed_at..].iter().any(|event| {
        matches!(
            event,
            HostEvent::Status {
                phase: HostPhase::Idle,
                ..
            }
        )
    })
}

fn seen_extensions_since(events: &[HostEvent], from: usize) -> usize {
    events[from..]
        .iter()
        .filter(|event| matches!(event, HostEvent::Extension(ProbeObservation::UsageObserved)))
        .count()
}

fn first_tool_details(events: &[HostEvent]) -> Option<Vec<String>> {
    events.iter().find_map(tool_detail_of)
}

fn last_tool_details(events: &[HostEvent]) -> Option<Vec<String>> {
    events.iter().rev().find_map(tool_detail_of)
}

fn tool_detail_of(event: &HostEvent) -> Option<Vec<String>> {
    let HostEvent::Usage(view) = event else {
        return None;
    };
    let breakdown = view.breakdown.as_ref()?;
    Some(
        breakdown
            .informational
            .iter()
            .filter(|row| {
                row.label.starts_with("Coding tools") || row.label.starts_with("Agent tools")
            })
            .filter_map(|row| row.detail.clone())
            .collect(),
    )
}

fn push_until(
    events: &crate::surface_protocol::HostEventReceiver<TestExtension>,
    seen: &mut Vec<HostEvent>,
    deadline: Instant,
    mut done: impl FnMut(&[HostEvent]) -> bool,
) {
    if done(seen) {
        return;
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "host event timed out; last: {:?}",
            seen.last()
        );
        match events.recv_timeout(left) {
            Ok(event) => seen.push(event),
            Err(error) => panic!("host event timed out ({error}); last: {:?}", seen.last()),
        }
        if done(seen) {
            return;
        }
    }
}

#[derive(Default)]
struct IdleToolFork {
    fork: AtomicBool,
    outcome: Mutex<Option<String>>,
}

impl SurfaceHandler<TestExtension> for Arc<IdleToolFork> {
    fn needs_usage(&self, _intent: &ProbeIntent) -> bool {
        true
    }

    fn dispatch(
        &self,
        _intent: ProbeIntent,
        _facts: SurfaceFacts<'_>,
        events: &Sender,
    ) -> SurfaceEffect {
        let _ = events.send(ContentEvent::Extension(ProbeObservation::UsageObserved));
        if !self.fork.load(Ordering::SeqCst) {
            return SurfaceEffect::default();
        }
        let outcome = Arc::clone(self);
        SurfaceEffect {
            profile_fork: Some(SurfaceProfileFork {
                account: "codex".into(),
                model: "offline-loopback-v2".into(),
                effort: Some("medium".into()),
                extra: None,
                composition: None,
                protocol: None,
                roster: crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
                switch: None,
                finish: Box::new(move |result| {
                    let label = match result {
                        ProfileForkOutcome::Forked => "forked".to_owned(),
                        ProfileForkOutcome::Switched => "switched".to_owned(),
                        ProfileForkOutcome::Bound => "bound".to_owned(),
                        ProfileForkOutcome::AlreadyActive => "unchanged".to_owned(),
                        ProfileForkOutcome::Refused(failure) => {
                            format!("refused: {failure}")
                        }
                    };
                    *outcome.outcome.lock().unwrap() = Some(label);
                }),
            }),
            ..SurfaceEffect::default()
        }
    }

    fn publish_template_readiness(
        &self,
        _catalog: &[plakous_host_protocol::views::ProviderSection],
        _events: &Sender,
    ) {
    }

    fn readiness_read(&self) -> ReadinessRead {
        Box::new(|| Ok((Vec::new(), Vec::new())))
    }
}

fn is_allowance_read(event: &HostEvent) -> bool {
    matches!(event, HostEvent::SubscriptionUsage(_))
}
