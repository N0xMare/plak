use super::{Deserialize, Digest, ModelEvent, SafeDiagnostic, Serialize, fmt};

/// A bounded provider-neutral diagnostic emitted by model preparation or execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelDiagnostic {
    message: SafeDiagnostic,
    #[serde(skip)]
    history_projection: Option<Digest>,
}

impl ModelDiagnostic {
    /// Creates a bounded, credential-safe diagnostic.
    #[must_use]
    pub fn new(message: impl AsRef<str>) -> Self {
        Self {
            message: SafeDiagnostic::new(message),
            history_projection: None,
        }
    }

    /// Creates a credential-safe notice for one historical projection transition.
    #[must_use]
    pub fn historical_projection(identity: Digest, message: impl AsRef<str>) -> Self {
        Self {
            message: SafeDiagnostic::new(message),
            history_projection: Some(identity),
        }
    }

    /// Returns the transition identity, present only on historical projection notices.
    #[must_use]
    pub const fn history_projection_identity(&self) -> Option<Digest> {
        self.history_projection
    }

    /// Returns the retained bounded diagnostic.
    #[must_use]
    pub const fn message(&self) -> &SafeDiagnostic {
        &self.message
    }
}

/// The transport or provider fault that broke a physical request a kernel is
/// about to send again.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTransportFault {
    /// The connection failed or was reset.
    Io,
    /// HTTP framing or protocol processing failed.
    Protocol,
    /// TLS failed.
    Tls,
    /// A deadline or the stream's idle limit elapsed.
    Timeout,
    /// The response stream ended before its terminal event.
    EndOfStream,
    /// The provider reported itself overloaded inside the stream.
    Overloaded,
    /// The provider reported a rate limit inside the stream.
    RateLimited,
}

impl fmt::Display for ModelTransportFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Io => "connection lost",
            Self::Protocol => "protocol error",
            Self::Tls => "TLS error",
            Self::Timeout => "timed out",
            Self::EndOfStream => "stream ended early",
            Self::Overloaded => "provider overloaded",
            Self::RateLimited => "provider rate limited",
        })
    }
}

/// Why a kernel is sending a turn request again.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelRetryCause {
    /// The provider answered with this HTTP status before any response stream.
    HttpStatus {
        /// The status code.
        status: u16,
    },
    /// The connection could not be established, or the request never left
    /// whole; the provider cannot have acted on it.
    Connect,
    /// The peer killed the connection after rejecting a TLS record of this
    /// incomplete request; no provider action can have occurred.
    RecordRejectedInTransit,
    /// The request may have reached the provider, and the exchange failed
    /// before any response byte arrived.
    TransportAfterSend {
        /// What broke.
        fault: ModelTransportFault,
    },
    /// The response stream broke after its head.
    StreamInterrupted {
        /// What broke.
        fault: ModelTransportFault,
    },
}

impl ModelRetryCause {
    /// The stable snake-case name of this cause's kind.
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::HttpStatus { .. } => "http_status",
            Self::Connect => "connect",
            Self::RecordRejectedInTransit => "record_rejected_in_transit",
            Self::TransportAfterSend { .. } => "transport_after_send",
            Self::StreamInterrupted { .. } => "stream_interrupted",
        }
    }
}

impl fmt::Display for ModelRetryCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus { status: 429 } => {
                formatter.write_str("provider rate limited (HTTP 429)")
            }
            Self::HttpStatus {
                status: status @ (503 | 529),
            } => write!(formatter, "provider busy (HTTP {status})"),
            Self::HttpStatus { status } => write!(formatter, "provider error (HTTP {status})"),
            Self::Connect => formatter.write_str("provider unreachable (connect failed)"),
            Self::RecordRejectedInTransit => {
                formatter.write_str("request corrupted in transit (TLS record rejected)")
            }
            Self::TransportAfterSend { fault } => {
                write!(formatter, "request failed before a response ({fault})")
            }
            Self::StreamInterrupted { fault } => {
                write!(formatter, "response stream interrupted ({fault})")
            }
        }
    }
}

/// Content-free physical transport phase of a replayed failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRetryPhase {
    /// Hostname resolution.
    Resolution,
    /// TCP connection establishment.
    Connect,
    /// TLS or negotiated HTTP establishment.
    Tls,
    /// Request head or body transmission.
    RequestWrite,
    /// Unobservable boundary between transmission and response head.
    RequestWriteOrResponseHead,
    /// Response head wait or validation.
    ResponseHead,
    /// Response body or stream decoding.
    ResponseBody,
    /// Stopping and joining connection work.
    Cleanup,
}

impl ModelRetryPhase {
    /// Stable analytics label; never provider text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resolution => "resolution",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::RequestWrite => "request_write",
            Self::RequestWriteOrResponseHead => "request_write_or_response_head",
            Self::ResponseHead => "response_head",
            Self::ResponseBody => "response_body",
            Self::Cleanup => "cleanup",
        }
    }
}

/// Content-free request transmission evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelNetworkTransmission {
    /// No request bytes were sent.
    NotSent,
    /// The peer could not receive a complete request.
    Incomplete,
    /// The complete request may have reached the peer.
    MayHaveSent,
}

/// Content-free network cause, suitable for public failure presentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelNetworkFailure {
    /// The physical phase that failed.
    pub phase: ModelRetryPhase,
    /// Whether a request could have reached the peer.
    pub sent: ModelNetworkTransmission,
    /// Whether a configured deadline elapsed.
    pub timed_out: bool,
}

impl ModelNetworkFailure {
    /// Whether no request bytes were submitted to the connection.
    #[must_use]
    pub const fn nothing_sent(self) -> bool {
        matches!(self.sent, ModelNetworkTransmission::NotSent)
    }
}

impl fmt::Display for ModelNetworkFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self.phase {
            ModelRetryPhase::Resolution if self.timed_out => {
                "DNS lookup timed out before anything was sent"
            }
            ModelRetryPhase::Resolution => "DNS lookup failed before anything was sent",
            ModelRetryPhase::Connect => "could not connect to the provider (nothing was sent)",
            ModelRetryPhase::Tls => {
                "TLS handshake failed before anything was sent (a captive portal or intercepting proxy can cause this)"
            }
            ModelRetryPhase::RequestWrite if self.sent == ModelNetworkTransmission::Incomplete => {
                "connection dropped while sending (the provider did not receive the request whole)"
            }
            ModelRetryPhase::ResponseBody => "response stream interrupted",
            _ if self.sent == ModelNetworkTransmission::NotSent => {
                "connection failed before anything was sent"
            }
            _ => "connection dropped after the request was sent, before a response",
        };
        formatter.write_str(text)
    }
}

/// Why an otherwise classifiable retry was declined by its owning kernel.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ModelRetryDeclineReason {
    /// A genuine caller deadline prevents the replay.
    Deadline,
    /// A physical, wait, or long-generation replay budget is exhausted.
    Budget,
    /// Replaying may duplicate provider-hosted effects.
    ServerEffect,
    /// The provider explicitly disabled retries.
    RetryDisabled,
    /// This operation is not a replayable turn workflow.
    Workflow,
}

impl ModelRetryDeclineReason {
    /// Closed, content-free telemetry spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::Budget => "budget",
            Self::ServerEffect => "server_effect",
            Self::RetryDisabled => "retry_disabled",
            Self::Workflow => "workflow",
        }
    }
}

/// Closed Messages `diagnostics.cache_miss_reason.type` class.
///
/// Unknown provider spellings are [`Self::Other`]. The raw spelling is never retained.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMissReason {
    /// `model_changed`.
    ModelChanged,
    /// `system_changed`.
    SystemChanged,
    /// `tools_changed`.
    ToolsChanged,
    /// `messages_changed`.
    MessagesChanged,
    /// `previous_message_not_found`.
    PreviousMessageNotFound,
    /// `unavailable`.
    Unavailable,
    /// Any other string, including an empty one.
    Other,
}

impl PromptCacheMissReason {
    /// Stable content-free spelling. Never a raw provider string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelChanged => "model_changed",
            Self::SystemChanged => "system_changed",
            Self::ToolsChanged => "tools_changed",
            Self::MessagesChanged => "messages_changed",
            Self::PreviousMessageNotFound => "previous_message_not_found",
            Self::Unavailable => "unavailable",
            Self::Other => "other",
        }
    }
}

/// Content-free Messages cache-miss diagnosis.
///
/// Advisory telemetry only. It is not usage, history, or replay authority.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PromptCacheDiagnosis {
    /// Closed miss class.
    pub reason: PromptCacheMissReason,
    /// `cache_missed_input_tokens` when the provider sent a non-negative integer.
    /// Absent when the key was omitted. Never a raw provider string.
    #[serde(default)]
    pub missed_input_tokens: Option<u64>,
}

/// Content-free activity facts, independent of canonical output projection.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelStreamActivity {
    /// Visible message/refusal or tool output has begun, including unprojected tool input.
    pub output_started: bool,
    /// Time from physical dispatch to the latest semantic event.
    pub last_semantic_event_elapsed_ms: Option<u64>,
    /// Age of that event at observation, in milliseconds.
    pub semantic_gap_ms: u64,
    /// Keepalive or unclassified events, cumulative per logical attempt.
    pub keepalive_count: u64,
    /// Largest gap observed while the semantic stall bound was active, in milliseconds.
    /// Cumulative per logical attempt.
    pub max_semantic_gap_ms: u64,
}

/// Content-free accounting of one failed physical response.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRetryLoss {
    /// Streamed assistant text bytes, not token estimates.
    pub assistant_text_bytes: u64,
    /// Streamed reasoning bytes, including blocks completed before the break.
    pub reasoning_bytes: u64,
    /// Closed streamed output items/blocks that never reached a complete response.
    pub completed_items: u64,
    /// Wall time of the broken physical request, before backoff.
    pub elapsed_ms: u64,
    /// The exact request can start hosted work beyond inference.
    pub server_effect: bool,
    /// Exact failed transport phase, or response phase for a protocol refusal.
    pub transport_phase: Option<ModelRetryPhase>,
    /// Present only when no retry will follow this observation.
    pub declined_reason: Option<ModelRetryDeclineReason>,
}

impl ModelRetryLoss {
    /// Counts a structural event without retaining its content.
    pub fn observe(&mut self, event: &ModelEvent) {
        match event {
            ModelEvent::TextDelta(delta) => {
                self.assistant_text_bytes = self
                    .assistant_text_bytes
                    .saturating_add(delta.bytes().len() as u64);
            }
            ModelEvent::ReasoningDelta(delta) => {
                self.reasoning_bytes = self
                    .reasoning_bytes
                    .saturating_add(delta.bytes().len() as u64);
            }
            ModelEvent::BlockCompleted(_) => {
                self.completed_items = self.completed_items.saturating_add(1);
            }
            _ => {}
        }
    }
}

/// A kernel is waiting to send a turn request again under its retry budget.
///
/// When [`Self::output_discarded`] is true the failed physical request had
/// already streamed events for this model call; every consumer must drop
/// what it accumulated from them, because the replay's events follow from a
/// fresh response start and only its terminal response can reach history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRetry {
    next_physical: u32,
    max_physical: u32,
    delay_ms: u64,
    cause: ModelRetryCause,
    output_discarded: bool,
    detail: SafeDiagnostic,
    loss: ModelRetryLoss,
    #[serde(skip)]
    request: Option<(SafeDiagnostic, SafeDiagnostic, u64)>,
}

impl ModelRetry {
    /// Creates a retry notice. `detail` is the bounded, credential-safe
    /// diagnostic of the failure being retried.
    #[must_use]
    pub fn new(
        next_physical: u32,
        max_physical: u32,
        delay: std::time::Duration,
        cause: ModelRetryCause,
        output_discarded: bool,
        detail: impl AsRef<str>,
    ) -> Self {
        Self {
            next_physical,
            max_physical,
            delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            cause,
            output_discarded,
            detail: SafeDiagnostic::new(detail),
            loss: ModelRetryLoss::default(),
            request: None,
        }
    }

    /// Attaches content-free physical-attempt accounting.
    #[must_use]
    pub const fn with_loss(mut self, loss: ModelRetryLoss) -> Self {
        self.loss = loss;
        self
    }

    /// Attaches ephemeral, content-free request logging facts. The route is a
    /// path only: query parameters, headers and body content are not retained.
    #[must_use]
    pub fn with_request(mut self, host: &str, route: &str, body_bytes: usize) -> Self {
        let path = route.split('?').next().unwrap_or_default();
        self.request = Some((
            SafeDiagnostic::new(host),
            SafeDiagnostic::new(path),
            u64::try_from(body_bytes).unwrap_or(u64::MAX),
        ));
        self
    }

    /// Content-free host, route path and exact serialized body byte count.
    /// These transient logging facts are not serialized with the event.
    #[must_use]
    pub fn request(&self) -> Option<(&str, &str, u64)> {
        self.request
            .as_ref()
            .map(|(host, route, bytes)| (host.as_str(), route.as_str(), *bytes))
    }

    /// What the failed physical response discarded.
    #[must_use]
    pub const fn loss(&self) -> ModelRetryLoss {
        self.loss
    }

    /// The proposed next physical request (2-based); never sent when declined.
    #[must_use]
    pub const fn next_physical(&self) -> u32 {
        self.next_physical
    }

    /// The physical-request budget of the logical attempt.
    #[must_use]
    pub const fn max_physical(&self) -> u32 {
        self.max_physical
    }

    /// The wait before the next physical request.
    #[must_use]
    pub const fn delay(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.delay_ms)
    }

    /// What broke the previous physical request.
    #[must_use]
    pub const fn cause(&self) -> ModelRetryCause {
        self.cause
    }

    /// Operator-facing cause, preserving the closed physical network phase.
    #[must_use]
    pub fn human_cause(&self) -> String {
        let Some(phase) = self.loss.transport_phase else {
            return self.cause.to_string();
        };
        let (sent, timed_out) = match self.cause {
            ModelRetryCause::Connect => (
                if phase == ModelRetryPhase::RequestWrite {
                    ModelNetworkTransmission::Incomplete
                } else {
                    ModelNetworkTransmission::NotSent
                },
                false,
            ),
            ModelRetryCause::TransportAfterSend { fault }
            | ModelRetryCause::StreamInterrupted { fault } => (
                ModelNetworkTransmission::MayHaveSent,
                fault == ModelTransportFault::Timeout,
            ),
            _ => return self.cause.to_string(),
        };
        let network = ModelNetworkFailure {
            phase,
            sent,
            timed_out,
        }
        .to_string();
        if self.cause == ModelRetryCause::Connect
            && matches!(
                phase,
                ModelRetryPhase::Resolution | ModelRetryPhase::Connect | ModelRetryPhase::Tls
            )
            && self.loss.declined_reason.is_none()
        {
            format!(
                "provider unreachable ({network}); waiting for network, try {} in {}s",
                self.next_physical(),
                self.delay().as_secs()
            )
        } else {
            network
        }
    }

    /// Whether events the failed physical request streamed are discarded.
    #[must_use]
    pub const fn output_discarded(&self) -> bool {
        self.output_discarded
    }

    /// The bounded diagnostic of the failure being retried.
    #[must_use]
    pub const fn detail(&self) -> &SafeDiagnostic {
        &self.detail
    }
}
