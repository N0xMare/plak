use super::*;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[test]
fn slow_link_web_outbox_caps_partial_and_completed_bytes_and_resets_on_take() {
    let mut outbox = Outbox::default();
    let chunk = vec![b'x'; 1024 * 1024 - 1];
    for _ in 0..64 {
        outbox.write_all(&chunk).unwrap();
        outbox.write_all(b"\n").unwrap();
    }
    assert!(outbox.write_all(b"x").is_err());
    let (lines, overflowed) = outbox.take();
    assert_eq!(lines.len(), 64);
    assert!(overflowed);
    drop(lines);
    let mut outbox = Outbox::default();
    for _ in 0..2 {
        for _ in 0..64 {
            outbox.write_all(&chunk).unwrap();
            outbox.write_all(b"\n").unwrap();
        }
        assert!(!outbox.take().1);
    }
    outbox.write_all(&vec![b'x'; MAX_PENDING_BYTES]).unwrap();
    assert!(outbox.write_all(b"x").is_err());
}

#[tokio::test(start_paused = true)]
async fn slow_link_attached_rearms_outstanding_pong_after_throttled_baseline() {
    let home = tempfile::tempdir().unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(home.path().to_path_buf());
    paths.ensure_layout().unwrap();
    let host = principal_tests::empty_host();
    let context = Served {
        host: Arc::clone(&host),
        stop: super::super::StopProbe::armed(),
        ui: None,
        gate: super::super::auth::Gate::new(
            super::super::auth::WebAccess::Dev {
                login: "fixture".into(),
                origin: super::super::auth::WebOrigin::parse("http://127.0.0.1:9191").unwrap(),
            },
            super::super::auth::CookieKey::load_or_create(&paths).unwrap(),
        ),
    };
    let outbox = Outbox::default();
    outbox
        .clone()
        .write_all(format!("{}\n", "x".repeat(512 * 1024)).as_bytes())
        .unwrap();
    let mut connection = host.connect(
        outbox.clone(),
        Face::Remote,
        plakous_hosted::host_service::wire::HostPrincipal::Web("fixture".into()),
        || false,
    );
    let (server, client) = tokio::io::duplex(8192);
    let mut server =
        WebSocketStream::from_raw_socket(ProgressIo::new(server), Role::Server, None).await;
    let reading = tokio::spawn(async move {
        let mut client = WebSocketStream::from_raw_socket(
            Throttled {
                io: client,
                next: Box::pin(tokio::time::sleep(Duration::ZERO)),
            },
            Role::Client,
            None,
        )
        .await;
        assert!(matches!(
            client.next().await.unwrap().unwrap(),
            Message::Text(_)
        ));
        tokio::time::sleep(Duration::from_secs(20)).await;
        client.send(Message::Pong(Bytes::new())).await.unwrap();
        client
            .send(Message::text("{\"frame\":\"bye\"}"))
            .await
            .unwrap();
        client
    });
    let began = Instant::now();
    let mut pong = Some(began);
    let ended = attached(&mut server, &context, &mut connection, &outbox, &mut pong).await;
    assert_eq!(ended.as_str(), "client left");
    assert!(began.elapsed() > PONG_DEADLINE);
    assert!(
        pong.is_none(),
        "buffered pong must be read before overdue ping ticks"
    );
    drop(reading.await.unwrap());
    connection.leave();
}

struct Throttled {
    io: tokio::io::DuplexStream,
    next: Pin<Box<tokio::time::Sleep>>,
}

impl AsyncRead for Throttled {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        use std::future::Future as _;
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

impl AsyncWrite for Throttled {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
