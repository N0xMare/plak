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
        "assistant_delta" => body.as_str(),
        "reasoning_delta" => body["text"].as_str(),
        _ => None,
    }
    .map_or(0, str::len);
    let presentation = match event {
        "snapshot" => format!(
            " rev={} page={} last={}",
            frame["revision"], frame["page"], frame["last"]
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
    let attachment = attach(&mut socket, &root, 1);
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
        assert_ne!(kind, "resync_required");
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
        if event == "assistant_delta" && body == PREFIX {
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
        if event == "assistant_delta" && body == MIDDLE {
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
            match serde_json::from_value::<HostFrame<Core>>(frame.clone()).unwrap() {
                HostFrame::Snapshot(page) => {
                    if let Some(snapshot) = self.pages.push(page).unwrap() {
                        self.transcript.replace(snapshot).unwrap();
                    }
                }
                HostFrame::Event {
                    observation: SessionObservation::TranscriptPatch(patch),
                    ..
                } => self.transcript.apply(patch).unwrap(),
                _ => unreachable!("only keyed frames are decoded"),
            }
        }
        frame
    }

    fn assert_durable(&self, fixture: &Fixture) {
        let session = self
            .transcript
            .items()
            .values()
            .find_map(|row| match row {
                TranscriptRow::SessionHeader { session_id, .. } => Some(session_id),
                _ => None,
            })
            .expect("the root baseline names its session");
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
fn late_root_attach_gets_the_prefix_before_future_web_deltas() {
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
        if frame["observation"]["kind"] == "assistant_delta" {
            assert_eq!(frame["observation"]["body"], PREFIX);
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
    attach(&mut late, &root, 1);
    let mut late_view = KeyedRoot::default();
    let mut released = false;
    loop {
        let frame = late_view.read(&mut late);
        if frame["observation"]["kind"] == "assistant_delta" {
            assert_eq!(
                assistant_text(late_view.transcript.items().values()),
                PREFIX,
                "the first raw delta arrived before its keyed prefix\nearly:\n{}\nlate:\n{}",
                early_trace.join("\n"),
                late_view.trace.join("\n")
            );
            assert_eq!(
                frame["observation"]["body"],
                MIDDLE,
                "{}",
                late_view.trace.join("\n")
            );
            break;
        }
        if !released && late_view.transcript.revision() != 0 {
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
    drop(late);
    drop(early);
    interrupt_and_wait(&mut web);
    late_view.assert_durable(&fixture);
}
