use std::future::{Future, poll_fn};
use std::task::Poll;

use plakous_http::{
    Bytes, CancellationToken as HttpCancellationToken, Client, ErrorKind, ExchangeControl, Failure,
    Request, Response, ResponseBody,
};
use plakous_model::CancellationToken;
use tokio::time::Instant;

use crate::{Cancellable, cancellable};

use super::Generation;
use super::host::wait_deadline;

/// One physical HTTP dispatch and its joined cancellation token.
pub struct GenerationTransport {
    deadline: Instant,
    control: ExchangeControl,
    cancellation: HttpCancellationToken,
}

impl GenerationTransport {
    /// Prepares transport controls without consuming or sending the request.
    #[must_use]
    pub fn new(generation: &Generation, body_deadline: Option<Instant>) -> Self {
        let cancellation = HttpCancellationToken::new();
        Self {
            deadline: generation.deadline,
            control: ExchangeControl::streaming(
                generation.deadline.into_std(),
                cancellation.clone(),
            )
            .with_body_deadline(body_deadline.map(Instant::into_std))
            .with_fresh_connection(generation.fresh_connection),
            cancellation,
        }
    }

    /// Returns a token shared by the HTTP head, body and cleanup.
    #[must_use]
    pub const fn cancellation(&self) -> &HttpCancellationToken {
        &self.cancellation
    }

    /// Dispatches when transport proof may settle even a cancelled exchange.
    pub(super) async fn send(
        self,
        client: &Client,
        request: Request,
        cancellation: &CancellationToken,
    ) -> Cancellable<Result<Response, Failure>> {
        cancellable(
            client.execute(request, self.control),
            cancellation,
            &self.cancellation,
        )
        .await
    }

    /// Dispatches immediately after synchronous final authorization. The
    /// first poll must happen before cancellation or expiry can win: the
    /// credential lease may already be irrevocably spent at that point.
    pub async fn send_authorized(
        self,
        client: &Client,
        request: Request,
        cancellation: &CancellationToken,
    ) -> AuthorizedHead {
        let exchange = client.execute(request, self.control);
        tokio::pin!(exchange);
        let first_poll = poll_fn(|context| Poll::Ready(exchange.as_mut().poll(context))).await;
        match first_poll {
            Poll::Ready(response) => AuthorizedHead::Resolved(response),
            Poll::Pending => match join_on_abort(
                exchange,
                cancellation,
                Some(self.deadline),
                &self.cancellation,
            )
            .await
            {
                Pull::Ready(response) => AuthorizedHead::Resolved(response),
                Pull::Cancelled => AuthorizedHead::Cancelled,
                Pull::Deadline => AuthorizedHead::Deadline,
            },
        }
    }
}

/// A response-head result after an irrevocable authorization fence.
pub enum AuthorizedHead {
    /// Exact result from the completed exchange.
    Resolved(Result<Response, Failure>),
    /// Cancellation won after send began; transmission is ambiguous.
    Cancelled,
    /// Dispatch expiry won after send began; transmission is ambiguous.
    Deadline,
}

async fn join_on_abort<F: Future>(
    future: F,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
    http_cancellation: &HttpCancellationToken,
) -> Pull<F::Output> {
    tokio::pin!(future);
    let aborted = tokio::select! {
        biased;
        () = cancellation.cancelled() => Pull::Cancelled,
        () = wait_deadline(deadline) => Pull::Deadline,
        result = &mut future => return Pull::Ready(result),
    };
    http_cancellation.cancel();
    let _ = future.await;
    aborted
}

/// A bounded exchange read; aborts join transport cleanup before returning.
pub enum Pull<T> {
    /// The read completed without a cancellation race.
    Ready(T),
    /// The logical caller cancelled.
    Cancelled,
    /// The read's hard or semantic deadline expired.
    Deadline,
}

/// One physical body pull, EOF, or a joined cancellation/deadline.
pub type BodyPull = Pull<Option<Result<Bytes, Failure>>>;

/// Reads one body chunk under a family-specific semantic/hard deadline.
pub async fn pull_body(
    body: &mut ResponseBody,
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
    http_cancellation: &HttpCancellationToken,
) -> BodyPull {
    join_on_abort(body.next_chunk(), cancellation, deadline, http_cancellation).await
}

/// Reads the complete error body under the client's transport limits. A
/// partial body cannot supply proof of a provider refusal.
///
/// # Errors
///
/// An internal read or close failure must prevent a retry based on this head.
pub async fn read_complete_error_body(
    mut body: ResponseBody,
    cancellation: &CancellationToken,
    deadline: Instant,
    http_cancellation: &HttpCancellationToken,
) -> Result<Option<Vec<u8>>, Failure> {
    let mut bytes = Vec::new();
    let read = loop {
        match pull_body(&mut body, cancellation, Some(deadline), http_cancellation).await {
            Pull::Ready(Some(Ok(chunk))) => bytes.extend_from_slice(&chunk),
            Pull::Ready(Some(Err(source))) if source.kind() == ErrorKind::Internal => {
                break Err(source);
            }
            Pull::Ready(None) => break Ok(Some(bytes)),
            _ => break Ok(None),
        }
    };
    let closed = body.close().await;
    read.and_then(|value| match closed {
        Err(source) if source.kind() == ErrorKind::Internal => Err(source),
        _ => Ok(value),
    })
}
