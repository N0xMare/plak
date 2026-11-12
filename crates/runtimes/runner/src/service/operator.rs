use plakous_runner_protocol::operator::{OperatorAnswer, OperatorOperation, OperatorRefusal};

use super::{
    Ordering, Request, RequestClass, RunnerFrame, RunnerService, RunnerVerb, ServeFailure,
    ServeFuture, Value, environment_failure, mpsc,
};

impl RunnerService {
    pub(super) fn operator_link_current(
        &self,
        host: &str,
        generation: &str,
        connection: u64,
    ) -> bool {
        super::lock(&self.inner.hosts)
            .get(&(host.to_owned(), generation.to_owned()))
            .is_some_and(|current| current.id == connection)
    }
    pub(super) async fn join_operator_shutdown(&self) {
        let Some(operator) = &self.inner.operator else {
            return;
        };
        if tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while operator.has_live_terminals() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .is_err()
        {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "operator terminal cleanup remains unknown after runner shutdown"
            );
        }
    }
    pub(super) fn serve_extension(&self, verb: RunnerVerb, payload: Value) -> ServeFuture {
        let Some(extension) = &self.inner.extension else {
            return Box::pin(std::future::ready(Err(ServeFailure::Unsupported)));
        };
        let admission = match (
            &self.inner.operator,
            extension.reclamation_path(verb, &payload),
        ) {
            (Some(operator), Some(path)) => match operator.reclaim(&path) {
                Ok(admission) => Some(admission),
                Err(_) => {
                    return Box::pin(std::future::ready(Err(environment_failure(
                        &plakous_tools::internal::runner::workspace_refusal(),
                    ))));
                }
            },
            _ => None,
        };
        let served = extension.serve(verb, payload);
        Box::pin(async move {
            let answer = served.await;
            drop(admission);
            answer
        })
    }
    /// Independent host-local and remote operator API; never a `RunnerServices` line port.
    #[must_use]
    pub fn operator_terminals(&self) -> Option<&crate::operator::OperatorTerminalService> {
        self.inner.operator.as_ref()
    }

    pub(super) fn operator_request(
        &self,
        host: &str,
        generation: &str,
        connection: u64,
        request: Request,
        sender: &mpsc::UnboundedSender<RunnerFrame>,
    ) {
        let request_id = request.request_id.clone();
        let answer = self.apply_operator(host, generation, connection, request);
        let payload = serde_json::to_value(answer).unwrap_or_default();
        let _ = sender.send(RunnerFrame::Result {
            request_id,
            payload,
        });
    }

    fn apply_operator(
        &self,
        host: &str,
        generation: &str,
        connection: u64,
        request: Request,
    ) -> OperatorAnswer {
        let refuse = |refusal| OperatorAnswer::Refused { refusal };
        if !self.operator_link_current(host, generation, connection) {
            return refuse(OperatorRefusal::ReconcileRequired);
        }
        if request.owner.is_some()
            || request.digest != crate::client::digest(&request.operation)
            || self.inner.closing.load(Ordering::Acquire)
        {
            return refuse(OperatorRefusal::InvalidRequest);
        }
        let Ok(operation) = serde_json::from_value::<OperatorOperation>(request.operation.payload)
        else {
            return refuse(OperatorRefusal::InvalidRequest);
        };
        if operation.verb() != request.operation.verb {
            return refuse(OperatorRefusal::InvalidRequest);
        }
        let RequestClass::OperatorControl { namespace, .. } = request.class else {
            return refuse(OperatorRefusal::InvalidRequest);
        };
        let Some(operator) = &self.inner.operator else {
            return match operation {
                OperatorOperation::Inventory => {
                    match self.inner.hello.instance.clone().try_into() {
                        Ok(instance) => OperatorAnswer::Inventory {
                            instance,
                            pins: Vec::new(),
                        },
                        Err(_) => refuse(OperatorRefusal::InvalidRequest),
                    }
                }
                OperatorOperation::Reclaim { .. } => OperatorAnswer::Reclamation { stream: None },
                _ => refuse(OperatorRefusal::InvalidRequest),
            };
        };
        if request.replay {
            return if let OperatorOperation::Open { key, .. } = &operation {
                operator.replay_open(host, generation, namespace, &request.digest, key)
            } else {
                refuse(OperatorRefusal::ReconcileRequired)
            };
        }
        operator.execute(
            host,
            generation,
            connection,
            namespace,
            &request.digest,
            operation,
        )
    }

    pub(super) fn operator_output(
        &self,
        host: &str,
        generation: &str,
        connection: u64,
        sender: &mpsc::Sender<RunnerFrame>,
    ) {
        if !self.operator_link_current(host, generation, connection) {
            return;
        }
        let Some(operator) = &self.inner.operator else {
            return;
        };
        // This connection's single producer reserves its remaining frame capacity
        // before output can advance a cursor or sequence. The consumer only frees it.
        for frame in operator.output_round(host, generation, connection, sender.capacity()) {
            let _ = sender.try_send(RunnerFrame::Stream(frame));
        }
    }
}
