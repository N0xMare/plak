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
    let (client, lines) = wire::lines(64);
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
            client_id: None,
            attachment: ROOT_ATTACHMENT,
            intent: sent,
            blobs: taken.ids,
        },
    ));
    sent_frames.push(request(
        4,
        Op::Intent {
            client_id: None,
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
            (3, Outcome::Ok(Answer::Accepted { duplicate: false })),
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
        let (client, lines) = wire::lines(1);
        client
            .send(Ok(line(&ClientFrame::Hello(
                plakous_host_protocol::wire::Hello {
                    proto,
                    binary_id: "c".into(),
                    caps: Vec::new(),
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
        .attach(
            &NodeAddress {
                tree_id: "t1".into(),
                agent_id: "c1".into(),
            },
            Face::Remote,
        )
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

/// A remote face is refused the intents only a local face may send (the
/// installed-build check and probe, which exec the installed build and
/// answer with its path, and the reload-request handover): none reaches the
/// session. stdio's local face sends them. A connection whose acceptor never
/// names its face is a remote one.
#[test]
fn only_a_local_face_may_probe_or_check_the_installed_build() {
    use plakous_hosted::host_service::wire::Face;
    for face in [None, Some(Face::Remote), Some(Face::Local)] {
        let (actions, intents) = user_action_channel();
        let (observations, events) = plakous_host_protocol::observation_channel();
        let (client, lines) = wire::lines(8);
        let intent = |id, intent| ClientFrame::Request {
            id,
            op: Op::Intent {
                client_id: None,
                attachment: ROOT_ATTACHMENT,
                intent: SessionIntent::Extension(intent),
                blobs: Vec::new(),
            },
        };
        for frame in [
            ClientFrame::Request {
                id: 1,
                op: Op::Attach { node: root() },
            },
            intent(2, SurfaceIntent::ProbeReload),
            intent(3, SurfaceIntent::CheckBuild),
            ClientFrame::Bye,
        ] {
            client.send(Ok(line(&frame))).unwrap();
        }
        let host = thread::spawn(move || fake_session(&intents, observations));
        let mut output = Vec::new();
        let adapter = Adapter::new(root(), &mut output, actions);
        match face {
            Some(face) => adapter.face(face),
            None => adapter,
        }
        .serve(&lines, &events)
        .unwrap();
        let taken = host.join().unwrap();
        let reached = taken
            .iter()
            .filter(|intent| matches!(intent, SessionIntent::Extension(_)))
            .count();
        let refused = frames(&output)
            .iter()
            .filter(|frame| {
                matches!(frame, HostFrame::Response {
                    outcome: Outcome::Refused(RequestRefusal::RefusedForRole { row }),
                    ..
                } if row == "local_face")
            })
            .count();
        match face {
            None | Some(Face::Remote) => {
                assert_eq!((reached, refused), (0, 2), "{face:?}: {taken:?}");
            }
            Some(Face::Local) => assert_eq!((reached, refused), (2, 0), "{taken:?}"),
        }
    }
}

/// A connection's frames, readable while the connection writes them.
#[derive(Clone, Default)]
struct SharedOutput(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for SharedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedOutput {
    fn frames(&self) -> Vec<Host> {
        frames(&self.0.lock().unwrap())
    }
}

fn binding(session: &str) -> SessionObservation<PlakExtension> {
    SessionObservation::Binding {
        session_id: Some(session.to_owned()),
        account: None,
        model: None,
        effort: None,
        extra: None,
        composition: plakous_host_protocol::CompositionSelection::FollowCatalog,
        protocol: plakous_host_protocol::ProtocolSelection::Auto,
        effective_presentation: "direct".to_owned(),
    }
}

fn marker(text: &str) -> SessionObservation<PlakExtension> {
    use plakous_host_protocol::views::transcript::{NoticeScope, TranscriptRow};
    SessionObservation::Row(TranscriptRow::info(NoticeScope::Host, text))
}

/// Publishes `observations` and a marker row, then flushes each connection
/// until its output shows the marker.
fn publish_until_marked(
    published: &crate::surface_protocol::HostEventSender,
    observations: Vec<SessionObservation<PlakExtension>>,
    mark: &str,
    connections: [(&mut Adapter<SharedOutput>, &SharedOutput); 2],
) {
    for observation in observations {
        published.send(observation).unwrap();
    }
    published.send(marker(mark)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    for (connection, out) in connections {
        loop {
            connection.flush().unwrap();
            let seen = out.frames().iter().any(|frame| {
                matches!(frame, HostFrame::Event { observation, .. }
                    if *observation == marker(mark))
            });
            if seen {
                break;
            }
            assert!(Instant::now() < deadline, "{mark}: {:?}", out.frames());
            thread::sleep(Duration::from_millis(2));
        }
    }
}

/// How many of `frames` are events of a local-only observation.
fn local_only_events(frames: &[Host]) -> usize {
    frames
        .iter()
        .filter(|frame| {
            matches!(frame, HostFrame::Event { observation, .. } if observation.local_only())
        })
        .count()
}

/// Asserts each attachment's events in `frames` are numbered from one with
/// no gap; returns how many attachments had events.
fn numbered_without_gaps(frames: &[Host]) -> usize {
    let mut numbered = std::collections::BTreeMap::<AttachmentId, Vec<u64>>::new();
    for frame in frames {
        if let HostFrame::Event {
            attachment, seq, ..
        } = frame
        {
            numbered.entry(*attachment).or_default().push(*seq);
        }
    }
    for seqs in numbered.values() {
        let expected: Vec<u64> = (1..=u64::try_from(seqs.len()).unwrap()).collect();
        assert_eq!(*seqs, expected, "{frames:?}");
    }
    numbered.len()
}

/// A remote face's attachment never receives an observation only a local
/// face may: not the launch's lookup progress (a state, kept for later
/// attaches), not a build status that answers no attachment (an addressed
/// answer that falls back to every root attachment), neither before a
/// session switch, nor in the next attachment's snapshot or replay, nor
/// after. Its events stay numbered without a gap. A local face on the same
/// session receives each of them.
#[test]
fn a_remote_face_never_receives_a_local_only_observation() {
    use plakous_hosted::host_service::wire::WireSession;
    let root_of = |id: &str| NodeAddress {
        tree_id: format!("tree-{id}"),
        agent_id: format!("tree-{id}"),
    };
    let (actions, _intents) = user_action_channel();
    let (published, events) = plakous_host_protocol::observation_channel();
    let session = WireSession::start(root_of("s1"), "s1".to_owned(), actions, events, move |id| {
        Some(root_of(id))
    });
    let (local_out, remote_out) = (SharedOutput::default(), SharedOutput::default());
    let mut local = session.connect(local_out.clone(), Face::Local, || false);
    let mut remote = session.connect(remote_out.clone(), Face::Remote, || false);
    let attach = |id, tree: &str| {
        Ok(line(&ClientFrame::Request {
            id,
            op: Op::Attach {
                node: root_of(tree),
            },
        }))
    };
    let lookup = |text: &str| {
        SessionObservation::Extension(SurfaceObservation::LookupStatus {
            text: Some(text.to_owned()),
        })
    };
    let build = SessionObservation::Extension(SurfaceObservation::BuildStatus {
        newer_installed: true,
    });
    assert!(local.frame(attach(1, "s1")).unwrap());
    assert!(remote.frame(attach(1, "s1")).unwrap());
    publish_until_marked(
        &published,
        vec![binding("s1"), lookup("before"), build.clone()],
        "first",
        [(&mut local, &local_out), (&mut remote, &remote_out)],
    );
    // The switch: its opening carries both kinds again, then the next
    // session's binding moves every attachment.
    for observation in [
        SessionObservation::SessionOpening,
        lookup("opening"),
        build.clone(),
        binding("s2"),
    ] {
        published.send(observation).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.session_id() != "s2" {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    local.flush().unwrap();
    remote.flush().unwrap();
    assert!(remote.frame(attach(2, "s2")).unwrap());
    assert!(local.frame(attach(2, "s2")).unwrap());
    publish_until_marked(
        &published,
        vec![lookup("after"), build],
        "second",
        [(&mut local, &local_out), (&mut remote, &remote_out)],
    );

    let remote_frames = remote_out.frames();
    assert_eq!(local_only_events(&remote_frames), 0, "{remote_frames:?}");
    assert_eq!(
        numbered_without_gaps(&remote_frames),
        2,
        "{remote_frames:?}"
    );
    let local_frames = local_out.frames();
    for text in ["before", "after"] {
        assert!(
            local_frames
                .iter()
                .any(|frame| matches!(frame, HostFrame::Event { observation, .. }
                if *observation == lookup(text))),
            "{text}: {local_frames:?}"
        );
    }
    assert!(local_only_events(&local_frames) >= 4, "{local_frames:?}");
}
