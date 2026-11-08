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
//! | `fault=<kind>` | `400`, `401`, `429`, `500`, `502`, `503`, `529`, `xai-refusal`, `connect`, `hang`, `write-reset`, `record-rejected`, `reset`, `eof` or `stall` |
//! | `retry-after=<secs>` | Status faults: send this `Retry-After` |
//! | `after-events=<n>` / `after-bytes=<n>` | Stream faults: deliver this many SSE events (comment-only blocks such as keep-alives do not count) or bytes of the body first |
//! | `seconds=<secs>` | `hang`: pre-connect wait; `stall`: how long the real body goes silent |
//!
//! Status faults answer from an in-memory peer before anything is resolved,
//! connected or sent upstream; `xai-refusal` is an HTTP 403 carrying xAI's
//! pre-generation usage-guideline refusal body. `connect` fails the exchange in the connect
//! phase. `hang` waits without resolving or connecting, then times out in the
//! connect phase; cancellation and the exchange deadline bound the wait.
//! `write-reset` fails it as a peer that resets the connection while
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
    Hang(Duration),
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
    Hang(Duration),
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
                    Fault::Hang(duration) => Selected::Hang(duration),
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
        "400" | "401" | "429" | "500" | "502" | "503" | "529" => Fault::Status {
            status: kind.parse().map_err(|_| "status")?,
            retry_after,
        },
        "xai-refusal" => Fault::Status {
            status: 403,
            retry_after,
        },
        "connect" => Fault::Connect,
        "hang" => Fault::Hang(seconds.ok_or("`hang` needs `seconds`")?),
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
            Fault::Hang(_)
                | Fault::Stream {
                    action: StreamAction::Stall(_),
                    ..
                }
        )
    {
        return Err("`seconds` applies to `hang` or `stall` only".to_owned());
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

pub(super) async fn injected_hang(duration: Duration, control: &ExchangeControl) -> Failure {
    let kind = tokio::select! {
        () = control.cancellation().cancelled() => ErrorKind::Cancelled,
        () = tokio::time::sleep_until(control.deadline().into()) => ErrorKind::DeadlineExceeded,
        () = tokio::time::sleep(duration) => ErrorKind::DeadlineExceeded,
    };
    Failure::new(Phase::Connect, TransmissionFact::NotStarted, kind)
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
        400 => "Bad Request",
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
mod tests;
