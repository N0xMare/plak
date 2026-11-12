//! Frames over a byte stream: one task writes queued frames, the other end of
//! the connection reads them one at a time.

use plakous_model::CancellationToken;
use plakous_runner_protocol::frame::{FrameError, FrameReader, decode, encode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// Runner writes control ahead of bounded operator bulk, draining queued refusals
/// before close. A stalled write has a fixed bound, not an unbounded bulk queue.
pub(crate) async fn write_prioritized<W, T>(
    mut writer: W,
    mut control: mpsc::UnboundedReceiver<T>,
    mut bulk: mpsc::Receiver<T>,
    closer: CancellationToken,
) where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    loop {
        let frame = tokio::select! {
            biased;
            frame=control.recv()=>frame,
            frame=bulk.recv()=>frame,
            ()=closer.cancelled()=>None,
        };
        let Some(frame) = frame else {
            break;
        };
        let Ok(bytes) = encode(&frame) else {
            continue;
        };
        let written = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            writer.write_all(&bytes).await?;
            writer.flush().await
        })
        .await;
        if !matches!(written, Ok(Ok(()))) {
            closer.cancel();
            break;
        }
    }
    let _ = writer.shutdown().await;
}

/// Writes each queued frame until the queue closes, the stream fails or
/// `closer` fires, then shuts the write half down so the peer sees the end. A
/// frame that cannot be encoded is skipped; senders check their frames first
/// with [`fits`].
pub(crate) async fn write_frames<W, T>(
    mut writer: W,
    mut frames: mpsc::UnboundedReceiver<T>,
    closer: CancellationToken,
) where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    loop {
        // Frames already queued are written before a close takes effect.
        let frame = tokio::select! {
            biased;
            frame = frames.recv() => frame,
            () = closer.cancelled() => None,
        };
        let Some(frame) = frame else {
            break;
        };
        let Ok(bytes) = encode(&frame) else {
            continue;
        };
        if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

/// Whether `frame` fits in one frame.
pub(crate) fn fits(frame: &impl Serialize) -> bool {
    encode(frame).is_ok()
}

/// The reading half of a connection.
pub(crate) struct FrameStream<R> {
    reader: R,
    frames: FrameReader,
    buffer: Box<[u8]>,
    /// A framing error left the stream without frame boundaries.
    broken: bool,
}

impl<R: AsyncRead + Unpin> FrameStream<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader,
            frames: FrameReader::new(),
            buffer: vec![0_u8; 64 * 1024].into_boxed_slice(),
            broken: false,
        }
    }

    /// The next frame; `None` once the stream ended or broke. A body that
    /// does not decode is one error and the stream goes on; an oversized
    /// prefix is the last answer, since nothing after it can be framed.
    pub(crate) async fn next<T: DeserializeOwned>(&mut self) -> Option<Result<T, FrameError>> {
        if self.broken {
            return None;
        }
        loop {
            match self.frames.next_frame() {
                Ok(Some(body)) => return Some(decode(&body)),
                Ok(None) => {}
                Err(error) => {
                    self.broken = true;
                    return Some(Err(error));
                }
            }
            match self.reader.read(&mut self.buffer).await {
                Ok(0) | Err(_) => return None,
                Ok(read) => self.frames.push(&self.buffer[..read]),
            }
        }
    }
}
