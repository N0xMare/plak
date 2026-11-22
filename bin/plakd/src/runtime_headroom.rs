//! Content-free runtime store diagnostics.

use std::io::Write;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use plakous::sessions::{SessionBackend, SessionStoreUsage};
use serde::Serialize;

use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult, ProcessOutcome};

#[derive(Serialize)]
struct Bound {
    used: u64,
    limit: u64,
    ordinary_threshold: u64,
    subordinate_threshold: u64,
    settlement_threshold: u64,
}

#[derive(Serialize)]
struct Report {
    state: &'static str,
    sessions: Bound,
    checkpoints: Bound,
    objects: Bound,
    object_bytes: Bound,
    feed_records: u64,
    feed_bytes: u64,
    file_bytes: u64,
    free_bytes: u64,
}

impl Report {
    fn new(usage: Option<SessionStoreUsage>, limits: [NonZeroUsize; 4]) -> Self {
        let state = if usage.is_some() {
            "available"
        } else {
            "empty"
        };
        let usage = usage.unwrap_or_else(|| SessionBackend::empty_sqlite_usage(limits));
        let (used, limit) = usage.session_capacity();
        let sessions = Bound {
            used,
            limit,
            ordinary_threshold: limit,
            subordinate_threshold: limit,
            settlement_threshold: limit,
        };
        let [checkpoints, objects, object_bytes] = usage.bounds().map(|bound| Bound {
            used: bound.used(),
            limit: bound.limit(),
            ordinary_threshold: bound.ordinary_threshold(),
            subordinate_threshold: bound.subordinate_threshold(),
            settlement_threshold: bound.settlement_threshold(),
        });
        Self {
            state,
            sessions,
            checkpoints,
            objects,
            object_bytes,
            feed_records: usage.feed_records(),
            feed_bytes: usage.feed_bytes(),
            file_bytes: usage.file_bytes(),
            free_bytes: usage.free_bytes(),
        }
    }

    fn bounds(&self) -> [(&'static str, &Bound); 4] {
        [
            ("sessions", &self.sessions),
            ("checkpoints", &self.checkpoints),
            ("objects", &self.objects),
            ("object bytes", &self.object_bytes),
        ]
    }
}

pub(crate) fn write_status(
    paths: &PlakPaths,
    out: &mut dyn Write,
    json: bool,
) -> CliResult<ProcessOutcome> {
    let limits = crate::product_config::ProductConfig::load_home_file(paths)
        .map_err(CliFailure::new)?
        .runtime_store_bounds;
    let usage = SessionBackend::inspect_sqlite_usage_with_bounds(paths.runtime_database(), limits)
        .map_err(CliFailure::from)?;
    if usage.is_none() && !json {
        return Ok(ProcessOutcome::Success);
    }
    let report = Report::new(usage, limits);
    let failure = |_| CliFailure::new("stdout failed");
    if json {
        serde_json::to_writer(&mut *out, &report).map_err(failure)?;
        writeln!(out).map_err(|_| CliFailure::new("stdout failed"))?;
    } else {
        for (name, bound) in report.bounds() {
            writeln!(
                out,
                "Runtime store {name}: {}/{} (ordinary {}, children {}, settlement {})",
                bound.used,
                bound.limit,
                bound.ordinary_threshold,
                bound.subordinate_threshold,
                bound.settlement_threshold
            )
            .map_err(|_| CliFailure::new("stdout failed"))?;
        }
        writeln!(
            out,
            "Runtime feed: {} rows, {} bytes",
            report.feed_records, report.feed_bytes
        )
        .map_err(|_| CliFailure::new("stdout failed"))?;
    }
    Ok(ProcessOutcome::Success)
}

#[derive(Default)]
struct Alerts {
    last: [Option<Instant>; 2],
}

impl Alerts {
    fn due(&mut self, now: Instant, fill: u64) -> impl Iterator<Item = u64> {
        let mut due = [None; 2];
        for (index, mark) in [50, 75].into_iter().enumerate() {
            if fill >= mark
                && self.last[index]
                    .is_none_or(|then| now.duration_since(then) >= Duration::from_secs(3600))
            {
                self.last[index] = Some(now);
                due[index] = Some(mark);
            }
        }
        due.into_iter().flatten()
    }
}

#[allow(dead_code, reason = "only the hosting product runs periodic checks")]
pub(crate) async fn watch(paths: PlakPaths, limits: [NonZeroUsize; 4]) {
    let mut alerts = Alerts::default();
    let mut ticks = tokio::time::interval(Duration::from_secs(60));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let database = paths.runtime_database();
    loop {
        ticks.tick().await;
        let path = database.clone();
        let Ok(Ok(Some(usage))) = tokio::task::spawn_blocking(move || {
            SessionBackend::inspect_sqlite_usage_with_bounds(path, limits)
        })
        .await
        else {
            continue;
        };
        let report = Report::new(Some(usage), limits);
        let Some((name, bound, fill)) = report
            .bounds()
            .into_iter()
            .map(|(name, bound)| {
                (
                    name,
                    bound,
                    u64::try_from(u128::from(bound.used) * 100 / u128::from(bound.limit))
                        .unwrap_or(u64::MAX),
                )
            })
            .max_by_key(|(_, _, fill)| *fill)
        else {
            continue;
        };
        for mark in alerts.due(Instant::now(), fill) {
            eprintln!(
                "plakd host: WARN runtime store {name} at {fill}% of its cap ({}/{}; ordinary {}; alert {mark}%)",
                bound.used, bound.limit, bound.ordinary_threshold,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_json_reads_home_caps_and_existing_store_without_creating_a_missing_store() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = PlakPaths::from_home(temporary.path().join("home"));
        paths.ensure_layout().unwrap();
        std::fs::write(
            paths.config_file(),
            "[runtime_store]\nmax_sessions=103\nmax_checkpoints=101\nmax_objects=203\nmax_object_bytes=65537\n",
        )
        .unwrap();
        let report = || {
            let mut out = Vec::new();
            write_status(&paths, &mut out, true).unwrap();
            serde_json::from_slice::<serde_json::Value>(&out).unwrap()
        };
        let empty = report();
        assert_eq!(empty["state"], "empty");
        assert!(!paths.runtime_database().exists());
        let _runtime = SessionBackend::sqlite(paths.runtime_database()).unwrap();
        let opened = report();
        assert_eq!(opened["state"], "available");
        assert_eq!(opened["objects"]["limit"], 203);
        assert_eq!(opened["sessions"]["limit"], 103);
        assert_eq!(opened["sessions"]["ordinary_threshold"], 103);
        assert_eq!(opened["object_bytes"]["limit"], 65_537);
        assert_eq!(opened["checkpoints"]["ordinary_threshold"], 88);
        assert_eq!(opened["checkpoints"]["subordinate_threshold"], 75);
        assert_eq!(opened["checkpoints"]["settlement_threshold"], 95);
        assert_eq!(opened["checkpoints"], empty["checkpoints"]);
        assert_eq!(opened["feed_records"], 0);
        let mut config = crate::product_config::ProductConfig::default_product();
        for invalid in ["0", "-1", "15", "9223372036854775808"] {
            assert!(
                config
                    .set_key("runtime_store.max_objects", invalid)
                    .is_err()
            );
        }
    }

    #[test]
    fn alerts_do_not_repeat_after_a_flap_until_the_hour_elapses() {
        let mut alerts = Alerts::default();
        let start = Instant::now();
        assert_eq!(alerts.due(start, 49).count(), 0);
        assert_eq!(alerts.due(start, 51).collect::<Vec<_>>(), [50]);
        assert_eq!(
            alerts
                .due(start + Duration::from_secs(1), 75)
                .collect::<Vec<_>>(),
            [75]
        );
        assert_eq!(alerts.due(start + Duration::from_secs(2), 40).count(), 0);
        assert_eq!(alerts.due(start + Duration::from_secs(3), 76).count(), 0);
        assert_eq!(
            alerts
                .due(start + Duration::from_secs(3601), 76)
                .collect::<Vec<_>>(),
            [50, 75]
        );
    }
}
