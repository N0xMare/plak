//! Loopback Anthropic endpoint for the process-level gateway restart fixture.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use plakous_host_protocol::observation::{SessionObservation, TurnPhase};

use super::{Answer, Client, Draft, HostFrame, Op, Outcome, RequestId, SessionIntent};

const MODELS: &str = r#"{"data":[{"id":"claude-sonnet-5"}],"has_more":false}"#;
const ANSWER: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_loopback","type":"message","role":"assistant","model":"claude-sonnet-5","content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":0}}}"#,
    "\n\nevent: content_block_start\n",
    r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    "\n\nevent: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer from the loopback relay"}}"#,
    "\n\nevent: content_block_stop\n",
    r#"data: {"type":"content_block_stop","index":0}"#,
    "\n\nevent: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":1}}"#,
    "\n\nevent: message_stop\n",
    r#"data: {"type":"message_stop"}"#,
    "\n\n",
);

pub(super) fn prompt(client: &mut Client, id: RequestId, attachment: u64, text: &str) {
    let accepted = client.request(
        id,
        Op::Intent {
            client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
            attachment,
            intent: SessionIntent::Prompt(Draft::from(text)),
            blobs: Vec::new(),
        },
    );
    assert_eq!(accepted, Outcome::Ok(Answer::Accepted { duplicate: false }));
    let mut seen_answer = false;
    let mut last_notice = None;
    loop {
        let HostFrame::Event {
            observation,
            attachment: observed,
            ..
        } = client.next()
        else {
            continue;
        };
        if observed != attachment {
            continue;
        }
        match &observation {
            SessionObservation::Row(plakous_hosted::sessions::TranscriptRow::Notice(notice)) => {
                last_notice = Some(notice.text.clone());
            }
            SessionObservation::TurnLifecycle(lifecycle)
                if lifecycle.phase != TurnPhase::Admitted =>
            {
                assert_eq!(
                    lifecycle.phase,
                    TurnPhase::Completed,
                    "{text}: {lifecycle:?}; {last_notice:?}"
                );
                assert!(seen_answer, "{text}: no answer from the loopback relay");
                break;
            }
            SessionObservation::DraftRejected(_) => panic!("{text}: {last_notice:?}"),
            observation => {
                seen_answer |= format!("{observation:?}").contains("answer from the loopback relay")
            }
        }
    }
}

pub(super) fn start() -> (u16, mpsc::Receiver<&'static str>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let deadline = Instant::now() + super::WAIT;
        let mut turns = 0;
        while turns < 3 && Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Some(stage) = respond(stream) {
                        if send.send(stage).is_err() {
                            break;
                        }
                        turns += 1;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("loopback accept: {error}"),
            }
        }
    });
    (port, receive)
}

fn respond(mut stream: TcpStream) -> Option<&'static str> {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut request = String::new();
    reader.read_line(&mut request).unwrap();
    let mut length = 0;
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().unwrap();
        }
    }
    let (ty, body, stage) = if request.starts_with("GET /v1/models") {
        ("application/json", MODELS, None)
    } else {
        assert!(request.starts_with("POST /v1/messages"), "{request}");
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let body = String::from_utf8(body).unwrap();
        let stage = ["after crash", "after drain", "before drain"]
            .into_iter()
            .find(|stage| body.contains(stage))
            .expect("the relayed model request contains the current prompt");
        ("text/event-stream", ANSWER, Some(stage))
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nrequest-id: req_fixture\r\ncontent-type: {ty}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    stage
}
