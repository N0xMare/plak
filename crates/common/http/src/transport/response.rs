use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, Response as WireResponse, Version};
use http_body::Body;
use http_body_util::BodyExt as _;
use hyper::body::Incoming;

use crate::ExchangeTerminal;
use crate::observation::Observation;
use crate::request::header_bytes;
use crate::{
    Bytes as PublicBytes, ConnectionUse, ErrorKind, ExchangeControl, Failure, Headers, LimitKind,
    Phase, RequestError, StatusCode, TransmissionFact, WireProtocol,
};

use super::{
    ConnectionTask, IdleConnection, Limits, RELEASE_READY_BOUND, RequestProgress, Response,
    ResponseBody, Reuse, body_pull_deadline, fixed_phase_deadline,
};

impl Response {
    /// Returns the wire status without interpreting it.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// Returns the negotiated response HTTP version.
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// Returns bounded, redaction-marked response headers.
    #[must_use]
    pub const fn headers(&self) -> &Headers {
        &self.headers
    }

    /// Whether the exchange ran on a fresh, reused or stale-replaced
    /// connection; `None` for a response injected without a connection.
    #[must_use]
    pub const fn connection_use(&self) -> Option<ConnectionUse> {
        self.connection
    }

    /// Consumes the response and returns its pull-driven body.
    #[must_use]
    pub fn into_body(self) -> ResponseBody {
        self.body
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Response")
            .field("status", &self.status)
            .field("version", &self.version)
            .field("headers", &self.headers)
            .field("connection", &self.connection)
            .field("body", &self.body)
            .finish()
    }
}

impl ResponseBody {
    /// Pulls one bounded data chunk.
    ///
    /// Trailers are validated and consumed internally. `None` is returned only
    /// after a clean end of body. A failure is returned at most once.
    pub async fn next_chunk(&mut self) -> Option<Result<PublicBytes, Failure>> {
        if self.terminal {
            return None;
        }
        let result = self.next_chunk_inner().await;
        match &result {
            Some(Ok(bytes)) => {
                if !self.observation.chunk(bytes) {
                    let failure = self.fail(ErrorKind::Internal).await;
                    self.observation.finish(ExchangeTerminal::Failed(failure));
                    return Some(Err(failure));
                }
            }
            Some(Err(failure)) => {
                if !self.observation.finish(ExchangeTerminal::Failed(*failure)) {
                    return Some(Err(Failure::new(
                        failure.phase(),
                        failure.transmission(),
                        ErrorKind::Internal,
                    )
                    .with_connection_use(self.connection_use)
                    .with_idle_age(self.idle_age)));
                }
            }
            None => {
                if !self.observation.finish(ExchangeTerminal::Complete) {
                    return Some(Err(self.fail(ErrorKind::Internal).await));
                }
                self.park().await;
            }
        }
        result
    }

    async fn next_chunk_inner(&mut self) -> Option<Result<PublicBytes, Failure>> {
        if self.terminal {
            return None;
        }
        if self.control.cancellation().is_cancelled() {
            return Some(Err(self.fail(ErrorKind::Cancelled).await));
        }
        if self
            .control
            .body_deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Some(Err(self.fail(ErrorKind::DeadlineExceeded).await));
        }
        #[cfg(feature = "fault-injection")]
        if let Some(ended) = self.injected_stream_fault().await {
            return ended;
        }
        if let Some(mut pending) = self.pending.take() {
            if pending.len() > self.limits.max_response_chunk_bytes {
                let remainder = pending.split_off(self.limits.max_response_chunk_bytes);
                self.pending = Some(remainder);
            }
            return Some(Ok(self.delivered(pending)));
        }
        loop {
            let deadline = body_pull_deadline(&self.control, self.limits.response_body_timeout);
            let cancellation = self.control.cancellation().clone();
            let frame = tokio::select! {
                () = cancellation.cancelled() => {
                    return Some(Err(self.fail(ErrorKind::Cancelled).await));
                }
                () = tokio::time::sleep_until(deadline) => {
                    return Some(Err(self.fail(ErrorKind::DeadlineExceeded).await));
                }
                frame = self.incoming.frame() => frame,
            };
            match frame {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => {
                        let Some(total) = self.bytes_seen.checked_add(data.len()) else {
                            return Some(Err(self
                                .fail(ErrorKind::LimitExceeded(LimitKind::ResponseBodyBytes))
                                .await));
                        };
                        self.bytes_seen = total;
                        if self.bytes_seen > self.limits.max_response_body_bytes {
                            return Some(Err(self
                                .fail(ErrorKind::LimitExceeded(LimitKind::ResponseBodyBytes))
                                .await));
                        }
                        let mut data = data;
                        if data.len() > self.limits.max_response_chunk_bytes {
                            let remainder = data.split_off(self.limits.max_response_chunk_bytes);
                            self.pending = Some(remainder);
                        }
                        return Some(Ok(self.delivered(data)));
                    }
                    Err(frame) => match frame.into_trailers() {
                        Ok(trailers) => {
                            if let Err(kind) = validate_response_headers(&trailers, &self.limits) {
                                return Some(Err(self.fail(kind).await));
                            }
                        }
                        Err(_) => return Some(Err(self.fail(ErrorKind::Protocol).await)),
                    },
                },
                Some(Err(error)) => {
                    return Some(Err(self
                        .fail(ErrorKind::Protocol)
                        .await
                        .with_protocol_cause(&error)));
                }
                None => {
                    self.terminal = true;
                    return self.finish_clean_eof().await;
                }
            }
        }
    }

    /// Stops and joins the owned connection task without consuming more bytes.
    ///
    /// # Errors
    ///
    /// Returns a cleanup [`Failure`] if an owned task panicked.
    pub async fn close(self) -> Result<(), Failure> {
        self.close_with_terminal(ExchangeTerminal::ClosedEarly)
            .await
    }

    /// Closes after the consumer has validated its protocol terminal.
    /// This only enriches observation; it grants no retry or task-success authority.
    ///
    /// A pooled client never waits here: only an end of body already
    /// received, with no further data, lets the connection carry another
    /// request. Otherwise the connection is closed.
    ///
    /// # Errors
    /// Returns a cleanup or required-observation failure, just like [`Self::close`].
    pub async fn close_after_protocol_terminal(self) -> Result<(), Failure> {
        self.close_with_terminal(ExchangeTerminal::ProtocolComplete)
            .await
    }

    async fn close_with_terminal(mut self, terminal: ExchangeTerminal) -> Result<(), Failure> {
        let keep = !self.terminal
            && terminal == ExchangeTerminal::ProtocolComplete
            && self.may_keep_connection()
            && self.body_ended_now();
        self.terminal = true;
        let result = if keep {
            self.release_connection(false).await
        } else {
            self.connection.abort_and_join().await
        }
        .map_err(|failure| {
            failure
                .with_connection_use(self.connection_use)
                .with_idle_age(self.idle_age)
        });
        let captured = self.observation.finish(match result {
            Ok(()) => terminal,
            Err(failure) => ExchangeTerminal::Failed(failure),
        });
        if result.is_ok() && !captured {
            self.reusable = false;
            let _ = self.connection.abort_and_join().await;
            return Err(Failure::new(
                Phase::Cleanup,
                TransmissionFact::MayHaveTransmitted,
                ErrorKind::Internal,
            )
            .with_connection_use(self.connection_use)
            .with_idle_age(self.idle_age));
        }
        self.park().await;
        result
    }

    /// Whether a clean end may return the connection to its pool: the
    /// request went out whole, so no part of it is still being sent, and the
    /// exchange was neither cancelled nor fault-injected.
    fn may_keep_connection(&self) -> bool {
        #[cfg(feature = "fault-injection")]
        if self.fault.is_some() {
            return false;
        }
        self.reuse
            .as_ref()
            .is_some_and(|reuse| reuse.slot.current().is_complete())
            && !self.control.cancellation().is_cancelled()
    }

    /// Whether the end of the body is already buffered, past nothing but
    /// empty data or valid trailers. Never waits.
    fn body_ended_now(&mut self) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let mut context = Context::from_waker(Waker::noop());
        loop {
            match Pin::new(&mut self.incoming).poll_frame(&mut context) {
                Poll::Ready(None) => return true,
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) if data.is_empty() => {}
                    Ok(_) => return false,
                    Err(frame) => match frame.into_trailers() {
                        Ok(trailers)
                            if validate_response_headers(&trailers, &self.limits).is_ok() => {}
                        _ => return false,
                    },
                },
                Poll::Ready(Some(Err(_))) | Poll::Pending => return false,
            }
        }
    }

    async fn finish_clean_eof(&mut self) -> Option<Result<PublicBytes, Failure>> {
        let result = if self.may_keep_connection() {
            self.release_connection(true).await
        } else {
            self.connection.abort_and_join().await
        };
        result.err().map(|failure| {
            Err(failure
                .with_connection_use(self.connection_use)
                .with_idle_age(self.idle_age))
        })
    }

    /// Takes the connection back from its supervisor once the body ended
    /// cleanly, keeping it only when it can carry another request. With
    /// `wait`, readiness may take up to `RELEASE_READY_BOUND`, cut short by
    /// cancellation or the body deadline; without it only readiness already
    /// at hand counts.
    async fn release_connection(&mut self, wait: bool) -> Result<(), Failure> {
        if !self.connection.release().await? || self.connection.panicked() {
            return self.connection.abort_and_join().await;
        }
        let ready = if wait {
            let deadline = body_pull_deadline(&self.control, self.limits.response_body_timeout)
                .min(tokio::time::Instant::now() + RELEASE_READY_BOUND);
            let cancellation = self.control.cancellation().clone();
            tokio::select! {
                () = cancellation.cancelled() => false,
                () = tokio::time::sleep_until(deadline) => false,
                ready = self.connection.ready() => ready,
            }
        } else {
            self.connection.ready_now()
        };
        if ready {
            self.reusable = true;
            return Ok(());
        }
        self.connection.abort_and_join().await
    }

    /// Hands a released connection to its pool, or closes it when the pool
    /// is gone.
    async fn park(&mut self) {
        if !std::mem::take(&mut self.reusable) {
            return;
        }
        let Some(reuse) = self.reuse.take() else {
            return;
        };
        let Some(pool) = reuse.pool.upgrade() else {
            let _ = self.connection.abort_and_join().await;
            return;
        };
        let connection = std::mem::replace(&mut self.connection, ConnectionTask::closed());
        pool.checkin(
            reuse.key,
            IdleConnection {
                connection,
                slot: reuse.slot,
                protocol: self.protocol,
                since: Instant::now(),
            },
        );
    }

    #[cfg(not(feature = "fault-injection"))]
    #[allow(
        clippy::unused_self,
        reason = "the fault-injection build inspects the chunk"
    )]
    fn delivered(&mut self, data: Bytes) -> Bytes {
        data
    }

    /// Counts a chunk against the stream fault and cuts it at the threshold.
    /// A stall keeps the bytes past the cut for after the pause.
    #[cfg(feature = "fault-injection")]
    fn delivered(&mut self, mut data: Bytes) -> Bytes {
        let Some(fault) = self.fault.as_mut() else {
            return data;
        };
        if let Some(cut) = fault.cut_point(&data) {
            let rest = data.split_off(cut);
            if fault.keeps_remainder() && !rest.is_empty() {
                self.pending = Some(match self.pending.take() {
                    Some(pending) => [rest, pending].concat().into(),
                    None => rest,
                });
            }
        }
        data
    }

    /// Applies a due stream fault: the same terminal a reset connection or a
    /// clean end of body produces, or a pause the body deadlines still bound.
    #[cfg(feature = "fault-injection")]
    async fn injected_stream_fault(&mut self) -> Option<Option<Result<PublicBytes, Failure>>> {
        match self.fault.as_mut()?.due()? {
            super::fault::StreamAction::Reset => {
                Some(Some(Err(self.fail(ErrorKind::Protocol).await)))
            }
            super::fault::StreamAction::Eof => {
                self.terminal = true;
                Some(self.finish_clean_eof().await)
            }
            super::fault::StreamAction::Stall(pause) => {
                let deadline = body_pull_deadline(&self.control, self.limits.response_body_timeout);
                let cancellation = self.control.cancellation().clone();
                tokio::select! {
                    () = cancellation.cancelled() => {
                        Some(Some(Err(self.fail(ErrorKind::Cancelled).await)))
                    }
                    () = tokio::time::sleep_until(deadline) => {
                        Some(Some(Err(self.fail(ErrorKind::DeadlineExceeded).await)))
                    }
                    () = tokio::time::sleep(pause) => None,
                }
            }
        }
    }

    async fn fail(&mut self, kind: ErrorKind) -> Failure {
        self.terminal = true;
        self.reuse = None;
        self.reusable = false;
        let _ = self.connection.abort_and_join().await;
        Failure::new(
            Phase::ResponseBody,
            TransmissionFact::MayHaveTransmitted,
            kind,
        )
        .with_wire_protocol(self.protocol)
        .with_connection_use(self.connection_use)
        .with_idle_age(self.idle_age)
    }
}

impl fmt::Debug for ResponseBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponseBody")
            .field("bytes_seen", &self.bytes_seen)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

pub(super) fn response_wait_deadline(
    last_write_progress: Instant,
    completed: Option<Instant>,
    http2: bool,
    absolute: Instant,
    write_idle: Duration,
    response_head_timeout: Duration,
) -> (tokio::time::Instant, Phase) {
    let write_deadline = fixed_phase_deadline(absolute, last_write_progress, write_idle);
    let timely_completion = completed.filter(|completed| *completed <= write_deadline.into_std());
    match timely_completion {
        Some(completed) => (
            fixed_phase_deadline(absolute, completed, response_head_timeout),
            if http2 {
                Phase::RequestWriteOrResponseHead
            } else {
                Phase::ResponseHead
            },
        ),
        None => (
            write_deadline,
            if http2 {
                Phase::RequestWriteOrResponseHead
            } else {
                Phase::RequestWrite
            },
        ),
    }
}

pub(super) async fn wait_response<F>(
    response: F,
    mut connection: ConnectionTask,
    progress: Arc<RequestProgress>,
    control: &ExchangeControl,
    limits: &Limits,
) -> Result<(WireResponse<Incoming>, ConnectionTask), Failure>
where
    F: Future<Output = Result<WireResponse<Incoming>, hyper::Error>>,
{
    tokio::pin!(response);
    progress.start_write_clock(limits.request_write_idle_timeout);
    loop {
        // Register before reading completion so its notification cannot be lost.
        let completed = progress.completed.notified();
        tokio::pin!(completed);
        let complete = progress.is_complete();
        let (deadline, deadline_phase) = progress.response_deadline(control.deadline(), limits);
        let cancellation = control.cancellation().clone();
        tokio::select! {
            () = cancellation.cancelled() => {
                connection.abort_and_join().await?;
                return Err(Failure::new(
                    Phase::RequestWriteOrResponseHead,
                    TransmissionFact::MayHaveTransmitted,
                    ErrorKind::Cancelled,
                ));
            }
            () = tokio::time::sleep_until(deadline) => {
                // Progress or a timely flush may race the old idle timer.
                // Recorded times, not select ordering, own the deadline.
                // Progress after an expired idle budget cannot revive it.
                if progress.response_deadline(control.deadline(), limits) != (deadline, deadline_phase) {
                    continue;
                }
                connection.abort_and_join().await?;
                return Err(Failure::new(
                    deadline_phase,
                    TransmissionFact::MayHaveTransmitted,
                    ErrorKind::DeadlineExceeded,
                ));
            }
            () = &mut completed, if !complete => {}
            result = &mut response => {
                // A response becoming ready cannot revive an expired phase.
                // Re-read the flush timestamp: the old write timer may have
                // raced either a timely flush or this response's publication.
                let (deadline, phase) = progress.response_deadline(control.deadline(), limits);
                if tokio::time::Instant::now() >= deadline {
                    connection.abort_and_join().await?;
                    return Err(Failure::new(
                        phase,
                        TransmissionFact::MayHaveTransmitted,
                        ErrorKind::DeadlineExceeded,
                    ));
                }
                return match result {
                    Ok(response) => Ok((response, connection)),
                    Err(error) => {
                        connection.abort_and_join().await?;
                        let phase = if error.is_parse() {
                            Phase::ResponseHead
                        } else {
                            Phase::RequestWriteOrResponseHead
                        };
                        let transmission = if progress.peer_lacks_body() {
                            TransmissionFact::RequestIncomplete
                        } else {
                            TransmissionFact::MayHaveTransmitted
                        };
                        let failure = Failure::new(phase, transmission, ErrorKind::Protocol)
                            .with_protocol_cause(&error);
                        Err(if progress.peer_rejected_record.load(Ordering::Acquire) {
                            failure.with_peer_rejected_record()
                        } else {
                            failure
                        })
                    }
                }
            }
        }
    }
}

pub(super) async fn response_from_wire(
    response: WireResponse<Incoming>,
    mut connection: ConnectionTask,
    control: ExchangeControl,
    protocol: WireProtocol,
    connection_use: Option<ConnectionUse>,
    reuse: Option<Reuse>,
    limits: &Limits,
) -> Result<Response, Failure> {
    let (parts, incoming) = response.into_parts();
    let head_failure = validate_response_version(parts.version)
        .err()
        .or_else(|| {
            validate_response_headers(&parts.headers, limits)
                .err()
                .map(|kind| {
                    Failure::new(
                        Phase::ResponseHead,
                        TransmissionFact::MayHaveTransmitted,
                        kind,
                    )
                })
        })
        .or_else(|| {
            (parts.status == StatusCode::SWITCHING_PROTOCOLS).then(|| {
                Failure::new(
                    Phase::ResponseHead,
                    TransmissionFact::MayHaveTransmitted,
                    ErrorKind::InvalidResponse,
                )
            })
        })
        .or_else(|| {
            content_length(&parts.headers)
                .filter(|length| {
                    *length > u64::try_from(limits.max_response_body_bytes).unwrap_or(u64::MAX)
                })
                .map(|_| {
                    Failure::new(
                        Phase::ResponseHead,
                        TransmissionFact::MayHaveTransmitted,
                        ErrorKind::LimitExceeded(LimitKind::ResponseBodyBytes),
                    )
                })
        });
    if let Some(failure) = head_failure {
        connection.abort_and_join().await?;
        return Err(failure);
    }
    let connection = connection.into_supervised(control.clone());
    Ok(Response {
        status: parts.status,
        version: parts.version,
        headers: Headers::new(parts.headers),
        connection: connection_use,
        body: ResponseBody {
            observation: Observation::default(),
            incoming,
            connection,
            control,
            protocol,
            connection_use,
            idle_age: None,
            reuse,
            reusable: false,
            limits: limits.clone(),
            bytes_seen: 0,
            pending: None,
            terminal: false,
            #[cfg(feature = "fault-injection")]
            fault: None,
        },
    })
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn validate_response_version(version: Version) -> Result<(), Failure> {
    if matches!(
        version,
        Version::HTTP_10 | Version::HTTP_11 | Version::HTTP_2
    ) {
        Ok(())
    } else {
        Err(Failure::new(
            Phase::ResponseHead,
            TransmissionFact::MayHaveTransmitted,
            ErrorKind::InvalidResponse,
        ))
    }
}

fn validate_response_headers(headers: &HeaderMap, limits: &Limits) -> Result<(), ErrorKind> {
    if headers.len() > limits.max_response_headers {
        return Err(ErrorKind::LimitExceeded(LimitKind::ResponseHeaderCount));
    }
    if header_bytes(headers).is_none_or(|bytes| bytes > limits.max_response_header_bytes) {
        return Err(ErrorKind::LimitExceeded(LimitKind::ResponseHeaderBytes));
    }
    Ok(())
}

pub(super) fn request_error_kind(error: &RequestError) -> ErrorKind {
    match error {
        RequestError::LimitExceeded(kind) => ErrorKind::LimitExceeded(*kind),
        _ => ErrorKind::Internal,
    }
}
