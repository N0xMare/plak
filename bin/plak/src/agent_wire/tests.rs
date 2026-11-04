use std::sync::mpsc;

use plakous_host_protocol::intent::{Draft, DraftImage};
use plakous_host_protocol::observation::ObservationClass;
use plakous_host_protocol::wire::{
    Answer, AttachmentId, ClientFrame, HostFrame, NodeAddress, Op, Outcome, RequestRefusal,
    WIRE_VERSION, encode_line, take_images,
};
use plakous_host_protocol::{SessionIntent, SessionObservation};
use plakous_hosted::host_service::HostService;

use super::*;

type Client = ClientFrame<PlakExtension>;
type Host = HostFrame<PlakExtension>;

/// The first attachment the service makes.
const ROOT_ATTACHMENT: AttachmentId = 1;
use crate::surface_protocol::{
    PlakExtension, SettingsView, SurfaceIntent, SurfaceObservation, user_action_channel,
};

fn root() -> NodeAddress {
    NodeAddress {
        tree_id: "t1".into(),
        agent_id: "t1".into(),
    }
}

fn line(frame: &Client) -> String {
    encode_line(frame).unwrap()
}

fn frames(output: &[u8]) -> Vec<Host> {
    String::from_utf8(output.to_vec())
        .unwrap()
        .lines()
        .map(|line| plakous_host_protocol::wire::decode_host_line(line).unwrap())
        .collect()
}

/// The session: answers the attach's resync with two observations, takes
/// what the adapter sends until it quits, then ends.
fn fake_session(
    intents: &crate::surface_protocol::UserActionReceiver,
    observations: crate::surface_protocol::HostEventSender,
) -> Vec<SessionIntent<PlakExtension>> {
    let mut taken = Vec::new();
    loop {
        let (intent, _acknowledged) = intents.recv().unwrap().into_parts();
        if matches!(intent, SessionIntent::ResyncTranscript { .. }) {
            observations
                .send(SessionObservation::SetBusy(true))
                .unwrap();
            observations
                .send(SessionObservation::Extension(
                    SurfaceObservation::SettingsView(SettingsView::default()),
                ))
                .unwrap();
        }
        let quit = matches!(intent, SessionIntent::Quit);
        taken.push(intent);
        if quit {
            return taken;
        }
    }
}

/// The adapter answers each request once, typed, forwards intents to the
/// session with their images restored, and sends the session's
/// observations as classed, numbered events; a client that leaves quits
/// the session.
#[test]
fn the_adapter_attaches_the_root_and_carries_intents_and_events() {
    let (actions, intents) = user_action_channel();
    let (observations, events) = plakous_host_protocol::observation_channel();
    let (client, lines) = mpsc::sync_channel(64);
    let image = DraftImage {
        name: "a.png".into(),
        media_type: "image/png".into(),
        bytes: vec![7; 2_500_000],
        source_dimensions: None,
    };
    let prompt = SessionIntent::Prompt(Draft::new("look".into(), vec![image]));
    let mut sent = prompt.clone();
    let taken = take_images(&mut sent, "r3");

    let request = |id, op| ClientFrame::Request { id, op };
    let mut sent_frames = vec![
        request(
            1,
            Op::Attach {
                node: NodeAddress {
                    tree_id: "t1".into(),
                    agent_id: "child".into(),
                },
            },
        ),
        request(2, Op::Attach { node: root() }),
    ];
    sent_frames.extend(taken.chunks.into_iter().map(ClientFrame::Blob));
    sent_frames.push(request(
        3,
        Op::Intent {
            attachment: ROOT_ATTACHMENT,
            intent: sent,
            blobs: taken.ids,
        },
    ));
    sent_frames.push(request(
        4,
        Op::Intent {
            attachment: 9,
            intent: SessionIntent::Interrupt,
            blobs: Vec::new(),
        },
    ));
    sent_frames.push(ClientFrame::Bye);
    for frame in &sent_frames {
        client.send(Ok(line(frame))).unwrap();
    }
    let host = thread::spawn(move || fake_session(&intents, observations));
    let mut output = Vec::new();
    Adapter::new(root(), &mut output, actions)
        .serve(&lines, &events)
        .unwrap();
    let taken = host.join().unwrap();
    assert!(matches!(taken[0], SessionIntent::ResyncTranscript { .. }));
    assert_eq!(taken[1], prompt);
    assert!(matches!(taken[2], SessionIntent::Quit));

    let frames = frames(&output);
    let responses: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            HostFrame::Response { id, outcome } => Some((*id, outcome.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        responses,
        [
            (1, Outcome::Refused(RequestRefusal::UnknownNode)),
            (
                2,
                Outcome::Ok(Answer::Attached {
                    attachment: ROOT_ATTACHMENT
                })
            ),
            (3, Outcome::Ok(Answer::Accepted)),
            (4, Outcome::Refused(RequestRefusal::NotAttached)),
        ]
    );
    let events: Vec<_> = frames
        .iter()
        .filter_map(|frame| match frame {
            HostFrame::Event { seq, class, .. } => Some((*seq, *class)),
            _ => None,
        })
        .collect();
    assert_eq!(
        events,
        [
            (1, ObservationClass::State),
            (2, ObservationClass::Addressed)
        ]
    );
    assert!(matches!(frames.last(), Some(HostFrame::Bye)));
}

/// The handshake admits this version and refuses another, typed.
#[test]
fn the_handshake_refuses_another_version() {
    let welcome = || session_welcome("b".into(), 1, "s1".into(), root());
    for (proto, admitted) in [(WIRE_VERSION, true), (WIRE_VERSION + 1, false)] {
        let (client, lines) = mpsc::sync_channel(1);
        client
            .send(Ok(line(&ClientFrame::Hello(
                plakous_host_protocol::wire::Hello {
                    proto,
                    binary_id: "c".into(),
                    caps: Vec::new(),
                    locality: plakous_host_protocol::wire::ClientLocality::default(),
                },
            ))))
            .unwrap();
        let mut output = Vec::new();
        assert_eq!(
            handshake::<PlakExtension>(&lines, welcome(), &mut output, &|| false).unwrap(),
            admitted
        );
        let frame: Host =
            plakous_host_protocol::wire::decode_host_line(std::str::from_utf8(&output).unwrap())
                .unwrap();
        assert_eq!(matches!(frame, HostFrame::Welcome(_)), admitted);
    }
}

/// Plak's extension traffic travels: settings round-trip without the
/// client-local terminal preferences, and a surface intent keeps its kind.
#[test]
fn the_extension_codec_round_trips_and_keeps_terminal_preferences_local() {
    let mut view = SettingsView::default();
    view.tui.mouse = !view.tui.mouse;
    let observation: SessionObservation<PlakExtension> =
        SessionObservation::Extension(SurfaceObservation::SettingsView(view.clone()));
    let encoded = serde_json::to_value(&observation).unwrap();
    assert!(encoded["body"]["settings_view"].get("tui").is_none());
    let decoded: SessionObservation<PlakExtension> = serde_json::from_value(encoded).unwrap();
    let SessionObservation::Extension(SurfaceObservation::SettingsView(received)) = decoded else {
        panic!("a settings view");
    };
    assert_eq!(received.product, view.product);
    assert_eq!(
        received.tui,
        crate::tui_preferences::TuiPreferences::default()
    );

    let intent: SessionIntent<PlakExtension> =
        SessionIntent::Extension(SurfaceIntent::ListAgentAcknowledgements {
            tree_id: "t1".into(),
        });
    let encoded = serde_json::to_value(&intent).unwrap();
    assert_eq!(
        encoded,
        serde_json::json!({
            "kind": "extension",
            "body": { "list_agent_acknowledgements": { "tree_id": "t1" } }
        })
    );
    assert_eq!(
        serde_json::from_value::<SessionIntent<PlakExtension>>(encoded).unwrap(),
        intent
    );
}

/// `--wire v2` is a hidden selection of this face; JSONL stays the default.
#[test]
fn the_hidden_wire_flag_selects_v2() {
    for (args, v2) in [
        (vec!["agent"], false),
        (vec!["agent", "--wire", "v1"], false),
        (vec!["agent", "--wire", "v2"], true),
    ] {
        let command = crate::cli::parse(args.into_iter().map(Into::into)).unwrap();
        assert!(matches!(
            command,
            crate::command::Command::Agent { wire_v2, .. } if wire_v2 == v2
        ));
    }
    assert!(crate::cli::parse(["agent", "--wire", "v3"].map(Into::into)).is_err());
}

/// A child attachment's usage overlay reads its node's usage; Plak's other
/// extensions stay the root's.
#[test]
fn a_child_face_reads_its_nodes_usage() {
    struct Open;
    impl plakous_hosted::host_service::ChildNodes for Open {
        fn state(&self, _agent_id: &str) -> plakous_hosted::host_service::NodeState {
            plakous_hosted::host_service::NodeState::Open
        }

        fn interrupt(&self, _agent_id: &str, done: plakous_hosted::host_service::ControlDone) {
            done(Ok(()));
        }
    }
    let (actions, intents) = user_action_channel();
    let service = HostService::new(actions, root(), 16).with_nodes(std::sync::Arc::new(Open));
    let child = service
        .attach(&NodeAddress {
            tree_id: "t1".into(),
            agent_id: "c1".into(),
        })
        .unwrap();
    while intents.try_recv().is_ok() {}
    for usage in [SurfaceIntent::OpenUsage, SurfaceIntent::RereadUsage] {
        service
            .intent(child, SessionIntent::Extension(usage))
            .unwrap();
        assert_eq!(
            intents.try_recv().unwrap().into_parts().0,
            SessionIntent::ReadTopologyNodeUsage {
                agent_id: "c1".into()
            }
        );
    }
    assert_eq!(
        service.intent(child, SessionIntent::Extension(SurfaceIntent::OpenSettings)),
        Err(RequestRefusal::RefusedForRole {
            row: "templates".into()
        })
    );
}

/// A failure before dispatch is a v2 `refused` frame naming why, never the
/// JSONL `error` record.
#[test]
fn a_failure_before_dispatch_is_a_v2_refusal() {
    let mut output = Vec::new();
    refuse_unavailable(&mut output, &CliFailure::new("no login for claude")).unwrap();
    let text = String::from_utf8(output).unwrap();
    let frame: Host = plakous_host_protocol::wire::decode_host_line(text.trim_end()).unwrap();
    assert!(
        matches!(
            frame,
            HostFrame::Refused(HandshakeRefusal::Unavailable { ref detail })
                if detail == "no login for claude"
        ),
        "{text}"
    );
    assert!(!text.contains("\"v\":1"), "{text}");
}
