use super::*;
use crate::web_endpoint::upgrade_at;
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
    pub(super) fn connect(path: &Path, cookie: &str) -> Self {
        let mut socket = upgrade_at(path, cookie);
        send(
            &mut socket,
            &ClientFrame::Hello(Hello {
                proto: WIRE_VERSION,
                binary_id: "install-predecessor".into(),
                caps: Vec::new(),
            }),
        );
        let HostFrame::Welcome(welcome) = next(&mut socket).unwrap() else {
            panic!("web welcome missing");
        };
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
            match next(&mut self.socket).unwrap() {
                HostFrame::Response { id, outcome } => {
                    assert_eq!(id, self.sequence);
                    return outcome;
                }
                frame => self.pending.push_back(frame),
            }
        }
    }

    fn next(&mut self) -> Option<HostFrame<PlakExtension>> {
        self.pending.pop_front().or_else(|| next(&mut self.socket))
    }

    pub(super) fn attach(&mut self, node: &NodeAddress) {
        let outcome = self.request(Op::Attach { node: node.clone() });
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
                        delta: TranscriptStreamDelta::Assistant(delta),
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

    pub(super) fn snapshot_contains(&mut self, expected: &[&str]) {
        let mut rows = Vec::new();
        loop {
            if let HostFrame::Snapshot(page) = self.next().unwrap()
                && page.attachment == self.attachment
            {
                rows.extend(page.items.into_iter().filter_map(|item| match item.row {
                    TranscriptRow::Assistant { text } => Some(text),
                    _ => None,
                }));
                if page.last {
                    break;
                }
            }
        }
        for text in expected {
            assert!(
                rows.iter().any(|row| row.contains(text)),
                "snapshot lost {text:?}: {rows:?}"
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
            let frame = self
                .pending
                .pop_front()
                .or_else(|| next_until(&mut self.socket, deadline, &mut check_upgrade));
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

fn next(socket: &mut WebSocket<UnixStream>) -> Option<HostFrame<PlakExtension>> {
    next_until(socket, Instant::now() + WAIT, || {})
}

fn next_until(
    socket: &mut WebSocket<UnixStream>,
    deadline: Instant,
    mut check: impl FnMut(),
) -> Option<HostFrame<PlakExtension>> {
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
            Ok(Message::Text(text)) => return Some(decode_line(text.as_str()).unwrap()),
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
