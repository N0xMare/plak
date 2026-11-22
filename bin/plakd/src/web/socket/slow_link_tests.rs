use super::*;
use plakous_host_protocol::wire::{
    Answer, HostFrame, Outcome, decode_line, encode_line, page_snapshot,
};
use std::io::Write as _;

mod paused_page;

#[tokio::test(start_paused = true)]
async fn web_paced_pages_deliver_ping_before_replay_without_closing() {
    use crate::host::connection::test_fixtures as paced;
    use plakous_host_protocol::SessionObservation;
    let (_home, mut context) = context();
    context.host = principal_tests::archived_history(paced::history);
    let outbox = Outbox::default();
    let mut connection = paced::connection(
        &context.host,
        &outbox,
        plakous_hosted::host_service::wire::HostPrincipal::Web("fixture".into()),
    );
    assert!(connection.frame(Ok(ATTACH.into())).unwrap());
    let (server, mut client) = pair().await;
    let reading = async {
        attached_answer(&mut client).await;
        paced::wait_for_snapshot(&outbox, 1).await;
        client
            .send(Message::text(
                r#"{"frame":"request","id":42,"op":{"op":"ping"}}"#,
            ))
            .await
            .unwrap();
        paced::wait_for_snapshot(&outbox, 2).await;
        let mut kinds = Vec::new();
        while kinds.len() < 6 {
            let frame = client.next().await.unwrap().unwrap();
            kinds.push(
                match decode_line::<HostFrame<PlakExtension>>(frame.to_text().unwrap()).unwrap() {
                    HostFrame::Snapshot(page) if page.last => "last page",
                    HostFrame::Snapshot(_) => "page",
                    HostFrame::Response {
                        id: 42,
                        outcome: Outcome::Ok(Answer::Pong),
                    } => "pong",
                    HostFrame::Event {
                        observation: SessionObservation::TranscriptPatch(_),
                        ..
                    } => "patch",
                    _ => panic!("unexpected paced frame"),
                },
            );
        }
        assert_eq!(
            kinds,
            ["page", "pong", "page", "last page", "patch", "patch"]
        );
        client
            .send(Message::text(r#"{"frame":"bye"}"#))
            .await
            .unwrap();
    };
    let (ended, ()) = tokio::join!(
        attached(server, &context, &mut connection, &outbox),
        reading
    );
    assert!(matches!(ended, Ended::Bye));
}

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
    check_ready_pings(2, false).await;
}

#[tokio::test(start_paused = true)]
async fn web_ready_pings_cannot_starve_an_attach_baseline() {
    // Eight reads may precede each of the two flush phases. The bye is
    // also ready; if read early it discards the still-pending baseline.
    check_ready_pings(16, true).await;
}

fn archived_connection(served: &Served, outbox: &Outbox) -> Connection {
    let snapshots = outbox.clone();
    served
        .host
        .connect(
            outbox.clone(),
            Face::Remote,
            plakous_hosted::host_service::wire::HostPrincipal::Web("fixture".into()),
            || false,
        )
        .with_snapshot_output(move |attachment, snapshot| {
            for page in page_snapshot(snapshot, attachment).unwrap() {
                let line = encode_line(&HostFrame::<PlakExtension>::Snapshot(page)).unwrap();
                snapshots.clone().write_all(line.as_bytes()).unwrap();
            }
            Ok(())
        })
}

const ATTACH: &str = r#"{"frame":"request","id":1,"op":{"op":"attach","node":{"tree_id":"archived","agent_id":"archived"}}}"#;

type Server = WebSocketStream<ProgressIo<tokio::io::DuplexStream>>;
type Client = WebSocketStream<tokio::io::DuplexStream>;

async fn pair() -> (Server, Client) {
    let (server, client) = tokio::io::duplex(4096);
    (
        WebSocketStream::from_raw_socket(ProgressIo::new(server), Role::Server, None).await,
        WebSocketStream::from_raw_socket(client, Role::Client, None).await,
    )
}

async fn send_pending(outbox: &Outbox, server: &mut Server) {
    let progress = server.get_ref().progress.clone();
    outbox
        .send_batch(
            &progress,
            || false,
            &mut WebWriter {
                sink: server,
                heartbeat: Heartbeat::default(),
            },
        )
        .await
        .unwrap();
}

async fn attached_answer(client: &mut Client) {
    let reply = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .expect("timed out waiting for Attached")
        .unwrap()
        .unwrap();
    assert!(matches!(
        decode_line::<HostFrame<PlakExtension>>(reply.to_text().unwrap()),
        Ok(HostFrame::Response {
            id: 1,
            outcome: Outcome::Ok(Answer::Attached { .. })
        })
    ));
}

async fn check_ready_pings(count: u64, expect_baseline: bool) {
    let (_home, mut context) = context();
    context.host = principal_tests::archived_host();
    let outbox = Outbox::default();
    let mut connection = archived_connection(&context, &outbox);
    // Send Attached, but leave its baseline in the service queue.
    assert!(connection.frame(Ok(ATTACH.into())).unwrap());
    let (mut server, mut client) = pair().await;
    send_pending(&outbox, &mut server).await;
    attached_answer(&mut client).await;
    for id in 2..2 + count {
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
        let mut pongs = Vec::new();
        let mut snapshots = 0;
        loop {
            let message = client.next().await.unwrap().unwrap();
            if matches!(message, Message::Close(_)) {
                break;
            }
            match decode_line::<HostFrame<PlakExtension>>(message.to_text().unwrap()).unwrap() {
                HostFrame::Response {
                    id,
                    outcome: Outcome::Ok(Answer::Pong),
                } => pongs.push(id),
                HostFrame::Snapshot(_) => snapshots += 1,
                _ => panic!("unexpected frame"),
            }
        }
        assert_eq!(pongs, (2..2 + count).collect::<Vec<_>>());
        assert_eq!(snapshots, usize::from(expect_baseline));
    };
    let (ended, ()) = tokio::join!(
        attached(server, &context, &mut connection, &outbox),
        reading
    );
    assert!(matches!(ended, Ended::Bye));
}

#[tokio::test]
async fn web_attach_reply_and_baseline_enter_the_same_ordered_batch() {
    let (_home, mut context) = context();
    context.host = principal_tests::archived_host();
    let outbox = Outbox::default();
    let mut connection = archived_connection(&context, &outbox).buffered_output();
    assert!(connection.frame(Ok(ATTACH.into())).unwrap());
    tokio::time::timeout(Duration::from_secs(5), connection.changed())
        .await
        .unwrap();

    let mut flush = ConnectionFlush::default();
    flush.step(&mut connection, &outbox).unwrap();
    flush.step(&mut connection, &outbox).unwrap();
    // No writer has run between the two steps.
    let (mut server, mut client) = pair().await;
    send_pending(&outbox, &mut server).await;
    attached_answer(&mut client).await;
    let page = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        decode_line::<HostFrame<PlakExtension>>(page.to_text().unwrap()),
        Ok(HostFrame::Snapshot(_))
    ));
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
            decode_line::<HostFrame<PlakExtension>>(&reply),
            Ok(HostFrame::Response {
                id: 42,
                outcome: Outcome::Ok(Answer::Pong)
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
