//! Retry only the internal control that failed, retaining attachment custody.

use serde::Serialize;

use super::{Inner, State, lock};
use crate::wire::{AttachmentId, ClientFrame, Op};
use crate::{ProtocolExtension, SessionIntent};

pub(super) enum Control {
    Attach,
    Detach(AttachmentId),
    OpenTranscript(String),
}

impl<P: ProtocolExtension> Inner<P>
where
    P::Intent: Serialize,
{
    pub(super) fn write_control(
        &self,
        state: &mut State<P>,
        control: Control,
    ) -> std::io::Result<()> {
        let op = match &control {
            Control::Attach => Op::Attach {
                node: state.root.clone(),
            },
            Control::Detach(attachment) => Op::Detach {
                attachment: *attachment,
            },
            Control::OpenTranscript(agent_id) => {
                let Some(attachment) = state.attachment else {
                    // The next attach reopens the retained agent transcript.
                    state.clear_control_retry();
                    return Ok(());
                };
                Op::Intent {
                    client_id: None,
                    attachment,
                    intent: SessionIntent::OpenAgentTranscript {
                        agent_id: agent_id.clone(),
                    },
                    blobs: Vec::new(),
                }
            }
        };
        let id = state.next_request;
        state.next_request += 1;
        if let Err(error) = self.write(&ClientFrame::Request { id, op }) {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                state.control_retry = Some(control);
                state.control_error = Some(error.to_string());
            }
            return Err(error);
        }
        if matches!(control, Control::Attach) {
            if state.attachment.is_some() {
                state.previous_attachment = state.attachment.take();
            }
            state.attaching = Some(id);
        }
        state.clear_control_retry();
        Ok(())
    }

    pub(super) fn retry_control(&self) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        let Some(control) = state.control_retry.take() else {
            return;
        };
        let sent = match control {
            Control::Detach(attachment) => self
                .write_control(&mut state, Control::Detach(attachment))
                .and_then(|()| self.attach_again(&mut state)),
            Control::Attach => self.attach_again(&mut state),
            Control::OpenTranscript(agent) => {
                self.write_control(&mut state, Control::OpenTranscript(agent))
            }
        };
        let progressed = sent.is_ok();
        if progressed && let Some(attachment) = state.current_attachment() {
            for intent in state.take_unsent() {
                self.write_intent(&mut state, attachment, intent);
            }
        }
        drop(state);
        if progressed {
            self.changed.notify_all();
        }
    }
}
