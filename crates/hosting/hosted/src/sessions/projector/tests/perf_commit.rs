//! Opt-in release-profile per-commit projection and long-session RSS workload.

use super::*;
use std::hint::black_box;
use std::time::Instant;

#[test]
#[ignore = "informational release-profile benchmark; no inference"]
#[allow(clippy::print_stderr, reason = "opt-in benchmark output")]
fn projection_800_rows_per_commit() {
    let cloned = std::env::var("PLAK_PROJECT_BENCH").is_ok_and(|mode| mode == "cloned");
    let iterations: u64 = std::env::var("PLAK_PROJECT_COMMITS")
        .ok()
        .map(|value| value.parse().unwrap())
        .unwrap_or(10_000);
    let mut feed = Feed::new();
    let mut views = super::super::project(
        SessionViews::empty(),
        &feed.intents,
        &feed.records,
        &feed.reads,
    )
    .unwrap()
    .views;
    for ordinal in 1..=799 {
        views.rows.insert(
            RowKey::new(0, Lane::Runtime, ordinal),
            TranscriptRow::assistant("x".repeat(4_096)),
        );
    }
    views.state.row_bytes = views
        .rows
        .values()
        .map(|row| plakous_host_protocol::views::presentation::row_bytes(row) as u64)
        .sum();
    let call: CallId = id(900);
    let turn: TurnId = id(901);
    let mut elapsed = std::time::Duration::ZERO;
    for index in 0..=iterations {
        feed.records.clear();
        feed.reads = ReadCache::default();
        feed.advance(
            vec![effect(
                902,
                turn,
                call,
                FeedCallerLineage::Direct,
                FeedOperationState::Dispatched,
            )],
            Reads {
                calls: vec![(
                    CallTarget::Operation(902),
                    CallRead::Tool {
                        canonical: "process_session_start".into(),
                        exposed: "bash".into(),
                        arguments: Some(format!("{{\"command\":\"echo {index}\"}}")),
                        result: None,
                    },
                )],
                ..Reads::default()
            },
        );
        let started = Instant::now();
        let projection = if cloned {
            // The former live cache, pure projector, blocking commit and store copies.
            let cached = views.clone();
            let projection =
                project::project_cloned(&cached, &feed.intents, &feed.records, &feed.reads)
                    .unwrap();
            let committed = projection.views.clone();
            black_box(committed.clone());
            projection
        } else {
            let projection =
                super::super::project(views, &feed.intents, &feed.records, &feed.reads).unwrap();
            black_box(projection.views.state.clone());
            projection
        };
        if index > 0 {
            elapsed += started.elapsed();
        }
        black_box(&projection.writes);
        views = projection.views;
        assert_eq!(views.rows.len(), 800);
    }
    eprintln!(
        "projection_commit mode={} rows=800 commits={iterations} ns_per_commit={} rss={}",
        if cloned { "cloned" } else { "owned" },
        elapsed.as_nanos() / u128::from(iterations),
        std::fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .find(|line| line.starts_with("VmHWM:"))
            .unwrap_or("unavailable")
    );
}
