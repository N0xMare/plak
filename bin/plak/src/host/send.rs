//! Send deadlines measure accepted bytes, not the size of an attach baseline.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Instant;

pub(crate) const IDLE: Duration = Duration::from_secs(10);
pub(crate) const TOTAL: Duration = Duration::from_secs(30 * 60);
pub(crate) const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Progress(Arc<Mutex<Instant>>);

impl Progress {
    fn mark(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    pub(crate) async fn sending<T>(&self, send: impl Future<Output = T>) -> Result<T, ()> {
        self.mark();
        let end = Instant::now() + TOTAL;
        tokio::pin!(send);
        loop {
            let last = *self.0.lock().unwrap_or_else(PoisonError::into_inner);
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(end) => return Err(()),
                result = &mut send => return Ok(result),
                () = tokio::time::sleep_until(last + IDLE) => {
                    let last = *self.0.lock().unwrap_or_else(PoisonError::into_inner);
                    if Instant::now().duration_since(last) >= IDLE {
                        return Err(());
                    }
                }
            }
        }
    }
}

pub(crate) struct ProgressIo<T> {
    io: T,
    pub(crate) progress: Progress,
}

impl<T> ProgressIo<T> {
    pub(crate) fn new(io: T) -> Self {
        Self {
            io,
            progress: Progress(Arc::new(Mutex::new(Instant::now()))),
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ProgressIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let written = Pin::new(&mut self.io).poll_write(cx, bytes);
        if matches!(written, Poll::Ready(Ok(count)) if count > 0) {
            self.progress.mark();
        }
        written
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ProgressIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
