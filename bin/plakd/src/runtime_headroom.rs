//! Content-free runtime store diagnostics.

use std::io::Write;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use plakous::sessions::{SessionBackend, SessionStoreBound, SessionStoreUsage};
use serde::Serialize;

use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult, ProcessOutcome};

pub(crate) fn cap_issues(usage: &SessionStoreUsage) -> impl Iterator<Item = CapIssue> {
    let (used, limit) = usage.session_capacity();
    let sessions = CapIssue::new("runtime_store.max_sessions", used, limit, limit, limit);
    usage
        .bounds()
        .iter()
        .filter_map(|bound| {
            let key = match bound.bound() {
                SessionStoreBound::Checkpoints => "runtime_store.max_checkpoints",
                SessionStoreBound::Objects => "runtime_store.max_objects",
                SessionStoreBound::ObjectBytes => "runtime_store.max_object_bytes",
                _ => return None,
            };
            CapIssue::new(
                key,
                bound.used(),
                bound.limit(),
                bound.ordinary_threshold(),
                bound.settlement_threshold(),
            )
        })
        .chain(sessions)
}

pub(crate) struct CapIssue {
    key: &'static str,
    used: u64,
    limit: u64,
    settlement: u64,
}

impl CapIssue {
    const fn new(
        key: &'static str,
        used: u64,
        limit: u64,
        ordinary: u64,
        settlement: u64,
    ) -> Option<Self> {
        if used < ordinary {
            return None;
        }
        Some(Self {
            key,
            used,
            limit,
            settlement,
        })
    }

    pub(crate) const fn blocks_settlement(&self) -> bool {
        self.used >= self.settlement
    }

    fn minimum(&self) -> u128 {
        let required = u128::from(self.used) + 1;
        if self.key == "runtime_store.max_sessions" {
            return required;
        }
        // The settlement reserve is floor((limit + 7) / 16). Invert
        // limit - reserve >= required to find the first admissible cap.
        (required + required.saturating_sub(8).div_ceil(15)).max(16)
    }
}

impl std::fmt::Display for CapIssue {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = if self.key == "runtime_store.max_sessions" {
            "new sessions cannot be created"
        } else if self.used >= self.limit {
            "ordinary, settlement and recovery growth are blocked until the cap is raised"
        } else if self.blocks_settlement() {
            "ordinary and settlement growth are blocked; only recovery headroom remains"
        } else {
            "ordinary growth is blocked; settlement and recovery still have headroom"
        };
        write!(
            out,
            "{}: used {}, cap {}: {reason} (minimum accepted cap {})",
            self.key,
            self.used,
            self.limit,
            self.minimum()
        )
    }
}

pub(crate) fn check_cap_edit(
    paths: &PlakPaths,
    key: &str,
    limits: [NonZeroUsize; 4],
) -> Result<Option<CapIssue>, String> {
    if !key.starts_with("runtime_store.") {
        return Ok(None);
    }
    let Ok(Some(usage)) =
        SessionBackend::inspect_sqlite_usage_with_bounds(paths.runtime_database(), limits)
    else {
        return Ok(None);
    };
    let issue = cap_issues(&usage).find(|issue| issue.key == key);
    if let Some(issue) = &issue
        && issue.blocks_settlement()
    {
        return Err(issue.to_string());
    }
    Ok(issue)
}

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
    let mut first_sample = true;
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
        if std::mem::take(&mut first_sample) {
            let issues = cap_issues(&usage)
                .map(|issue| issue.to_string())
                .collect::<Vec<_>>();
            if !issues.is_empty() {
                eprintln!("plakd host: WARN {}", issues.join("; "));
                continue;
            }
        }
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

    #[tokio::test]
    async fn config_set_guards_settlement_and_warns_at_ordinary_threshold() {
        use crate::product_config::{ConfigWriteScope, ProductConfig};
        use plakous::sessions::{
            AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionOperationId,
        };
        use plakous_offline_crash_harness::{
            BoundaryController, protocol::CrashBoundary, text_runtime_binding,
        };

        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::from_home(root.path().join("home"));
        paths.ensure_layout().unwrap();
        let backend = SessionBackend::sqlite(paths.runtime_database()).unwrap();
        let sessions = AgentSessions::new(backend).unwrap();
        let binding = text_runtime_binding(
            root.path(),
            BoundaryController::quiet_from_evidence_path(
                CrashBoundary::None,
                root.path().join("evidence"),
            )
            .unwrap(),
            "synthetic",
            "synthetic",
        )
        .unwrap();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("headroom-test").unwrap(),
                binding,
            ))
            .await
            .unwrap()
        else {
            panic!("new headroom fixture")
        };
        session.release().await.unwrap();
        let usage = SessionBackend::inspect_sqlite_usage(paths.runtime_database())
            .unwrap()
            .unwrap();
        let used = usage.bounds()[2].used();
        let key = "runtime_store.max_object_bytes";
        let cap = CapIssue::new(key, used, used, used, used).unwrap();
        let min = cap.minimum();
        let set = |limit: u128| {
            let services = crate::shell::services::Services::for_home(paths.clone());
            let mut stderr = Vec::new();
            let result = crate::dispatch::execute_config_set(
                &services,
                &mut Vec::new(),
                &mut stderr,
                key.into(),
                limit.to_string(),
                ConfigWriteScope::User,
            );
            (result, String::from_utf8(stderr).unwrap())
        };
        let refusal = set(min - 1).0.unwrap_err().to_string();
        assert!(refusal.contains(&format!("{key}: used {used}, cap {}", min - 1)));
        assert!(refusal.contains(&format!("minimum accepted cap {min}")));
        assert!(!paths.config_file().exists());

        let (result, warning) = set(min);
        result.unwrap();
        assert!(warning.contains("ordinary growth is blocked"));
        let config = ProductConfig::load_home_file(&paths).unwrap();
        assert_eq!(config.runtime_store_bounds[2].get() as u128, min);

        let (result, warning) = set(u128::from(used) * 2);
        result.unwrap();
        assert_eq!(warning, "");
    }

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
