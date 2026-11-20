//! Daily append-only audit. Metadata only: no headers, bodies, queries, keys,
//! tokens, pastes or device codes.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use super::io_file::ensure_private_dir;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct AuditEvent {
    pub ts: String,
    pub instance: String,
    pub caller: String,
    pub verb: String,
    pub account: Option<String>,
    pub upstream_origin: Option<String>,
    pub upstream_path: Option<String>,
    pub method: Option<String>,
    pub operation: Option<String>,
    pub attempt: Option<u32>,
    pub upstream_status: Option<u16>,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub duration_ms: u64,
    pub certainty: Option<String>,
    pub gateway_error: Option<String>,
}

pub(crate) struct Audit {
    dir: PathBuf,
    retention_days: u16,
    failures: std::sync::atomic::AtomicU64,
    /// Where the failure count is published: `<instance> <count>`.
    failures_file: Option<(PathBuf, String)>,
}

impl Audit {
    pub(crate) fn open(dir: PathBuf, retention_days: u16) -> std::io::Result<Self> {
        ensure_private_dir(&dir)?;
        let audit = Self {
            dir,
            retention_days,
            failures: std::sync::atomic::AtomicU64::new(0),
            failures_file: None,
        };
        audit.prune();
        Ok(audit)
    }

    /// Publishes this instance's failure count at `path`, starting at zero.
    pub(crate) fn publishing_failures(mut self, path: PathBuf, instance: &str) -> Self {
        self.failures_file = Some((path, instance.to_owned()));
        self.publish(0);
        self
    }

    pub(crate) fn record(&self, event: &AuditEvent) {
        // A failed audit line must not fail the call it records; it is
        // counted, logged and shown by `plakd gateway status`.
        if let Err(error) = self.append(event) {
            let count = self
                .failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            eprintln!("plakd gateway: audit append failed ({count} so far): {error}");
            self.publish(count);
        }
    }

    fn publish(&self, count: u64) {
        if let Some((path, instance)) = &self.failures_file {
            let _ = super::io_file::write_private(path, format!("{instance} {count}\n").as_bytes());
        }
    }

    fn append(&self, event: &AuditEvent) -> std::io::Result<()> {
        let path = self.dir.join(format!("{}.jsonl", utc_day()));
        let mut options = OpenOptions::new();
        options.create(true).append(true).mode(0o600);
        let mut file = options.open(path)?;
        // One write per line: concurrent appenders must never interleave a
        // record with another record's newline.
        let mut line = serde_json::to_vec(event)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        line.push(b'\n');
        file.write_all(&line)?;
        Ok(())
    }

    fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let today = utc_day_ordinal();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(day) = name.strip_suffix(".jsonl").and_then(day_ordinal) else {
                continue;
            };
            if today.saturating_sub(day) > u64::from(self.retention_days) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

fn utc_day() -> String {
    let seconds = super::callers::now_ms() / 1000;
    format_day(seconds)
}

fn utc_day_ordinal() -> u64 {
    super::callers::now_ms() / 1000 / 86_400
}

fn day_ordinal(day: &str) -> Option<u64> {
    let mut parts = day.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    u64::try_from(days_from_civil(year, month, day)).ok()
}

fn format_day(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let (year, month, day) = civil_from_days(days).unwrap_or((1970, 1, 1));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's civil-from-days, for the UTC file name. No time zone.
fn civil_from_days(days: i64) -> Option<(i32, u32, u32)> {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let doe = u64::try_from(shifted - era * 146_097).unwrap_or(0);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = i64::try_from(yoe).unwrap_or(0) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    Some((
        i32::try_from(y).ok()?,
        u32::try_from(m).ok()?,
        u32::try_from(d).ok()?,
    ))
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = u64::try_from(y - era * 400).unwrap_or(0);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * u64::from(mp) + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + i64::try_from(doe).unwrap_or(0) - 719_468
}

/// UTC timestamp for one audit line. No secret, no local zone name.
pub(crate) fn stamp() -> String {
    jiff::Timestamp::now().to_string()
}

/// The published failure count of gateway `instance`, if it published one.
pub(crate) fn published_failures(path: &Path, instance: &str) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let (owner, count) = text.trim().split_once(' ')?;
    (owner == instance).then(|| count.parse().ok()).flatten()
}

/// Parses `audit --since`: an RFC 3339 timestamp.
pub(crate) fn parse_since(text: &str) -> Result<jiff::Timestamp, String> {
    text.parse::<jiff::Timestamp>().map_err(|_| {
        format!("--since must be an RFC 3339 timestamp, such as 2026-11-18T00:00:00Z; got {text}")
    })
}

/// Reads audit lines for `plakd gateway audit`. A line that is not JSON is
/// skipped; `since` compares instants, not text.
pub(crate) fn read_lines(
    dir: &Path,
    since: Option<jiff::Timestamp>,
    caller: Option<&str>,
) -> std::io::Result<Vec<String>> {
    let mut names = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
            {
                names.push(entry.path());
            }
        }
    }
    names.sort();
    let mut lines = Vec::new();
    for path in names {
        let text = std::fs::read_to_string(path)?;
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(since) = since {
                let at = value
                    .get("ts")
                    .and_then(|item| item.as_str())
                    .and_then(|ts| ts.parse::<jiff::Timestamp>().ok());
                if at.is_none_or(|at| at < since) {
                    continue;
                }
            }
            if let Some(caller) = caller {
                let row = value
                    .get("caller")
                    .and_then(|item| item.as_str())
                    .unwrap_or("");
                if row != caller {
                    continue;
                }
            }
            lines.push(line.to_owned());
        }
    }
    Ok(lines)
}
