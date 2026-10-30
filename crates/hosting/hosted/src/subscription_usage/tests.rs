use super::*;
use plakous::model::{
    SubscriptionUsageMeter, SubscriptionUsagePercent, SubscriptionUsageWindow, Timestamp,
};
use plakous::openrouter::decode_openrouter_key_usage;

#[test]
fn stale_reason_names_only_provider_window_data() {
    assert_eq!(
        unavailable_reason(SubscriptionUsageUnavailable::Stale, "grok"),
        "provider reported a usage window that has already ended"
    );
}

fn codex_snapshot() -> Result<SubscriptionUsageRead, Box<dyn std::error::Error>> {
    let observed = Timestamp::new(1_788_000_000, 0)?;
    Ok(SubscriptionUsageRead::Snapshot(
        SubscriptionUsageSnapshot::new(
            observed,
            Some("ChatGPT Pro".to_owned()),
            vec![SubscriptionUsageMeter::new(
                "codex".to_owned(),
                None,
                vec![
                    SubscriptionUsageWindow::new(
                        Some("primary".to_owned()),
                        SubscriptionUsagePercent::new(22.9)?,
                        Some(SubscriptionUsagePeriod::Rolling {
                            duration_seconds: 18_000,
                        }),
                        Some(Timestamp::new(1_788_018_000, 0)?),
                    )?,
                    SubscriptionUsageWindow::new(
                        Some("secondary".to_owned()),
                        SubscriptionUsagePercent::new(50.7)?,
                        Some(SubscriptionUsagePeriod::Rolling {
                            duration_seconds: 604_800,
                        }),
                        Some(Timestamp::new(1_788_600_000, 0)?),
                    )?,
                ],
            )?],
        )?,
    ))
}

fn grok_snapshot() -> Result<SubscriptionUsageRead, Box<dyn std::error::Error>> {
    let observed = Timestamp::new(1_788_000_000, 0)?;
    Ok(SubscriptionUsageRead::Snapshot(
        SubscriptionUsageSnapshot::new(
            observed,
            Some("SuperGrok Heavy".to_owned()),
            vec![SubscriptionUsageMeter::new(
                "included-credits".to_owned(),
                None,
                vec![SubscriptionUsageWindow::new(
                    None,
                    SubscriptionUsagePercent::new(46.4)?,
                    Some(SubscriptionUsagePeriod::ProviderPeriod {
                        kind: "USAGE_PERIOD_TYPE_WEEKLY".to_owned(),
                        starts_at: None,
                    }),
                    Some(Timestamp::new(1_788_300_000, 0)?),
                )?],
            )?],
        )?
        .with_balances(vec![
            SubscriptionUsageBalance::new(
                SUBSCRIPTION_BALANCE_PREPAID_CREDITS.to_owned(),
                Some(9999),
                None,
                None,
            )?,
            SubscriptionUsageBalance::new(
                SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO.to_owned(),
                None,
                Some(355),
                Some(5000),
            )?,
        ])?,
    ))
}

fn openrouter_usage() -> Result<VerticalObservation, Box<dyn std::error::Error>> {
    let usage = decode_openrouter_key_usage(
        br#"{"data":{"limit":10.0,"limit_remaining":6.42,"usage":3.58,"usage_daily":0.4,
                 "usage_weekly":1.1,"usage_monthly":2.9,"is_free_tier":true}}"#,
        Timestamp::new(1_788_000_000, 0)?,
    )?;
    Ok(VerticalObservation::OpenRouterKey(
        Box::new(usage),
        "OPENROUTER_API_KEY".to_owned(),
    ))
}

#[test]
fn all_four_verticals_are_always_listed_in_order() {
    let view = project_limits([], SessionLedgerView::default());
    assert_eq!(view.verticals.len(), 4);
    let order: Vec<_> = view
        .verticals
        .iter()
        .map(|vertical| vertical.vertical.clone())
        .collect();
    assert_eq!(order, product_verticals().to_vec());
    let text = render_limits_text(&view);
    for vertical in product_verticals() {
        assert!(
            text.contains(vertical.provider_label()),
            "{} missing from:\n{text}",
            vertical.provider_label()
        );
    }
}

#[test]
fn claude_ready_meters_include_fable_as_a_grouped_weekly_window()
-> Result<(), Box<dyn std::error::Error>> {
    let window = |name: &str, used: f64, seconds: u64, reset: u64| {
        SubscriptionUsageWindow::new(
            Some(name.to_owned()),
            SubscriptionUsagePercent::new(used).expect("percent"),
            Some(SubscriptionUsagePeriod::Rolling {
                duration_seconds: seconds,
            }),
            Some(Timestamp::new(reset, 0).expect("timestamp")),
        )
        .expect("window")
    };
    let snapshot = SubscriptionUsageSnapshot::new(
        Timestamp::new(1_788_100_000, 0)?,
        None,
        vec![
            SubscriptionUsageMeter::new(
                "claude".to_owned(),
                None,
                vec![
                    window("five_hour", 5.0, 18_000, 1_788_110_000),
                    window("seven_day", 72.0, 604_800, 1_788_392_640),
                ],
            )?,
            SubscriptionUsageMeter::new(
                "fable".to_owned(),
                Some("Fable".to_owned()),
                vec![window("seven_day", 96.0, 604_800, 1_788_392_640)],
            )?,
        ],
    )?;
    let view = project_limits(
        [(
            claude_vertical(),
            VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
        )],
        SessionLedgerView::default(),
    );
    let claude = view
        .verticals
        .iter()
        .find(|vertical| vertical.vertical == claude_vertical())
        .expect("Claude is always listed");
    let VerticalUsageState::Ready { meters } = &claude.state else {
        panic!("Claude is Ready");
    };
    assert_eq!(meters[0].label, "Weekly limit");
    assert_eq!(meters[1].label, "5h limit");
    assert_eq!(meters[2].group.as_deref(), Some("Fable limit"));
    assert_eq!(meters[2].label, "Weekly limit");
    let text = render_limits_text(&view);
    assert!(text.contains("Fable limit:"), "{text}");
    assert!(text.contains("  95% left"), "{text}");
    assert!(text.contains("  28% left"), "{text}");
    assert!(text.contains("   4% left"), "{text}");
    Ok(())
}

#[test]
fn claude_account_email_is_projected_onto_the_account_line()
-> Result<(), Box<dyn std::error::Error>> {
    let snapshot = SubscriptionUsageSnapshot::new(
        Timestamp::new(1_788_000_000, 0)?,
        Some("Claude Max".to_owned()),
        Vec::new(),
    )?
    .with_account(Some("user@example.com".to_owned()))?;
    let view = project_limits(
        [(
            claude_vertical(),
            VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
        )],
        SessionLedgerView::default(),
    );
    let claude = &view.verticals[2];
    assert_eq!(claude.account_email.as_deref(), Some("user@example.com"));
    assert_eq!(claude.plan_label.as_deref(), Some("Claude Max"));
    let text = render_limits_text(&view);
    assert!(
        text.contains("\nClaude\n  Account: user@example.com (Claude Max)\n"),
        "{text}"
    );
    Ok(())
}

#[test]
fn a_non_ready_vertical_carries_no_numbers_and_no_bar() {
    let view = project_limits(
        [(codex_vertical(), VerticalObservation::NotConnected)],
        SessionLedgerView::default(),
    );
    let codex = &view.verticals[0];
    assert_eq!(codex.state, VerticalUsageState::NotConnected);
    assert_eq!(codex.plan_label, None);
    let text = render_limits_text(&view);
    let block = text
        .split("\n\n")
        .find(|block| block.starts_with("Codex"))
        .expect("Codex block");
    assert!(block.contains("not connected"), "{block}");
    assert!(!block.contains(LIMIT_BAR_FILL), "{block}");
}

#[test]
fn plan_and_tier_labels_survive_from_the_wire() -> Result<(), Box<dyn std::error::Error>> {
    let view = project_limits(
        [
            (
                codex_vertical(),
                VerticalObservation::Native(codex_snapshot()?),
            ),
            (
                supergrok_vertical(),
                VerticalObservation::Native(grok_snapshot()?),
            ),
        ],
        SessionLedgerView::default(),
    );
    assert_eq!(view.verticals[0].plan_label.as_deref(), Some("ChatGPT Pro"));
    assert_eq!(
        view.verticals[1].plan_label.as_deref(),
        Some("SuperGrok Heavy")
    );
    let text = render_limits_text(&view);
    // The vertical names the header; the plan is its own account line.
    assert!(text.contains("Codex\n  Account: ChatGPT Pro"), "{text}");
    assert!(
        text.contains("SuperGrok\n  Account: SuperGrok Heavy"),
        "{text}"
    );
    Ok(())
}

#[test]
fn codex_reports_weekly_before_five_hour_with_floored_percentages()
-> Result<(), Box<dyn std::error::Error>> {
    let view = project_limits(
        [(
            codex_vertical(),
            VerticalObservation::Native(codex_snapshot()?),
        )],
        SessionLedgerView::default(),
    );
    let VerticalUsageState::Ready { meters } = &view.verticals[0].state else {
        panic!("Codex is Ready");
    };
    assert_eq!(meters[0].label, "Weekly limit");
    assert_eq!(meters[1].label, "5h limit");
    assert_eq!(
        meters[0].reading,
        UsageMeterReading::Percent {
            floored_percent: 50,
            exact_percent: "50.7".to_owned(),
            percent_left: Some(49),
        }
    );
    assert_eq!(
        meters[1].reading,
        UsageMeterReading::Percent {
            floored_percent: 22,
            exact_percent: "22.9".to_owned(),
            percent_left: Some(77),
        }
    );
    Ok(())
}

#[test]
fn supergrok_names_its_one_window_from_the_reported_period_and_keeps_balances()
-> Result<(), Box<dyn std::error::Error>> {
    let view = project_limits(
        [(
            supergrok_vertical(),
            VerticalObservation::Native(grok_snapshot()?),
        )],
        SessionLedgerView::default(),
    );
    let vertical = &view.verticals[1];
    let VerticalUsageState::Ready { meters } = &vertical.state else {
        panic!("SuperGrok is Ready");
    };
    assert_eq!(meters.len(), 1);
    // The cadence is the provider's own period type, never guessed.
    assert_eq!(meters[0].label, "Weekly limit");
    assert_eq!(
        vertical
            .balances
            .iter()
            .map(|balance| (balance.label.as_str(), balance.value.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("Credits", "$99.99"),
            ("Pay as you go", "$3.55 used of $50.00 cap"),
        ]
    );
    let text = render_limits_text(&view);
    assert!(
        text.contains("Credits") && text.contains("$99.99"),
        "{text}"
    );
    assert!(
        text.contains("Pay as you go") && text.contains("$3.55 used of $50.00 cap"),
        "{text}"
    );
    Ok(())
}

/// proto3 omits a true 0.0 `creditUsagePercent`. A named weekly SuperGrok
/// period with no percentage is 0% used (a full bar), and zero prepaid /
/// disabled pay-as-you-go lines are omitted.
#[test]
fn a_supergrok_period_without_a_usage_figure_is_an_untouched_weekly_limit()
-> Result<(), Box<dyn std::error::Error>> {
    let observed = Timestamp::new(1_788_000_000, 0)?;
    let snapshot = SubscriptionUsageSnapshot::new(
        observed,
        None,
        vec![SubscriptionUsageMeter::new(
            "included-credits".to_owned(),
            None,
            vec![SubscriptionUsageWindow::new(
                None,
                SubscriptionUsagePercent::new(0.0)?,
                Some(SubscriptionUsagePeriod::ProviderPeriod {
                    kind: "USAGE_PERIOD_TYPE_WEEKLY".to_owned(),
                    starts_at: Some(observed),
                }),
                Some(Timestamp::new(1_789_046_548, 0)?),
            )?],
        )?],
    )?
    .with_balances(vec![
        SubscriptionUsageBalance::new(
            SUBSCRIPTION_BALANCE_PREPAID_CREDITS.to_owned(),
            Some(0),
            None,
            None,
        )?,
        SubscriptionUsageBalance::new(
            SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO.to_owned(),
            None,
            Some(0),
            Some(0),
        )?,
    ])?;
    let view = project_limits(
        [(
            supergrok_vertical(),
            VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
        )],
        SessionLedgerView::default(),
    );
    let vertical = &view.verticals[1];
    let VerticalUsageState::Ready { meters } = &vertical.state else {
        panic!("SuperGrok is Ready");
    };
    assert_eq!(meters.len(), 1);
    assert_eq!(meters[0].label, "Weekly limit");
    assert_eq!(
        meters[0].reading,
        UsageMeterReading::Percent {
            floored_percent: 0,
            exact_percent: "0".to_owned(),
            percent_left: Some(100),
        }
    );
    let text = render_limits_text(&view);
    assert!(text.contains("100% left"), "{text}");
    assert!(text.contains(&limit_bar(100)), "{text}");
    assert!(!text.contains("$0.00"), "{text}");
    assert!(!text.contains("Pay as you go"), "{text}");
    assert!(!text.contains("Credits"), "{text}");
    Ok(())
}

#[test]
fn a_supergrok_period_with_no_reported_cadence_is_not_named_weekly()
-> Result<(), Box<dyn std::error::Error>> {
    let observed = Timestamp::new(1_788_000_000, 0)?;
    let snapshot = SubscriptionUsageSnapshot::new(
        observed,
        None,
        vec![SubscriptionUsageMeter::new(
            "included-credits".to_owned(),
            None,
            vec![SubscriptionUsageWindow::new(
                None,
                SubscriptionUsagePercent::new(10.0)?,
                None,
                Some(Timestamp::new(1_788_300_000, 0)?),
            )?],
        )?],
    )?;
    let view = project_limits(
        [(
            supergrok_vertical(),
            VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
        )],
        SessionLedgerView::default(),
    );
    let VerticalUsageState::Ready { meters } = &view.verticals[1].state else {
        panic!("SuperGrok is Ready");
    };
    assert_eq!(meters[0].label, "Usage limit");
    Ok(())
}

#[test]
fn openrouter_amounts_render_and_a_null_limit_is_no_cap() -> Result<(), Box<dyn std::error::Error>>
{
    let view = project_limits(
        [(openrouter_vertical(), openrouter_usage()?)],
        SessionLedgerView::default(),
    );
    let text = render_limits_text(&view);
    // The winning source labels the key, not the provider's masked label.
    // Only an explicit `is_free_tier: true` becomes a tier line.
    assert!(
        text.contains("OpenRouter\n  Account: OPENROUTER_API_KEY · free tier"),
        "{text}"
    );
    assert!(!text.contains("API key"), "{text}");
    assert!(text.contains("$10.00 cap"), "{text}");
    assert!(text.contains("$6.42 remaining"), "{text}");
    assert!(text.contains("$0.40 today"), "{text}");

    let uncapped = decode_openrouter_key_usage(
        br#"{"data":{"limit":null,"usage":1.0}}"#,
        Timestamp::new(1, 0)?,
    )?;
    let view = project_limits(
        [(
            openrouter_vertical(),
            VerticalObservation::OpenRouterKey(Box::new(uncapped), "sha256:1234abcd".to_owned()),
        )],
        SessionLedgerView::default(),
    );
    let text = render_limits_text(&view);
    assert!(text.contains("no cap"), "{text}");
    assert!(!text.contains("$0.00 cap"), "{text}");
    assert!(text.contains("Account: sha256:1234abcd"), "{text}");
    // A file fingerprint never implies a paid tier.
    assert!(!text.contains("paid tier"), "{text}");
    assert!(!text.contains("free tier"), "{text}");
    Ok(())
}

#[test]
fn openrouter_usage_never_renders_provider_key_material() -> Result<(), Box<dyn std::error::Error>>
{
    let usage = decode_openrouter_key_usage(
        br#"{"data":{"label":"sk-or-masked-secret-suffix","usage":1.0}}"#,
        Timestamp::new(1, 0)?,
    )?;
    let view = project_limits(
        [(
            openrouter_vertical(),
            VerticalObservation::OpenRouterKey(Box::new(usage), "OPENROUTER_API_KEY".to_owned()),
        )],
        SessionLedgerView::default(),
    );
    let text = render_limits_text(&view);
    assert!(!text.contains("sk-or-masked-secret-suffix"));
    Ok(())
}

/// Spec 02 §7.2: a reset reads as the time first and then the day it falls
/// on, in the operator's own wall clock.
#[test]
fn a_reset_names_the_time_then_the_day() {
    let rendered = format_reset_wall_clock(1_788_018_000);
    assert!(!rendered.contains('T'), "{rendered}");
    assert!(!rendered.contains("2026"), "{rendered}");
    let (clock, day) = rendered.split_once(" on ").expect("time then day");
    let (hour, minute) = clock.split_once(':').expect("a wall clock time");
    assert!(hour.len() == 2 && minute.len() == 2, "{rendered}");
    // `6 Sep`, or `6 Sep UTC` when the local offset is indeterminate.
    assert!(day.split(' ').count() <= 3, "{rendered}");
}

/// Spec 02 §7.2: an observation time is wall clock, and never an unlabelled
/// instant in a zone the operator does not use.
#[test]
fn reset_times_render_as_wall_clock_and_name_utc_when_local_is_unknown() {
    let rendered = format_unix_seconds(1_788_018_000);
    // Never RFC 3339: no `T` separator and no bare `Z`.
    assert!(!rendered.contains('T'), "{rendered}");
    assert!(!rendered.ends_with('Z'), "{rendered}");
    assert!(rendered.contains("2026-"), "{rendered}");
    match time::UtcOffset::current_local_offset() {
        // The local offset is known: the value is local, and says nothing
        // about UTC because it is not UTC.
        Ok(offset) if offset != time::UtcOffset::UTC => {
            assert!(!rendered.ends_with(" UTC"), "{rendered}");
        }
        // Indeterminate (or genuinely UTC): the zone is named rather than
        // presented as if it were the operator's own.
        _ => assert!(
            rendered.ends_with(" UTC") || !rendered.ends_with('Z'),
            "{rendered}"
        ),
    }
}

#[test]
fn cost_is_never_rendered_as_zero_when_it_is_unknown() {
    let ledger = project_session_ledger(120, 100, 30, 10, 4, 3, Some(Duration::from_secs(192)));
    assert!(matches!(ledger.cost, UsageCostView::NotAvailable { .. }));
    let view = project_limits([], ledger);
    let text = render_limits_text(&view);
    assert!(text.contains("Cost:"), "{text}");
    assert!(text.contains("not available"), "{text}");
    assert!(!text.contains("$0"), "{text}");
    assert!(
        text.contains("turn wall time (host-measured): 3m12s"),
        "{text}"
    );
    assert!(!text.contains("API time"), "{text}");
}

#[test]
fn a_stale_projection_reports_its_age_and_never_claims_unavailable()
-> Result<(), Box<dyn std::error::Error>> {
    let mut view = project_limits(
        [(
            codex_vertical(),
            VerticalObservation::Native(codex_snapshot()?),
        )],
        SessionLedgerView::default(),
    );
    view.stale_age_seconds = Some(75);
    let text = render_limits_text(&view);
    assert!(text.contains("1m15s old"), "{text}");
    // The held Codex numbers are served, not replaced by an unavailable
    // claim; only Claude, which has no transport at all, says unavailable.
    let codex = text
        .split("\n\n")
        .find(|block| block.starts_with("Codex"))
        .expect("Codex block");
    assert!(codex.contains("Weekly limit"), "{codex}");
    // 50.7% used is 49% left, floored the same way.
    assert!(codex.contains("49% left"), "{codex}");
    assert!(!codex.contains("unavailable"), "{codex}");
    assert!(!codex.contains("not connected"), "{codex}");
    Ok(())
}

fn strip_sgr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[test]
/// The fill is the percent LEFT over exactly twenty floored cells: a full
/// bar is an untouched limit, an empty bar a reached one (spec 02 §7.2).
fn the_bar_fill_is_the_percent_left_over_twenty_floored_cells() {
    for (percent_left, filled) in [(0_u8, 0_usize), (1, 0), (50, 10), (99, 19), (100, 20)] {
        let bar = limit_bar(percent_left);
        let visible: String = strip_sgr(&bar);
        assert_eq!(
            visible.chars().count(),
            LIMIT_BAR_CELLS + 2,
            "brackets included"
        );
        assert_eq!(
            visible
                .chars()
                .filter(|cell| *cell == LIMIT_BAR_FILL)
                .count(),
            LIMIT_BAR_CELLS
        );
        let empty = LIMIT_BAR_CELLS - filled;
        if empty == 0 {
            assert!(!bar.contains('\u{1b}'));
        } else {
            assert!(bar.contains(LIMIT_BAR_EMPTY_SGR), "{bar:?}");
        }
        assert!(bar.starts_with('[') && bar.ends_with(']'));
    }
}

/// A lowercase wire `plan_type` such as `pro` is shown as `Pro`, and a wire
/// e-mail (when one is carried) joins it on the Account line.
#[test]
fn plan_is_title_cased_and_email_joins_the_account_line() -> Result<(), Box<dyn std::error::Error>>
{
    let snapshot = SubscriptionUsageSnapshot::new(
        Timestamp::new(1_788_000_000, 0)?,
        Some("pro".to_owned()),
        Vec::new(),
    )?
    .with_account(Some("operator@example.test".to_owned()))?;
    let view = project_limits(
        [(
            codex_vertical(),
            VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
        )],
        SessionLedgerView::default(),
    );
    assert_eq!(view.verticals[0].plan_label.as_deref(), Some("Pro"));
    assert_eq!(
        view.verticals[0].account_email.as_deref(),
        Some("operator@example.test")
    );
    let text = render_limits_text(&view);
    assert!(
        text.contains("\nCodex\n  Account: operator@example.test (Pro)\n"),
        "{text}"
    );
    assert!(
        !text.contains("Pro (Codex)") && !text.contains("Codex (Pro)"),
        "{text}"
    );
    Ok(())
}

/// Shaped like the operator's Codex account: the plan's own weekly window
/// at 0.5% used, plus a `GPT-5.3-Codex-Spark` feature whose 5h window is
/// untouched and whose weekly window is exhausted. The feature's windows
/// arrive weekly-first, so labels must follow the seconds, never the
/// primary/secondary position.
#[test]
fn codex_spark_feature_is_grouped_and_labelled_by_window_duration()
-> Result<(), Box<dyn std::error::Error>> {
    let window = |name: &str, used: f64, seconds: u64, reset: u64| {
        SubscriptionUsageWindow::new(
            Some(name.to_owned()),
            SubscriptionUsagePercent::new(used).expect("percent"),
            Some(SubscriptionUsagePeriod::Rolling {
                duration_seconds: seconds,
            }),
            Some(Timestamp::new(reset, 0).expect("timestamp")),
        )
        .expect("window")
    };
    let snapshot = SubscriptionUsageSnapshot::new(
        Timestamp::new(1_788_100_000, 0)?,
        Some("pro".to_owned()),
        vec![
            SubscriptionUsageMeter::new(
                "codex".to_owned(),
                None,
                vec![window("primary", 0.5, 604_800, 1_788_392_640)],
            )?,
            SubscriptionUsageMeter::new(
                "codex_spark".to_owned(),
                Some("GPT-5.3-Codex-Spark".to_owned()),
                vec![
                    window("primary", 100.0, 604_800, 1_788_391_380),
                    window("secondary", 0.0, 18_000, 1_788_110_940),
                ],
            )?,
        ],
    )?;
    let view = project_limits(
        [(
            codex_vertical(),
            VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
        )],
        SessionLedgerView::default(),
    );
    let codex = &view.verticals[0];
    assert_eq!(codex.plan_label.as_deref(), Some("Pro"));
    assert_eq!(codex.account_email, None);
    let VerticalUsageState::Ready { meters } = &codex.state else {
        panic!("Codex is Ready");
    };
    assert_eq!(meters.len(), 3);
    assert_eq!(meters[0].group, None);
    assert_eq!(meters[0].label, "Weekly limit");
    assert_eq!(
        meters[0].reading,
        UsageMeterReading::Percent {
            floored_percent: 0,
            exact_percent: "0.5".to_owned(),
            percent_left: Some(99),
        }
    );
    for meter in &meters[1..] {
        assert_eq!(meter.group.as_deref(), Some("GPT-5.3-Codex-Spark limit"));
    }
    assert_eq!(meters[1].label, "Weekly limit");
    assert_eq!(
        meters[1].reading,
        UsageMeterReading::Percent {
            floored_percent: 100,
            exact_percent: "100".to_owned(),
            percent_left: Some(0),
        }
    );
    assert_eq!(meters[2].label, "5h limit");
    assert_eq!(
        meters[2].reading,
        UsageMeterReading::Percent {
            floored_percent: 0,
            exact_percent: "0".to_owned(),
            percent_left: Some(100),
        }
    );
    let text = render_limits_text(&view);
    assert!(text.contains("\nCodex\n  Account: Pro\n"), "{text}");
    assert!(text.contains("GPT-5.3-Codex-Spark limit:"), "{text}");
    assert!(text.contains(" 99% left"), "{text}");
    assert!(text.contains("  0% left"), "{text}");
    assert!(text.contains("100% left"), "{text}");
    let bar_at = |needle: &str| {
        text.lines()
            .find(|line| line.contains(needle))
            .and_then(|line| line.find('['))
    };
    assert_eq!(
        bar_at("99% left"),
        bar_at("  0% left"),
        "grouped Spark bars must share a column with the plan meters:\n{text}"
    );
    assert_eq!(bar_at("99% left"), bar_at("100% left"), "{text}");
    Ok(())
}

/// A local lock miss or a fenced credential is named as exactly that,
/// never as a failed transport; only `Transport` says the HTTP call failed.
#[test]
fn local_refusals_are_never_called_a_transport_failure() {
    let busy = unavailable_reason(SubscriptionUsageUnavailable::Busy, "grok");
    assert_eq!(busy, USAGE_BUSY_REASON);
    assert!(!busy.contains("transport"), "{busy}");
    for (state, expected, remedy) in [
        (
            SubscriptionUsageCredentialState::RefreshRequired,
            "credential needs a token refresh",
            "the next turn refreshes it, or refresh explicitly",
        ),
        (
            SubscriptionUsageCredentialState::Quarantined,
            "credential is quarantined",
            "replace the login",
        ),
        (
            SubscriptionUsageCredentialState::Revoked,
            "credential is revoked",
            "replace the login",
        ),
    ] {
        let text = unavailable_reason(SubscriptionUsageUnavailable::CredentialState(state), "grok");
        assert!(text.contains(expected), "{text}");
        assert!(text.contains(remedy), "{text}");
        assert!(!text.contains("transport"), "{text}");
    }
    let grok_quarantine = unavailable_reason(
        SubscriptionUsageUnavailable::CredentialState(
            SubscriptionUsageCredentialState::Quarantined,
        ),
        "grok",
    );
    assert!(
        grok_quarantine.contains("refresh explicitly"),
        "{grok_quarantine}"
    );
    let codex_quarantine = unavailable_reason(
        SubscriptionUsageUnavailable::CredentialState(
            SubscriptionUsageCredentialState::Quarantined,
        ),
        "codex",
    );
    assert!(
        codex_quarantine.contains("replace the login to recover"),
        "{codex_quarantine}"
    );
    assert!(
        !codex_quarantine.contains("plak refresh"),
        "{codex_quarantine}"
    );
    let transport = unavailable_reason(SubscriptionUsageUnavailable::Transport(None), "grok");
    assert!(transport.contains("HTTP request"), "{transport}");
    assert_eq!(
        unavailable_reason(SubscriptionUsageUnavailable::RateLimited, "claude"),
        USAGE_RATE_LIMITED_REASON
    );
}

fn ready_claude_view() -> UsageLimitsView {
    let mut view = project_limits([], SessionLedgerView::default());
    view.verticals[2].observed_at = Some("2026-09-04 12:00".to_owned());
    view.verticals[2].state = VerticalUsageState::Ready {
        meters: vec![UsageMeterView {
            label: "Weekly limit".to_owned(),
            group: None,
            reading: UsageMeterReading::Percent {
                floored_percent: 10,
                exact_percent: "10".to_owned(),
                percent_left: Some(90),
            },
            resets_at: None,
        }],
    };
    view
}

fn claude_unavailable(reason: &str) -> UsageLimitsView {
    let mut view = project_limits([], SessionLedgerView::default());
    view.verticals[2].state = VerticalUsageState::Unavailable {
        reason: reason.to_owned(),
    };
    view
}

#[test]
fn a_fresh_held_projection_is_served_as_current() {
    let mut view = ready_claude_view();
    view.stale_age_seconds = Some(12);
    let served = cached_if_fresh(&view, Duration::from_secs(59)).expect("still fresh");
    assert_eq!(served.stale_age_seconds, None);
    assert_eq!(served.verticals[2].state, view.verticals[2].state);
    assert!(cached_if_fresh(&view, USAGE_PROJECTION_FRESHNESS).is_none());
    assert!(cached_if_fresh(&view, Duration::from_secs(61)).is_none());
}

#[test]
fn a_rate_limited_claude_vertical_keeps_the_last_ready_meters() {
    let previous = ready_claude_view();
    let incoming = claude_unavailable(USAGE_RATE_LIMITED_REASON);
    let merged = preserve_ready_across_rate_limit(incoming, Some(&previous));
    assert_eq!(merged.verticals[2], previous.verticals[2]);
    assert!(matches!(
        merged.verticals[2].state,
        VerticalUsageState::Ready { .. }
    ));
    assert_eq!(
        merged.verticals[2].observed_at.as_deref(),
        Some("2026-09-04 12:00")
    );
}

#[test]
fn a_busy_claude_vertical_keeps_the_last_ready_meters() {
    let previous = ready_claude_view();
    let incoming = claude_unavailable(USAGE_BUSY_REASON);
    let merged = preserve_ready_across_rate_limit(incoming, Some(&previous));
    assert_eq!(merged.verticals[2], previous.verticals[2]);
}

#[test]
fn a_busy_only_projection_is_not_served_as_fresh() {
    let view = claude_unavailable(USAGE_BUSY_REASON);
    assert!(projection_is_lock_contention(&view));
    assert!(cached_if_fresh(&view, Duration::from_secs(1)).is_none());
    let ready = ready_claude_view();
    assert!(!projection_is_lock_contention(&ready));
    assert!(cached_if_fresh(&ready, Duration::from_secs(1)).is_some());
}

#[test]
fn a_transport_failure_does_not_keep_stale_ready_meters() {
    let previous = ready_claude_view();
    let incoming = claude_unavailable("usage HTTP request to the provider failed");
    let merged = preserve_ready_across_rate_limit(incoming.clone(), Some(&previous));
    assert_eq!(merged.verticals[2], incoming.verticals[2]);
    assert!(matches!(
        merged.verticals[2].state,
        VerticalUsageState::Unavailable { .. }
    ));
}

#[test]
fn a_rate_limited_read_without_a_prior_ready_stays_unavailable() {
    let incoming = claude_unavailable(USAGE_RATE_LIMITED_REASON);
    let merged = preserve_ready_across_rate_limit(incoming.clone(), None);
    assert_eq!(merged, incoming);
    let previous = project_limits([], SessionLedgerView::default());
    let merged = preserve_ready_across_rate_limit(incoming.clone(), Some(&previous));
    assert_eq!(merged.verticals[2], incoming.verticals[2]);
}

#[test]
fn a_successful_sibling_vertical_still_updates_across_a_claude_429() {
    let previous = ready_claude_view();
    let mut incoming = claude_unavailable(USAGE_RATE_LIMITED_REASON);
    incoming.verticals[0].state = VerticalUsageState::Ready {
        meters: vec![UsageMeterView {
            label: "Weekly limit".to_owned(),
            group: None,
            reading: UsageMeterReading::Percent {
                floored_percent: 1,
                exact_percent: "1".to_owned(),
                percent_left: Some(99),
            },
            resets_at: None,
        }],
    };
    let merged = preserve_ready_across_rate_limit(incoming.clone(), Some(&previous));
    assert_eq!(merged.verticals[0], incoming.verticals[0]);
    assert_eq!(merged.verticals[2], previous.verticals[2]);
}

#[test]
fn the_rate_limit_sentence_is_the_exact_operator_text() {
    assert!(is_rate_limited_reason(USAGE_RATE_LIMITED_REASON));
    assert!(!is_rate_limited_reason(
        "usage HTTP request to the provider failed"
    ));
    assert!(is_busy_reason(USAGE_BUSY_REASON));
    assert!(!is_busy_reason(USAGE_RATE_LIMITED_REASON));
}
