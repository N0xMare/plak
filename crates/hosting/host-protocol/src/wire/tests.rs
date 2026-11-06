use super::*;
use crate::intent::{Core, DraftImage};
use crate::views::transcript::{Lane, RowKey};

type Client = ClientFrame<Core>;
type Host = HostFrame<Core>;

fn hello(proto: u32) -> Hello {
    Hello {
        proto,
        binary_id: "b1".into(),
        caps: Vec::new(),
    }
}

fn welcome() -> Welcome {
    Welcome {
        proto: WIRE_VERSION,
        binary_id: "b1".into(),
        host_generation: 7,
        caps: Vec::new(),
        session: Some(WelcomeSession {
            session_id: "s1".into(),
            tree_id: "t1".into(),
            root: NodeAddress {
                tree_id: "t1".into(),
                agent_id: "t1".into(),
            },
        }),
    }
}

/// The handshake admits exactly this version and refuses any other, typed.
#[test]
fn the_handshake_admits_only_the_exact_version() {
    assert_eq!(admit_hello(&hello(WIRE_VERSION), welcome()), Ok(welcome()));
    assert_eq!(
        admit_hello(&hello(1), welcome()),
        Err(HandshakeRefusal::VersionMismatch {
            host: WIRE_VERSION,
            client: 1
        })
    );
    let line = encode_line(&Host::Refused(HandshakeRefusal::VersionMismatch {
        host: 2,
        client: 1,
    }))
    .unwrap();
    assert_eq!(
        line,
        "{\"frame\":\"refused\",\"kind\":\"version_mismatch\",\"host\":2,\"client\":1}\n"
    );
}

/// Frames round-trip through one line each.
#[test]
fn frames_round_trip_as_lines() {
    let frames = [
        Client::Hello(hello(WIRE_VERSION)),
        Client::Request {
            id: 1,
            op: Op::Attach {
                node: NodeAddress {
                    tree_id: "t1".into(),
                    agent_id: "c1".into(),
                },
            },
        },
        Client::Request {
            id: 2,
            op: Op::Intent {
                attachment: 3,
                intent: SessionIntent::Steer(Draft::from("go")),
                blobs: Vec::new(),
            },
        },
        Client::Request {
            id: 3,
            op: Op::Ping,
        },
        Client::Bye,
    ];
    for frame in frames {
        let line = encode_line(&frame).unwrap();
        assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
        assert_eq!(decode_line::<Client>(&line).unwrap(), frame);
    }
    let frames = [
        Host::Welcome(welcome()),
        Host::Response {
            id: 1,
            outcome: Outcome::Ok(Answer::Attached { attachment: 3 }),
        },
        Host::Response {
            id: 2,
            outcome: Outcome::Refused(RequestRefusal::RefusedForRole {
                row: "workflow".into(),
            }),
        },
        Host::Event {
            attachment: 3,
            seq: 1,
            class: ObservationClass::State,
            observation: SessionObservation::SetBusy(true),
            blobs: Vec::new(),
        },
        Host::ResyncRequired { attachment: 3 },
        Host::Bye,
    ];
    for frame in frames {
        let line = encode_line(&frame).unwrap();
        assert_eq!(decode_line::<Host>(&line).unwrap(), frame);
    }
    assert!(matches!(
        decode_line::<Client>("{\"frame\":\"hello\"}"),
        Err(WireError::Malformed(_))
    ));
}

/// The session requests, their answers and the `moved` they cause round
/// trip; a switch no request caused names none.
#[test]
fn session_requests_round_trip_as_lines() {
    for frame in [
        Client::Request {
            id: 4,
            op: Op::RetryAttach {
                attachment: 3,
                request_id: "r1".into(),
            },
        },
        Client::Request {
            id: 5,
            op: Op::ResumeSession {
                attachment: 3,
                request_id: "r2".into(),
                session: "s2".into(),
            },
        },
        Client::Request {
            id: 6,
            op: Op::Rebind {
                attachment: 3,
                request_id: "r3".into(),
                cause: "the binding changed".into(),
            },
        },
        Client::Request {
            id: 7,
            op: Op::StopSession {
                session: "s2".into(),
            },
        },
    ] {
        let line = encode_line(&frame).unwrap();
        assert_eq!(decode_line::<Client>(&line).unwrap(), frame);
    }
    for frame in [
        Host::Response {
            id: 5,
            outcome: Outcome::Ok(Answer::Moved {
                node: NodeAddress {
                    tree_id: "t2".into(),
                    agent_id: "t2".into(),
                },
            }),
        },
        Host::Response {
            id: 6,
            outcome: Outcome::Refused(RequestRefusal::SessionUnavailable {
                detail: "no launcher".into(),
            }),
        },
        Host::Response {
            id: 7,
            outcome: Outcome::Ok(Answer::Stopped),
        },
        Host::Response {
            id: 7,
            outcome: Outcome::Refused(RequestRefusal::Unsupported {
                detail: "not yet".into(),
            }),
        },
        Host::Moved {
            attachment: 3,
            node: NodeAddress {
                tree_id: "t2".into(),
                agent_id: "t2".into(),
            },
            request_id: Some("r2".into()),
        },
    ] {
        let line = encode_line(&frame).unwrap();
        assert_eq!(decode_line::<Host>(&line).unwrap(), frame);
    }
    // A switch no request caused names none.
    let line = encode_line(&Host::Moved {
        attachment: 3,
        node: NodeAddress {
            tree_id: "t2".into(),
            agent_id: "t2".into(),
        },
        request_id: None,
    })
    .unwrap();
    assert!(!line.contains("request_id"), "{line}");
}

fn row() -> SessionRow {
    SessionRow {
        session: "s1".into(),
        node: NodeAddress {
            tree_id: "s1".into(),
            agent_id: "s1".into(),
        },
        title: None,
        workspace: "/w".into(),
        profile: None,
        updated_at: 9,
        residency: SessionResidency::Dormant,
        activity: SessionActivity::Idle,
        live_work: LiveWork::default(),
        environment: SessionEnvironment::Host,
        hosted_elsewhere: None,
    }
}

/// The session directory's requests round trip, and their bytes are
/// pinned: a client reads them by these names, and an absent optional
/// field is absent, not null.
#[test]
fn the_directory_requests_round_trip_with_pinned_bytes() {
    let client = [
        (
            Client::Request {
                id: 1,
                op: Op::ListSessions { workspace: None },
            },
            r#"{"frame":"request","id":1,"op":{"op":"list_sessions"}}"#,
        ),
        (
            Client::Request {
                id: 2,
                op: Op::WatchSessions,
            },
            r#"{"frame":"request","id":2,"op":{"op":"watch_sessions"}}"#,
        ),
        (
            Client::Request {
                id: 3,
                op: Op::UnwatchSessions { attachment: 4 },
            },
            r#"{"frame":"request","id":3,"op":{"op":"unwatch_sessions","attachment":4}}"#,
        ),
        (
            Client::Request {
                id: 4,
                op: Op::NewSession {
                    workspace: "/w".into(),
                    template: Some("coder".into()),
                    request_id: "00000000-0000-4000-8000-000000000001".into(),
                },
            },
            r#"{"frame":"request","id":4,"op":{"op":"new_session","workspace":"/w","template":"coder","request_id":"00000000-0000-4000-8000-000000000001"}}"#,
        ),
        (
            Client::Request {
                id: 5,
                op: Op::ReleaseSession {
                    session: "s1".into(),
                },
            },
            r#"{"frame":"request","id":5,"op":{"op":"release_session","session":"s1"}}"#,
        ),
    ];
    for (frame, pinned) in client {
        let line = encode_line(&frame).unwrap();
        assert_eq!(line.trim_end(), pinned);
        assert_eq!(decode_line::<Client>(&line).unwrap(), frame);
    }
}

fn held_elsewhere() -> SessionRow {
    SessionRow {
        title: Some("fix the build".into()),
        profile: Some("coder".into()),
        residency: SessionResidency::Resident,
        activity: SessionActivity::Busy,
        live_work: LiveWork {
            children: 1,
            processes: 2,
            held_children: 0,
        },
        environment: SessionEnvironment::Cli,
        hosted_elsewhere: Some(HostedElsewhere {
            host_name: "desk".into(),
            binary: None,
            pid: 42,
        }),
        ..row()
    }
}

/// The directory's answers, refusals and host frames round trip with pinned
/// bytes, as its requests do.
#[test]
fn the_directory_answers_and_frames_round_trip_with_pinned_bytes() {
    let host = [
        (
            Host::Response {
                id: 1,
                outcome: Outcome::Ok(Answer::Sessions { rows: vec![row()] }),
            },
            r#"{"frame":"response","id":1,"outcome":{"ok":{"answer":"sessions","rows":[{"session":"s1","node":{"tree_id":"s1","agent_id":"s1"},"workspace":"/w","updated_at":9,"residency":"dormant","activity":"idle","live_work":{"children":0,"processes":0,"held_children":0},"environment":"host"}]}}}"#,
        ),
        (
            Host::Response {
                id: 2,
                outcome: Outcome::Ok(Answer::Watching { attachment: 4 }),
            },
            r#"{"frame":"response","id":2,"outcome":{"ok":{"answer":"watching","attachment":4}}}"#,
        ),
        (
            Host::Response {
                id: 4,
                outcome: Outcome::Ok(Answer::Created {
                    session: "s1".into(),
                    node: row().node,
                    environment: SessionEnvironment::Host,
                }),
            },
            r#"{"frame":"response","id":4,"outcome":{"ok":{"answer":"created","session":"s1","node":{"tree_id":"s1","agent_id":"s1"},"environment":"host"}}}"#,
        ),
        (
            Host::Response {
                id: 5,
                outcome: Outcome::Ok(Answer::Released),
            },
            r#"{"frame":"response","id":5,"outcome":{"ok":{"answer":"released"}}}"#,
        ),
        (
            Host::Response {
                id: 5,
                outcome: Outcome::Ok(Answer::Resident {
                    reason: ResidentReason::Processes,
                }),
            },
            r#"{"frame":"response","id":5,"outcome":{"ok":{"answer":"resident","reason":"processes"}}}"#,
        ),
        (
            Host::Response {
                id: 6,
                outcome: Outcome::Refused(RequestRefusal::TreeHostedElsewhere {
                    host_name: "desk".into(),
                    binary: Some("0123456789ab".into()),
                    pid: 42,
                }),
            },
            r#"{"frame":"response","id":6,"outcome":{"refused":{"refusal":"tree_hosted_elsewhere","host_name":"desk","binary":"0123456789ab","pid":42}}}"#,
        ),
        (
            Host::Response {
                id: 7,
                outcome: Outcome::Refused(RequestRefusal::UnknownSession),
            },
            r#"{"frame":"response","id":7,"outcome":{"refused":{"refusal":"unknown_session"}}}"#,
        ),
        (
            Host::Directory {
                attachment: 4,
                seq: 2,
                snapshot: false,
                rows: vec![held_elsewhere()],
            },
            r#"{"frame":"directory","attachment":4,"seq":2,"snapshot":false,"rows":[{"session":"s1","node":{"tree_id":"s1","agent_id":"s1"},"title":"fix the build","workspace":"/w","profile":"coder","updated_at":9,"residency":"resident","activity":"busy","live_work":{"children":1,"processes":2,"held_children":0},"environment":"cli","hosted_elsewhere":{"host_name":"desk","pid":42}}]}"#,
        ),
        (
            Host::Detached { attachment: 3 },
            r#"{"frame":"detached","attachment":3}"#,
        ),
        (
            Host::Welcome(Welcome {
                session: None,
                ..welcome()
            }),
            r#"{"frame":"welcome","proto":4,"binary_id":"b1","host_generation":7,"caps":[]}"#,
        ),
    ];
    for (frame, pinned) in host {
        let line = encode_line(&frame).unwrap();
        assert_eq!(line.trim_end(), pinned);
        assert_eq!(decode_host_line::<Core>(&line).unwrap(), frame);
    }
}

/// A line over the cap is refused when written and when read, and the
/// reader skips it whole without holding it.
/// Every host frame a client decodes goes through serde's buffered tagged
/// content, which has no 128-bit integers: the context meter's numbers
/// stay 64-bit so a usage event decodes.
#[test]
fn a_usage_event_decodes() {
    use crate::views::presentation::ContextUsage;
    let observation = SessionObservation::<Core>::Usage(ContextUsage {
        breakdown: None,
        prepared_input: None,
        total_window: Some(272_000),
        admitted_ceiling: None,
        gauge_percent: Some(42),
        display_percent: Some(42),
        ceiling_excess: None,
        evidence: "measured".into(),
        non_guarantee: None,
        status: "fits".into(),
    });
    let frame = Host::Event {
        attachment: 1,
        seq: 1,
        class: observation.class(),
        observation,
        blobs: Vec::new(),
    };
    let line = encode_line(&frame).unwrap();
    assert_eq!(decode_host_line::<Core>(&line).unwrap(), frame);
}

#[test]
fn a_line_over_the_cap_is_refused_and_skipped() {
    let huge = Client::Request {
        id: 1,
        op: Op::Intent {
            attachment: 1,
            intent: SessionIntent::Prompt(Draft::from("x".repeat(MAX_LINE_BYTES))),
            blobs: Vec::new(),
        },
    };
    assert_eq!(encode_line(&huge), Err(WireError::TooLarge));
    let mut reader = LineReader::default();
    let mut lines = reader.push(b"{\"frame\":\"bye\"}\n{\"fra");
    lines.extend(reader.push(b"me\":\"bye\"}\n"));
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(Result::is_ok));
    let mut stream = vec![b'x'; MAX_LINE_BYTES + 10];
    stream.extend_from_slice(b"\n{\"frame\":\"bye\"}\n");
    let lines = reader.push(&stream);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], Err(WireError::TooLarge));
    assert_eq!(
        decode_line::<Client>(lines[1].as_ref().unwrap()).unwrap(),
        Client::Bye
    );
}

fn image(len: usize) -> DraftImage {
    DraftImage {
        name: "a.png".into(),
        media_type: "image/png".into(),
        bytes: (0..len)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect(),
        source_dimensions: None,
    }
}

/// Image bytes leave the frame as blob chunks and come back exact; a chunk
/// out of order or a missing blob is refused.
#[test]
fn images_travel_as_blobs() {
    let draft = Draft::new(
        "look".into(),
        vec![image(BLOB_CHUNK_BYTES * 2 + 5), image(0)],
    );
    let original = SessionIntent::<Core>::Prompt(draft);
    let mut sent = original.clone();
    let taken = take_images(&mut sent, "r1");
    assert_eq!(taken.ids, ["r1.0", "r1.1"]);
    assert_eq!(taken.chunks.len(), 4);
    for chunk in &taken.chunks {
        assert!(encode_line(&Client::Blob(chunk.clone())).is_ok());
    }
    let mut assembler = BlobAssembler::default();
    for chunk in taken.chunks.clone() {
        assembler.push(chunk).unwrap();
    }
    assembler.restore_images(&mut sent, &taken.ids).unwrap();
    assert_eq!(sent, original);

    let mut assembler = BlobAssembler::default();
    assert!(assembler.push(taken.chunks[1].clone()).is_err());
    let mut again = original.clone();
    let taken = take_images(&mut again, "r2");
    assert!(
        BlobAssembler::default()
            .restore_images(&mut again, &taken.ids)
            .is_err()
    );
}

fn snapshot(rows: usize, text: usize) -> PresentationSnapshot {
    let mut snapshot = PresentationSnapshot::empty(9);
    snapshot.items = (0..rows)
        .map(|index| TranscriptItem {
            key: RowKey::new(u64::try_from(index).unwrap() + 1, Lane::Runtime, 0),
            row: crate::views::transcript::TranscriptRow::User {
                text: "x".repeat(text),
                sent_at_ms: None,
            },
        })
        .collect();
    snapshot
}

/// A snapshot too large for one line travels as ordered pages of one
/// revision and is reassembled and validated; a page out of order is
/// refused.
#[test]
fn a_large_snapshot_travels_as_pages() {
    let whole = snapshot(5, 3 * 1024 * 1024);
    whole.validate().unwrap();
    let pages = page_snapshot(whole.clone(), 4).unwrap();
    assert!(pages.len() > 1);
    let mut assembler = SnapshotAssembler::default();
    let mut rebuilt = None;
    for page in pages.clone() {
        assert!(encode_line(&Host::Snapshot(page.clone())).is_ok());
        rebuilt = assembler.push(page).unwrap();
    }
    assert_eq!(rebuilt, Some(whole));
    let mut assembler = SnapshotAssembler::default();
    assert!(assembler.push(pages[1].clone()).is_err());
    let small = page_snapshot(snapshot(2, 10), 4).unwrap();
    assert_eq!(small.len(), 1);
    assert!(small[0].last);
}

fn chunk(id: &str, len: usize) -> BlobChunk {
    use base64::Engine as _;
    BlobChunk {
        id: id.into(),
        offset: 0,
        data: base64::engine::general_purpose::STANDARD.encode(vec![1_u8; len]),
        last: true,
    }
}

/// A connection holds a bounded number and size of blobs, and the next
/// frame drops whatever it does not name.
#[test]
fn blobs_held_are_bounded_and_dropped_when_unnamed() {
    let mut assembler = BlobAssembler::default();
    for index in 0..MAX_PENDING_BLOBS {
        assembler.push(chunk(&format!("b{index}"), 1)).unwrap();
    }
    assert_eq!(
        assembler.push(chunk("one-more", 1)),
        Err(WireError::BlobLimit)
    );

    let mut assembler = BlobAssembler::default();
    assembler.push(chunk("b0", 16)).unwrap();
    let mut big = BlobAssembler::default();
    let mut refused = None;
    for index in 0..MAX_PENDING_BLOBS {
        let id = format!("big{index}");
        for part in 0..5_u64 {
            let mut piece = chunk(&id, BLOB_CHUNK_BYTES);
            piece.offset = part * u64::try_from(BLOB_CHUNK_BYTES).unwrap();
            piece.last = part == 4;
            if let Err(error) = big.push(piece) {
                refused = Some(error);
                break;
            }
        }
        if refused.is_some() {
            break;
        }
    }
    assert_eq!(refused, Some(WireError::BlobLimit));

    // A frame that names no blob drops them: they cannot be named later.
    let mut intent = SessionIntent::<Core>::Interrupt;
    assembler.restore_images(&mut intent, &[]).unwrap();
    let mut prompt = SessionIntent::<Core>::Prompt(Draft::new("x".into(), vec![image(1)]));
    assert!(
        assembler
            .restore_images(&mut prompt, &["b0".into()])
            .is_err()
    );
}

/// A snapshot over its page bound is refused.
#[test]
fn a_snapshot_over_its_page_bound_is_refused() {
    let mut assembler = SnapshotAssembler::default();
    let mut refused = None;
    for page in 0..=MAX_SNAPSHOT_PAGES {
        let result = assembler.push(SnapshotPage {
            attachment: 1,
            schema: crate::views::presentation::PRESENTATION_SCHEMA_V3,
            scope: TranscriptScope::Root,
            revision: 1,
            page,
            items: Vec::new(),
            last: false,
        });
        if let Err(error) = result {
            refused = Some((page, error));
            break;
        }
    }
    assert_eq!(refused, Some((MAX_SNAPSHOT_PAGES, WireError::TooLarge)));
}

/// The receiver derives an event's class: a class the observation does not
/// have is refused, and a frame refuses fields it does not know.
#[test]
fn a_received_frame_is_checked() {
    let event = Host::Event {
        attachment: 1,
        seq: 1,
        class: ObservationClass::Addressed,
        observation: SessionObservation::SetBusy(true),
        blobs: Vec::new(),
    };
    let line = encode_line(&event).unwrap();
    assert!(decode_line::<Host>(&line).is_ok());
    assert!(matches!(
        decode_host_line::<Core>(&line),
        Err(WireError::Malformed(_))
    ));
    assert!(
        decode_line::<Client>(r#"{"frame":"request","id":1,"op":{"op":"ping"},"extra":1}"#)
            .is_err()
    );
    assert!(
        decode_line::<Host>(r#"{"frame":"resync_required","attachment":1,"extra":true}"#).is_err()
    );
}
