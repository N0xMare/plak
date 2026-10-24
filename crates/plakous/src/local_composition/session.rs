//! The driver's session seam over a `plakous-cell` session: verdicts and
//! losses map one to one onto the local-composition types.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the local composition module that shares its vocabulary"
)]

use super::*;

use plakous_agent::internal::{
    LocalCompositionEvent, LocalCompositionEventReceiver, LocalCompositionExecuteRequest,
    LocalCompositionNestedCall, LocalCompositionNestedDelivery, LocalCompositionNestedResponder,
    LocalCompositionNotification, LocalCompositionNotificationResponder, LocalCompositionResponse,
    LocalCompositionSession, LocalCompositionSessionFuture, LocalCompositionSessionLoss,
    LocalCompositionStartedCell, LocalCompositionTerminateResult, LocalCompositionWaitOutcome,
    LocalCompositionWaitRequest,
};
use plakous_cell::{
    CellClient, CellResponse, CellSession, CellSessionEvent, CellSessionEvents, CellStartError,
    NestedDeliveryOutcome, NestedResponder, NotificationResponder, OutputItem, WaitOutcome,
};
use plakous_compose::javascript_globals;

/// The session bundle over one connected worker.
pub(crate) fn session_bundle(client: Arc<CellClient>) -> LocalCompositionSessionBundle {
    let (session, events) = CellSession::new(client);
    let exec_call = Arc::new(std::sync::Mutex::new(String::new()));
    LocalCompositionSessionBundle {
        session: Box::new(CellSessionAdapter {
            session,
            exec_call: Arc::clone(&exec_call),
        }),
        events: Box::new(CellEventsAdapter { events, exec_call }),
    }
}

struct CellSessionAdapter {
    session: CellSession,
    /// The tool call that started the running cell; its notifications name it.
    exec_call: Arc<std::sync::Mutex<String>>,
}

struct CellEventsAdapter {
    events: CellSessionEvents,
    exec_call: Arc<std::sync::Mutex<String>>,
}

fn host_lost(message: String) -> LocalCompositionSessionLoss {
    LocalCompositionSessionLoss::HostLost(message)
}

fn content_item(item: OutputItem) -> LocalCompositionContentItem {
    match item {
        OutputItem::Text { text } | OutputItem::Notify { text } => {
            LocalCompositionContentItem::Text(text)
        }
        OutputItem::Image { url, .. } => LocalCompositionContentItem::Image {
            url,
            detail: Some(LocalCompositionImageDetail::High),
        },
        OutputItem::Audio { url } => LocalCompositionContentItem::Audio(url),
    }
}

fn response(response: CellResponse) -> LocalCompositionResponse {
    match response {
        CellResponse::Yielded { cell, items } => LocalCompositionResponse::Yielded {
            cell_id: cell.to_string(),
            items: items.into_iter().map(content_item).collect(),
        },
        CellResponse::Result { cell, items, error } => LocalCompositionResponse::Result {
            cell_id: cell.to_string(),
            items: items.into_iter().map(content_item).collect(),
            error,
        },
    }
}

impl LocalCompositionSession for CellSessionAdapter {
    fn execute(
        &self,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'_, LocalCompositionStartedCell> {
        Box::pin(async move {
            let tool_call_id = request.tool_call_id.clone();
            let globals =
                javascript_globals(request.enabled_tools.iter().map(|tool| tool.name.as_str()));
            let tools = request
                .enabled_tools
                .iter()
                .zip(globals)
                .map(|(tool, global)| {
                    serde_json::json!({
                        "name": global,
                        "tool_name": tool.name,
                        "kind": "function",
                        "description": tool.description,
                        "input_schema": tool.input_schema,
                    })
                })
                .collect();
            let started = self
                .session
                .execute(
                    &request.source,
                    tools,
                    request.yield_time_ms,
                    request.start_admission.admit(),
                )
                .await
                .map_err(|error| match error {
                    CellStartError::Refused(loss) => loss,
                    CellStartError::Lost(message) => host_lost(message),
                })?;
            // Only a started cell's notifications name this call.
            *self
                .exec_call
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = tool_call_id;
            let initial = started.initial;
            Ok(LocalCompositionStartedCell {
                cell_id: started.cell.to_string(),
                initial_response: Box::pin(async move {
                    initial.await.map(response).map_err(host_lost)
                }),
            })
        })
    }

    fn wait(
        &self,
        request: LocalCompositionWaitRequest,
    ) -> LocalCompositionFuture<'_, LocalCompositionWaitOutcome> {
        Box::pin(async move {
            Ok(
                match self
                    .session
                    .wait(&request.cell_id, request.yield_time_ms)
                    .await
                {
                    WaitOutcome::Live(live) => LocalCompositionWaitOutcome::Live(response(live)),
                    WaitOutcome::Missing { error } => {
                        LocalCompositionWaitOutcome::Missing(LocalCompositionResponse::Result {
                            cell_id: request.cell_id,
                            items: Vec::new(),
                            error,
                        })
                    }
                    WaitOutcome::Lost(loss) => LocalCompositionWaitOutcome::Lost(host_lost(loss)),
                },
            )
        })
    }

    fn terminate(
        &self,
        cell_id: String,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionTerminateResult> + Send + '_>> {
        Box::pin(async move {
            match self.session.shutdown().await {
                Ok(_) => LocalCompositionTerminateResult::Joined(
                    LocalCompositionWaitOutcome::Live(LocalCompositionResponse::Terminated {
                        cell_id,
                        items: Vec::new(),
                    }),
                ),
                Err(loss) => LocalCompositionTerminateResult::Lost(host_lost(loss)),
            }
        })
    }

    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        Box::pin(async move {
            match self.session.shutdown().await {
                Ok(usage) => {
                    let child_exit = usage.map(|usage| plakous_agent::internal::ChildProcessExit {
                        child: plakous_agent::internal::ChildProcessKind::JsEvalSidecar,
                        user_cpu: usage.user_cpu,
                        system_cpu: usage.system_cpu,
                        peak_rss_bytes: usage.peak_rss_bytes,
                        // Forced or unproven cleanup remains a sticky error;
                        // only voluntary cleanup reaches this receipt.
                        escalation: plakous_agent::internal::ChildExitEscalation::Voluntary,
                    });
                    LocalCompositionShutdownResult::Joined { child_exit }
                }
                Err(loss) => LocalCompositionShutdownResult::Lost(host_lost(loss)),
            }
        })
    }
}

impl LocalCompositionEventReceiver for CellEventsAdapter {
    fn receive(&mut self) -> LocalCompositionSessionFuture<'_, Option<LocalCompositionEvent>> {
        Box::pin(async move {
            let event = self.events.receive().await;
            Ok(event.map(|event| match event {
                CellSessionEvent::NestedCall {
                    cell,
                    call,
                    name,
                    input,
                    responder,
                } => LocalCompositionEvent::NestedCall {
                    call: LocalCompositionNestedCall {
                        cell_id: cell.to_string(),
                        runtime_call_id: call.to_string(),
                        tool_name: name,
                        input: Some(input),
                    },
                    responder: Box::new(NestedAdapter(responder)),
                },
                CellSessionEvent::Notification {
                    cell,
                    text,
                    responder,
                } => LocalCompositionEvent::Notification {
                    notification: LocalCompositionNotification {
                        call_id: self
                            .exec_call
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone(),
                        cell_id: cell.to_string(),
                        text,
                    },
                    responder: Box::new(NotificationAdapter(std::sync::Mutex::new(Some(
                        responder,
                    )))),
                },
            }))
        })
    }
}

/// Reports the worker's own answer. A refusal names no pending request and
/// leaves the cell undisturbed. A non-delivery proves the result never reached
/// the evaluator, so the cell cannot have acted on it and nothing is unknown.
/// Only a loss with the result already handed over leaves acceptance
/// undetermined.
fn delivery(outcome: NestedDeliveryOutcome) -> LocalCompositionNestedDelivery {
    match outcome {
        NestedDeliveryOutcome::Accepted => LocalCompositionNestedDelivery::Accepted,
        NestedDeliveryOutcome::Refused(refusal) => LocalCompositionNestedDelivery::Refused(refusal),
        NestedDeliveryOutcome::NotDelivered(reason) => {
            LocalCompositionNestedDelivery::NotDelivered(reason)
        }
        NestedDeliveryOutcome::Lost(_) => LocalCompositionNestedDelivery::HostLost,
    }
}

struct NestedAdapter(NestedResponder);

impl LocalCompositionNestedResponder for NestedAdapter {
    fn resolve(
        self: Box<Self>,
        value: serde_json::Value,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        Box::pin(async move { Ok(delivery(self.0.resolve(value).await)) })
    }

    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        Box::pin(async move { Ok(delivery(self.0.reject(reason).await)) })
    }
}

struct NotificationAdapter(std::sync::Mutex<Option<NotificationResponder>>);

impl LocalCompositionNotificationResponder for NotificationAdapter {
    fn acknowledge(self: Box<Self>) -> bool {
        self.0
            .lock()
            .ok()
            .and_then(|mut responder| responder.take())
            .is_some_and(NotificationResponder::acknowledge)
    }

    fn reject(self: Box<Self>, _reason: String) -> bool {
        self.acknowledge()
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, reason = "owned worker failure regressions")]
mod tests {
    use super::*;
    use plakous_cell::{
        CELL_PROTOCOL_REVISION, EngineConfig, HostFrame, QUICKJS_WORKER, WorkerFrame, encode_frame,
    };
    use std::{os::unix::fs::PermissionsExt, time::Duration};

    /// Bytes of the one `Configure` frame the client sends after `Hello`.
    fn configure_len() -> usize {
        encode_frame(&HostFrame::Configure {
            revision: CELL_PROTOCOL_REVISION.to_owned(),
            engine: EngineConfig::QUICKJS,
        })
        .expect("configure frame")
        .len()
    }

    /// A shell worker that says hello as the `QuickJS` worker, then runs `body`.
    fn shell_worker(directory: &Path, body: &str) -> PathBuf {
        let hello = encode_frame(&WorkerFrame::Hello {
            revision: CELL_PROTOCOL_REVISION.to_owned(),
            engine: QUICKJS_WORKER.engine.to_owned(),
            adapter: QUICKJS_WORKER.adapter.to_owned(),
        })
        .expect("hello frame");
        let hello_path = directory.join("hello.bin");
        std::fs::write(&hello_path, hello).expect("hello bytes");
        let program = directory.join("worker");
        let staging = directory.join("worker.pending");
        std::fs::write(
            &staging,
            format!("#!/bin/sh\ncat '{}'\n{body}", hello_path.display()),
        )
        .expect("program");
        std::fs::rename(staging, &program).expect("publish program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
            .expect("permissions");
        program
    }

    async fn connect(program: &Path) -> LocalCompositionSessionBundle {
        let client = CellClient::spawn(program, QUICKJS_WORKER, EngineConfig::QUICKJS)
            .await
            .expect("worker hello");
        session_bundle(Arc::new(client))
    }

    #[tokio::test]
    async fn accepted_start_eof_and_malformed_frame_complete_without_turn_timeout() {
        // The worker consumes `Configure` and the first byte of the start, so
        // the start is accepted before the worker ends or misbehaves.
        let consumed = configure_len() + 1;
        for (name, after) in [
            ("eof", String::new()),
            (
                "malformed",
                "printf '\\000\\000\\000\\010not-json'\n".to_owned(),
            ),
        ] {
            let directory = tempfile::tempdir().expect("directory");
            let program = shell_worker(
                directory.path(),
                &format!("head -c {consumed} >/dev/null\n{after}"),
            );
            let bundle = connect(&program).await;
            let started = bundle
                .session
                .execute(LocalCompositionExecuteRequest {
                    start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(
                        || Box::pin(async { Ok(()) }),
                    ),
                    tool_call_id: "start".into(),
                    source: "text('once')".into(),
                    enabled_tools: vec![],
                    yield_time_ms: 10_000,
                    max_output_tokens: 100,
                })
                .await
                .expect("start admitted once");
            // Well under the 10 s yield, so a pass still proves the loss did
            // not wait for the turn; the margin is for process start under load.
            let result = tokio::time::timeout(Duration::from_secs(5), started.initial_response)
                .await
                .expect("prompt loss");
            assert!(result.is_err(), "{name} must be worker loss");
            assert!(matches!(
                bundle.session.shutdown().await,
                LocalCompositionShutdownResult::Joined { .. }
            ));
        }
    }

    #[tokio::test]
    async fn forced_termination_remains_lost_on_shutdown() {
        let directory = tempfile::tempdir().expect("directory");
        let program = shell_worker(directory.path(), "exec sleep 60\n");
        let bundle = connect(&program).await;
        assert!(matches!(
            bundle.session.terminate("1".into()).await,
            LocalCompositionTerminateResult::Lost(_)
        ));
        assert!(matches!(
            bundle.session.shutdown().await,
            LocalCompositionShutdownResult::Lost(_)
        ));
    }

    #[test]
    fn non_identifier_names_get_codex_aliases_unless_they_collide() {
        let names = ["exec_command", "mcp__everything__get-sum", "mcp__a__x.y"];
        assert_eq!(
            javascript_globals(names.into_iter()),
            ["exec_command", "mcp__everything__get_sum", "mcp__a__x_y"]
        );
        let colliding = [
            "mcp__s__get-sum",
            "mcp__s__get_sum",
            "mcp__s__a-b",
            "mcp__s__a.b",
        ];
        assert_eq!(
            javascript_globals(colliding.into_iter()),
            [
                "mcp__s__get-sum",
                "mcp__s__get_sum",
                "mcp__s__a-b",
                "mcp__s__a.b"
            ]
        );
    }
}
