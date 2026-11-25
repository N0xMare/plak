use super::*;
use plakous_host_protocol::SessionObservation;
use plakous_host_protocol::views::transcript::{TranscriptRow, TranscriptScope, TranscriptState};
use plakous_host_protocol::wire::{HostFrame, SnapshotAssembler};

const PREFIX: &str = "first line\n";
const MIDDLE: &str = "second line\n";
const TAIL: &str = "third line\n";

fn assistant_rows(items: &Value) -> impl Iterator<Item = &str> {
    items
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| &item["row"])
        .filter(|row| row["kind"] == "assistant")
        .filter_map(|row| row["text"].as_str())
}

fn stream_fixture() -> Fixture {
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    fixture.write_script(&json!({
        "version": 1,
        "steps": [
            {
                "stream": [{"reasoning": "considering the tool"}],
                "output": {"tool": {"name": "loopback_effect", "arguments": {}}},
                "finish": "tool_calls"
            },
            {
                "stream": [{"text": PREFIX}, {"gate": {"file": fixture.root.join("repair")}},
                           {"text": MIDDLE}, {"gate": {"file": fixture.root.join("finish")}},
                           {"reasoning": "considering the answer"},
                           {"text": TAIL}],
                "output": {"text": format!("{PREFIX}{MIDDLE}{TAIL}")}
            }
        ]
    }));
    fixture
}

fn start_stream(socket: &mut WebSocket<UnixStream>, attachment: u64) -> Vec<String> {
    let mut trace = Vec::new();
    loop {
        let frame = next_frame(socket);
        trace.push(trace_frame(&frame));
        if frame["frame"] == "snapshot" && frame["last"] == true {
            break;
        }
    }
    send(
        socket,
        &ClientFrame::Request {
            id: 2,
            op: Op::Intent {
                attachment,
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                intent: SessionIntent::Prompt(Draft::from("run the stream fixture")),
                blobs: Vec::new(),
            },
        },
    );
    trace
}

fn trace_frame(frame: &Value) -> String {
    let observation = &frame["observation"];
    let body = &observation["body"];
    let event = observation["kind"]
        .as_str()
        .or_else(|| frame["frame"].as_str())
        .unwrap();
    let rows = match event {
        "snapshot" => &frame["items"],
        "transcript_patch" => &body["upserts"],
        _ => &Value::Null,
    };
    let rows: Vec<_> = rows
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| {
            let row = &item["row"];
            format!(
                "{}:{}:{}B",
                row["kind"],
                item["key"],
                row["text"].as_str().map_or(0, str::len)
            )
        })
        .collect();
    let bytes = match event {
        "transcript_stream" => body["text"].as_str(),
        _ => None,
    }
    .map_or(0, str::len);
    let presentation = match event {
        "snapshot" => format!(
            " rev={} page={} last={}",
            frame["revision"]["revision"], frame["part"], frame["last"]
        ),
        "transcript_patch" => format!(
            " rev={}->{} deletes={}",
            body["from_revision"], body["to_revision"], body["deletes"]
        ),
        _ => String::new(),
    };
    format!(
        "seq={} {event}{presentation} delta_bytes={bytes} rows=[{}]",
        frame["seq"],
        rows.join(", ")
    )
}

#[test]
fn root_repair_preserves_a_streaming_prefix_on_the_web() {
    let fixture = stream_fixture();
    let repair = fixture.root.join("repair");
    let finish = fixture.root.join("finish");
    let complete = format!("{PREFIX}{MIDDLE}{TAIL}");
    let mut web = start(&fixture);
    let mut socket = upgrade(&web, &cookie(&web));
    let root = hello(&mut socket);
    let mut attachment = attach(&mut socket, &root, 1);
    let mut trace = start_stream(&mut socket, attachment);
    let mut repairing = false;
    let mut repaired_prefix = false;
    let mut settled_text = false;
    loop {
        let frame = next_frame(&mut socket);
        let observation = &frame["observation"];
        let body = &observation["body"];
        let kind = frame["frame"].as_str().unwrap();
        let event = observation["kind"].as_str().unwrap_or(kind);
        trace.push(trace_frame(&frame));
        if kind == "resync_required" {
            assert!(repairing, "unexpected repair boundary: {trace:?}");
            assert_eq!(frame["attachment"], attachment);
            attachment = attach(&mut socket, &root, 4);
            continue;
        }
        if event == "transcript_patch" && body["scope"] == "root" {
            settled_text |= assistant_rows(&body["upserts"]).any(|text| text == complete);
        }
        if repairing && event == "snapshot" && frame["scope"] == "root" {
            repaired_prefix |= assistant_rows(&frame["items"]).any(|text| text == PREFIX);
            if frame["last"] == true {
                std::fs::write(&repair, []).unwrap();
                repairing = false;
            }
        }
        if event == "transcript_stream" && body["text"] == PREFIX {
            repairing = true;
            send(
                &mut socket,
                &ClientFrame::Request {
                    id: 3,
                    op: Op::Intent {
                        attachment,
                        client_id: None,
                        intent: SessionIntent::ResyncTranscript {
                            scope: TranscriptScope::Root,
                        },
                        blobs: Vec::new(),
                    },
                },
            );
        }
        if event == "transcript_stream" && body["text"] == MIDDLE {
            std::fs::write(&finish, []).unwrap();
        }
        if event == "set_busy" && body == false && finish.exists() {
            break;
        }
    }
    drop(socket);
    interrupt_and_wait(&mut web);
    assert!(settled_text, "{}", trace.join("\n"));
    assert!(repaired_prefix, "{}", trace.join("\n"));
}

#[test]
fn coherent_root_repair_keeps_the_token_on_the_web() {
    const ANSWER: &str = "one settled response";
    let fixture = Fixture::new(&json!({
        "version":1,"steps":[{"output":{"text":ANSWER}}]
    }));
    let mut web = start(&fixture);
    let mut socket = upgrade(&web, &cookie(&web));
    let root = hello(&mut socket);
    let attachment = attach(&mut socket, &root, 1);
    let mut view = KeyedRoot::default();
    let revision = loop {
        let frame = view.read(&mut socket);
        if frame["frame"] == "snapshot" && frame["last"] == true {
            break frame["revision"].clone();
        }
    };
    send(
        &mut socket,
        &ClientFrame::Request {
            id: 2,
            op: Op::Intent {
                attachment,
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                intent: SessionIntent::Prompt(Draft::from("write a reply")),
                blobs: Vec::new(),
            },
        },
    );
    loop {
        let frame = view.read(&mut socket);
        if frame["observation"]["kind"] == "set_busy"
            && frame["observation"]["body"] == false
            && assistant_text(view.transcript.items().values()) == ANSWER
        {
            break;
        }
    }
    send(
        &mut socket,
        &ClientFrame::Request {
            id: 3,
            op: Op::Intent {
                attachment,
                client_id: None,
                intent: SessionIntent::ResyncTranscript {
                    scope: TranscriptScope::Root,
                },
                blobs: Vec::new(),
            },
        },
    );
    loop {
        let frame = view.read(&mut socket);
        if frame["frame"] == "snapshot" && frame["last"] == true {
            assert_eq!(frame["attachment"], attachment);
            assert_eq!(
                frame["revision"]["host_generation"],
                revision["host_generation"]
            );
            assert_eq!(frame["revision"]["token"], revision["token"]);
            assert_eq!(assistant_text(view.transcript.items().values()), ANSWER);
            break;
        }
    }
    drop(socket);
    interrupt_and_wait(&mut web);
}

fn trace_through_settlement(socket: &mut WebSocket<UnixStream>, trace: &mut Vec<String>) {
    let complete = format!("{PREFIX}{MIDDLE}{TAIL}");
    loop {
        let frame = next_frame(socket);
        trace.push(trace_frame(&frame));
        let event = &frame["observation"];
        if event["kind"] == "transcript_patch"
            && assistant_rows(&event["body"]["upserts"]).any(|text| text == complete)
        {
            break;
        }
    }
}

#[derive(Default)]
struct KeyedRoot {
    pages: SnapshotAssembler,
    transcript: TranscriptState,
    trace: Vec<String>,
}

impl KeyedRoot {
    fn read(&mut self, socket: &mut WebSocket<UnixStream>) -> Value {
        let frame = next_frame(socket);
        self.trace.push(trace_frame(&frame));
        assert_ne!(
            frame["frame"],
            "resync_required",
            "{}",
            self.trace.join("\n")
        );
        if frame["frame"] == "snapshot" || frame["observation"]["kind"] == "transcript_patch" {
            match crate::protocol_13::decode(&frame.to_string()).unwrap() {
                Some(HostFrame::Snapshot(page)) => {
                    if let Some(snapshot) = self.pages.push(page).unwrap() {
                        self.transcript.replace(snapshot).unwrap();
                    }
                }
                Some(HostFrame::Event {
                    observation: SessionObservation::TranscriptPatch(patch),
                    ..
                }) => self.transcript.apply(patch).unwrap(),
                _ => unreachable!("only keyed frames are decoded"),
            }
        }
        frame
    }

    fn assert_durable(&self, fixture: &Fixture, session: &str) {
        let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
        let stored =
            plakd::store::ProductStore::inspect_session_read_only(&paths, session).unwrap();
        let items = stored.transcript.items();
        let durable = assistant_text(items.iter().map(|item| &item.row));
        assert_eq!(durable, format!("{PREFIX}{MIDDLE}{TAIL}"));
        assert_eq!(
            assistant_text(self.transcript.items().values()),
            durable,
            "{}",
            self.trace.join("\n")
        );
    }
}

fn assistant_text<'a>(rows: impl Iterator<Item = &'a TranscriptRow>) -> String {
    rows.filter_map(|row| match row {
        TranscriptRow::Assistant { text } => Some(text.as_str()),
        _ => None,
    })
    .collect()
}

#[test]
fn late_root_attach_and_retained_replay_get_the_prefix_before_future_web_deltas() {
    let fixture = stream_fixture();
    let mut web = start(&fixture);
    let cookie = cookie(&web);
    let mut early = upgrade(&web, &cookie);
    let root = hello(&mut early);
    let attachment = attach(&mut early, &root, 1);
    let mut early_trace = start_stream(&mut early, attachment);
    loop {
        let frame = next_frame(&mut early);
        early_trace.push(trace_frame(&frame));
        if frame["observation"]["kind"] == "transcript_stream"
            && frame["observation"]["body"]["text"] == PREFIX
        {
            break;
        }
    }

    let mut late = upgrade(&web, &cookie);
    send(
        &mut late,
        &ClientFrame::Hello(Hello {
            proto: WIRE_VERSION,
            binary_id: "late-web-viewer".into(),
            caps: Vec::new(),
        }),
    );
    assert_eq!(next_frame(&mut late)["frame"], "welcome");
    let late_attachment = attach(&mut late, &root, 1);
    let mut late_view = KeyedRoot::default();
    let mut released = false;
    loop {
        let frame = late_view.read(&mut late);
        if frame["observation"]["kind"] == "transcript_stream"
            && frame["observation"]["body"]["text"] == MIDDLE
        {
            assert_eq!(
                assistant_text(late_view.transcript.items().values()),
                PREFIX,
                "the first keyed append arrived before its prefix\nearly:\n{}\nlate:\n{}",
                early_trace.join("\n"),
                late_view.trace.join("\n")
            );
            break;
        }
        if !released && frame["frame"] == "snapshot" && frame["last"] == true {
            let retained = frame["revision"].clone();
            let hit = attach_retained(&mut late, &root, 4, &retained);
            assert_eq!(hit["answer"], "attached");
            assert_ne!(hit["attachment"], late_attachment);
            assert_eq!(hit["transcript"]["mode"], "replay");
            assert_eq!(hit["transcript"]["from"], retained);
            let mut wrong = retained.clone();
            wrong["token"] = json!(uuid::Uuid::new_v4().to_string());
            let miss = attach_retained(&mut late, &root, 5, &wrong);
            assert_eq!(miss["answer"], "attached");
            assert_eq!(miss["transcript"]["mode"], "first_page");
            loop {
                let replacement = late_view.read(&mut late);
                if replacement["frame"] == "snapshot" && replacement["last"] == true {
                    assert_eq!(replacement["attachment"], miss["attachment"]);
                    assert_ne!(replacement["revision"]["token"], wrong["token"]);
                    assert_ne!(replacement["revision"]["token"], retained["token"]);
                    break;
                }
            }
            std::fs::write(fixture.root.join("repair"), []).unwrap();
            released = true;
        }
    }
    std::fs::write(fixture.root.join("finish"), []).unwrap();
    loop {
        let frame = late_view.read(&mut late);
        if frame["observation"]["kind"] == "set_busy" && frame["observation"]["body"] == false {
            break;
        }
    }
    trace_through_settlement(&mut early, &mut early_trace);
    send(
        &mut late,
        &ClientFrame::Request {
            id: 3,
            op: Op::ListSessions {
                workspace: None,
                runner: None,
                include_archived: false,
            },
        },
    );
    let listed = response(&mut late, 3);
    let sessions = listed["ok"]["rows"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let session = sessions[0]["session"].as_str().unwrap();
    drop(late);
    drop(early);
    interrupt_and_wait(&mut web);
    late_view.assert_durable(&fixture, session);
}

fn attach_retained(
    socket: &mut WebSocket<UnixStream>,
    root: &NodeAddress,
    id: u64,
    revision: &Value,
) -> Value {
    socket
        .send(Message::text(
            json!({
                "frame": "request", "id": id,
                "op": { "op": "attach", "node": root, "retained": revision },
            })
            .to_string(),
        ))
        .unwrap();
    let outcome = response(socket, id);
    assert!(outcome["ok"].is_object(), "{outcome}");
    outcome["ok"].clone()
}
