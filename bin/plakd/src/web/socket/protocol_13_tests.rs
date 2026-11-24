use futures_util::{SinkExt, StreamExt};
use plakd_wire_types::{PlakExtension, v13};
use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::transcript::{TranscriptRow, TranscriptScope};
use plakous_host_protocol::wire::{self, FramedTranscriptRow, NodeAddress};
use plakous_host_protocol::{
    ContentObservation, SessionObservation, intent_channel, observation_channel,
};
use plakous_hosted::host_service::{HostService, Outbound, wire::Face};
use plakous_hosted::runtime::{Sender, test_kit};
use serde_json::{Value, json};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::protocol::{Message, Role},
};

use crate::host::protocol_13;

type Socket = WebSocketStream<tokio::io::DuplexStream>;

async fn transfer<T: serde::de::DeserializeOwned>(
    from: &mut Socket,
    to: &mut Socket,
    frame: &impl serde::Serialize,
) -> T {
    from.send(Message::text(wire::encode_line(frame).unwrap()))
        .await
        .unwrap();
    let arrived = to.next().await.unwrap().unwrap();
    serde_json::from_str(arrived.to_text().unwrap()).unwrap()
}

async fn attach(
    client: &mut Socket,
    host: &mut Socket,
    service: &HostService<PlakExtension>,
    node: NodeAddress,
) -> wire::AttachmentId {
    let v13::ClientFrame::Draft(v13::ClientFrameV13::Request { id, op }) = transfer(
        client,
        host,
        &json!({"frame": "request", "id": 1, "op": {"op": "attach", "node": node}}),
    )
    .await
    else {
        panic!("draft attach request")
    };
    let v13::Op::Draft(v13::OpV13::Attach {
        node,
        retained: None,
    }) = *op
    else {
        panic!("new attachment")
    };
    let attachment = service.attach_keyed_root(&node, Face::Remote).unwrap();
    let outcome = v13::Outcome::Ok(Box::new(v13::Answer::Draft(v13::AnswerV13::Attached {
        attachment,
        transcript: v13::AttachTranscript::FirstPage,
    })));
    let received: Value =
        transfer(host, client, &v13::HostFrameV13::Response { id, outcome }).await;
    assert_eq!(
        received,
        json!({
            "frame": "response", "id": 1, "outcome": {"ok": {
                "answer": "attached", "attachment": attachment, "transcript": {"mode": "first_page"}
            }},
        })
    );
    attachment
}

#[tokio::test]
async fn protocol_13_websocket_late_attach_and_addressed_row_repair() {
    // Use a duplex transport, not a listener that could select the draft.
    let (client, host) = tokio::io::duplex(4096);
    let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let mut host = WebSocketStream::from_raw_socket(host, Role::Server, None).await;
    let node = NodeAddress {
        tree_id: "tree".into(),
        agent_id: "root".into(),
    };
    let (actions, inbox) = intent_channel();
    let service = HostService::<PlakExtension>::new(actions, node.clone(), 8);
    let (channel, events) = observation_channel();
    let (sender, root) = Sender::over_keyed_root(channel);
    let relay = || events.try_iter().for_each(|event| service.observe(event));
    assert!(test_kit::replace_transcript(
        &sender,
        PresentationSnapshot::default()
    ));
    sender.send(ContentObservation::AssistantDelta("Café".into()));
    relay();

    let attachment = attach(&mut client, &mut host, &service, node).await;
    sender.send(ContentObservation::AssistantDelta(" ☕".into()));
    relay();
    assert_eq!(service.take(attachment).as_slice(), &[]);
    assert!(test_kit::repair_root_attachment(
        &sender,
        &inbox.try_recv().unwrap()
    ));
    sender.send(ContentObservation::AssistantDelta("!".into()));
    relay();
    let [
        Outbound::Event {
            observation: SessionObservation::PresentationSnapshot(snapshot),
            ..
        },
        Outbound::Event {
            seq,
            class,
            observation:
                SessionObservation::TranscriptStream {
                    scope: TranscriptScope::Root,
                    delta,
                },
            ..
        },
    ]: [Outbound<PlakExtension>; 2] = service.take(attachment).try_into().unwrap()
    else {
        panic!("first page followed by its suffix")
    };

    let baseline = first_page(attachment, snapshot);
    let received: Value = transfer(
        &mut host,
        &mut client,
        &v13::HostFrameV13::Snapshot(baseline),
    )
    .await;
    let row = received["items"][0]["key"].clone();
    assert_eq!(received["scope"], "root");
    assert_eq!(received["items"][0]["row"]["text"], "Café ☕");
    assert_eq!(received["live_rows"], json!([{"row": row, "len": 9}]));

    let received: Value = transfer(
        &mut host,
        &mut client,
        &v13::HostFrameV13::Event {
            attachment,
            seq,
            class,
            observation: Box::new(protocol_13::append(delta).unwrap()),
            client_id: None,
            blobs: Vec::new(),
        },
    )
    .await;
    assert_eq!(
        received["observation"],
        json!({
            "kind": "transcript_stream", "body": {"row": row, "offset": 9, "end": 10, "text": "!"},
        })
    );

    let received = repair(&mut client, &mut host, &root, attachment, &row).await;
    assert_eq!(
        received,
        json!({
            "frame": "response", "id": 2, "outcome": {"ok": {
                "answer": "transcript_row", "row": row, "offset": 3, "end": 10, "text": "é ☕!", "last": true,
            }},
        })
    );
    assert!(
        service.take(attachment).is_empty(),
        "the read didn't publish a baseline"
    );
}

async fn repair(
    client: &mut Socket,
    host: &mut Socket,
    root: &plakous_hosted::runtime::RootTranscript,
    attachment: wire::AttachmentId,
    row: &Value,
) -> Value {
    let v13::ClientFrame::Draft(v13::ClientFrameV13::Request { id, op }) = transfer(
        client,
        host,
        &json!({
            "frame": "request", "id": 2,
            "op": {"op": "read_transcript_row", "attachment": attachment, "row": row, "offset": 3},
        }),
    )
    .await
    else {
        panic!("draft row read request")
    };
    let v13::Op::Draft(op) = *op else {
        panic!("draft row read")
    };
    let outcome = protocol_13::read_live_row(root, attachment, &op).unwrap();
    transfer(host, client, &v13::HostFrameV13::Response { id, outcome }).await
}

fn first_page(attachment: wire::AttachmentId, snapshot: PresentationSnapshot) -> v13::Baseline {
    let live_rows = protocol_13::live_lengths(&snapshot.items);
    v13::Baseline {
        attachment,
        scope: snapshot.scope,
        revision: v13::RetainedRevision {
            host_generation: 1,
            revision: snapshot.revision,
            token: "test-viewport".into(),
        },
        part: 0,
        last: true,
        items: snapshot
            .items
            .into_iter()
            .map(|item| {
                assert_eq!(item.row, TranscriptRow::assistant("Café ☕"));
                v13::PageItem {
                    key: item.key,
                    row: FramedTranscriptRow(item.row),
                    truncated_bytes: None,
                }
            })
            .collect(),
        has_older: false,
        older_rows: 0,
        older_bytes: 0,
        older_cursor: None,
        live_rows,
    }
}
