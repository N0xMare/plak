use super::*;
use crate::host::connection::test_fixtures as paced;
use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::wire::{
    AttachmentId, ClientDraftId, ClientFrame, NodeAddress, Op, RequestRefusal, SnapshotAssembler,
};
use plakous_host_protocol::{SessionIntent, SessionObservation};
use plakous_hosted::host_service::wire::HostPrincipal;

async fn read_frame(client: &mut Client) -> HostFrame<PlakExtension> {
    let message = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .expect("timed out waiting for paced output")
        .unwrap()
        .unwrap();
    decode_line(message.to_text().unwrap()).unwrap()
}

async fn drain_paused_baseline(
    client: &mut Client,
    attachment: AttachmentId,
    snapshot: &PresentationSnapshot,
) {
    let mut assembled = SnapshotAssembler::default();
    for number in 0..3 {
        let HostFrame::Snapshot(page) = read_frame(client).await else {
            panic!("expected snapshot page {number}");
        };
        assert_eq!(
            (page.attachment, page.page, page.last),
            (attachment, number, number == 2)
        );
        let complete = assembled.push(page).unwrap();
        assert_eq!(complete.as_ref(), (number == 2).then_some(snapshot));
        if number == 0 {
            assert!(matches!(
                read_frame(client).await,
                HostFrame::Response {
                    id: 2,
                    outcome: Outcome::Ok(Answer::Pong)
                }
            ));
            assert!(matches!(
                read_frame(client).await,
                HostFrame::Response {
                    id: 3,
                    outcome: Outcome::Refused(RequestRefusal::Backpressure)
                }
            ));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn web_upgrade_pause_keeps_a_resident_baseline_and_control_responsive() {
    let (_home, context) = context();
    let (resident, _hold) = context.host.slot("resident").resident().unwrap();
    let Some(SessionObservation::PresentationSnapshot(snapshot)) =
        paced::history().into_iter().next()
    else {
        panic!("expected a resident baseline fixture");
    };
    resident
        .publisher()
        .publish(SessionObservation::PresentationSnapshot(snapshot.clone()));
    let outbox = Outbox::default();
    let mut connection =
        paced::connection(&context.host, &outbox, HostPrincipal::Web("fixture".into()));
    let attach = ClientFrame::<PlakExtension>::Request {
        id: 1,
        op: Op::Attach {
            node: NodeAddress {
                tree_id: "resident".into(),
                agent_id: "resident".into(),
            },
        },
    };
    assert!(connection.frame(Ok(encode_line(&attach).unwrap())).unwrap());

    let (server, mut client) = pair().await;
    let reading = async {
        let HostFrame::Response {
            id: 1,
            outcome: Outcome::Ok(Answer::Attached { attachment }),
        } = read_frame(&mut client).await
        else {
            panic!("expected a live attachment");
        };
        paced::wait_for_snapshot(&outbox, 1).await;
        let pause = context
            .host
            .pause_admissions_for_upgrade(Arc::default(), true)
            .unwrap();
        let draft_id = ClientDraftId::new();
        let prompt = |id| {
            Message::text(
                encode_line(&ClientFrame::<PlakExtension>::Request {
                    id,
                    op: Op::Intent {
                        attachment,
                        intent: SessionIntent::Prompt("prompt".into()),
                        client_id: Some(draft_id),
                        blobs: Vec::new(),
                    },
                })
                .unwrap(),
            )
        };
        client
            .send(Message::text(
                r#"{"frame":"request","id":2,"op":{"op":"ping"}}"#,
            ))
            .await
            .unwrap();
        client.send(prompt(3)).await.unwrap();
        // Both replies are already queued while the first page fills the link.
        paced::wait_for_snapshot(&outbox, 3).await;

        drain_paused_baseline(&mut client, attachment, &snapshot).await;
        drop(pause);
        // The same draft is first admitted on the original attachment now.
        client.send(prompt(4)).await.unwrap();
        assert!(matches!(
            read_frame(&mut client).await,
            HostFrame::Response {
                id: 4,
                outcome: Outcome::Ok(Answer::Accepted { duplicate: false })
            }
        ));
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
    let failures = context.host.hand_off_all(Duration::from_secs(2));
    assert!(failures.is_empty(), "{failures:?}");
}
