//! The driver's session seam over a `plakous-cell` session: verdicts and
//! losses map one to one onto the local-composition types. The engine behind
//! a session decides only how cells start, stop and are observed.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the local composition module that shares its vocabulary"
)]

use super::*;

use plakous_agent::internal::{
    ChildExitEscalation, ChildProcessExit, ChildProcessKind, LocalCompositionEvent,
    LocalCompositionEventReceiver, LocalCompositionExecuteRequest, LocalCompositionNestedCall,
    LocalCompositionNestedDelivery, LocalCompositionNestedResponder, LocalCompositionNotification,
    LocalCompositionNotificationResponder, LocalCompositionResponse, LocalCompositionSession,
    LocalCompositionSessionFuture, LocalCompositionSessionLoss, LocalCompositionStartedCell,
    LocalCompositionTerminateResult, LocalCompositionWaitOutcome, LocalCompositionWaitRequest,
};
use plakous_cell::{
    CellClient, CellPrelude, CellResponse, CellSession, CellSessionEvent, CellSessionEvents,
    CellStartError, ChildProcessUsage, LossClass, NestedDeliveryOutcome, NestedResponder,
    NotificationResponder, ObservationMode, OutputItem, WaitOutcome,
};
use plakous_compose::javascript_globals;

/// A future the session seam returns, borrowing the session.
pub(super) type SessionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The engine behind one session: how its cells start, how long an
/// observation may wait, and how its worker stops.
pub(super) trait SessionEngine: Send + Sync {
    fn mode(&self) -> ObservationMode;

    fn start<'a>(
        &'a self,
        core: &'a SessionCore,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'a, LocalCompositionStartedCell>;

    /// The observation window a `wait` asks for, as this engine bounds it.
    fn wait_window(&self, requested: u64) -> u64 {
        requested
    }

    fn terminate<'a>(
        &'a self,
        core: &'a SessionCore,
        cell_id: String,
    ) -> SessionFuture<'a, LocalCompositionTerminateResult>;

    fn shutdown<'a>(
        &'a self,
        core: &'a SessionCore,
    ) -> SessionFuture<'a, LocalCompositionShutdownResult>;
}

/// `QuickJS`: engine-parked yields; one worker per root turn. Forced or
/// unproven cleanup remains a sticky error; only voluntary cleanup reaches a
/// joined receipt.
pub(super) struct Quickjs;

impl SessionEngine for Quickjs {
    fn mode(&self) -> ObservationMode {
        ObservationMode::EngineParked
    }

    fn start<'a>(
        &'a self,
        core: &'a SessionCore,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'a, LocalCompositionStartedCell> {
        Box::pin(async move {
            let prelude = quickjs_prelude(&request);
            core.start(request, prelude).await
        })
    }

    fn terminate<'a>(
        &'a self,
        core: &'a SessionCore,
        cell_id: String,
    ) -> SessionFuture<'a, LocalCompositionTerminateResult> {
        Box::pin(async move {
            match core.stop_quickjs().await {
                Ok(_) => LocalCompositionTerminateResult::Joined(
                    LocalCompositionWaitOutcome::Live(LocalCompositionResponse::Terminated {
                        cell_id,
                        items: Vec::new(),
                    }),
                ),
                Err(loss) => LocalCompositionTerminateResult::Lost(loss),
            }
        })
    }

    fn shutdown<'a>(
        &'a self,
        core: &'a SessionCore,
    ) -> SessionFuture<'a, LocalCompositionShutdownResult> {
        Box::pin(async move {
            match core.stop_quickjs().await {
                Ok(child_exit) => LocalCompositionShutdownResult::Joined { child_exit },
                Err(loss) => LocalCompositionShutdownResult::Lost(loss),
            }
        })
    }
}

fn quickjs_prelude(request: &LocalCompositionExecuteRequest) -> CellPrelude {
    let globals = javascript_globals(request.enabled_tools.iter().map(|tool| tool.name.as_str()));
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
    CellPrelude::Quickjs {
        tools,
        stored: std::collections::HashMap::new(),
    }
}

/// The session bundle over one connected worker.
pub(crate) fn session_bundle(
    client: Arc<CellClient>,
    engine: Box<dyn SessionEngine>,
) -> LocalCompositionSessionBundle {
    let (session, events) = CellSession::new(Arc::clone(&client), engine.mode());
    let exec_call = Arc::new(std::sync::Mutex::new(String::new()));
    LocalCompositionSessionBundle {
        session: Box::new(CellSessionAdapter {
            core: SessionCore {
                session,
                client,
                exec_call: Arc::clone(&exec_call),
            },
            engine,
        }),
        events: Box::new(CellEventsAdapter { events, exec_call }),
    }
}

/// What every engine shares: the cell session, its worker client, and the
/// tool call that started the running cell.
pub(super) struct SessionCore {
    pub(super) session: CellSession,
    client: Arc<CellClient>,
    /// The tool call that started the running cell; its notifications name it.
    exec_call: Arc<std::sync::Mutex<String>>,
}

struct CellSessionAdapter {
    core: SessionCore,
    engine: Box<dyn SessionEngine>,
}

struct CellEventsAdapter {
    events: CellSessionEvents,
    exec_call: Arc<std::sync::Mutex<String>>,
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

pub(super) fn response(response: CellResponse) -> LocalCompositionResponse {
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

/// A worker loss, typed as a protocol violation when the worker said so.
fn loss_of(client: &CellClient, message: String) -> LocalCompositionSessionLoss {
    if client.loss_class() == Some(LossClass::ProtocolViolation) {
        LocalCompositionSessionLoss::ProtocolViolation(message)
    } else {
        LocalCompositionSessionLoss::HostLost(message)
    }
}

/// The receipt of a stopped worker: its usage, killed when it had to be.
pub(super) fn child_exit(
    kind: ChildProcessKind,
    usage: Option<ChildProcessUsage>,
    forced: bool,
) -> ChildProcessExit {
    let usage = usage.unwrap_or_default();
    ChildProcessExit {
        child: kind,
        user_cpu: usage.user_cpu,
        system_cpu: usage.system_cpu,
        peak_rss_bytes: usage.peak_rss_bytes,
        escalation: if forced {
            ChildExitEscalation::Killed
        } else {
            ChildExitEscalation::Voluntary
        },
    }
}

impl SessionCore {
    /// The loss a worker message describes.
    pub(super) fn loss(&self, message: String) -> LocalCompositionSessionLoss {
        loss_of(&self.client, message)
    }

    /// Starts one cell with `prelude` and maps its observations.
    pub(super) async fn start(
        &self,
        request: LocalCompositionExecuteRequest,
        prelude: CellPrelude,
    ) -> Result<LocalCompositionStartedCell, LocalCompositionSessionLoss> {
        let tool_call_id = request.tool_call_id.clone();
        let started = self
            .session
            .execute(
                &request.source,
                prelude,
                request.yield_time_ms,
                request.start_admission.admit(),
            )
            .await
            .map_err(|error| match error {
                CellStartError::Refused(loss) => loss,
                CellStartError::Lost(message) => self.loss(message),
            })?;
        // Only a started cell's notifications name this call.
        *self
            .exec_call
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = tool_call_id;
        let initial = started.initial;
        let client = Arc::clone(&self.client);
        Ok(LocalCompositionStartedCell {
            cell_id: started.cell.to_string(),
            initial_response: Box::pin(async move {
                initial
                    .await
                    .map(response)
                    .map_err(|loss| loss_of(&client, loss))
            }),
        })
    }

    async fn stop_quickjs(&self) -> Result<Option<ChildProcessExit>, LocalCompositionSessionLoss> {
        let usage = self
            .session
            .shutdown()
            .await
            .map_err(|loss| self.loss(loss))?;
        Ok(usage.map(|usage| child_exit(ChildProcessKind::JsEvalSidecar, Some(usage), false)))
    }
}

impl LocalCompositionSession for CellSessionAdapter {
    fn execute(
        &self,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'_, LocalCompositionStartedCell> {
        self.engine.start(&self.core, request)
    }

    fn wait(
        &self,
        request: LocalCompositionWaitRequest,
    ) -> LocalCompositionFuture<'_, LocalCompositionWaitOutcome> {
        Box::pin(async move {
            let window = self.engine.wait_window(request.yield_time_ms);
            Ok(
                match self.core.session.wait(&request.cell_id, window).await {
                    WaitOutcome::Live(live) => LocalCompositionWaitOutcome::Live(response(live)),
                    WaitOutcome::Missing { error } => {
                        LocalCompositionWaitOutcome::Missing(LocalCompositionResponse::Result {
                            cell_id: request.cell_id,
                            items: Vec::new(),
                            error,
                        })
                    }
                    WaitOutcome::Lost(loss) => {
                        LocalCompositionWaitOutcome::Lost(self.core.loss(loss))
                    }
                },
            )
        })
    }

    fn terminate(
        &self,
        cell_id: String,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionTerminateResult> + Send + '_>> {
        self.engine.terminate(&self.core, cell_id)
    }

    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        self.engine.shutdown(&self.core)
    }

    fn generation_lost(&self) -> bool {
        self.core.session.generation_lost()
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
/// leaves the cell undisturbed. A non-delivery or non-observation proves the
/// result never reached the evaluator, so the cell cannot have acted on it
/// and nothing is unknown. Only a loss with the result already handed over
/// leaves acceptance undetermined.
fn delivery(outcome: NestedDeliveryOutcome) -> LocalCompositionNestedDelivery {
    match outcome {
        NestedDeliveryOutcome::Accepted => LocalCompositionNestedDelivery::Accepted,
        NestedDeliveryOutcome::Refused(refusal) => LocalCompositionNestedDelivery::Refused(refusal),
        NestedDeliveryOutcome::NotDelivered(reason) => {
            LocalCompositionNestedDelivery::NotDelivered(reason)
        }
        NestedDeliveryOutcome::NotObserved(reason) => {
            LocalCompositionNestedDelivery::NotObserved(reason)
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

#[cfg(all(test, feature = "composition-python"))]
#[allow(clippy::expect_used, reason = "persistent Python session regression")]
mod python_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn terminating_a_buffered_result_preserves_persistent_globals() {
        let Some(worker) = std::env::var_os("PLAK_PY_HOST") else {
            return;
        };
        let client = Arc::new(
            CellClient::spawn(
                Path::new(&worker),
                plakous_cell::MONTY_WORKER,
                plakous_cell::EngineConfig::MONTY,
            )
            .await
            .expect("worker"),
        );
        let engine = python::Monty::new(None, Duration::from_secs(90));
        let (session, _events) = CellSession::new(Arc::clone(&client), engine.mode());
        let core = SessionCore {
            session,
            client,
            exec_call: Arc::new(std::sync::Mutex::new(String::new())),
        };
        let request = |source: &str| LocalCompositionExecuteRequest {
            start_admission: plakous_agent::internal::LocalCompositionStartAdmission::new(|| {
                Box::pin(async { Ok(()) })
            }),
            tool_call_id: "buffered".into(),
            enabled_tools: Vec::new(),
            source: source.into(),
            yield_time_ms: 30_000,
            max_output_tokens: 100,
        };
        let started = engine
            .start(&core, request("counter = 41\ncounter"))
            .await
            .expect("start");
        let id = started.cell_id;
        drop(started.initial_response);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !core.session.terminal_ready(1).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal buffered without observation");
        let terminal = engine.terminate(&core, id).await;
        assert!(
            matches!(terminal, LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(LocalCompositionResponse::Result { error: None, ref items, .. })) if items.iter().any(|item| matches!(item, LocalCompositionContentItem::Text(text) if text == "41")))
        );
        assert!(!core.session.generation_lost());
        let next = engine
            .start(&core, request("counter + 1"))
            .await
            .expect("next cell")
            .initial_response
            .await
            .expect("result");
        assert!(
            matches!(next, LocalCompositionResponse::Result { error: None, ref items, .. } if items.iter().any(|item| matches!(item, LocalCompositionContentItem::Text(text) if text == "42")))
        );
        assert!(matches!(
            engine.shutdown(&core).await,
            LocalCompositionShutdownResult::Joined { .. }
        ));
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
        session_bundle(Arc::new(client), Box::new(Quickjs))
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
