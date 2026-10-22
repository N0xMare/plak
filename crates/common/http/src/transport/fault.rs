//! Repository-only transport fault injection (`fault-injection` feature).
//!
//! A [`FaultPlan`] makes chosen exchanges fail the way a real network or
//! provider would, so the classifiers above the transport see the same
//! [`Failure`]s and response heads they see in the field. The plak binary
//! reads its plan from `PLAK_TEST_FAULTS`; tests attach one with
//! [`Client::with_faults`](super::Client::with_faults).
//!
//! A plan is `;`-separated rules of `,`-separated `key=value` fields:
//!
//! | Field | Meaning |
//! | --- | --- |
//! | `host=<host>` | Only requests to this origin host (any host when absent) |
//! | `path=<prefix>` | Only requests whose path and query start with this |
//! | `nth=<n>` | Only the n-th matching request, counting from 1 (every one when absent) |
//! | `fault=<kind>` | `401`, `429`, `500`, `502`, `503`, `529`, `xai-refusal`, `connect`, `write-reset`, `record-rejected`, `reset`, `eof` or `stall` |
//! | `retry-after=<secs>` | Status faults: send this `Retry-After` |
//! | `after-events=<n>` / `after-bytes=<n>` | Stream faults: deliver this many SSE events (comment-only blocks such as keep-alives do not count) or bytes of the body first |
//! | `seconds=<secs>` | `stall`: how long the body goes silent |
//!
//! Status faults answer from an in-memory peer before anything is resolved,
//! connected or sent upstream; `xai-refusal` is an HTTP 403 carrying xAI's
//! pre-generation usage-guideline refusal body. `connect` fails the exchange in the connect
//! phase. `write-reset` fails it as a peer that resets the connection while
//! the request is still being written does. `record-rejected` supplies the
//! HTTP/1.1 TLS `bad_record_mac` proof without connecting or sending content.
//! `reset` and `eof` let the real response through and then end its
//! body with a transport error or a clean end of body; `stall` pauses it
//! under the body's own deadlines. Every rule counts the requests it matches,
//! so ordinals are per rule, and the first rule that fires wins.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http::Request as WireRequest;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, duplex};

use super::{
    ConnectionTask, RequestBody, RequestProgress, Response, internal_request_failure,
    response_from_wire, wait_response,
};
use crate::{ErrorKind, ExchangeControl, Failure, Limits, Phase, TransmissionFact, WireProtocol};

/// The environment variable the plak binary reads its plan from.
pub const FAULTS_ENV: &str = "PLAK_TEST_FAULTS";

/// A parsed fault plan. Clones share their per-rule request counters.
#[derive(Clone, Debug)]
pub struct FaultPlan(Arc<Vec<Rule>>);

/// A `PLAK_TEST_FAULTS` value that does not parse.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{FAULTS_ENV} rule {rule}: {reason}")]
pub struct FaultSpecError {
    rule: usize,
    reason: String,
}

#[derive(Debug)]
struct Rule {
    host: Option<String>,
    path: Option<String>,
    nth: Option<u64>,
    fault: Fault,
    matched: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    Status {
        status: u16,
        retry_after: Option<u64>,
    },
    Connect,
    WriteReset,
    RecordRejected,
    Invalid,
    Stream {
        action: StreamAction,
        after: Threshold,
    },
}

/// What happens to a response body once its threshold is reached.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StreamAction {
    Reset,
    Eof,
    Stall(Duration),
}

#[derive(Clone, Copy, Debug)]
enum Threshold {
    Events(usize),
    Bytes(usize),
}

/// The fault chosen for one exchange.
pub(super) enum Selected {
    Status {
        status: u16,
        retry_after: Option<u64>,
    },
    Connect,
    WriteReset,
    RecordRejected,
    Invalid,
    Stream(Box<StreamFault>),
}

impl FaultPlan {
    /// Parses a plan in the `PLAK_TEST_FAULTS` syntax.
    ///
    /// # Errors
    ///
    /// Returns [`FaultSpecError`] naming the first rule that does not parse.
    pub fn parse(spec: &str) -> Result<Self, FaultSpecError> {
        spec.split(';')
            .map(str::trim)
            .filter(|rule| !rule.is_empty())
            .enumerate()
            .map(|(index, rule)| {
                parse_rule(rule).map_err(|reason| FaultSpecError {
                    rule: index + 1,
                    reason,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|rules| Self(Arc::new(rules)))
    }

    /// Counts this request against every matching rule and returns the fault
    /// of the first rule whose ordinal it is.
    pub(super) fn select(&self, host: &str, path_and_query: &str) -> Option<Selected> {
        let mut selected = None;
        for rule in self.0.iter() {
            if rule
                .host
                .as_deref()
                .is_some_and(|want| !want.eq_ignore_ascii_case(host))
                || rule
                    .path
                    .as_deref()
                    .is_some_and(|want| !path_and_query.starts_with(want))
            {
                continue;
            }
            let ordinal = rule.matched.fetch_add(1, Ordering::Relaxed) + 1;
            if selected.is_none() && rule.nth.is_none_or(|nth| nth == ordinal) {
                selected = Some(match rule.fault {
                    Fault::Status {
                        status,
                        retry_after,
                    } => Selected::Status {
                        status,
                        retry_after,
                    },
                    Fault::Connect => Selected::Connect,
                    Fault::WriteReset => Selected::WriteReset,
                    Fault::RecordRejected => Selected::RecordRejected,
                    Fault::Invalid => Selected::Invalid,
                    Fault::Stream { action, after } => {
                        Selected::Stream(Box::new(StreamFault::new(action, after)))
                    }
                });
            }
        }
        selected
    }
}

/// The process plan from `PLAK_TEST_FAULTS`, parsed once and shared by every
/// client so request ordinals are process-wide.
pub(super) fn from_env() -> Option<FaultPlan> {
    static PLAN: OnceLock<Option<FaultPlan>> = OnceLock::new();
    PLAN.get_or_init(|| plan_from(std::env::var(FAULTS_ENV).ok().as_deref()))
        .clone()
}

/// A run with a mistyped plan must not quietly run fault-free, and the
/// transport cannot report it on stderr (the product may hold the stderr
/// lock), so every exchange then fails before it starts with a failure no
/// network event produces: `Failure(Resolution, NotStarted, Internal)`.
fn plan_from(spec: Option<&str>) -> Option<FaultPlan> {
    let plan = FaultPlan::parse(spec?).unwrap_or_else(|_| {
        FaultPlan(Arc::new(vec![Rule {
            host: None,
            path: None,
            nth: None,
            fault: Fault::Invalid,
            matched: AtomicU64::new(0),
        }]))
    });
    Some(plan)
}

fn parse_rule(rule: &str) -> Result<Rule, String> {
    let mut host = None;
    let mut path = None;
    let mut nth = None;
    let mut kind = None;
    let mut retry_after = None;
    let mut after = None;
    let mut seconds = None;
    for field in rule.split(',').map(str::trim) {
        let (key, value) = field
            .split_once('=')
            .ok_or_else(|| format!("`{field}` is not key=value"))?;
        let number = || {
            value
                .parse::<u64>()
                .map_err(|_| format!("`{key}` needs a whole number, not `{value}`"))
        };
        match key {
            "host" => host = Some(value.to_owned()),
            "path" => path = Some(value.to_owned()),
            "nth" => {
                let ordinal = number()?;
                if ordinal == 0 {
                    return Err("`nth` counts from 1".to_owned());
                }
                nth = Some(ordinal);
            }
            "fault" => kind = Some(value),
            "retry-after" => retry_after = Some(number()?),
            "after-events" => after = Some(Threshold::Events(to_usize(number()?)?)),
            "after-bytes" => after = Some(Threshold::Bytes(to_usize(number()?)?)),
            "seconds" => {
                seconds = Some(
                    value
                        .parse::<f64>()
                        .ok()
                        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
                        .ok_or_else(|| format!("`seconds` needs a duration, not `{value}`"))?,
                );
            }
            _ => return Err(format!("unknown field `{key}`")),
        }
    }
    let kind = kind.ok_or("`fault` is required")?;
    let fault = match kind {
        "401" | "429" | "500" | "502" | "503" | "529" => Fault::Status {
            status: kind.parse().map_err(|_| "status")?,
            retry_after,
        },
        "xai-refusal" => Fault::Status {
            status: 403,
            retry_after,
        },
        "connect" => Fault::Connect,
        "write-reset" => Fault::WriteReset,
        "record-rejected" => Fault::RecordRejected,
        "reset" | "eof" | "stall" => Fault::Stream {
            action: match kind {
                "reset" => StreamAction::Reset,
                "eof" => StreamAction::Eof,
                _ => StreamAction::Stall(seconds.ok_or("`stall` needs `seconds`")?),
            },
            after: after.unwrap_or(Threshold::Events(0)),
        },
        other => return Err(format!("unknown fault `{other}`")),
    };
    let stream = matches!(fault, Fault::Stream { .. });
    if retry_after.is_some() && !matches!(fault, Fault::Status { .. }) {
        return Err("`retry-after` applies to status faults only".to_owned());
    }
    if after.is_some() && !stream {
        return Err("`after-events`/`after-bytes` apply to stream faults only".to_owned());
    }
    if seconds.is_some()
        && !matches!(
            fault,
            Fault::Stream {
                action: StreamAction::Stall(_),
                ..
            }
        )
    {
        return Err("`seconds` applies to `stall` only".to_owned());
    }
    Ok(Rule {
        host,
        path,
        nth,
        fault,
        matched: AtomicU64::new(0),
    })
}

fn to_usize(value: u64) -> Result<usize, String> {
    usize::try_from(value).map_err(|_| format!("`{value}` is too large"))
}

/// A stream fault's progress through one response body.
#[derive(Debug)]
pub(super) struct StreamFault {
    action: StreamAction,
    after: Threshold,
    bytes: usize,
    events: usize,
    previous: u8,
    block_has_field: bool,
    reached: bool,
    fired: bool,
}

impl StreamFault {
    fn new(action: StreamAction, after: Threshold) -> Self {
        let reached = matches!(after, Threshold::Events(0) | Threshold::Bytes(0));
        Self {
            action,
            after,
            bytes: 0,
            events: 0,
            previous: b'\n',
            block_has_field: false,
            reached,
            fired: false,
        }
    }

    /// Returns the action once, on the first pull after the threshold.
    pub(super) fn due(&mut self) -> Option<StreamAction> {
        (self.reached && !self.fired).then(|| {
            self.fired = true;
            self.action
        })
    }

    /// Whether bytes past the threshold are delivered after the action.
    pub(super) fn keeps_remainder(&self) -> bool {
        matches!(self.action, StreamAction::Stall(_))
    }

    /// Counts delivered bytes and returns where the threshold falls inside
    /// `data`, if it does. A blank line ends an SSE block, and the block is
    /// an event only if one of its lines is not a `:` comment, so keep-alive
    /// blocks dispatch nothing and count nothing. `\r` is ignored so CRLF
    /// framing counts the same.
    pub(super) fn cut_point(&mut self, data: &[u8]) -> Option<usize> {
        if self.reached {
            return None;
        }
        for (index, &byte) in data.iter().enumerate() {
            self.bytes += 1;
            let line_start = self.previous == b'\n';
            if byte == b'\n' && line_start && self.block_has_field {
                self.events += 1;
                self.block_has_field = false;
            } else if line_start && !matches!(byte, b'\r' | b'\n' | b':') {
                self.block_has_field = true;
            }
            if byte != b'\r' {
                self.previous = byte;
            }
            self.reached = match self.after {
                Threshold::Events(events) => self.events >= events,
                Threshold::Bytes(bytes) => self.bytes >= bytes,
            };
            if self.reached {
                return Some(index + 1);
            }
        }
        None
    }
}

/// Answers one exchange with a synthetic status from an in-memory HTTP/1.1
/// peer. The caller's request is never sent anywhere; the response head and
/// body then pass through the same parsing and limits as a real one.
pub(super) async fn injected_status(
    status: u16,
    retry_after: Option<u64>,
    control: ExchangeControl,
    limits: &Limits,
) -> Result<Response, Failure> {
    let reply = status_reply(status, retry_after);
    let (client_io, mut peer_io) = duplex(64 * 1_024);
    tokio::spawn(async move {
        // Answer only after the stand-in request head arrives: an HTTP/1.1
        // client treats bytes before its request as a protocol error.
        let mut head = Vec::new();
        let mut buffer = [0_u8; 512];
        while !head.ends_with(b"\r\n\r\n") {
            match peer_io.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => head.extend_from_slice(&buffer[..read]),
            }
        }
        if peer_io.write_all(&reply).await.is_ok() {
            let _ = peer_io.shutdown().await;
        }
    });
    let not_started = |kind| Failure::new(Phase::Connect, TransmissionFact::NotStarted, kind);
    let (mut sender, connection) = http1::Builder::new()
        .handshake(TokioIo::new(client_io))
        .await
        .map_err(|_| not_started(ErrorKind::Protocol))?;
    let connection = ConnectionTask::new(tokio::spawn(connection), None);
    let progress = Arc::new(RequestProgress::new(true, false, 0));
    let request = WireRequest::builder()
        .uri("/")
        .header(http::header::HOST, "fault.invalid")
        .body(RequestBody::new(Bytes::new(), Arc::clone(&progress)))
        .map_err(internal_request_failure)?;
    let (response, connection) = wait_response(
        sender.send_request(request),
        connection,
        progress,
        &control,
        limits,
    )
    .await?;
    response_from_wire(
        response,
        connection,
        control,
        WireProtocol::Http1,
        None,
        None,
        limits,
    )
    .await
}

fn status_reply(status: u16, retry_after: Option<u64>) -> Vec<u8> {
    let body = if status == 403 {
        // xAI's pre-generation usage-guideline refusal, byte for byte.
        r#"{"code":"permission-denied","error":"I'm sorry, I can't help with that request."}"#
            .to_owned()
    } else {
        dialect_error_body(status)
    };
    let reason = match status {
        401 => "Unauthorized",
        403 => "Forbidden",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Overloaded",
    };
    let retry_after = retry_after
        .map(|seconds| format!("retry-after: {seconds}\r\n"))
        .unwrap_or_default();
    format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n{retry_after}x-plak-injected-fault: {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn dialect_error_body(status: u16) -> String {
    let error_type = match status {
        401 => "authentication_error",
        429 => "rate_limit_error",
        500 | 502 | 503 => "api_error",
        _ => "overloaded_error",
    };
    // One body both error dialects read: Anthropic's `type`/`error.type` and
    // the OpenAI-style `error.code`/`error.message`.
    format!(
        r#"{{"type":"error","error":{{"type":"{error_type}","code":{status},"message":"injected by {FAULTS_ENV}"}}}}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fired(plan: &FaultPlan, host: &str, path: &str) -> Option<&'static str> {
        plan.select(host, path).map(|selected| match selected {
            Selected::Status { .. } => "status",
            Selected::Connect => "connect",
            Selected::WriteReset => "write-reset",
            Selected::RecordRejected => "record-rejected",
            Selected::Invalid => "invalid",
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
    fn malformed_rules_name_the_rule_and_the_problem() {
        for (spec, reason) in [
            ("fault=418", "unknown fault `418`"),
            ("nth=0,fault=500", "`nth` counts from 1"),
            ("fault=stall", "`stall` needs `seconds`"),
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
}
