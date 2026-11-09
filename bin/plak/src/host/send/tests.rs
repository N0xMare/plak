use super::*;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test(start_paused = true)]
async fn slow_link_stalled_reader_hits_idle_deadline() {
    let (write, _unread) = tokio::io::duplex(1);
    let mut write = ProgressIo::new(write);
    let began = Instant::now();
    assert!(
        write
            .progress
            .clone()
            .sending(write.write_all(&[42; 100]))
            .await
            .is_err()
    );
    assert_eq!(Instant::now().duration_since(began), IDLE);
}

#[tokio::test(start_paused = true)]
async fn slow_link_trickling_reader_still_hits_hard_total_deadline() {
    let (write, mut read) = tokio::io::duplex(1);
    let mut write = ProgressIo::new(write);
    let drain = tokio::spawn(async move {
        let mut byte = [0];
        while read.read_exact(&mut byte).await.is_ok() {
            tokio::time::sleep(IDLE / 2).await;
        }
    });
    let began = Instant::now();
    assert!(
        write
            .progress
            .clone()
            .sending(write.write_all(&[42; 1000]))
            .await
            .is_err()
    );
    assert_eq!(Instant::now().duration_since(began), TOTAL);
    drop(write);
    drain.await.unwrap();
}

#[cfg(feature = "web")]
#[tokio::test(start_paused = true)]
async fn slow_link_websocket_large_frame_makes_byte_progress() {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{Message, protocol::Role},
    };

    let (server, client) = tokio::io::duplex(8192);
    let mut server =
        WebSocketStream::from_raw_socket(ProgressIo::new(server), Role::Server, None).await;
    let reader = tokio::spawn(async move {
        let mut client =
            WebSocketStream::from_raw_socket(Throttled::new(client), Role::Client, None).await;
        client.next().await.unwrap().unwrap()
    });
    let text = "x".repeat(2 * 1024 * 1024);
    let began = Instant::now();
    server
        .get_ref()
        .progress
        .clone()
        .sending(server.send(Message::text(text.clone())))
        .await
        .unwrap()
        .unwrap();
    assert!(Instant::now().duration_since(began) > IDLE);
    assert_eq!(reader.await.unwrap(), Message::text(text));
}

#[cfg(feature = "web")]
struct Throttled {
    io: tokio::io::DuplexStream,
    next: Pin<Box<tokio::time::Sleep>>,
}

#[cfg(feature = "web")]
impl Throttled {
    fn new(io: tokio::io::DuplexStream) -> Self {
        Self {
            io,
            next: Box::pin(tokio::time::sleep(Duration::ZERO)),
        }
    }
}

#[cfg(feature = "web")]
impl AsyncRead for Throttled {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.next.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        let mut chunk = [0; 8192];
        let count = buf.remaining().min(chunk.len());
        let mut limited = ReadBuf::new(&mut chunk[..count]);
        match Pin::new(&mut self.io).poll_read(cx, &mut limited) {
            Poll::Ready(Ok(())) => {
                buf.put_slice(limited.filled());
                self.next
                    .as_mut()
                    .reset(Instant::now() + Duration::from_secs(1));
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

#[cfg(feature = "web")]
impl AsyncWrite for Throttled {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
