use super::*;
use crate::web_endpoint::upgrade_at;
use plakd_wire_types::v13;
use plakous_host_protocol::SessionObservation;
use plakous_host_protocol::observation::TurnPhase;
use plakous_host_protocol::views::transcript::{TranscriptRow, TranscriptStreamDelta};
use plakous_host_protocol::wire::Welcome;
use std::collections::VecDeque;
use tungstenite::{Message, WebSocket};

pub(super) struct Browser {
    socket: WebSocket<UnixStream>,
    pub(super) welcome: Welcome<PlakExtension>,
    attachment: u64,
    sequence: u64,
    pending: VecDeque<HostFrame<PlakExtension>>,
}

impl Browser {
    pub(super) fn connect(path: &Path, cookie: &str, protocol: u32) -> Self {
        let mut socket = upgrade_at(path, cookie);
        send(
            &mut socket,
            &ClientFrame::Hello(Hello {
                proto: protocol,
                binary_id: "install-predecessor".into(),
                caps: Vec::new(),
            }),
        );
        let HostFrame::Welcome(welcome) = next(&mut socket, protocol).unwrap() else {
            panic!("web welcome missing");
        };
        assert_eq!(welcome.proto, protocol);
        Self {
            socket,
            welcome,
            attachment: 0,
            sequence: 0,
            pending: VecDeque::new(),
        }
    }

    fn request(&mut self, op: Op<PlakExtension>) -> Outcome<PlakExtension> {
        self.sequence += 1;
        send(
            &mut self.socket,
            &ClientFrame::Request {
                id: self.sequence,
                op,
            },
        );
        loop {
            match next(&mut self.socket, self.welcome.proto).unwrap() {
                HostFrame::Response { id, outcome } => {
                    assert_eq!(id, self.sequence);
                    return outcome;
                }
                frame => self.pending.push_back(frame),
            }
        }
    }

    fn next(&mut self) -> Option<HostFrame<PlakExtension>> {
        self.pending
            .pop_front()
            .or_else(|| next(&mut self.socket, self.welcome.proto))
    }

    /// Attaches, retrying `backpressure` while a resident root reaches its
    /// first viewport cut.
    pub(super) fn attach(&mut self, node: &NodeAddress) {
        let deadline = Instant::now() + WAIT;
        let outcome = loop {
            let outcome = self.request(Op::Attach { node: node.clone() });
            if !matches!(
                outcome,
                Outcome::Refused(plakous_host_protocol::wire::RequestRefusal::Backpressure)
            ) || Instant::now() >= deadline
            {
                break outcome;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let Outcome::Ok(Answer::Attached { attachment }) = outcome else {
            panic!("web attachment was refused: {outcome:?}");
        };
        self.attachment = attachment;
    }

    pub(super) fn prompt(&mut self, text: &str) {
        assert_eq!(
            self.request(Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment: self.attachment,
                intent: SessionIntent::Prompt(Draft::from(text)),
                blobs: Vec::new(),
            }),
            Outcome::Ok(Answer::Accepted { duplicate: false })
        );
    }

    pub(super) fn wait_for_text(&mut self, text: &str) {
        loop {
            let frame = self.next().unwrap();
            assert!(
                terminal(&frame, self.attachment).is_none(),
                "ended before {text:?}: {frame:?}"
            );
            if let HostFrame::Event {
                attachment,
                observation:
                    SessionObservation::AssistantDelta(delta)
                    | SessionObservation::TranscriptStream {
                        delta:
                            TranscriptStreamDelta::Assistant(delta)
                            | TranscriptStreamDelta::Keyed { text: delta, .. },
                        ..
                    },
                ..
            } = frame
                && attachment == self.attachment
                && delta.contains(text)
            {
                return;
            }
        }
    }

    pub(super) fn history_contains(&mut self, expected: &[&str]) {
        let until = Instant::now() + WAIT;
        let attachment = self.attachment;
        let mut rows = Vec::new();
        let mut cursor = None;
        loop {
            let v13::HostFrame::Draft(frame) = next_until(
                &mut self.socket,
                until,
                || {},
                |text| Some(decode_line::<v13::HostFrame>(text).unwrap()),
            )
            .unwrap() else {
                continue;
            };
            let (page, last, older) = match *frame {
                v13::HostFrameV13::Snapshot(page) if page.attachment == attachment => {
                    (page.items, page.last, page.older_cursor)
                }
                v13::HostFrameV13::HistoryPage {
                    attachment: owner,
                    id,
                    items,
                    last,
                    older_cursor,
                    ..
                } if owner == attachment && id == self.sequence => (items, last, older_cursor),
                v13::HostFrameV13::Response { id, outcome } if id == self.sequence => {
                    assert!(matches!(outcome, v13::Outcome::Ok(answer)
                        if matches!(*answer, v13::Answer::Draft(v13::AnswerV13::HistoryStarted))));
                    continue;
                }
                _ => continue,
            };
            rows.extend(page.into_iter().filter_map(|item| match item.row.0 {
                TranscriptRow::Assistant { text } => Some(text),
                _ => None,
            }));
            if !last {
                continue;
            }
            if expected
                .iter()
                .all(|text| rows.iter().any(|row| row.contains(text)))
            {
                break;
            }
            if let Some(older) = older {
                assert_ne!(
                    cursor.as_ref(),
                    Some(&older),
                    "older cursor did not advance"
                );
                cursor = Some(older.clone());
                self.sequence += 1;
                let line = encode_line(&v13::ClientFrameV13::Request {
                    id: self.sequence,
                    op: Box::new(v13::Op::Draft(v13::OpV13::OlderHistory {
                        attachment,
                        cursor: older,
                    })),
                })
                .unwrap();
                self.socket.send(Message::text(line)).unwrap();
            } else {
                break;
            }
        }
        for text in expected {
            assert!(
                rows.iter().any(|row| row.contains(text)),
                "history lost {text:?}: {rows:?}"
            );
        }
    }

    pub(super) fn lifecycle(
        &mut self,
        expected: HostLifecyclePhase,
        binary: &str,
        mut check_upgrade: impl FnMut(),
    ) -> usize {
        let deadline = Instant::now() + WAIT;
        let mut terminals = 0;
        loop {
            check_upgrade();
            assert!(
                Instant::now() < deadline,
                "lifecycle {expected:?} was not sent"
            );
            let frame = self.pending.pop_front().or_else(|| {
                next_until(&mut self.socket, deadline, &mut check_upgrade, |text| {
                    decode(text, self.welcome.proto)
                })
            });
            match frame.unwrap() {
                HostFrame::HostLifecycle {
                    host_generation,
                    phase,
                    reason,
                    target,
                    ..
                } => {
                    assert_eq!(host_generation, self.welcome.host_generation);
                    assert_eq!(phase, expected);
                    assert_eq!(reason, HostLifecycleReason::PairedUpgrade);
                    let target = target.unwrap();
                    assert_eq!(target.binary_id, binary);
                    assert_eq!(target.state_key, self.welcome.state_key);
                    return terminals;
                }
                frame => terminals += usize::from(terminal(&frame, self.attachment).is_some()),
            }
        }
    }

    pub(super) fn expect_completed(&mut self) {
        loop {
            let frame = self.next().unwrap();
            assert!(
                !matches!(frame, HostFrame::HostLifecycle { .. }),
                "unexpected lifecycle: {frame:?}"
            );
            if let Some(phase) = terminal(&frame, self.attachment) {
                assert_eq!(phase, TurnPhase::Completed, "{frame:?}");
                return;
            }
        }
    }

    pub(super) fn expect_closed(&mut self) -> usize {
        let mut terminals = 0;
        while let Some(frame) = self.next() {
            assert!(
                !matches!(frame, HostFrame::HostLifecycle { .. }),
                "lifecycle after Restarting: {frame:?}"
            );
            terminals += usize::from(terminal(&frame, self.attachment).is_some());
        }
        terminals
    }
}

fn terminal(frame: &HostFrame<PlakExtension>, current: u64) -> Option<TurnPhase> {
    match frame {
        HostFrame::Event {
            attachment,
            observation: SessionObservation::TurnLifecycle(turn),
            ..
        } if *attachment == current && turn.phase != TurnPhase::Admitted => Some(turn.phase),
        _ => None,
    }
}

fn next(socket: &mut WebSocket<UnixStream>, protocol: u32) -> Option<HostFrame<PlakExtension>> {
    next_until(
        socket,
        Instant::now() + WAIT,
        || {},
        |text| decode(text, protocol),
    )
}

fn decode(text: &str, protocol: u32) -> Option<HostFrame<PlakExtension>> {
    if protocol == WIRE_VERSION {
        crate::protocol_13::decode(text).unwrap()
    } else {
        Some(decode_line(text).unwrap())
    }
}

fn next_until<T>(
    socket: &mut WebSocket<UnixStream>,
    deadline: Instant,
    mut check: impl FnMut(),
    decode: impl Fn(&str) -> Option<T>,
) -> Option<T> {
    socket
        .get_mut()
        .set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();
    loop {
        check();
        assert!(
            Instant::now() < deadline,
            "no web frame before the deadline"
        );
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Some(frame) = decode(text.as_str()) {
                    return Some(frame);
                }
            }
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            Ok(Message::Close(_))
            | Err(
                tungstenite::Error::ConnectionClosed
                | tungstenite::Error::AlreadyClosed
                | tungstenite::Error::Protocol(
                    tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                ),
            ) => return None,
            Err(tungstenite::Error::Io(error))
                if error.kind() == std::io::ErrorKind::BrokenPipe =>
            {
                return None;
            }
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            other => panic!("unexpected web transport reply: {other:?}"),
        }
    }
}

fn send(socket: &mut WebSocket<UnixStream>, frame: &ClientFrame<PlakExtension>) {
    socket
        .send(Message::text(encode_line(frame).unwrap()))
        .unwrap();
}
