//! Byte-stream and body trait implementations of the request I/O.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::{RequestBody, RequestIo, WireStream};

impl AsyncRead for WireStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            #[cfg(test)]
            Self::Duplex(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for WireStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            #[cfg(test)]
            Self::Duplex(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Tls(stream) => stream.is_write_vectored(),
            Self::Plain(stream) => stream.is_write_vectored(),
            #[cfg(test)]
            Self::Duplex(stream) => stream.is_write_vectored(),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write_vectored(cx, bufs),
            Self::Plain(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            #[cfg(test)]
            Self::Duplex(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(test)]
            Self::Duplex(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(test)]
            Self::Duplex(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

impl AsyncRead for RequestIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        let progress = self.slot.current();
        if buf.filled().len() > before {
            progress.response_started.store(true, Ordering::Release);
        }
        if let Poll::Ready(Err(error)) = &result {
            self.record_io_error(error);
        }
        result
    }
}

impl AsyncWrite for RequestIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, buf);
        if let Poll::Ready(Err(error)) = &result {
            self.record_io_error(error);
        }
        if let Poll::Ready(Ok(n)) = result
            && n > 0
        {
            self.slot.current().record_accepted(n);
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Err(error)) = &result {
            self.record_io_error(error);
        }
        if let Poll::Ready(Ok(n)) = result
            && n > 0
        {
            self.slot.current().record_accepted(n);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let result = Pin::new(&mut self.stream).poll_flush(cx);
        if let Poll::Ready(Err(error)) = &result {
            self.record_io_error(error);
        }
        if matches!(result, Poll::Ready(Ok(()))) {
            let progress = self.slot.current();
            if !progress.http2
                && progress.accepted() > 0
                && progress.consumed.load(Ordering::Acquire)
            {
                progress.mark_complete();
            }
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(mut bytes) = self.bytes.take() {
            // H2 polls the body as flow-control capacity becomes available.
            // Bounded frames expose that progress instead of consuming the whole
            // request in one poll before it has left the dispatcher.
            let frame = if self.progress.http2 {
                let frame = bytes.split_to(bytes.len().min(16 * 1024));
                self.progress.record_write_progress();
                frame
            } else {
                bytes.split_to(bytes.len())
            };
            self.length -= u64::try_from(frame.len()).unwrap_or(u64::MAX);
            if bytes.is_empty() {
                self.progress.mark_consumed();
            } else {
                self.bytes = Some(bytes);
            }
            Poll::Ready(Some(Ok(Frame::data(frame))))
        } else {
            self.progress.mark_consumed();
            Poll::Ready(None)
        }
    }

    fn is_end_stream(&self) -> bool {
        self.bytes.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.length)
    }
}
