//! Bounded retries for exact-URL, read-only model lookup GETs.

use plakous_http::{
    Client, ErrorKind, ExchangeControl, Failure, HeaderName, Headers, Method, Phase, Request,
    StatusCode,
};
use std::{
    fmt,
    time::{Duration, Instant, SystemTime},
};
use zeroize::Zeroizing;

const OVERALL_CAP: Duration = Duration::from_secs(60);

/// Content-free cause of a transient lookup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LookupCause {
    /// A physical transport failure, including body reads.
    Transport(Failure),
    /// A provider status.
    Status(u16),
}

impl LookupCause {
    pub(super) fn retryable(self) -> bool {
        match self {
            Self::Status(status) => matches!(status, 429 | 500 | 502 | 503 | 504),
            Self::Transport(failure) => retryable_transport(failure.phase(), failure.kind()),
        }
    }
}

impl fmt::Display for LookupCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(failure) => failure.fmt(f),
            Self::Status(status) => write!(f, "HTTP {status}"),
        }
    }
}

/// A lookup failed without interpreting catalog contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LookupError {
    /// A non-retryable transport failure.
    Transport(Failure),
    /// The caller's existing body bound was exceeded.
    BodyLimit,
    /// The successful response head was not the required JSON media type.
    MediaType,
    /// The caller refused physical dispatch or could not settle its accounting.
    DispatchDenied,
    /// Only GET requests may enter this policy.
    NotGet,
    /// A transient cause exhausted the attempt or wall-clock budget.
    Exhausted {
        /// Physical attempts that actually ran.
        attempts: u8,
        /// The final transient cause.
        cause: LookupCause,
    },
}

impl LookupError {
    /// Content-free exhausted-attempt summary for higher-level error boundaries.
    #[must_use]
    pub fn exhaustion(self) -> Option<(u8, &'static str)> {
        let Self::Exhausted { attempts, cause } = self else {
            return None;
        };
        let label = match cause {
            LookupCause::Status(429) => "HTTP 429",
            LookupCause::Status(500) => "HTTP 500",
            LookupCause::Status(502) => "HTTP 502",
            LookupCause::Status(503) => "HTTP 503",
            LookupCause::Status(504) => "HTTP 504",
            LookupCause::Status(_) => "HTTP provider failure",
            LookupCause::Transport(failure) => match failure.kind() {
                ErrorKind::DeadlineExceeded => "transport timeout",
                ErrorKind::Resolution => "DNS failure",
                _ if failure.phase() == Phase::Connect => "connect failure",
                _ => "transport stream failure",
            },
        };
        Some((attempts, label))
    }
}

impl fmt::Display for LookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(failure) => failure.fmt(f),
            Self::BodyLimit => f.write_str("model lookup exceeded its body bound"),
            Self::MediaType => f.write_str("model lookup has an invalid media type"),
            Self::DispatchDenied => f.write_str("model lookup dispatch was not authorized"),
            Self::NotGet => f.write_str("model lookup requires GET"),
            Self::Exhausted { attempts, cause } => {
                write!(f, "model lookup failed after {attempts} attempts: {cause}")
            }
        }
    }
}

impl std::error::Error for LookupError {}

/// A bounded HTTP lookup reply; catalog validation stays with the provider.
pub struct LookupReply {
    /// The exact response status (non-retryable statuses are returned unchanged).
    pub status: StatusCode,
    /// Headers of the selected exchange, never an earlier attempt.
    pub headers: Headers,
    /// Bounded body, wiped on drop.
    pub body: Zeroizing<Vec<u8>>,
}

/// One wall-clock budget shared by all GETs in a catalog/discovery operation.
#[derive(Clone, Copy)]
pub struct LookupBudget {
    attempt_timeout: Duration,
    deadline: tokio::time::Instant,
    maximum_attempts: u8,
}

impl LookupBudget {
    /// Retains the existing first-attempt duration, with a 60-second operation cap.
    #[must_use]
    pub fn from_deadline(deadline: Instant) -> Self {
        Self {
            attempt_timeout: deadline.saturating_duration_since(Instant::now()),
            deadline: tokio::time::Instant::now() + OVERALL_CAP,
            maximum_attempts: 3,
        }
    }

    /// Keeps in-session receipt renewal single-dispatch under its existing deadline.
    #[must_use]
    pub fn single_attempt(deadline: Instant) -> Self {
        Self {
            maximum_attempts: 1,
            ..Self::from_deadline(deadline)
        }
    }
}

/// Executes only the supplied GET, at most three times within 60 seconds.
/// Each attempt keeps the original exchange duration and transport phase limits.
/// Catalog parsing and evidence validation MUST happen outside this function.
///
/// # Errors
/// Returns transport, resource or exhausted-retry errors without response prose.
pub async fn get(
    client: &Client,
    request: Request,
    budget: LookupBudget,
    maximum_body_bytes: usize,
) -> Result<LookupReply, LookupError> {
    get_inner(
        client,
        request,
        budget,
        maximum_body_bytes,
        false,
        &mut |_| Ok(()),
    )
    .await
}

/// Like [`get`], but rejects a non-JSON successful response head before reading
/// its body. This preserves callers' fail-fast media-type validation.
///
/// # Errors
/// Returns [`LookupError::MediaType`] or the same failures as [`get`].
pub async fn get_json(
    client: &Client,
    request: Request,
    budget: LookupBudget,
    maximum_body_bytes: usize,
) -> Result<LookupReply, LookupError> {
    get_inner(
        client,
        request,
        budget,
        maximum_body_bytes,
        true,
        &mut |_| Ok(()),
    )
    .await
}

/// Executes a lookup with caller-owned authorization immediately before every
/// physical attempt. A transport failure is reported to the hook before retry
/// classification, so trusted `NotStarted` proof can refund the spent permit.
///
/// # Errors
/// Returns [`LookupError::DispatchDenied`] if the hook refuses dispatch or refund,
/// or the same failures as [`get`].
pub async fn get_with_dispatch(
    client: &Client,
    request: Request,
    budget: LookupBudget,
    maximum_body_bytes: usize,
    dispatch: &mut (dyn FnMut(Option<Failure>) -> Result<(), LookupError> + Send),
) -> Result<LookupReply, LookupError> {
    get_inner(client, request, budget, maximum_body_bytes, false, dispatch).await
}

async fn get_inner(
    client: &Client,
    request: Request,
    budget: LookupBudget,
    maximum_body_bytes: usize,
    require_json: bool,
    dispatch: &mut (dyn FnMut(Option<Failure>) -> Result<(), LookupError> + Send),
) -> Result<LookupReply, LookupError> {
    if request.method() != Method::GET {
        return Err(LookupError::NotGet);
    }
    let _status = LookupStatus(client);
    Box::pin(run(
        budget.attempt_timeout,
        budget
            .deadline
            .saturating_duration_since(tokio::time::Instant::now()),
        |deadline| {
            once(
                client,
                request.clone(),
                deadline,
                maximum_body_bytes,
                require_json,
            )
        },
        |attempt, cause| client.observe_lookup_retry(attempt, cause),
        dispatch,
        budget.maximum_attempts,
    ))
    .await
}

pub(super) async fn run<F, Fut>(
    timeout: Duration,
    overall_cap: Duration,
    mut exchange: F,
    mut notify: impl FnMut(u8, &str),
    dispatch: &mut (dyn FnMut(Option<Failure>) -> Result<(), LookupError> + Send),
    maximum_attempts: u8,
) -> Result<LookupReply, LookupError>
where
    F: FnMut(Instant) -> Fut,
    Fut: Future<Output = Result<LookupReply, LookupError>>,
{
    let cap = tokio::time::Instant::now() + overall_cap;
    if timeout.is_zero() || overall_cap.is_zero() {
        let deadline = (tokio::time::Instant::now() + timeout).min(cap);
        return exchange(deadline.into_std()).await;
    }
    for attempt in 1..=maximum_attempts {
        let now = tokio::time::Instant::now();
        dispatch(None)?;
        let result = exchange((now + timeout).min(cap).into_std()).await;
        if let Err(LookupError::Transport(failure)) = &result {
            dispatch(Some(*failure))?;
        }
        if maximum_attempts == 1 {
            return result;
        }
        let (cause, retry_after) = match result {
            Ok(reply) if LookupCause::Status(reply.status.as_u16()).retryable() => {
                let delay = reply
                    .headers
                    .get(&HeaderName::from_static("retry-after"))
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after);
                (LookupCause::Status(reply.status.as_u16()), delay)
            }
            Err(LookupError::Transport(failure)) if LookupCause::Transport(failure).retryable() => {
                (LookupCause::Transport(failure), None)
            }
            result => return result,
        };
        let mut jitter = [0];
        let _ = getrandom::fill(&mut jitter);
        let delay = retry_delay(attempt, retry_after, jitter[0]);
        let remaining = cap.saturating_duration_since(tokio::time::Instant::now());
        let useful_attempt = timeout.min(Duration::from_secs(5));
        if attempt >= maximum_attempts
            || (retry_after.is_some() && remaining.saturating_sub(delay) < useful_attempt)
            || !can_retry(
                attempt,
                delay,
                cap.saturating_duration_since(tokio::time::Instant::now()),
            )
        {
            return Err(LookupError::Exhausted {
                attempts: attempt,
                cause,
            });
        }
        notify(attempt + 1, &cause.to_string());
        tokio::time::sleep(delay).await;
    }
    unreachable!("the third transient failure returns exhausted")
}

async fn once(
    client: &Client,
    request: Request,
    deadline: Instant,
    limit: usize,
    require_json: bool,
) -> Result<LookupReply, LookupError> {
    let response = client
        .execute(request, ExchangeControl::new(deadline))
        .await
        .map_err(LookupError::Transport)?;
    let status = response.status();
    let headers = response.headers().clone();
    let mut body = response.into_body();
    let mut bytes = Zeroizing::new(Vec::new());
    // Error status bodies are not catalog evidence. Close them before retrying.
    if status != StatusCode::OK {
        if let Err(failure) = body.close().await
            && failure.kind() == ErrorKind::Internal
        {
            return Err(LookupError::Transport(failure));
        }
        return Ok(LookupReply {
            status,
            headers,
            body: bytes,
        });
    }
    if require_json
        && !headers
            .get(&HeaderName::from_static("content-type"))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
    {
        let _ = body.close().await;
        return Err(LookupError::MediaType);
    }
    while let Some(chunk) = body.next_chunk().await {
        let chunk = chunk.map_err(LookupError::Transport)?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            let _ = body.close().await;
            return Err(LookupError::BodyLimit);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(LookupReply {
        status,
        headers,
        body: bytes,
    })
}

pub(super) fn retry_delay(attempt: u8, retry_after: Option<Duration>, jitter: u8) -> Duration {
    let base = Duration::from_millis(if attempt == 1 { 500 } else { 2000 });
    (base + Duration::from_millis(u64::from(jitter) * 250 / 255))
        .max(retry_after.unwrap_or_default())
}

pub(super) fn can_retry(attempt: u8, delay: Duration, remaining: Duration) -> bool {
    attempt < 3 && delay < remaining
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()?
                .duration_since(SystemTime::now())
                .ok()
        })
}

pub(super) fn retryable_transport(phase: Phase, kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::Resolution | ErrorKind::Io | ErrorKind::DeadlineExceeded
    ) || (phase == Phase::ResponseBody && kind == ErrorKind::Protocol)
}

struct LookupStatus<'a>(&'a Client);

impl Drop for LookupStatus<'_> {
    fn drop(&mut self) {
        self.0.observe_lookup_finished();
    }
}
