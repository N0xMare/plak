use super::*;
use plakous_host_protocol::wire::{Answer, HostFrame, Outcome, decode_line};
use std::io::Write as _;

fn context() -> (tempfile::TempDir, Served) {
    let home = tempfile::tempdir().unwrap();
    let paths = crate::home::paths::PlakPaths::explicit(home.path().to_path_buf());
    paths.ensure_layout().unwrap();
    let host = principal_tests::empty_host();
    let context = Served {
        host,
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
    (home, context)
}

#[tokio::test(start_paused = true)]
async fn web_reads_ready_pings_before_draining_an_attach_baseline() {
    let (_home, mut context) = context();
    context.host = principal_tests::archived_host();
    let outbox = Outbox::default();
    let mut connection = context
        .host
        .connect(
            outbox.clone(),
            Face::Remote,
            plakous_hosted::host_service::wire::HostPrincipal::Web("fixture".into()),
            || false,
        )
        .with_snapshot_output(|_, _| panic!("baseline drained before the ready client frames"));
    // Settle the attach and let its reply go out, leaving its real baseline
    // in the service queue rather than the transport outbox.
    assert!(connection
        .frame(Ok(r#"{"frame":"request","id":1,"op":{"op":"attach","node":{"tree_id":"archived","agent_id":"archived"}}}"#.into()))
        .unwrap());
    let (server, client) = tokio::io::duplex(4096);
    let mut server =
        WebSocketStream::from_raw_socket(ProgressIo::new(server), Role::Server, None).await;
    let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let progress = server.get_ref().progress.clone();
    outbox
        .send_batch(
            &progress,
            || false,
            &mut WebWriter {
                sink: &mut server,
                heartbeat: Heartbeat::default(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        decode_line::<HostFrame<PlakExtension>>(
            client.next().await.unwrap().unwrap().to_text().unwrap()
        ),
        Ok(HostFrame::Response {
            id: 1,
            outcome: Outcome::Ok(Answer::Attached { .. })
        })
    ));
    for id in [2, 3] {
        client
            .send(Message::text(format!(
                r#"{{"frame":"request","id":{id},"op":{{"op":"ping"}}}}"#
            )))
            .await
            .unwrap();
    }
    client
        .send(Message::text(r#"{"frame":"bye"}"#))
        .await
        .unwrap();
    let reading = async {
        for id in [2, 3] {
            assert!(matches!(
                decode_line::<HostFrame<PlakExtension>>(
                    client.next().await.unwrap().unwrap().to_text().unwrap()
                ),
                Ok(HostFrame::Response { id: got, outcome: Outcome::Ok(Answer::Pong) }) if got == id
            ));
        }
        assert!(matches!(client.next().await, Some(Ok(Message::Close(_)))));
    };
    let (ended, ()) = tokio::join!(
        attached(server, &context, &mut connection, &outbox),
        reading
    );
    assert!(matches!(ended, Ended::Bye));
}

#[tokio::test(start_paused = true)]
async fn slow_link_web_reads_ping_during_a_blocked_send_and_answers_before_the_next_page() {
    let (_home, context) = context();
    let outbox = Outbox::default();
    let first = "x".repeat(64 * 1024);
    let second = "second page";
    for page in [&*first, second] {
        outbox
            .clone()
            .write_all(format!("{page}\n").as_bytes())
            .unwrap();
    }
    let mut connection = context
        .host
        .connect(
            outbox.clone(),
            Face::Remote,
            plakous_hosted::host_service::wire::HostPrincipal::Web("fixture".into()),
            || false,
        )
        .buffered_output();
    let (server, client) = tokio::io::duplex(4096);
    let server =
        WebSocketStream::from_raw_socket(ProgressIo::new(server), Role::Server, None).await;
    let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let reading = async {
        // Let the writer fill the pipe with a frame the client hasn't read.
        tokio::time::sleep(Duration::from_secs(1)).await;
        client
            .send(Message::text(
                r#"{"frame":"request","id":42,"op":{"op":"ping"}}"#,
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(client.next().await.unwrap().unwrap(), Message::text(first));
        let reply = client.next().await.unwrap().unwrap().into_text().unwrap();
        assert!(matches!(
            plakous_host_protocol::wire::decode_line::<
                plakous_host_protocol::wire::HostFrame<PlakExtension>,
            >(&reply),
            Ok(plakous_host_protocol::wire::HostFrame::Response {
                id: 42,
                outcome: plakous_host_protocol::wire::Outcome::Ok(
                    plakous_host_protocol::wire::Answer::Pong
                )
            })
        ));
        assert_eq!(client.next().await.unwrap().unwrap(), Message::text(second));
        client
            .send(Message::text(r#"{"frame":"bye"}"#))
            .await
            .unwrap();
        assert!(matches!(client.next().await, Some(Ok(Message::Close(_)))));
    };
    let (ended, ()) = tokio::join!(
        attached(server, &context, &mut connection, &outbox),
        reading
    );
    assert!(matches!(ended, Ended::Bye));
}

#[tokio::test(start_paused = true)]
async fn slow_link_web_queued_heartbeat_has_no_deadline_until_the_writer_reaches_it() {
    let heartbeat = Heartbeat::default();
    let outbox = Outbox::default();
    heartbeat
        .tick(&outbox)
        .unwrap_or_else(|_| panic!("first heartbeat was refused"));
    tokio::time::advance(PONG_DEADLINE + PING_EVERY).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), heartbeat.expired())
            .await
            .is_err()
    );
    let (server, client) = tokio::io::duplex(4096);
    let server =
        WebSocketStream::from_raw_socket(ProgressIo::new(server), Role::Server, None).await;
    let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let progress = server.get_ref().progress.clone();
    outbox
        .send_batch(
            &progress,
            || false,
            &mut WebWriter {
                sink: server,
                heartbeat: heartbeat.clone(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(client.next().await, Some(Ok(Message::Ping(_)))));
    tokio::time::advance(PONG_DEADLINE).await;
    assert!(
        tokio::time::timeout(Duration::ZERO, heartbeat.expired())
            .await
            .is_ok()
    );
}
