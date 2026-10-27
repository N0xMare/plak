//! Real HTTP/1 writes over a bounded stream, with a virtual deadline clock.

#![allow(clippy::unwrap_used, reason = "test-owned HTTP exchange")]

use std::sync::Arc;
use std::time::Duration;

use http_body_util::BodyExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
use tokio::time::{Instant, advance};

use super::{
    ConnectionTask, Limits, ProgressSlot, RequestBody, RequestProgress, Sender, WireStream,
};
use crate::{ErrorKind, ExchangeControl, Failure, Phase, TransmissionFact, WireProtocol};

type Exchange = tokio::task::JoinHandle<
    Result<(http::Response<hyper::body::Incoming>, ConnectionTask), Failure>,
>;

async fn start_upload(limits: Limits) -> (DuplexStream, Arc<RequestProgress>, Exchange) {
    let (stream, peer) = tokio::io::duplex(16 * 1024);
    let bytes = vec![b'x'; limits.max_request_body_bytes].into();
    let progress = Arc::new(RequestProgress::new(
        false,
        false,
        limits.max_request_body_bytes.try_into().unwrap(),
    ));
    let mut connection = super::connection::establish(
        WireStream::Duplex(stream),
        WireProtocol::Http1,
        &limits,
        ProgressSlot::new(Arc::clone(&progress)),
    )
    .await
    .unwrap();
    let Some(Sender::Http1(mut sender)) = connection.sender.take() else {
        panic!("the fixture established HTTP/1");
    };
    let request = http::Request::builder()
        .method("POST")
        .uri("/steady-write")
        .header("host", "fixture.invalid")
        .body(RequestBody::new(bytes, Arc::clone(&progress)))
        .unwrap();
    let writer = Arc::clone(&progress);
    let control = ExchangeControl::new((Instant::now() + Duration::from_secs(15)).into_std());
    let exchange = tokio::spawn(async move {
        super::wait_response(
            sender.send_request(request),
            connection,
            writer,
            &control,
            &limits,
        )
        .await
    });
    (peer, progress, exchange)
}

#[tokio::test(start_paused = true)]
async fn steady_request_write_outlives_idle_bound() {
    let limits = Limits {
        request_write_idle_timeout: Duration::from_millis(300),
        response_head_timeout: Duration::from_millis(500),
        ..Limits::default()
    };
    for reply in [true, false] {
        let (mut peer, progress, exchange) = start_upload(limits.clone()).await;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            head.push(peer.read_u8().await.unwrap());
        }
        let start = Instant::now();
        let mut body = vec![0; 1024 * 1024];
        // The pipe cannot accept the whole body. Drain it in eight groups;
        // each real positive RequestIo write refreshes the idle clock. Unlike
        // TCP/TLS buffering, the remaining tail has a fixed, known bound.
        for group in 0..8 {
            peer.read_exact(&mut body).await.unwrap();
            assert!(body.iter().all(|byte| *byte == b'x'));
            if group < 7 {
                advance(Duration::from_millis(200)).await;
                assert!(
                    !exchange.is_finished(),
                    "steady writes must outlive the idle bound"
                );
            }
        }
        assert!(Instant::now() - start > Duration::from_secs(1));
        assert!(Instant::now() - start > limits.request_write_idle_timeout);
        let flushed = progress.completed.notified();
        if !progress.is_complete() {
            flushed.await;
        }
        let completed = Instant::from_std(*progress.complete.get().unwrap());
        assert_eq!(completed, Instant::now());
        advance(
            limits
                .response_head_timeout
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
        )
        .await;
        assert!(
            !exchange.is_finished(),
            "the full head allowance follows the final flush"
        );
        if reply {
            peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
            let (response, mut connection) = exchange.await.unwrap().unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "ok"
            );
            connection.abort_and_join().await.unwrap();
        } else {
            advance(Duration::from_millis(1)).await;
            let failure = exchange.await.unwrap().err().unwrap();
            assert_eq!(failure.phase(), Phase::ResponseHead);
            assert_eq!(failure.kind(), ErrorKind::DeadlineExceeded);
            assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
            assert_eq!(Instant::now(), completed + limits.response_head_timeout);
        }
    }
}
