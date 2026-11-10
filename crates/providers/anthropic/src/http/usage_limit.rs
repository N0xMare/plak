//! Claude subscription usage-limit attestation on a rejected Messages request.
//!
//! The reviewed native client reads the same `anthropic-ratelimit-unified-*`
//! response headers: a 429 whose unified status is `rejected` and whose
//! representative claim names a subscription window is that window's limit,
//! not a transient rate limit. Anything else stays a generic rate limit.

use std::time::{Duration, SystemTime};

use plakous_http::{HeaderName, HeaderValue};

const STATUS: HeaderName = HeaderName::from_static("anthropic-ratelimit-unified-status");
const CLAIM: HeaderName =
    HeaderName::from_static("anthropic-ratelimit-unified-representative-claim");
const RESET: HeaderName = HeaderName::from_static("anthropic-ratelimit-unified-reset");
const OVERAGE_STATUS: HeaderName =
    HeaderName::from_static("anthropic-ratelimit-unified-overage-status");
/// Keeps provider-controlled timestamps inside the HTTP-date formatter's range.
const MAX_RESET_SECONDS: u64 = 253_402_300_799;

/// The subscription window a rejected request's representative claim names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Window {
    /// `five_hour`.
    Session,
    /// `seven_day`.
    Weekly,
    /// `seven_day_opus`.
    WeeklyOpus,
    /// `seven_day_sonnet`.
    WeeklySonnet,
}

impl Window {
    fn from_claim(claim: &[u8]) -> Option<Self> {
        match claim {
            b"five_hour" => Some(Self::Session),
            b"seven_day" => Some(Self::Weekly),
            b"seven_day_opus" => Some(Self::WeeklyOpus),
            b"seven_day_sonnet" => Some(Self::WeeklySonnet),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Session => "session limit",
            Self::Weekly => "weekly limit",
            Self::WeeklyOpus => "Opus limit",
            Self::WeeklySonnet => "Sonnet limit",
        }
    }
}

/// An attested Claude subscription limit and its reset, when representable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SubscriptionLimit {
    window: Window,
    resets_at: Option<SystemTime>,
}

impl SubscriptionLimit {
    /// Reads the unified rate-limit headers of a 429. Each header must occur
    /// at most once; a duplicated, unknown or missing attestation, or a
    /// request the account's usage credits still admit, establishes nothing.
    pub(super) fn from_headers(headers: &plakous_http::Headers, now: SystemTime) -> Option<Self> {
        Self::from_pairs(&headers.iter().collect::<Vec<_>>(), now)
    }

    fn from_pairs(headers: &[(&HeaderName, &HeaderValue)], now: SystemTime) -> Option<Self> {
        if single(headers, &STATUS)? != b"rejected" {
            return None;
        }
        let window = Window::from_claim(single(headers, &CLAIM)?)?;
        let mut overage = values(headers, &OVERAGE_STATUS);
        if overage.clone().count() > 1
            || overage
                .next()
                .is_some_and(|status| matches!(status, b"allowed" | b"allowed_warning"))
        {
            return None;
        }
        let resets_at = single(headers, &RESET)
            .and_then(|value| std::str::from_utf8(value).ok())
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|seconds| *seconds <= MAX_RESET_SECONDS)
            .and_then(|seconds| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds)))
            .filter(|time| *time > now);
        Some(Self { window, resets_at })
    }

    /// The operator notice: which limit, when it resets in UTC, and the two
    /// ways forward.
    pub(super) fn notice(self) -> String {
        let name = self.window.name();
        self.resets_at.map_or_else(
            || {
                format!(
                    "You've hit your Claude {name}; try again after it resets, or switch to another Claude account."
                )
            },
            |time| {
                format!(
                    "You've hit your Claude {name}; try again after {}, or switch to another Claude account.",
                    httpdate::fmt_http_date(time)
                )
            },
        )
    }
}

fn values<'a>(
    headers: &'a [(&'a HeaderName, &'a HeaderValue)],
    name: &HeaderName,
) -> impl Iterator<Item = &'a [u8]> + Clone + use<'a> {
    let name = name.clone();
    headers
        .iter()
        .filter(move |(candidate, _)| **candidate == name)
        .map(|(_, value)| value.as_bytes())
}

/// The value of a header that occurs exactly once.
fn single<'a>(
    headers: &'a [(&'a HeaderName, &'a HeaderValue)],
    name: &HeaderName,
) -> Option<&'a [u8]> {
    let mut found = values(headers, name);
    let value = found.next()?;
    found.next().is_none().then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(lines: &[(&'static str, &'static str)]) -> plakous_http::HeaderMap {
        let mut headers = plakous_http::HeaderMap::new();
        for (name, value) in lines {
            headers.append(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        headers
    }

    const NOW: Duration = Duration::from_secs(1_800_000_000);
    const FUTURE: &str = "4102444812";

    fn read(lines: &[(&'static str, &'static str)]) -> Option<SubscriptionLimit> {
        let headers = headers(lines);
        SubscriptionLimit::from_pairs(
            &headers.iter().collect::<Vec<_>>(),
            SystemTime::UNIX_EPOCH + NOW,
        )
    }

    #[test]
    fn each_reviewed_window_is_named_with_its_reset() {
        for (claim, name) in [
            ("five_hour", "session limit"),
            ("seven_day", "weekly limit"),
            ("seven_day_opus", "Opus limit"),
            ("seven_day_sonnet", "Sonnet limit"),
        ] {
            let limit = read(&[
                ("anthropic-ratelimit-unified-status", "rejected"),
                ("anthropic-ratelimit-unified-representative-claim", claim),
                ("anthropic-ratelimit-unified-reset", FUTURE),
            ])
            .unwrap_or_else(|| panic!("{claim} was not attested"));
            assert_eq!(
                limit.notice(),
                format!(
                    "You've hit your Claude {name}; try again after Fri, 01 Jan 2100 00:00:12 GMT, or switch to another Claude account."
                )
            );
        }
    }

    #[test]
    fn a_missing_past_or_malformed_reset_keeps_the_limit_without_a_time() {
        for reset in [
            None,
            Some("1700000000"),
            Some("soon"),
            Some("1.5e9"),
            Some(""),
        ] {
            let mut lines = vec![
                ("anthropic-ratelimit-unified-status", "rejected"),
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "seven_day",
                ),
            ];
            if let Some(reset) = reset {
                lines.push(("anthropic-ratelimit-unified-reset", reset));
            }
            let limit = read(&lines).unwrap_or_else(|| panic!("{reset:?} lost the limit"));
            assert_eq!(
                limit.notice(),
                "You've hit your Claude weekly limit; try again after it resets, or switch to another Claude account."
            );
        }
    }

    #[test]
    fn anything_short_of_an_exact_rejected_window_claim_establishes_nothing() {
        let rejected = ("anthropic-ratelimit-unified-status", "rejected");
        let weekly = (
            "anthropic-ratelimit-unified-representative-claim",
            "seven_day",
        );
        for lines in [
            vec![],
            vec![weekly],
            vec![rejected],
            vec![("anthropic-ratelimit-unified-status", "allowed"), weekly],
            vec![
                ("anthropic-ratelimit-unified-status", "allowed_warning"),
                weekly,
            ],
            vec![rejected, rejected, weekly],
            vec![rejected, weekly, weekly],
            vec![
                rejected,
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "overage",
                ),
            ],
            vec![
                rejected,
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "seven_day_overage_included",
                ),
            ],
            vec![
                rejected,
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "unknown_limit",
                ),
            ],
            vec![
                rejected,
                weekly,
                ("anthropic-ratelimit-unified-overage-status", "allowed"),
            ],
            vec![
                rejected,
                weekly,
                (
                    "anthropic-ratelimit-unified-overage-status",
                    "allowed_warning",
                ),
            ],
        ] {
            assert_eq!(read(&lines), None, "{lines:?}");
        }
        assert!(
            read(&[
                rejected,
                weekly,
                ("anthropic-ratelimit-unified-overage-status", "rejected"),
            ])
            .is_some()
        );
    }
}
