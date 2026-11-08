//! Read-only retained terminal subtotals, independent of session admission.

use crate::home::paths::PlakPaths;
use plakd_wire_types::console::{
    Coverage, CoverageReason, Currency, Decimal, RecordedUsage, ReportedCost,
};
use rusqlite::{Connection, OpenFlags, params};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) struct RecordedSnapshot {
    pub accounts: BTreeMap<String, RecordedUsage>,
    pub empty: RecordedUsage,
    pub unattributed: RecordedUsage,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Unavailable {
    Storage,
    LegacySchema,
    Deadline,
}

#[derive(Default)]
struct Measured {
    tokens: Option<[u128; 5]>,
    calls: Option<u128>,
    cost: Cost,
    missing: bool,
}

#[derive(Default)]
enum Cost {
    #[default]
    Unknown,
    Reported(u128),
}

impl Measured {
    fn add(&mut self, row: &rusqlite::Row<'_>) -> Result<(), Unavailable> {
        let calls = counter(row, 6)?;
        let reporting = counter(row, 7)?;
        if reporting > calls {
            return Err(Unavailable::Storage);
        }
        self.calls = Some(
            self.calls
                .unwrap_or(0)
                .checked_add(u128::from(calls))
                .ok_or(Unavailable::Storage)?,
        );
        self.missing |= reporting < calls;
        if reporting > 0 || calls == 0 {
            for (offset, total) in self.tokens.get_or_insert([0; 5]).iter_mut().enumerate() {
                *total = total
                    .checked_add(u128::from(counter(row, offset + 1)?))
                    .ok_or(Unavailable::Storage)?;
            }
        }
        let cost: Option<String> = row.get(8).map_err(|_| Unavailable::Storage)?;
        self.missing |= cost.is_none() && calls > 0;
        self.cost = match (&self.cost, cost) {
            (Cost::Unknown, Some(cost)) => Cost::Reported(u128::from(decimal(&cost)?)),
            (Cost::Reported(total), Some(cost)) => Cost::Reported(
                total
                    .checked_add(u128::from(decimal(&cost)?))
                    .ok_or(Unavailable::Storage)?,
            ),
            (Cost::Reported(total), None) => Cost::Reported(*total),
            (Cost::Unknown, None) => Cost::Unknown,
        };
        Ok(())
    }

    fn project(self, mut coverage: Coverage) -> RecordedUsage {
        if self.missing || self.calls.is_none() {
            push(&mut coverage.reasons, CoverageReason::MissingMeasurements);
        }
        RecordedUsage {
            input_tokens: self.tokens.map(|tokens| Decimal::from(tokens[0])),
            output_tokens: self.tokens.map(|tokens| Decimal::from(tokens[1])),
            reasoning_tokens: self.tokens.map(|tokens| Decimal::from(tokens[2])),
            cached_input_tokens: self.tokens.map(|tokens| Decimal::from(tokens[3])),
            cache_write_tokens: self.tokens.map(|tokens| Decimal::from(tokens[4])),
            calls: self.calls.map(Decimal::from),
            cost: match self.cost {
                Cost::Reported(total) => Some(ReportedCost {
                    micros: Decimal::from(total),
                    currency: Currency::Usd,
                }),
                Cost::Unknown => None,
            },
            coverage,
        }
    }
}

fn counter(row: &rusqlite::Row<'_>, index: usize) -> Result<u64, Unavailable> {
    decimal(
        &row.get::<_, String>(index)
            .map_err(|_| Unavailable::Storage)?,
    )
}
fn decimal(value: &str) -> Result<u64, Unavailable> {
    if value.is_empty()
        || value.len() > 20
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(Unavailable::Storage);
    }
    value.parse().map_err(|_| Unavailable::Storage)
}
fn push(reasons: &mut Vec<CoverageReason>, reason: CoverageReason) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}
fn utc(at: i64) -> Option<String> {
    super::clock::utc(at)
}

pub(super) fn unknown(reason: CoverageReason) -> RecordedUsage {
    Measured::default().project(Coverage {
        from: None,
        to: None,
        complete: false,
        reasons: vec![reason],
    })
}

/// `to` is the single accepted millisecond-precision UTC instant for every
/// page. Missing files stay missing; this function opens no other database.
pub(super) fn read(
    paths: &PlakPaths,
    from: i64,
    to: i64,
    selected: Option<&str>,
    maximum_accounts: usize,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Result<RecordedSnapshot, Unavailable> {
    if Instant::now() >= deadline || cancelled.load(Ordering::Acquire) {
        return Err(Unavailable::Deadline);
    }
    let mut connection = Connection::open_with_flags(
        paths.telemetry_database(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| Unavailable::Storage)?;
    connection
        .busy_timeout(Duration::from_millis(25))
        .map_err(|_| Unavailable::Storage)?;
    let cancel = Arc::clone(&cancelled);
    connection
        .progress_handler(
            1_000,
            Some(move || Instant::now() >= deadline || cancel.load(Ordering::Acquire)),
        )
        .map_err(|_| Unavailable::Storage)?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|_| Unavailable::Storage)?;
    if version != crate::telemetry_store::TELEMETRY_SCHEMA_VERSION {
        return Err(Unavailable::LegacySchema);
    }
    let transaction = connection.transaction().map_err(|_| Unavailable::Storage)?;
    let (started, retained, dropped, errors): (i64, Option<i64>, i64, i64) = transaction.query_row(
        "SELECT started_at_ms, retained_from_ms, dropped, write_errors FROM recording_evidence WHERE singleton=1", [],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    ).map_err(|_| Unavailable::Storage)?;
    let mut reasons = Vec::new();
    if from < started {
        reasons.push(CoverageReason::PreRecording);
    }
    if retained.is_some_and(|retained| from < retained) {
        reasons.push(CoverageReason::Retention);
    }
    if dropped > 0 || errors > 0 {
        reasons.push(CoverageReason::WriterGap);
    }
    let lower = from.max(started).max(retained.unwrap_or(from));
    let mut coverage = Coverage {
        from: (lower <= to).then(|| utc(lower)).flatten(),
        to: (lower <= to).then(|| utc(to)).flatten(),
        complete: false,
        reasons,
    };
    let mut measured = BTreeMap::<String, Measured>::new();
    let mut unattributed = Measured::default();
    let mut truncated = false;
    let mut statement = transaction.prepare(
        "SELECT substr(account,1,65),substr(input_tokens,1,21),substr(output_tokens,1,21),substr(reasoning_tokens,1,21),substr(cached_input_tokens,1,21),substr(cache_write_tokens,1,21),substr(model_operations,1,21),substr(reporting_operations,1,21),substr(cost_micros,1,21) FROM recorded_terminals WHERE committed_at_ms>=?1 AND committed_at_ms<?2 AND (?3 IS NULL OR account=?3 OR account IS NULL) ORDER BY committed_at_ms",
    ).map_err(|_| Unavailable::Storage)?;
    let mut rows = statement
        .query(params![from, to, selected])
        .map_err(|_| Unavailable::Storage)?;
    while let Some(row) = rows.next().map_err(|_| Unavailable::Storage)? {
        if Instant::now() >= deadline || cancelled.load(Ordering::Acquire) {
            return Err(Unavailable::Deadline);
        }
        let account: Option<String> = row.get(0).map_err(|_| Unavailable::Storage)?;
        let account = account
            .filter(|value| crate::telemetry_store::recorded_usage::exact_account(value).is_some());
        match account {
            Some(account) => {
                if !measured.contains_key(&account) && measured.len() >= maximum_accounts {
                    truncated = true;
                    continue;
                }
                measured.entry(account).or_default().add(row)?;
            }
            None => unattributed.add(row)?,
        }
    }
    if unattributed.calls.is_some() {
        push(&mut coverage.reasons, CoverageReason::UnknownAttribution);
    }
    // Recording evidence is not proof that all producers reached this writer.
    // In particular, no rows must not become a fabricated complete zero.
    Ok(RecordedSnapshot {
        empty: Measured::default().project(coverage.clone()),
        accounts: measured
            .into_iter()
            .map(|(account, total)| (account, total.project(coverage.clone())))
            .collect(),
        unattributed: unattributed.project(coverage),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_use_terminal_commit_time_and_keep_large_subtotals_incomplete()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let paths = PlakPaths::explicit(root.path().join("home"));
        let _store = crate::telemetry_store::TelemetryStore::open(&paths)?;
        let connection = Connection::open(paths.telemetry_database())?;
        connection.execute("UPDATE recording_evidence SET started_at_ms=0", [])?;
        for (id, at, account) in [
            ("before", 9, Some("openai/work")),
            ("first", 10, Some("openai/work")),
            ("second", 19, Some("openai/work")),
            ("at-end", 20, Some("openai/work")),
            ("unknown", 11, None),
            ("deleted-account", 15, Some("codex/removed")),
        ] {
            connection.execute("INSERT INTO recorded_terminals(turn_id,session_id,branch_id,committed_at_ms,account,input_tokens,output_tokens,reasoning_tokens,cached_input_tokens,cache_write_tokens,model_operations,reporting_operations,cost_micros) VALUES(?1,'session','branch',?2,?3,?4,'0','0','0','0','2','1','3')", params![id,at,account,u64::MAX.to_string()])?;
        }
        connection.execute("INSERT INTO recorded_terminals(turn_id,session_id,branch_id,committed_at_ms,account,input_tokens,output_tokens,reasoning_tokens,cached_input_tokens,cache_write_tokens,model_operations,reporting_operations,cost_micros) VALUES('missing-cost','session','branch',18,'openai/work','0','0','0','0','0','1','1',NULL)", [])?;
        let snapshot = read(
            &paths,
            10,
            20,
            None,
            512,
            Instant::now() + Duration::from_secs(10),
            Arc::new(AtomicBool::new(false)),
        )
        .map_err(|error| format!("{error:?}"))?;
        let row = &snapshot.accounts["openai/work"];
        assert_eq!(row.calls.as_ref().map(Decimal::as_str), Some("5"));
        assert_eq!(
            row.cost.as_ref().map(|cost| cost.micros.as_str()),
            Some("6")
        );
        assert!(
            row.coverage
                .reasons
                .contains(&CoverageReason::MissingMeasurements)
        );
        assert_eq!(
            row.input_tokens.as_ref().map(Decimal::as_str),
            Some("36893488147419103230")
        );
        assert!(!row.coverage.complete);
        assert!(snapshot.accounts.contains_key("codex/removed"));
        assert_eq!(
            snapshot.unattributed.calls.as_ref().map(Decimal::as_str),
            Some("2")
        );
        assert!(
            snapshot
                .unattributed
                .coverage
                .reasons
                .contains(&CoverageReason::UnknownAttribution)
        );
        let empty = read(
            &paths,
            21,
            30,
            None,
            512,
            Instant::now() + Duration::from_secs(10),
            Arc::new(AtomicBool::new(false)),
        )
        .map_err(|error| format!("{error:?}"))?;
        assert!(empty.accounts.is_empty());
        assert_eq!(empty.unattributed.calls, None);
        assert_eq!(empty.unattributed.cost, None);
        assert!(!empty.unattributed.coverage.complete);
        Ok(())
    }

    #[test]
    fn missing_store_is_not_created_and_limits_and_cancellation_are_explicit()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let paths = PlakPaths::explicit(root.path().join("absent"));
        assert!(matches!(
            read(
                &paths,
                0,
                10,
                None,
                1,
                Instant::now() + Duration::from_secs(10),
                Arc::new(AtomicBool::new(false))
            ),
            Err(Unavailable::Storage)
        ));
        assert!(!paths.home().exists());
        assert!(matches!(
            read(
                &paths,
                0,
                10,
                None,
                1,
                Instant::now(),
                Arc::new(AtomicBool::new(true))
            ),
            Err(Unavailable::Deadline)
        ));
        assert!(!paths.home().exists());
        Ok(())
    }
}
