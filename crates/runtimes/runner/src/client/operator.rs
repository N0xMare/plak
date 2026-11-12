//! Operator runner calls never enter the effect journal or replay keyboard input.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use plakous_runner_protocol::StreamEvent;
use plakous_runner_protocol::operator::{
    OperatorAnswer, OperatorId, OperatorNamespace, OperatorOperation, TerminalEvent,
};

use super::{
    HostFrame, Operation, Request, RequestClass, RunnerClient, RunnerFailure, StreamFrame, digest,
    fits, lock, mpsc, oneshot,
};

/// One connection-scoped terminal stream. Loss drops it; input is never replayed.
pub struct OperatorTerminalStream {
    client: RunnerClient,
    connection: u64,
    stream: u64,
    inbound_seq: u64,
    outbound_seq: u64,
    received: mpsc::Receiver<StreamFrame>,
}

impl RunnerClient {
    /// Sends one operator request without an agent owner, journal or automatic retry.
    /// Retry an ambiguous open only with the same terminal id and runner instance.
    ///
    /// # Errors
    /// Link loss is an unknown outcome, not permission to repeat an input or spawn.
    pub async fn operator_control(
        &self,
        namespace: OperatorNamespace,
        control_id: OperatorId,
        operation: OperatorOperation,
    ) -> Result<OperatorAnswer, RunnerFailure> {
        self.operator_request(namespace, control_id, operation, false)
            .await
    }

    /// Resolves a lost open answer from a retained reservation only. A missing
    /// receipt, including on a restarted runner, never creates a replacement shell.
    ///
    /// # Errors
    /// The operation is not Open, or its reconciliation answer is unavailable.
    pub async fn operator_recover_open(
        &self,
        namespace: OperatorNamespace,
        control_id: OperatorId,
        operation: OperatorOperation,
    ) -> Result<OperatorAnswer, RunnerFailure> {
        if !matches!(operation, OperatorOperation::Open { .. }) {
            return Err(RunnerFailure::NotSent(
                "open recovery requires an open operation".into(),
            ));
        }
        self.operator_request(namespace, control_id, operation, true)
            .await
    }

    async fn operator_request(
        &self,
        namespace: OperatorNamespace,
        control_id: OperatorId,
        operation: OperatorOperation,
        replay: bool,
    ) -> Result<OperatorAnswer, RunnerFailure> {
        let operation = Operation {
            verb: operation.verb().into(),
            payload: serde_json::to_value(operation)
                .map_err(|_| RunnerFailure::Refused("invalid operator request".into()))?,
        };
        let request = Request {
            request_id: self.inner.next_id("operator"),
            class: RequestClass::OperatorControl {
                namespace,
                control_id,
            },
            owner: None,
            deadline_ms: None,
            digest: digest(&operation),
            replay,
            age_ms: None,
            operation,
        };
        let frame = HostFrame::Request(request.clone());
        if !fits(&frame) {
            return Err(RunnerFailure::Refused(
                "operator request exceeds frame limit".into(),
            ));
        }
        let request_id = request.request_id.clone();
        let (answer, answered) = oneshot::channel();
        self.inner.dispatch(request, frame, answer)?;
        match tokio::time::timeout(self.inner.windows.read, answered).await {
            Ok(Ok(Ok(value))) => serde_json::from_value(value)
                .map_err(|_| RunnerFailure::Unavailable("invalid operator answer".into())),
            Ok(Ok(Err(failure))) => Err(failure),
            _ => {
                lock(&self.inner.state).pending.remove(&request_id);
                Err(RunnerFailure::OutcomeUnknown(
                    "operator answer was lost; reconcile before control".into(),
                ))
            }
        }
    }

    /// Registers a successful Attach answer's stream before granting output credit.
    ///
    /// # Errors
    /// Stream is already registered, the link was lost, or the table is full.
    pub fn operator_stream(&self, stream: u64) -> Result<OperatorTerminalStream, RunnerFailure> {
        let mut state = lock(&self.inner.state);
        if state.sender.is_none()
            || state.operator_streams.len() >= 16
            || state.operator_streams.contains_key(&stream)
        {
            return Err(RunnerFailure::Unavailable(
                "operator stream unavailable".into(),
            ));
        }
        let (sender, received) = mpsc::channel(8);
        state.operator_streams.insert(stream, sender);
        Ok(OperatorTerminalStream {
            client: self.clone(),
            connection: state.connection,
            stream,
            inbound_seq: 0,
            outbound_seq: 0,
            received,
        })
    }
}

impl OperatorTerminalStream {
    /// Grants at most 64 KiB decoded output credit; the runner enforces the total.
    ///
    /// # Errors
    /// Invalid grant or lost connection.
    pub fn credit(&mut self, bytes: u64) -> Result<(), RunnerFailure> {
        if bytes == 0 || bytes > 64 * 1024 {
            return Err(RunnerFailure::Refused("invalid terminal credit".into()));
        }
        self.send(StreamEvent::Credit { bytes })
    }

    /// Queues raw keyboard bytes once, qualified by their absolute input offset.
    /// Never call this again to repair an ambiguous transport failure.
    ///
    /// # Errors
    /// Invalid chunk or lost connection. Acceptance arrives as `InputAccepted`.
    pub fn input(&mut self, offset: u64, bytes: &[u8]) -> Result<(), RunnerFailure> {
        if bytes.is_empty() || bytes.len() > 16 * 1024 {
            return Err(RunnerFailure::Refused("invalid terminal input size".into()));
        }
        self.send(StreamEvent::Data {
            payload: serde_json::to_value(TerminalEvent::Data {
                offset,
                bytes_b64: STANDARD.encode(bytes),
            })
            .unwrap_or_default(),
        })
    }

    /// Receives the next typed event, or loss. This does not grant credit itself.
    ///
    /// # Errors
    /// Link loss, corrupt sequence or malformed event.
    pub async fn next(&mut self) -> Result<TerminalEvent, RunnerFailure> {
        let frame = self
            .received
            .recv()
            .await
            .ok_or_else(|| RunnerFailure::OutcomeUnknown("terminal stream lost".into()))?;
        if frame.seq != self.inbound_seq {
            return Err(RunnerFailure::Unavailable("terminal sequence gap".into()));
        }
        self.inbound_seq += 1;
        let StreamEvent::Data { payload } = frame.event else {
            return Err(RunnerFailure::Unavailable("terminal stream closed".into()));
        };
        serde_json::from_value(payload)
            .map_err(|_| RunnerFailure::Unavailable("invalid terminal event".into()))
    }

    fn send(&mut self, event: StreamEvent) -> Result<(), RunnerFailure> {
        let state = lock(&self.client.inner.state);
        if state.connection != self.connection {
            return Err(RunnerFailure::NotSent("terminal link changed".into()));
        }
        let frame = HostFrame::Stream(StreamFrame {
            stream: self.stream,
            seq: self.outbound_seq,
            event,
        });
        if state
            .sender
            .as_ref()
            .is_none_or(|sender| sender.send(frame).is_err())
        {
            return Err(RunnerFailure::OutcomeUnknown(
                "terminal input acceptance unknown".into(),
            ));
        }
        self.outbound_seq += 1;
        Ok(())
    }
}

impl super::Inner {
    pub(super) fn operator_frame(&self, frame: &StreamFrame) -> bool {
        let sender = lock(&self.state)
            .operator_streams
            .get(&frame.stream)
            .cloned();
        let Some(sender) = sender else {
            return false;
        };
        if sender.try_send(frame.clone()).is_err() {
            lock(&self.state).operator_streams.remove(&frame.stream);
            self.send(HostFrame::Stream(StreamFrame {
                stream: frame.stream,
                seq: 0,
                event: StreamEvent::Close {
                    reason: Some("slow_reader".into()),
                },
            }));
        }
        true
    }
}

impl Drop for OperatorTerminalStream {
    fn drop(&mut self) {
        let _ = self.send(StreamEvent::Close { reason: None });
        let mut state = lock(&self.client.inner.state);
        if state.connection == self.connection {
            state.operator_streams.remove(&self.stream);
        }
    }
}
