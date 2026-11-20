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
//! | `host=<host>` | Only requests to this origin host (any host when absent). On `hop=gateway`, the logical upstream host |
//! | `path=<prefix>` | Only requests whose path and query start with this. On `hop=gateway`, the logical upstream path |
//! | `nth=<n>` | Only the n-th matching request, counting from 1 (every one when absent) |
//! | `hop=<gateway\|upstream>` | Which hop the rule applies to. Absent means `upstream` |
//! | `fault=<kind>` | `400`, `401`, `429`, `500`, `502`, `503`, `529`, `xai-refusal`, `connect`, `hang`, `write-reset`, `record-rejected`, `reset`, `eof`, `stall`, `relay-lost=<before-head\|after-head>` or `gateway-refusal=<code>` |
//! | `retry-after=<secs>` | Status faults: send this `Retry-After` |
//! | `retry-after-ms=<n>` | `gateway-refusal=busy`: the refusal's `retry_after_ms` |
//! | `credential-status=<status>` | `gateway-refusal=credential_not_ready`: the credential status |
//! | `supported=<versions>` | `gateway-refusal=version_unsupported`: `+`-separated versions (default `1`) |
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
//! under the body's own deadlines. Every rule counts the requests it matches
//! on its own hop, so ordinals are per rule, and the first rule that fires wins.
//!
//! `hop=upstream` (the default) applies to this process's provider client: the
//! direct client, or the in-process gateway's own upstream client. It is refused
//! when [`FaultPlacement::ExternalGateway`] is set, so a plan that cannot reach
//! a provider from this process is never silently fault-free. `hop=gateway`
//! matches the host and path of the exchange it is applied to.
//! `relay-lost` and `gateway-refusal` are hop 1 only.
//!
//! A relaying client applies its plan's `hop=gateway` rules to each relay
//! request it sends, matched on the logical upstream host and path, through
//! the hop mapping a real hop event takes: `connect`, `hang`, `write-reset`
//! and `record-rejected` fail the hop before the gateway holds the whole
//! request; a status or `gateway-refusal` answers in the gateway's place;
//! `relay-lost=before-head` sends the request and loses its answer;
//! `relay-lost=after-head` drops the hop right after the relayed head; a
//! stream fault applies to the relayed body. The dialed hop itself is
//! excluded from selection. A direct client placed on `hop=gateway` applies
//! hop-1 rules to its own exchange. Hop-2 rules are selected by the gateway's
//! upstream client.
//!
//! A gateway service process can use [`FaultPlan::parse_provider`]: today's grammar, with
//! no `hop` field and no hop-1 fault. [`PLAK_TEST_GATEWAY`] tells the host plan
//! which placement it is in (`in-process`, `external`, or unset for direct
//! local custody). `provider` makes the host plan fail closed, because that
//! process must use the provider grammar.

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

/// Where this process's gateway is, for a host fault plan.
///
/// Set [`PLAK_TEST_GATEWAY`] to `external` when `[gateway] address` names
/// another process, or `in-process` for the in-process gateway. Unset means
/// direct local custody. `provider` is the gateway service: the host plan
/// fails closed and the service reads [`FaultPlan::parse_provider`].
pub const PLAK_TEST_GATEWAY: &str = "PLAK_TEST_GATEWAY";

/// Which hop a fault rule applies to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultHop {
    /// The caller's exchange with the gateway.
    Gateway,
    /// The gateway's, or a direct client's, exchange with the provider.
    Upstream,
}

/// Whether an upstream fault in this process can reach a provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultPlacement {
    /// Direct local custody, or an in-process gateway in this process.
    Local,
    /// An external gateway address is configured. Upstream rules are refused.
    ExternalGateway,
}

/// Where `relay-lost` cuts hop 1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelayCut {
    /// The gateway may have dialed; no relay `Head` arrives.
    BeforeHead,
    /// A relay `Head` arrives, then hop 1 drops.
    AfterHead,
}

/// A parsed fault plan. Clones share their per-rule request counters.
#[derive(Clone, Debug)]
pub struct FaultPlan {
    rules: Arc<Vec<Rule>>,
    /// The hop this holder applies. Rules for the other hop are not counted.
    hop: FaultHop,
}

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
    /// `None` matches every hop. A parsed rule always sets this; the invalid
    /// environment plan leaves it unset so every exchange fails.
    hop: Option<FaultHop>,
    fault: Fault,
    matched: AtomicU64,
}

#[derive(Clone, Debug)]
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
    RelayLost(RelayCut),
    GatewayRefusal(GatewayRefusalFault),
    Stream {
        action: StreamAction,
        after: Threshold,
    },
}

/// A synthetic gateway refusal. The body is the protocol's refusal JSON.
#[derive(Clone, Debug)]
pub(super) struct GatewayRefusalFault {
    status: u16,
    code: String,
    body: String,
}

impl GatewayRefusalFault {
    pub(super) fn reply(&self) -> Vec<u8> {
        let reason = reason_phrase(self.status);
        format!(
            "HTTP/1.1 {} {reason}\r\ncontent-type: application/json\r\nplak-gateway-error: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            self.status,
            self.code,
            self.body.len(),
            self.body
        )
        .into_bytes()
    }
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
    RelayLost(RelayCut),
    GatewayRefusal(GatewayRefusalFault),
    Stream(Box<StreamFault>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Grammar {
    /// The host process: `hop` and the hop-1 faults.
    Host,
    /// The gateway service's provider hop: today's grammar, unchanged.
    Provider,
}

impl FaultPlan {
    /// Parses a host plan. Upstream rules are allowed ([`FaultPlacement::Local`]).
    ///
    /// # Errors
    ///
    /// Returns [`FaultSpecError`] naming the first rule that does not parse.
    pub fn parse(spec: &str) -> Result<Self, FaultSpecError> {
        Self::parse_for(spec, FaultPlacement::Local)
    }

    /// Parses a host plan for `placement`.
    ///
    /// # Errors
    ///
    /// Returns [`FaultSpecError`] naming the first rule that does not parse,
    /// including an upstream rule under [`FaultPlacement::ExternalGateway`].
    pub fn parse_for(spec: &str, placement: FaultPlacement) -> Result<Self, FaultSpecError> {
        Self::parse_with(spec, Grammar::Host, placement)
    }

    /// Parses the gateway service's provider-hop plan. `hop`, `relay-lost` and
    /// `gateway-refusal` are refused, which is today's grammar unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`FaultSpecError`] naming the first rule that does not parse.
    pub fn parse_provider(spec: &str) -> Result<Self, FaultSpecError> {
        Self::parse_with(spec, Grammar::Provider, FaultPlacement::Local)
    }

    fn parse_with(
        spec: &str,
        grammar: Grammar,
        placement: FaultPlacement,
    ) -> Result<Self, FaultSpecError> {
        let rules = spec
            .split(';')
            .map(str::trim)
            .filter(|rule| !rule.is_empty())
            .enumerate()
            .map(|(index, rule)| {
                parse_rule(rule, grammar).map_err(|reason| FaultSpecError {
                    rule: index + 1,
                    reason,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if placement == FaultPlacement::ExternalGateway {
            for (index, rule) in rules.iter().enumerate() {
                if rule.hop == Some(FaultHop::Upstream) {
                    return Err(FaultSpecError {
                        rule: index + 1,
                        reason: "hop=upstream does not apply when an external gateway address \
                                  is configured"
                            .to_owned(),
                    });
                }
            }
        }
        Ok(Self {
            rules: Arc::new(rules),
            hop: FaultHop::Upstream,
        })
    }

    /// Applies this plan on `hop`. The rules and their counters are shared.
    pub(super) fn with_hop(mut self, hop: FaultHop) -> Self {
        self.hop = hop;
        self
    }

    /// Counts this request against every rule on this plan's hop and returns
    /// the fault of the first rule whose ordinal it is. Rules on the other
    /// hop are not counted.
    pub(super) fn select(&self, host: &str, path_and_query: &str) -> Option<Selected> {
        let mut selected = None;
        let hop = self.hop;
        for rule in self.rules.iter() {
            if rule.hop.is_some_and(|rule_hop| rule_hop != hop) {
                continue;
            }
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
                selected = Some(selected_fault(&rule.fault));
            }
        }
        selected
    }
}

fn selected_fault(fault: &Fault) -> Selected {
    match fault {
        Fault::Status {
            status,
            retry_after,
        } => Selected::Status {
            status: *status,
            retry_after: *retry_after,
        },
        Fault::Connect => Selected::Connect,
        Fault::Hang(duration) => Selected::Hang(*duration),
        Fault::WriteReset => Selected::WriteReset,
        Fault::RecordRejected => Selected::RecordRejected,
        Fault::Invalid => Selected::Invalid,
        Fault::RelayLost(cut) => Selected::RelayLost(*cut),
        Fault::GatewayRefusal(refusal) => Selected::GatewayRefusal(refusal.clone()),
        Fault::Stream { action, after } => {
            Selected::Stream(Box::new(StreamFault::new(*action, *after)))
        }
    }
}

/// The process plan from `PLAK_TEST_FAULTS`, parsed once and shared by every
/// client so request ordinals are process-wide.
pub(super) fn from_env() -> Option<FaultPlan> {
    static PLAN: OnceLock<Option<FaultPlan>> = OnceLock::new();
    PLAN.get_or_init(|| plan_from_placement(read_env(FAULTS_ENV).as_deref(), placement_from_env()))
        .clone()
}

pub(super) fn read_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn placement_from_env() -> Option<FaultPlacement> {
    match read_env(PLAK_TEST_GATEWAY).as_deref() {
        None | Some("" | "in-process") => Some(FaultPlacement::Local),
        Some("external") => Some(FaultPlacement::ExternalGateway),
        Some(_) => None,
    }
}

/// A run with a mistyped plan must not quietly run fault-free, and the
/// transport cannot report it on stderr (the product may hold the stderr
/// lock), so every exchange then fails before it starts with a failure no
/// network event produces: `Failure(Resolution, NotStarted, Internal)`.
#[cfg(test)]
fn plan_from(spec: Option<&str>) -> Option<FaultPlan> {
    plan_from_placement(spec, Some(FaultPlacement::Local))
}

fn plan_from_placement(spec: Option<&str>, placement: Option<FaultPlacement>) -> Option<FaultPlan> {
    let spec = spec?;
    let Some(placement) = placement else {
        return Some(invalid_plan());
    };
    Some(FaultPlan::parse_for(spec, placement).unwrap_or_else(|_| invalid_plan()))
}

fn invalid_plan() -> FaultPlan {
    FaultPlan {
        rules: Arc::new(vec![Rule {
            host: None,
            path: None,
            nth: None,
            hop: None,
            fault: Fault::Invalid,
            matched: AtomicU64::new(0),
        }]),
        hop: FaultHop::Upstream,
    }
}

struct RuleFields {
    host: Option<String>,
    path: Option<String>,
    nth: Option<u64>,
    hop: FaultHop,
    kind: Option<String>,
    retry_after: Option<u64>,
    after: Option<Threshold>,
    seconds: Option<Duration>,
    retry_after_ms: Option<u64>,
    credential_status: Option<String>,
    supported: Option<String>,
}

fn parse_rule(rule: &str, grammar: Grammar) -> Result<Rule, String> {
    let fields = parse_fields(rule, grammar)?;
    let kind = fields.kind.as_deref().ok_or("`fault` is required")?;
    let fault = parse_fault(kind, &fields, grammar)?;
    if matches!(fault, Fault::RelayLost(_) | Fault::GatewayRefusal(_))
        && fields.hop != FaultHop::Gateway
    {
        return Err("`relay-lost` and `gateway-refusal` require hop=gateway".to_owned());
    }
    let stream = matches!(fault, Fault::Stream { .. });
    if fields.retry_after.is_some() && !matches!(fault, Fault::Status { .. }) {
        return Err("`retry-after` applies to status faults only".to_owned());
    }
    if fields.after.is_some() && !stream {
        return Err("`after-events`/`after-bytes` apply to stream faults only".to_owned());
    }
    if fields.seconds.is_some()
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
    if fields.retry_after_ms.is_some() && !matches!(fault, Fault::GatewayRefusal(_)) {
        return Err("`retry-after-ms` applies to `gateway-refusal=busy` only".to_owned());
    }
    if fields.credential_status.is_some() && !matches!(fault, Fault::GatewayRefusal(_)) {
        return Err(
            "`credential-status` applies to `gateway-refusal=credential_not_ready` only".to_owned(),
        );
    }
    if fields.supported.is_some() && !matches!(fault, Fault::GatewayRefusal(_)) {
        return Err("`supported` applies to `gateway-refusal=version_unsupported` only".to_owned());
    }
    Ok(Rule {
        host: fields.host,
        path: fields.path,
        nth: fields.nth,
        hop: Some(fields.hop),
        fault,
        matched: AtomicU64::new(0),
    })
}

fn parse_fields(rule: &str, grammar: Grammar) -> Result<RuleFields, String> {
    let mut fields = RuleFields {
        host: None,
        path: None,
        nth: None,
        hop: FaultHop::Upstream,
        kind: None,
        retry_after: None,
        after: None,
        seconds: None,
        retry_after_ms: None,
        credential_status: None,
        supported: None,
    };
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
            "host" => fields.host = Some(value.to_owned()),
            "path" => fields.path = Some(value.to_owned()),
            "nth" => {
                let ordinal = number()?;
                if ordinal == 0 {
                    return Err("`nth` counts from 1".to_owned());
                }
                fields.nth = Some(ordinal);
            }
            "hop" => fields.hop = parse_hop(value, grammar)?,
            "fault" => fields.kind = Some(value.to_owned()),
            "retry-after" => fields.retry_after = Some(number()?),
            "retry-after-ms" => {
                refuse_provider_field(grammar, key)?;
                fields.retry_after_ms = Some(number()?);
            }
            "credential-status" => {
                refuse_provider_field(grammar, key)?;
                fields.credential_status = Some(value.to_owned());
            }
            "supported" => {
                refuse_provider_field(grammar, key)?;
                fields.supported = Some(value.to_owned());
            }
            "after-events" => fields.after = Some(Threshold::Events(to_usize(number()?)?)),
            "after-bytes" => fields.after = Some(Threshold::Bytes(to_usize(number()?)?)),
            "seconds" => {
                fields.seconds = Some(
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
    Ok(fields)
}

fn parse_hop(value: &str, grammar: Grammar) -> Result<FaultHop, String> {
    refuse_provider_field(grammar, "hop")?;
    match value {
        "gateway" => Ok(FaultHop::Gateway),
        "upstream" => Ok(FaultHop::Upstream),
        other => Err(format!("unknown hop `{other}`")),
    }
}

fn refuse_provider_field(grammar: Grammar, key: &str) -> Result<(), String> {
    if grammar == Grammar::Provider {
        Err(format!("unknown field `{key}`"))
    } else {
        Ok(())
    }
}

fn parse_fault(kind: &str, fields: &RuleFields, grammar: Grammar) -> Result<Fault, String> {
    if grammar == Grammar::Provider
        && (kind.starts_with("relay-lost") || kind.starts_with("gateway-refusal"))
    {
        return Err(format!("unknown fault `{kind}`"));
    }
    Ok(match kind {
        "400" | "401" | "429" | "500" | "502" | "503" | "529" => Fault::Status {
            status: kind.parse().map_err(|_| "status")?,
            retry_after: fields.retry_after,
        },
        "xai-refusal" => Fault::Status {
            status: 403,
            retry_after: fields.retry_after,
        },
        "connect" => Fault::Connect,
        "hang" => Fault::Hang(fields.seconds.ok_or("`hang` needs `seconds`")?),
        "write-reset" => Fault::WriteReset,
        "record-rejected" => Fault::RecordRejected,
        "reset" | "eof" | "stall" => Fault::Stream {
            action: match kind {
                "reset" => StreamAction::Reset,
                "eof" => StreamAction::Eof,
                _ => StreamAction::Stall(fields.seconds.ok_or("`stall` needs `seconds`")?),
            },
            after: fields.after.unwrap_or(Threshold::Events(0)),
        },
        "relay-lost=before-head" => Fault::RelayLost(RelayCut::BeforeHead),
        "relay-lost=after-head" => Fault::RelayLost(RelayCut::AfterHead),
        other => {
            let Some(code) = other.strip_prefix("gateway-refusal=") else {
                return Err(format!("unknown fault `{other}`"));
            };
            Fault::GatewayRefusal(gateway_refusal(code, fields)?)
        }
    })
}

fn gateway_refusal(code: &str, fields: &RuleFields) -> Result<GatewayRefusalFault, String> {
    let (status, body) = match code {
        "unauthenticated" => (401, unit_body(code)),
        "scope_denied" => (403, unit_body(code)),
        "account_unknown" => (404, unit_body(code)),
        "account_changed" => (409, unit_body(code)),
        "upstream_not_pinned"
        | "reserved_header"
        | "custodied_headers_mismatch"
        | "envelope_invalid" => (422, unit_body(code)),
        "limit_exceeded" => (413, unit_body(code)),
        "internal" => (500, unit_body(code)),
        "gateway_disabled" | "account_disabled" => (503, unit_body(code)),
        "credential_not_ready" => {
            let status_name = fields
                .credential_status
                .as_deref()
                .ok_or("`credential_not_ready` needs `credential-status`")?;
            if !CREDENTIAL_STATUSES.contains(&status_name) {
                return Err(format!("unknown credential status `{status_name}`"));
            }
            (
                409,
                format!(
                    r#"{{"gateway_error":{{"code":"credential_not_ready","status":"{status_name}"}}}}"#
                ),
            )
        }
        "busy" => {
            let retry_after_ms = fields
                .retry_after_ms
                .ok_or("`busy` needs `retry-after-ms`")?;
            (
                503,
                format!(
                    r#"{{"gateway_error":{{"code":"busy","retry_after_ms":{retry_after_ms}}}}}"#
                ),
            )
        }
        "version_unsupported" => {
            let supported = supported_list(fields.supported.as_deref())?;
            (
                426,
                format!(
                    r#"{{"gateway_error":{{"code":"version_unsupported","supported":[{supported}]}}}}"#
                ),
            )
        }
        "fence_moved" => {
            return Err(
                "`fence_moved` is scripted on the fixture gateway, not injected as a status"
                    .to_owned(),
            );
        }
        other => return Err(format!("unknown gateway refusal `{other}`")),
    };
    if code != "busy" && fields.retry_after_ms.is_some() {
        return Err("`retry-after-ms` applies to `gateway-refusal=busy` only".to_owned());
    }
    if code != "credential_not_ready" && fields.credential_status.is_some() {
        return Err(
            "`credential-status` applies to `gateway-refusal=credential_not_ready` only".to_owned(),
        );
    }
    if code != "version_unsupported" && fields.supported.is_some() {
        return Err("`supported` applies to `gateway-refusal=version_unsupported` only".to_owned());
    }
    Ok(GatewayRefusalFault {
        status,
        code: code.to_owned(),
        body,
    })
}

fn unit_body(code: &str) -> String {
    format!(r#"{{"gateway_error":{{"code":"{code}"}}}}"#)
}

fn supported_list(value: Option<&str>) -> Result<String, String> {
    let value = value.unwrap_or("1");
    if value.is_empty() {
        return Err("`supported` needs a version".to_owned());
    }
    let mut versions = Vec::new();
    for version in value.split('+') {
        let version = version
            .parse::<u16>()
            .map_err(|_| format!("`supported` needs versions, not `{value}`"))?;
        versions.push(version.to_string());
    }
    Ok(versions.join(","))
}

const CREDENTIAL_STATUSES: &[&str] = &[
    "signed_out",
    "ready",
    "refresh_required",
    "refreshing",
    "quarantined",
    "login_required",
    "profile_changed",
    "written_by_newer_build",
    "revoked",
];

fn reason_phrase(status: u16) -> &'static str {
    match status {
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Content Too Large",
        422 => "Unprocessable Content",
        426 => "Upgrade Required",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn to_usize(value: u64) -> Result<usize, String> {
    usize::try_from(value).map_err(|_| format!("`{value}` is too large"))
}

pub(super) enum Applied {
    Done(Box<Response>),
    Continue(ExchangeControl, Option<Box<StreamFault>>),
}

pub(super) async fn apply(
    plan: Option<&FaultPlan>,
    host: &str,
    path_and_query: &str,
    control: ExchangeControl,
    limits: &Limits,
) -> Result<Applied, Failure> {
    let selected = plan.and_then(|plan| plan.select(host, path_and_query));
    match selected {
        None => Ok(Applied::Continue(control, None)),
        Some(Selected::Invalid) => Err(Failure::new(
            Phase::Resolution,
            TransmissionFact::NotStarted,
            ErrorKind::Internal,
        )),
        Some(Selected::Connect) => {
            Err(
                Failure::new(Phase::Connect, TransmissionFact::NotStarted, ErrorKind::Io)
                    .with_io_cause(&std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
            )
        }
        Some(Selected::Hang(duration)) => Err(injected_hang(duration, &control).await),
        Some(Selected::WriteReset) => Err(Failure::new(
            Phase::RequestWriteOrResponseHead,
            TransmissionFact::RequestIncomplete,
            ErrorKind::Protocol,
        )),
        Some(Selected::RecordRejected) => Err(Failure::new(
            Phase::RequestWriteOrResponseHead,
            TransmissionFact::RequestIncomplete,
            ErrorKind::Protocol,
        )
        .with_peer_rejected_record()
        .with_wire_protocol(WireProtocol::Http1)),
        Some(Selected::Status {
            status,
            retry_after,
        }) => Ok(Applied::Done(Box::new(
            injected_status(status, retry_after, control, limits).await?,
        ))),
        Some(Selected::RelayLost(RelayCut::BeforeHead)) => Err(Failure::new(
            Phase::ResponseHead,
            TransmissionFact::MayHaveTransmitted,
            ErrorKind::Io,
        )),
        Some(Selected::RelayLost(RelayCut::AfterHead)) => Ok(Applied::Done(Box::new(
            injected_relay_lost_after_head(control, limits).await?,
        ))),
        Some(Selected::GatewayRefusal(refusal)) => Ok(Applied::Done(Box::new(
            injected_reply(refusal.reply(), control, limits).await?,
        ))),
        Some(Selected::Stream(stream_fault)) => Ok(Applied::Continue(control, Some(stream_fault))),
    }
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
pub(crate) struct StreamFault {
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
    injected_reply(status_reply(status, retry_after), control, limits).await
}

/// Answers with a relay `Head` frame and then drops the peer, leaving
/// `content-length` unsatisfied so the caller sees a mid-body break.
pub(super) async fn injected_relay_lost_after_head(
    control: ExchangeControl,
    limits: &Limits,
) -> Result<Response, Failure> {
    let head = head_frame();
    let claimed = head.len() + 64;
    let reply = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.plak.relay-frames\r\ncontent-length: {claimed}\r\nconnection: close\r\n\r\n"
    );
    let mut bytes = reply.into_bytes();
    bytes.extend_from_slice(&head);
    injected_reply_then_drop(bytes, control, limits).await
}

pub(super) async fn injected_reply(
    reply: Vec<u8>,
    control: ExchangeControl,
    limits: &Limits,
) -> Result<Response, Failure> {
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
    finish_injected(client_io, control, limits).await
}

/// Writes `prefix` and drops the peer without a clean close.
async fn injected_reply_then_drop(
    prefix: Vec<u8>,
    control: ExchangeControl,
    limits: &Limits,
) -> Result<Response, Failure> {
    let (client_io, mut peer_io) = duplex(64 * 1_024);
    tokio::spawn(async move {
        let mut head = Vec::new();
        let mut buffer = [0_u8; 512];
        while !head.ends_with(b"\r\n\r\n") {
            match peer_io.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => head.extend_from_slice(&buffer[..read]),
            }
        }
        let _ = peer_io.write_all(&prefix).await;
    });
    finish_injected(client_io, control, limits).await
}

async fn finish_injected(
    client_io: tokio::io::DuplexStream,
    control: ExchangeControl,
    limits: &Limits,
) -> Result<Response, Failure> {
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

fn head_frame() -> Vec<u8> {
    let payload = br#"{"status":200,"headers":[]}"#;
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(0x01);
    let length = u32::try_from(payload.len()).unwrap_or(0);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
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
    let retry_after = retry_after.map_or_default(|seconds| format!("retry-after: {seconds}\r\n"));
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
