use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub(super) fn now_ms() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

pub(super) fn utc(milliseconds: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(milliseconds) * 1_000_000)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

pub(super) fn milliseconds(utc: &str) -> Option<i64> {
    let at = OffsetDateTime::parse(utc, &Rfc3339).ok()?;
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok()
}

pub(super) fn date(seconds: u64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(i64::try_from(seconds).ok()?)
        .ok()
        .map(|at| at.date().to_string())
}
