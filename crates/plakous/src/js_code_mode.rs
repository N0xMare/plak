//! rquickjs Code Mode integration over the `plakous-js-host` sidecar.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use plakous_agent::internal::{
    LocalCompositionContentItem, LocalCompositionDescriptor, LocalCompositionEvent,
    LocalCompositionEventReceiver, LocalCompositionExecuteInput, LocalCompositionExecuteRequest,
    LocalCompositionFuture, LocalCompositionImageDetail, LocalCompositionIntegration,
    LocalCompositionLifecycle, LocalCompositionNestedCall, LocalCompositionNestedDelivery,
    LocalCompositionNestedResponder, LocalCompositionNotification,
    LocalCompositionNotificationResponder, LocalCompositionResponse,
    LocalCompositionResultProjection, LocalCompositionSession, LocalCompositionSessionBundle,
    LocalCompositionSessionFuture, LocalCompositionSessionLoss, LocalCompositionShutdownResult,
    LocalCompositionStartedCell, LocalCompositionTerminateResult, LocalCompositionWaitInput,
    LocalCompositionWaitOutcome, LocalCompositionWaitRequest,
};
use plakous_compose::{
    CodexMinimalLocalCodeModeV1, CodexNativeLocalCodeModeV1,
    validate_codex_minimal_local_code_mode_candidate_v1,
    validate_codex_native_local_code_mode_candidate_v1,
};
use plakous_js_eval::{RuntimeEvent, SidecarHost};
use plakous_model::{CompiledProviderConfiguration, ToolName};
use plakous_tools::{FrozenToolRegistry, ProviderModelProjectionIdentityV2, ToolProjectionMapV2};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::local_composition::{
    CodexIntegrationProfile, WaitWireInput, from_native_item, neutral_descriptor,
    neutral_minimal_descriptor, to_native_item, whole_u64,
};

pub(crate) struct RquickjsCodeModeIntegration {
    pub(crate) identity: Vec<u8>,
    pub(crate) profile: CodexIntegrationProfile,
    pub(crate) nested_catalog: Vec<ToolName>,
    pub(crate) sidecar: std::path::PathBuf,
}

type CellCompletion = Result<LocalCompositionResponse, LocalCompositionSessionLoss>;
type CellResponseStream = Arc<Mutex<mpsc::Receiver<CellCompletion>>>;
type CompletionSenders = Arc<Mutex<HashMap<u64, mpsc::Sender<CellCompletion>>>>;

struct RquickjsSession {
    host: Arc<SidecarHost>,
    next_cell: AtomicU64,
    stored: Arc<Mutex<HashMap<String, Value>>>,
    events_tx: mpsc::Sender<LocalCompositionEvent>,
    completions: CompletionSenders,
    streams: Arc<Mutex<HashMap<u64, CellResponseStream>>>,
    pump: Mutex<Option<PumpTask>>,
}

struct PumpTask(Option<tokio::task::JoinHandle<()>>);
impl Drop for PumpTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}
impl RquickjsSession {
    async fn join_pump(&self) {
        if let Some(mut owner) = self.pump.lock().await.take()
            && let Some(task) = owner.0.take()
        {
            task.abort();
            let _ = task.await;
        }
        self.completions.lock().await.clear();
        self.streams.lock().await.clear();
    }
}

struct RquickjsEvents {
    rx: Mutex<mpsc::Receiver<LocalCompositionEvent>>,
}

impl LocalCompositionIntegration for RquickjsCodeModeIntegration {
    fn runtime_contract(&self) -> plakous_agent::internal::LocalCompositionRuntimeContract {
        plakous_agent::internal::LocalCompositionRuntimeContract::JAVASCRIPT
    }
    fn lifecycle(&self) -> LocalCompositionLifecycle {
        LocalCompositionLifecycle::RootTurnJoined
    }

    fn result_projection(&self) -> LocalCompositionResultProjection {
        LocalCompositionResultProjection::TokensScriptV1
    }

    fn environment_identity(&self) -> Vec<u8> {
        self.identity.clone()
    }

    fn parse_execute(&self, input: Value) -> Result<LocalCompositionExecuteInput, String> {
        let Value::String(raw) = input else {
            return Err("Code Mode exec input is not a custom UTF-8 string".to_owned());
        };
        let parsed = plakous_code_mode::parse_exec_source(&raw)?;
        Ok(LocalCompositionExecuteInput {
            source: parsed.code,
            yield_time_ms: parsed.yield_time_ms.unwrap_or(10_000),
            max_output_tokens: parsed.max_output_tokens.unwrap_or(10_000),
        })
    }

    fn parse_wait(&self, input: Value) -> Result<LocalCompositionWaitInput, String> {
        let input: WaitWireInput =
            serde_json::from_value(input).map_err(|_| "invalid Code Mode wait input".to_owned())?;
        Ok(LocalCompositionWaitInput {
            cell_id: input.cell_id,
            yield_time_ms: input
                .yield_time_ms
                .map_or(Ok(10_000), |value| whole_u64(value, "yield_time_ms"))?
                .min(600_000),
            max_output_tokens: input.max_tokens.map_or(Ok(10_000), |value| {
                whole_u64(value, "max_tokens").and_then(|value| {
                    usize::try_from(value).map_err(|_| {
                        "Code Mode wait max_tokens exceeds the host integer boundary".to_owned()
                    })
                })
            })?,
            terminate: input.terminate.unwrap_or(false),
        })
    }

    fn create_session(
        &self,
        _generation: u64,
        _max_yield_time_ms: u64,
        _max_output_tokens: usize,
    ) -> LocalCompositionFuture<'_, LocalCompositionSessionBundle> {
        let sidecar = self.sidecar.clone();
        let identity = self.identity.clone();
        Box::pin(async move {
            crate::local_composition::verify_sidecar_before_spawn(&sidecar, &identity)?;
            let host = SidecarHost::spawn(&sidecar)?;
            Ok(session_bundle_from_engine(Arc::new(host)))
        })
    }

    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        Box::pin(async move { LocalCompositionShutdownResult::Joined { child_exit: None } })
    }

    fn rebuild_descriptor(
        &self,
        job_config: plakous_tools::JobConfig,
        registry: FrozenToolRegistry,
        target: ProviderModelProjectionIdentityV2,
    ) -> Result<LocalCompositionDescriptor, String> {
        match self.profile {
            CodexIntegrationProfile::ExactReference => {
                CodexNativeLocalCodeModeV1::compile(job_config, registry, target)
                    .map_err(|error| error.to_string())
                    .and_then(|value| neutral_descriptor(&value))
            }
            CodexIntegrationProfile::MinimalCore => {
                CodexMinimalLocalCodeModeV1::compile_for_catalog(
                    job_config,
                    registry,
                    target,
                    &self.nested_catalog,
                )
                .map_err(|error| error.to_string())
                .and_then(|value| neutral_minimal_descriptor(&value))
            }
        }
    }

    fn validate_configuration(
        &self,
        job_config: plakous_tools::JobConfig,
        descriptor: &LocalCompositionDescriptor,
        projection: &ToolProjectionMapV2,
        registry: FrozenToolRegistry,
        configuration: &CompiledProviderConfiguration,
    ) -> Result<(), String> {
        match self.profile {
            CodexIntegrationProfile::ExactReference => {
                let exact = CodexNativeLocalCodeModeV1::compile(
                    job_config,
                    registry.clone(),
                    descriptor.outer_projection.target().clone(),
                )
                .map_err(|error| error.to_string())?;
                if neutral_descriptor(&exact)?.identity != descriptor.identity {
                    return Err(
                        "neutral descriptor disagrees with exact provider descriptor".into(),
                    );
                }
                validate_codex_native_local_code_mode_candidate_v1(
                    &exact,
                    projection,
                    registry,
                    configuration,
                )
                .map_err(|error| error.to_string())
            }
            CodexIntegrationProfile::MinimalCore => {
                let exact = CodexMinimalLocalCodeModeV1::compile_for_catalog(
                    job_config,
                    registry.clone(),
                    descriptor.outer_projection.target().clone(),
                    &self.nested_catalog,
                )
                .map_err(|error| error.to_string())?;
                if neutral_minimal_descriptor(&exact)?.identity != descriptor.identity {
                    return Err(
                        "neutral descriptor disagrees with minimal provider descriptor".into(),
                    );
                }
                validate_codex_minimal_local_code_mode_candidate_v1(
                    &exact,
                    projection,
                    registry,
                    configuration,
                )
                .map_err(|error| error.to_string())
            }
        }
    }

    fn truncate_items(
        &self,
        items: Vec<LocalCompositionContentItem>,
        max_output_tokens: usize,
    ) -> Vec<LocalCompositionContentItem> {
        plakous_code_mode::truncate_content_items(
            items.into_iter().map(to_native_item).collect(),
            max_output_tokens,
        )
        .into_iter()
        .map(from_native_item)
        .collect()
    }

    fn max_yield_time_ms(&self) -> u64 {
        plakous_code_mode::MAX_YIELD_TIME_MS
    }

    fn max_output_tokens(&self) -> usize {
        plakous_code_mode::MAX_OUTPUT_TOKENS
    }
}

pub(crate) fn session_bundle_from_engine(host: Arc<SidecarHost>) -> LocalCompositionSessionBundle {
    let (events_tx, events_rx) = mpsc::channel(64);
    LocalCompositionSessionBundle {
        session: Box::new(RquickjsSession {
            host,
            next_cell: AtomicU64::new(1),
            stored: Arc::new(Mutex::new(HashMap::new())),
            events_tx,
            completions: Arc::new(Mutex::new(HashMap::new())),
            streams: Arc::new(Mutex::new(HashMap::new())),
            pump: Mutex::new(None),
        }),
        events: Box::new(RquickjsEvents {
            rx: Mutex::new(events_rx),
        }),
    }
}

impl LocalCompositionEventReceiver for RquickjsEvents {
    fn receive(&mut self) -> LocalCompositionSessionFuture<'_, Option<LocalCompositionEvent>> {
        Box::pin(async move { Ok(self.rx.lock().await.recv().await) })
    }
}

impl LocalCompositionSession for RquickjsSession {
    fn execute(
        &self,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'_, LocalCompositionStartedCell> {
        Box::pin(async move {
            let mut pump = self.pump.lock().await;
            if !self.completions.lock().await.is_empty() {
                return Err(LocalCompositionSessionLoss::HostLost(
                    "QuickJS cell is already active".into(),
                ));
            }
            if let Some(mut previous) = pump.take()
                && let Some(task) = previous.0.take()
            {
                let _ = task.await;
            }
            let cell_id = self.next_cell.fetch_add(1, Ordering::Relaxed);
            let tools = request
                .enabled_tools
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "name": tool.name,
                        "tool_name": tool.name,
                        "kind": "function",
                        "description": tool.description,
                        "input_schema": tool.input_schema,
                    })
                })
                .collect();
            let (tx, rx) = mpsc::channel(2);
            let rx = Arc::new(Mutex::new(rx));
            self.completions.lock().await.insert(cell_id, tx);
            self.streams.lock().await.insert(cell_id, Arc::clone(&rx));
            let stored = self.stored.lock().await.clone();
            if let Err(error) = request.start_admission.admit().await {
                self.completions.lock().await.remove(&cell_id);
                self.streams.lock().await.remove(&cell_id);
                return Err(error);
            }
            if let Err(error) = self.host.start_cell(
                cell_id,
                &request.source,
                stored,
                tools,
                request.yield_time_ms,
            ) {
                self.completions.lock().await.remove(&cell_id);
                self.streams.lock().await.remove(&cell_id);
                return Err(LocalCompositionSessionLoss::HostLost(error));
            }
            *pump = Some(PumpTask(Some(tokio::spawn(pump_cell(
                Arc::clone(&self.host),
                cell_id,
                request.tool_call_id.clone(),
                self.events_tx.clone(),
                Arc::clone(&self.completions),
                Arc::clone(&self.stored),
            )))));
            let streams = Arc::clone(&self.streams);
            Ok(LocalCompositionStartedCell {
                cell_id: cell_id.to_string(),
                initial_response: Box::pin(async move {
                    let response = rx.lock().await.recv().await.unwrap_or_else(|| {
                        Err(LocalCompositionSessionLoss::HostLost("cell ended".into()))
                    });
                    if !matches!(response, Ok(LocalCompositionResponse::Yielded { .. })) {
                        streams.lock().await.remove(&cell_id);
                    }
                    response
                }),
            })
        })
    }

    fn wait(
        &self,
        request: LocalCompositionWaitRequest,
    ) -> LocalCompositionFuture<'_, LocalCompositionWaitOutcome> {
        Box::pin(async move {
            let Ok(cell_id) = request.cell_id.parse::<u64>() else {
                return Ok(LocalCompositionWaitOutcome::Missing(
                    LocalCompositionResponse::Result {
                        cell_id: request.cell_id,
                        items: Vec::new(),
                        error: Some("invalid cell id".into()),
                    },
                ));
            };
            let Some(rx) = self.streams.lock().await.get(&cell_id).cloned() else {
                return Ok(LocalCompositionWaitOutcome::Missing(
                    LocalCompositionResponse::Result {
                        cell_id: request.cell_id,
                        items: Vec::new(),
                        error: None,
                    },
                ));
            };
            let mut rx = rx.lock().await;
            let response = match rx.try_recv() {
                Ok(response) => Some(response),
                Err(mpsc::error::TryRecvError::Disconnected) => None,
                Err(mpsc::error::TryRecvError::Empty) => {
                    if let Err(error) = self.host.resume(cell_id, request.yield_time_ms) {
                        self.streams.lock().await.remove(&cell_id);
                        return Ok(LocalCompositionWaitOutcome::Lost(
                            LocalCompositionSessionLoss::HostLost(error),
                        ));
                    }
                    rx.recv().await
                }
            };
            match response {
                Some(Ok(response)) => {
                    if !matches!(response, LocalCompositionResponse::Yielded { .. }) {
                        self.streams.lock().await.remove(&cell_id);
                    }
                    Ok(LocalCompositionWaitOutcome::Live(response))
                }
                Some(Err(loss)) => {
                    self.streams.lock().await.remove(&cell_id);
                    Ok(LocalCompositionWaitOutcome::Lost(loss))
                }
                None => {
                    self.streams.lock().await.remove(&cell_id);
                    Ok(LocalCompositionWaitOutcome::Lost(
                        LocalCompositionSessionLoss::HostLost("cell completion closed".into()),
                    ))
                }
            }
        })
    }

    fn terminate(
        &self,
        cell_id: String,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionTerminateResult> + Send + '_>> {
        Box::pin(async move {
            self.join_pump().await;
            if let Err(loss) = self.host.terminate().await {
                return LocalCompositionTerminateResult::Lost(
                    LocalCompositionSessionLoss::HostLost(loss),
                );
            }
            LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(
                LocalCompositionResponse::Terminated {
                    cell_id,
                    items: Vec::new(),
                },
            ))
        })
    }

    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        Box::pin(async move {
            self.join_pump().await;
            match self.host.terminate().await {
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
                Err(loss) => LocalCompositionShutdownResult::Lost(
                    LocalCompositionSessionLoss::HostLost(loss),
                ),
            }
        })
    }
}

async fn pump_cell(
    host: Arc<SidecarHost>,
    cell_id: u64,
    exec_call_id: String,
    events_tx: mpsc::Sender<LocalCompositionEvent>,
    completions: CompletionSenders,
    stored: Arc<Mutex<HashMap<String, Value>>>,
) {
    let completion = PumpCompletion {
        host,
        cell_id,
        events_tx,
        completions,
        stored,
    };
    let mut content = CollectedContent::default();
    loop {
        let event = match completion.host.read_event().await {
            Ok(event) => event,
            Err(error) => {
                completion.lose(error).await;
                break;
            }
        };
        match event {
            RuntimeEvent::ToolCall {
                id, name, input, ..
            } => {
                let (response_tx, response_rx) = oneshot::channel();
                let event = LocalCompositionEvent::NestedCall {
                    call: LocalCompositionNestedCall {
                        cell_id: cell_id.to_string(),
                        runtime_call_id: id.to_string(),
                        tool_name: name,
                        input: Some(input),
                    },
                    responder: Box::new(JsNestedResponder {
                        host: Arc::clone(&completion.host),
                        execution_id: cell_id,
                        id,
                        reply: Mutex::new(Some(response_tx)),
                    }),
                };
                if !completion.forward(event, response_rx).await {
                    break;
                }
            }
            RuntimeEvent::Notify { text, .. } => {
                let (ack_tx, ack_rx) = oneshot::channel();
                let event = LocalCompositionEvent::Notification {
                    notification: LocalCompositionNotification {
                        call_id: exec_call_id.clone(),
                        cell_id: cell_id.to_string(),
                        text,
                    },
                    responder: Box::new(JsNotifyResponder {
                        ack: Mutex::new(Some(ack_tx)),
                    }),
                };
                if !completion.forward(event, ack_rx).await {
                    break;
                }
            }
            RuntimeEvent::Content { content: value, .. } => {
                if let Err(error) = content.append(&value) {
                    completion.lose(error).await;
                    break;
                }
            }
            RuntimeEvent::Yielded { .. } => {
                let sent = completion
                    .completions
                    .lock()
                    .await
                    .get(&cell_id)
                    .is_some_and(|tx| {
                        tx.try_send(Ok(LocalCompositionResponse::Yielded {
                            cell_id: cell_id.to_string(),
                            items: std::mem::take(&mut content.items),
                        }))
                        .is_ok()
                    });
                if !sent {
                    completion
                        .lose("resource limit: QuickJS completion queue".into())
                        .await;
                    break;
                }
            }
            RuntimeEvent::Done { stored, .. } => {
                completion.finish(stored, content.items, None).await;
                break;
            }
            RuntimeEvent::Error {
                message, stored, ..
            } => {
                completion
                    .finish(stored, content.items, Some(message))
                    .await;
                break;
            }
        }
    }
}

#[derive(Default)]
struct CollectedContent {
    items: Vec<LocalCompositionContentItem>,
    bytes: usize,
    count: usize,
}
impl CollectedContent {
    fn append(&mut self, content: &Value) -> Result<(), String> {
        let item = content_item(content);
        let bytes = match &item {
            LocalCompositionContentItem::Text(text) | LocalCompositionContentItem::Audio(text) => {
                text.len()
            }
            LocalCompositionContentItem::Image { url, .. } => url.len(),
        };
        if bytes > (8 * 1024 * 1024usize).saturating_sub(self.bytes) || self.count >= 4096 {
            return Err("resource limit: QuickJS collected output".into());
        }
        self.bytes += bytes;
        self.count += 1;
        self.items.push(item);
        Ok(())
    }
}

struct PumpCompletion {
    host: Arc<SidecarHost>,
    cell_id: u64,
    events_tx: mpsc::Sender<LocalCompositionEvent>,
    completions: CompletionSenders,
    stored: Arc<Mutex<HashMap<String, Value>>>,
}

impl PumpCompletion {
    async fn lose(&self, reason: String) {
        if let Some(tx) = self.completions.lock().await.remove(&self.cell_id) {
            let _ = tx.try_send(Err(LocalCompositionSessionLoss::HostLost(reason)));
        }
        let _ = self.host.terminate().await;
    }

    async fn forward(
        &self,
        event: LocalCompositionEvent,
        acknowledgement: oneshot::Receiver<()>,
    ) -> bool {
        if self.events_tx.try_send(event).is_err() {
            self.lose("composition event delivery unavailable".into())
                .await;
            return false;
        }
        let loss = tokio::select! {
            result = acknowledgement => result.err().map(|_| "composition acknowledgement closed".into()),
            loss = self.host.wait_loss() => Some(loss),
        };
        if let Some(loss) = loss {
            self.lose(loss).await;
            return false;
        }
        true
    }

    async fn finish(
        &self,
        stored: HashMap<String, Value>,
        items: Vec<LocalCompositionContentItem>,
        error: Option<String>,
    ) {
        let mut retained = self.stored.lock().await;
        retained.extend(stored);
        if plakous_js_eval::encode_frame(&*retained).is_err() {
            retained.clear();
            drop(retained);
            self.lose("resource limit: QuickJS retained storage".into())
                .await;
            return;
        }
        drop(retained);
        if let Some(tx) = self.completions.lock().await.remove(&self.cell_id) {
            let _ = tx.try_send(Ok(LocalCompositionResponse::Result {
                cell_id: self.cell_id.to_string(),
                items,
                error,
            }));
        }
    }
}

fn content_item(content: &Value) -> LocalCompositionContentItem {
    match content.get("type").and_then(Value::as_str) {
        Some("input_image") => LocalCompositionContentItem::Image {
            url: content
                .get("image_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            detail: Some(LocalCompositionImageDetail::High),
        },
        Some("input_audio") => LocalCompositionContentItem::Audio(
            content
                .get("audio_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
        _ => LocalCompositionContentItem::Text(
            content
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ),
    }
}

struct JsNestedResponder {
    host: Arc<SidecarHost>,
    execution_id: u64,
    id: u64,
    reply: Mutex<Option<oneshot::Sender<()>>>,
}

impl JsNestedResponder {
    /// Reports the engine's own answer. A refusal names no pending request and
    /// leaves the cell undisturbed. A non-delivery is the host's proof that the
    /// result never reached the evaluator, so the cell cannot have acted on it
    /// and nothing is unknown. Only a loss with the result already handed over
    /// leaves acceptance undetermined.
    async fn deliver(
        self: Box<Self>,
        value: Value,
        success: bool,
    ) -> LocalCompositionNestedDelivery {
        let acknowledgement = self
            .host
            .deliver_tool_result(self.execution_id, self.id, value, success)
            .await;
        if let Some(reply) = self.reply.lock().await.take() {
            let _ = reply.send(());
        }
        match acknowledgement {
            plakous_js_eval::NestedDeliveryOutcome::Accepted => {
                LocalCompositionNestedDelivery::Accepted
            }
            plakous_js_eval::NestedDeliveryOutcome::Refused(refusal) => {
                LocalCompositionNestedDelivery::Refused(refusal)
            }
            plakous_js_eval::NestedDeliveryOutcome::NotDelivered(reason) => {
                LocalCompositionNestedDelivery::NotDelivered(reason)
            }
            plakous_js_eval::NestedDeliveryOutcome::Lost(_) => {
                LocalCompositionNestedDelivery::HostLost
            }
        }
    }
}

impl LocalCompositionNestedResponder for JsNestedResponder {
    fn resolve(
        self: Box<Self>,
        value: Value,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        Box::pin(async move { Ok(self.deliver(value, true).await) })
    }

    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        Box::pin(async move { Ok(self.deliver(Value::String(reason), false).await) })
    }
}

struct JsNotifyResponder {
    ack: Mutex<Option<oneshot::Sender<()>>>,
}

impl LocalCompositionNotificationResponder for JsNotifyResponder {
    fn acknowledge(self: Box<Self>) -> bool {
        self.ack
            .try_lock()
            .ok()
            .and_then(|mut guard| guard.take())
            .is_some_and(|tx| tx.send(()).is_ok())
    }

    fn reject(self: Box<Self>, _reason: String) -> bool {
        self.acknowledge()
    }
}

#[cfg(all(test, unix))]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "owned evaluator failure regressions"
)]
mod ownership_tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, time::Duration};

    #[test]
    fn filtered_native_catalog_rebuild_keeps_file_writing_absent() {
        let mut builder = plakous_tools::ToolRegistryBuilder::default();
        plakous_tools::register_packs(
            &plakous_compose::standard_coding_packs_in_memory().expect("packs"),
            &mut builder,
        )
        .expect("register");
        CodexMinimalLocalCodeModeV1::register_markers_into(&mut builder).expect("markers");
        let registry = builder.freeze();
        let target = ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Standard,
            "openai.codex.responses",
            "openai.codex_subscription",
            "openai.codex.compat.0.153.3",
            "responses-family",
            "gpt-5.3-codex",
            "gpt-5.3-codex",
            serde_json::from_value(
                serde_json::to_value(
                    plakous_model::Digest::sha256(
                        plakous_model::DigestDomain::EffectiveProviderConfiguration,
                        5,
                        &[b"filtered-native"],
                    )
                    .expect("digest"),
                )
                .expect("json"),
            )
            .expect("configuration"),
            1,
        )
        .expect("target");
        let catalog = ["exec_command", "write_stdin", "update_plan"]
            .into_iter()
            .map(|name| ToolName::new(name).expect("name"))
            .collect::<Vec<_>>();
        let original = CodexMinimalLocalCodeModeV1::compile_for_catalog(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            target.clone(),
            &catalog,
        )
        .expect("descriptor");
        let integration = RquickjsCodeModeIntegration {
            identity: Vec::new(),
            profile: CodexIntegrationProfile::MinimalCore,
            nested_catalog: catalog,
            sidecar: "unused".into(),
        };
        let rebuilt = integration
            .rebuild_descriptor(plakous_tools::JobConfig::default(), registry, target)
            .expect("rebuild");
        assert_eq!(
            rebuilt.identity,
            neutral_minimal_descriptor(&original)
                .expect("neutral")
                .identity
        );
        assert!(
            rebuilt
                .nested_projection
                .by_exposed(&ToolName::new("apply_patch").expect("name"))
                .is_none()
        );
    }

    #[test]
    fn code_mode_wait_clamps_to_ten_minutes_and_keeps_its_default() {
        let integration = RquickjsCodeModeIntegration {
            identity: Vec::new(),
            profile: CodexIntegrationProfile::ExactReference,
            nested_catalog: Vec::new(),
            sidecar: "unused-test-sidecar".into(),
        };
        assert_eq!(
            integration
                .parse_wait(serde_json::json!({"cell_id": "1"}))
                .expect("default wait")
                .yield_time_ms,
            10_000
        );
        for (hint, expected) in [
            (0, 0),
            (10_001, 10_001),
            (600_000, 600_000),
            (900_000, 600_000),
        ] {
            assert_eq!(
                integration
                    .parse_wait(serde_json::json!({"cell_id": "1", "yield_time_ms": hint}))
                    .expect("whole wait hint")
                    .yield_time_ms,
                expected
            );
        }
        for hint in [
            serde_json::json!(-1),
            serde_json::json!(600_000.5),
            serde_json::json!("900000"),
        ] {
            assert!(
                integration
                    .parse_wait(serde_json::json!({"cell_id": "1", "yield_time_ms": hint}))
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn wait_delivers_buffered_terminal_without_resuming_engine() {
        let dir = tempfile::tempdir().expect("directory");
        let program = dir.path().join("worker");
        std::fs::write(&program, "#!/bin/sh\nIFS= read -r line\nexit 0\n").expect("program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
            .expect("permissions");
        let host = Arc::new(SidecarHost::spawn(&program).expect("host"));
        let (events_tx, _) = mpsc::channel(1);
        let (tx, rx) = mpsc::channel(2);
        tx.send(Ok(LocalCompositionResponse::Result {
            cell_id: "1".into(),
            items: vec![],
            error: None,
        }))
        .await
        .expect("buffered terminal");
        host.terminate()
            .await
            .expect("engine retired after buffering response");
        let session = RquickjsSession {
            host,
            next_cell: AtomicU64::new(2),
            stored: Arc::new(Mutex::new(HashMap::new())),
            events_tx,
            completions: Arc::new(Mutex::new(HashMap::new())),
            streams: Arc::new(Mutex::new(HashMap::from([(1, Arc::new(Mutex::new(rx)))]))),
            pump: Mutex::new(None),
        };
        let response = session
            .wait(LocalCompositionWaitRequest {
                cell_id: "1".into(),
                yield_time_ms: 0,
            })
            .await
            .expect("wait");
        assert!(matches!(
            response,
            LocalCompositionWaitOutcome::Live(LocalCompositionResponse::Result { .. })
        ));
        assert!(session.streams.lock().await.is_empty());
        let _ = session.shutdown().await;
    }

    #[tokio::test]
    async fn accepted_start_eof_and_malformed_frame_complete_without_turn_timeout() {
        for (name, body) in [
            ("eof", "IFS= read -r line\n"),
            ("malformed", "IFS= read -r line\nprintf 'not-json\\n'\n"),
        ] {
            let dir = std::env::temp_dir().join(format!("plak-js-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("directory");
            let program = dir.join("worker");
            let staging = dir.join("worker.pending");
            std::fs::write(&staging, format!("#!/bin/sh\n{body}")).expect("program");
            std::fs::rename(staging, &program).expect("publish program");
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
                .expect("permissions");
            let host = Arc::new(SidecarHost::spawn(&program).expect("host"));
            let bundle = session_bundle_from_engine(Arc::clone(&host));
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
            assert!(result.is_err(), "{name} must be host loss");
            assert!(matches!(
                bundle.session.shutdown().await,
                LocalCompositionShutdownResult::Joined { .. }
            ));
            std::fs::remove_dir_all(dir).expect("remove directory");
        }
    }

    #[tokio::test]
    async fn forced_termination_remains_lost_on_shutdown() {
        let dir = std::env::temp_dir().join(format!("plak-js-forced-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("directory");
        let program = dir.join("worker");
        std::fs::write(&program, "#!/bin/sh\nexec sleep 60\n").expect("program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
            .expect("permissions");
        let host = Arc::new(SidecarHost::spawn(&program).expect("host"));
        let bundle = session_bundle_from_engine(host);
        assert!(matches!(
            bundle.session.terminate("1".into()).await,
            LocalCompositionTerminateResult::Lost(_)
        ));
        assert!(matches!(
            bundle.session.shutdown().await,
            LocalCompositionShutdownResult::Lost(_)
        ));
        std::fs::remove_dir_all(dir).expect("remove directory");
    }
}
