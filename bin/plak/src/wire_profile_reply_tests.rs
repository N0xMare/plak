//! Addressed profile verdicts follow only the requester's live wire handoff.

#![allow(clippy::unwrap_used, reason = "deterministic wire fixture")]

use plakd_wire_types::{PlakExtension, ProfileOutcome, SurfaceObservation};
use plakous_host_protocol::wire::{
    Answer, ClientFrame, HostFrame, NodeAddress, Op, Outcome, decode_line, encode_line,
};
use plakous_host_protocol::{SessionObservation, observation_channel};
use plakous_hosted::host_service::wire::{Face, WireConnection, WireSession};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Bytes(Arc<Mutex<Vec<u8>>>);

impl Write for Bytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Bytes {
    fn frames(&self) -> Vec<HostFrame<PlakExtension>> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| decode_line(line).unwrap())
            .collect()
    }

    fn applied(&self, name: &str) -> Vec<u64> {
        self.frames()
            .into_iter()
            .filter_map(|frame| match frame {
                HostFrame::Event {
                    attachment,
                    observation:
                        SessionObservation::Extension(SurfaceObservation::ProfileApplied {
                            name: found,
                            ..
                        }),
                    ..
                } if found == name => Some(attachment),
                _ => None,
            })
            .collect()
    }
}

struct Fixture {
    session: WireSession<PlakExtension>,
    published: plakous_host_protocol::ObservationSender<PlakExtension>,
    intents: plakous_host_protocol::IntentReceiver<PlakExtension>,
    clock: tokio::runtime::Runtime,
}

impl Fixture {
    fn new(same_tree: bool) -> Self {
        let (actions, intents) = plakous_host_protocol::intent_channel::<PlakExtension>();
        let (published, events) = observation_channel::<PlakExtension>();
        Self {
            session: WireSession::start(node("s1"), "s1".into(), actions, events, move |id| {
                Some(node(if same_tree { "s1" } else { id }))
            }),
            published,
            intents,
            clock: tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap(),
        }
    }

    fn publish(&self, event: SessionObservation<PlakExtension>) {
        let mut changed = self.session.changes();
        changed.borrow_and_update();
        self.published.send(event).unwrap();
        self.clock.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), changed.changed())
                .await
                .unwrap()
                .unwrap();
        });
    }

    fn switched(&self, id: &str) {
        self.publish(SessionObservation::SessionOpening);
        self.publish(SessionObservation::Binding {
            session_id: Some(id.into()),
            account: None,
            model: None,
            effort: None,
            extra: None,
            composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
            protocol: plakous_host_protocol::ProtocolSelection::Auto,
            effective_presentation: "direct".into(),
        });
    }

    fn verdict(&self, requester: u64, name: &str) {
        self.begin_verdict(requester);
        self.finish_verdict(name);
    }

    fn begin_verdict(&self, requester: u64) {
        self.publish(SessionObservation::IntentStarted(Some(format!(
            "answer:attachment:{requester}"
        ))));
    }

    fn finish_verdict(&self, name: &str) {
        self.publish(SessionObservation::Extension(
            SurfaceObservation::ProfileApplied {
                name: name.into(),
                outcome: ProfileOutcome::Switched,
            },
        ));
        self.publish(SessionObservation::IntentStarted(None));
    }
}

fn node(id: &str) -> NodeAddress {
    NodeAddress {
        tree_id: id.into(),
        agent_id: id.into(),
    }
}

type Connection = WireConnection<PlakExtension, Bytes>;

fn request(
    connection: &mut Connection,
    out: &Bytes,
    id: u64,
    op: Op<PlakExtension>,
) -> Answer<PlakExtension> {
    assert!(
        connection
            .frame(Ok(encode_line(&ClientFrame::Request { id, op }).unwrap()))
            .unwrap()
    );
    connection.flush().unwrap();
    out.frames()
        .into_iter()
        .rev()
        .find_map(|frame| match frame {
            HostFrame::Response {
                id: found,
                outcome: Outcome::Ok(answer),
            } if found == id => Some(answer),
            _ => None,
        })
        .unwrap()
}

fn attach(connection: &mut Connection, out: &Bytes, root: NodeAddress) -> u64 {
    let Answer::Attached { attachment } = request(connection, out, 1, Op::Attach { node: root })
    else {
        panic!("not attached");
    };
    attachment
}

fn late_verdict(same_tree: bool, face: Face, started_before: bool) {
    let fixture = Fixture::new(same_tree);
    let outputs = [
        Bytes::default(),
        Bytes::default(),
        Bytes::default(),
        Bytes::default(),
    ];
    let other_face = if face == Face::Local {
        Face::Remote
    } else {
        Face::Local
    };
    // Each connection owns a different output and requester, even on the same face.
    let mut connections = Vec::new();
    for (index, client_face) in [face, face, other_face, face].into_iter().enumerate() {
        let mut connection = fixture
            .session
            .connect(outputs[index].clone(), client_face, || false);
        assert_eq!(
            attach(&mut connection, &outputs[index], node("s1")),
            u64::try_from(index).unwrap() + 1
        );
        connections.push(connection);
    }
    fixture.switched("s2");
    for (connection, out) in connections.iter_mut().zip(&outputs) {
        request(connection, out, 2, Op::Ping);
    }
    connections[3].leave();
    if started_before {
        fixture.begin_verdict(1);
    }
    let next = fixture.session.root();
    let requester = attach(&mut connections[0], &outputs[0], next.clone());
    for index in [1, 2] {
        attach(&mut connections[index], &outputs[index], next.clone());
    }
    // A new host generation can reuse the same node, session and attachment id.
    let fresh = Fixture::new(same_tree);
    let fresh_out = Bytes::default();
    let mut fresh_connection = fresh.session.connect(fresh_out.clone(), face, || false);
    assert_eq!(attach(&mut fresh_connection, &fresh_out, node("s1")), 1);
    if started_before {
        fixture.finish_verdict("offline-low");
    } else {
        fixture.verdict(1, "offline-low");
    }
    fixture.verdict(4, "unattached");
    for (connection, out) in connections[..3].iter_mut().zip(&outputs) {
        request(connection, out, 3, Op::Ping);
    }
    request(&mut fresh_connection, &fresh_out, 3, Op::Ping);
    assert_eq!(
        outputs[0].applied("offline-low"),
        [requester],
        "late ProfileApplied must reach only its requester's replacement"
    );
    for out in outputs.iter().skip(1).chain([&fresh_out]) {
        assert_eq!(out.applied("offline-low"), [] as [u64; 0]);
    }
    for out in outputs.iter().chain([&fresh_out]) {
        assert_eq!(out.applied("unattached"), [] as [u64; 0]);
    }
    next_switch(&fixture, &mut connections, &outputs, requester);
    for connection in &mut connections {
        connection.leave();
    }
    fresh_connection.leave();
}

fn next_switch(
    fixture: &Fixture,
    connections: &mut [Connection],
    outputs: &[Bytes],
    requester: u64,
) {
    while fixture.intents.try_recv().is_ok() {}
    request(
        &mut connections[0],
        &outputs[0],
        4,
        Op::Intent {
            client_id: None,
            attachment: requester,
            intent: plakous_host_protocol::SessionIntent::RefreshReadiness,
            blobs: Vec::new(),
        },
    );
    let Some(plakous_host_protocol::ContentObservation::IntentStarted(origin)) =
        fixture.intents.try_recv().unwrap().started()
    else {
        panic!("no requester origin");
    };
    assert_eq!(origin.as_deref(), Some("attachment:1"));
    fixture.publish(SessionObservation::IntentStarted(origin));
    fixture.switched("s3");
    for (connection, out) in connections[..3].iter_mut().zip(outputs) {
        request(connection, out, 5, Op::Ping);
    }
    let root = fixture.session.root();
    let next = attach(&mut connections[0], &outputs[0], root.clone());
    fixture.verdict(1, "offline-medium");
    // Viewers that attach after the reply must not get it through replay either.
    for index in [1, 2] {
        attach(&mut connections[index], &outputs[index], root.clone());
    }
    for (connection, out) in connections[..3].iter_mut().zip(outputs) {
        request(connection, out, 6, Op::Ping);
    }
    assert_eq!(outputs[0].applied("offline-medium"), [next]);
    for out in outputs.iter().skip(1) {
        assert_eq!(out.applied("offline-medium"), [] as [u64; 0]);
    }
}

#[test]
fn late_profile_applied_follows_only_its_requesters_same_generation_reattach() {
    for same_tree in [true, false] {
        for face in [Face::Local, Face::Remote] {
            for started_before in [true, false] {
                late_verdict(same_tree, face, started_before);
            }
        }
    }
}
