use super::{
    Arc, AtomicBool, AtomicU64, BoundaryController, ContentBlock, CountObservation, CrashBoundary,
    Digest, DigestDomain, EvidenceKind, FoldDisposition, HarnessResult, LanguageModel,
    LoopbackEffect, LoopbackModel, LoopbackScenario, ModelError, ModelErrorKind, ModelEvent,
    ModelPreparationContext, ModelPreparationError, ModelPreparationErrorKind, ModelStream,
    NamespaceId, NonZeroU32, Ordering, PreparedCacheUse, PreparedCallRecord, PreparedModelCall,
    PreparedModelProjection, PreparedOutputLimit, PreparedTokenProjection, ResolvedModelDescriptor,
    RuntimeModelAuthority, RuntimeModelAuthorityError, RuntimeModelAuthorityOperation,
    ScriptFaultPhase, StreamExt, Timestamp, ToolContext, ToolExecutor, ToolFuture, ToolInvocation,
    ToolOutcome, ToolRuntimeError, ToolRuntimeErrorKind, ValidatedRequestPlan, descriptor,
    fixture_time, io, loopback_response, model_evidence_error, native_summary_response,
    scripted_error, scripted_stall_is_deaf, scripted_stream, seal_fixture_artifact, stall, stream,
};

impl LoopbackModel {
    fn new(
        controller: BoundaryController,
        account: &[u8],
        compaction_summary: bool,
    ) -> HarnessResult<Self> {
        Self::with_descriptor(
            controller,
            account,
            compaction_summary,
            descriptor(account)?,
        )
    }

    pub(super) fn with_descriptor(
        controller: BoundaryController,
        account: &[u8],
        compaction_summary: bool,
        descriptor: ResolvedModelDescriptor,
    ) -> HarnessResult<Self> {
        let authority = RuntimeModelAuthority::new(
            &descriptor,
            Digest::sha256(DigestDomain::ModelBinding, 1, &[account])?,
            Digest::sha256(
                DigestDomain::ModelBinding,
                1,
                &[b"offline-loopback-credential-authority"],
            )?,
            1,
            fixture_time()?,
            None,
        )?;
        Ok(Self {
            credential_prepared: AtomicBool::new(false),
            credential_ready: AtomicBool::new(false),
            descriptor,
            authority,
            controller,
            next_round: Arc::new(AtomicU64::new(1)),
            scenario: if compaction_summary {
                LoopbackScenario::Compaction
            } else {
                LoopbackScenario::Effect
            },
            seal_final_artifact: false,
            identity_namespace: account.iter().fold(1_u128, |value, byte| {
                value.wrapping_mul(257).wrapping_add(u128::from(*byte))
            }) & 0x0000_0000_0000_0000_0000_ffff_ffff_ffff,
        })
    }

    /// Replaces this model's private round cursor with a shared one.
    ///
    /// A script's position belongs to the session, not to the binding. The host
    /// may build more than one runtime for one session, and each would
    /// otherwise restart the script at step one.
    pub(super) fn with_shared_round(mut self, cursor: Arc<AtomicU64>) -> Self {
        self.next_round = cursor;
        self
    }

    pub(super) fn topology(controller: BoundaryController, account: &[u8]) -> HarnessResult<Self> {
        let mut model = Self::new(controller, account, false)?;
        model.scenario = LoopbackScenario::Topology;
        Ok(model)
    }

    pub(super) fn exited_child_continuation(
        controller: BoundaryController,
        account: &[u8],
    ) -> HarnessResult<Self> {
        let mut model = Self::new(controller, account, false)?;
        model.scenario = LoopbackScenario::ExitedChildContinuation;
        Ok(model)
    }
}

impl LanguageModel for LoopbackModel {
    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            if matches!(&self.scenario, LoopbackScenario::Scripted(script) if script.credential_ready.is_some())
                && self.credential_prepared.load(Ordering::Acquire)
                && !self.credential_ready.load(Ordering::Acquire)
            {
                return Err(RuntimeModelAuthorityError::AwaitingCredential);
            }
            self.authority.validate(&self.descriptor, checked_at)?;
            Ok(self.authority.clone())
        })
    }

    fn wait_for_runtime_authority(
        self: Arc<Self>,
    ) -> plakous::model::RuntimeModelAuthorityWaitOperation {
        Box::pin(async move {
            let LoopbackScenario::Scripted(script) = &self.scenario else {
                return Err(RuntimeModelAuthorityError::Unavailable);
            };
            let gate = script
                .credential_ready
                .as_ref()
                .ok_or(RuntimeModelAuthorityError::Unavailable)?;
            crate::gate::wait_gate(gate, &plakous::model::CancellationToken::new(), false)
                .await
                .map_err(|_| RuntimeModelAuthorityError::Unavailable)?;
            self.credential_ready.store(true, Ordering::Release);
            Ok(())
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the loopback preparation keeps one exact inspected projection and dispatch capability together"
    )]
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        self.descriptor.validate_plan_binding(&plan)?;
        context
            .authority()
            .validate(&self.descriptor, context.prepared_at())?;
        if !self.authority.same_selected_authority(context.authority()) {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        // This repository fake deliberately gives the independently admitted
        // summary request a small projection while measuring the ordinary
        // source request in full. The private compaction instruction is the
        // only stable discriminator in this no-tools loopback fixture; drift
        // should make the crash precondition fail visibly. The driver emits it
        // as the trailing user item of the fold's input — never as a prefix
        // instruction — so the input items are where it has to be found.
        let native_namespace = NamespaceId::new("plakous.compaction").map_err(|error| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                error.to_string(),
            )
        })?;
        let is_native_compaction = plan
            .request()
            .stable_prefix()
            .native()
            .entries()
            .get(&native_namespace)
            .is_some_and(|value| value == "native_responses_trigger_v1");
        let is_summary_request = is_native_compaction
            || plan.request().input().items().iter().any(|item| {
                item.blocks().iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text(text)
                            if text.text() == plakous::model::SEMANTIC_COMPACTION_INSTRUCTION
                    )
                })
            });
        let default_output_tokens = NonZeroU32::new(1_024).ok_or_else(|| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "loopback output maximum is invalid",
            )
        })?;
        let output_tokens = plan
            .request()
            .limits()
            .max_output_tokens()
            .unwrap_or(default_output_tokens);
        if let LoopbackScenario::Scripted(script) | LoopbackScenario::ScriptedSummary(script, _) =
            &self.scenario
            && is_summary_request
            && script.fold == FoldDisposition::Refuse
        {
            // A refused fold is not a turn failure: the compaction ladder is
            // expected to fall to its offline placeholder rung instead.
            return Err(ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "scripted fold refusal",
            ));
        }
        let content_sensitive = match &self.scenario {
            // Scripted folds measure source and successor on this model. A flat
            // counter would publish a fold that did not shrink. The summary
            // request itself stays on the identity projection below.
            LoopbackScenario::Compaction
            | LoopbackScenario::ContentSensitiveEffect
            | LoopbackScenario::ScriptedSummary(_, _) => true,
            // Turn scripts opt in. Otherwise a host pass compares identity-sized
            // counts and no placeholder successor can shrink.
            LoopbackScenario::Scripted(script) => script.content_sensitive_projection,
            _ => false,
        };
        let wire = if content_sensitive && !is_summary_request {
            serde_json::to_vec(&(
                plan.request(),
                plan.decisions(),
                ("max_output_tokens", output_tokens),
            ))
        } else {
            serde_json::to_vec(&(
                plan.request().model_input_identity(),
                plan.decisions(),
                ("max_output_tokens", output_tokens),
            ))
        }
        .map_err(|_| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::Projection,
                "loopback request projection serialization failed",
            )
        })?;
        let wire_size = u64::try_from(wire.len()).map_err(|_| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::ResourceLimit,
                "loopback request projection exceeds addressable size",
            )
        })?;
        let wire_digest = Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"plakous-offline-crash-harness/loopback-wire-v1", &wire],
        )
        .map_err(plakous::model::PreparedCallError::from)?;
        // Integrity digests are transport-only. Their decimal widths varied with
        // fresh ids, making independent complete/crash runs clip differently.
        // A fixed maximum-width encoding preserves this fake's byte upper bound.
        let counted_tokens = if content_sensitive
            && !is_summary_request
            && !matches!(self.scenario, LoopbackScenario::Compaction)
        {
            fn normalize_digests(value: &mut serde_json::Value) {
                match value {
                    serde_json::Value::Object(object) => {
                        if object.contains_key("algorithm")
                            && object.contains_key("domain")
                            && object.contains_key("version")
                            && object
                                .get("bytes")
                                .and_then(serde_json::Value::as_array)
                                .is_some_and(|bytes| bytes.len() == 32)
                        {
                            object.insert("bytes".to_owned(), serde_json::json!(vec![255; 32]));
                        } else {
                            for value in object.values_mut() {
                                normalize_digests(value);
                            }
                        }
                    }
                    serde_json::Value::Array(values) => {
                        for value in values {
                            normalize_digests(value);
                        }
                    }
                    _ => {}
                }
            }
            let mut counted: serde_json::Value = serde_json::from_slice(&wire).map_err(|_| {
                ModelPreparationError::rejected(
                    ModelPreparationErrorKind::Projection,
                    "loopback input counting failed",
                )
            })?;
            normalize_digests(&mut counted);
            let counted = serde_json::to_vec(&counted).map_err(|_| {
                ModelPreparationError::rejected(
                    ModelPreparationErrorKind::Projection,
                    "loopback input counting failed",
                )
            })?;
            u64::try_from(counted.len()).map_err(|_| {
                ModelPreparationError::rejected(
                    ModelPreparationErrorKind::ResourceLimit,
                    "loopback input count is too large",
                )
            })?
        } else {
            wire_size
        };
        let token_projection = PreparedTokenProjection::new(
            wire_digest,
            wire_size,
            Some(
                CountObservation::new("offline-loopback-byte-counter", "exact-v1", counted_tokens)
                    .map_err(|_| {
                        ModelPreparationError::rejected(
                            ModelPreparationErrorKind::InvalidConfiguration,
                            "loopback token observation is invalid",
                        )
                    })?,
            ),
        )?
        // This fake counts logical input bytes; chrome uses that same scale.
        .with_input_parts(plakous::model::ContextInputParts::measure_wire(
            format!(
                r#"{{"input":"{}"}}"#,
                "x".repeat(
                    usize::try_from(counted_tokens)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(4)
                )
            )
            .as_bytes(),
        ));
        let output_limit = PreparedOutputLimit::enforced_wire_maximum(
            output_tokens,
            "max_output_tokens",
            wire_digest,
            "offline-loopback-lowering-v1",
        )?;
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            self.descriptor.protocol().clone(),
            "offline-loopback-lowering-v1",
            "offline-loopback-projection-v1",
            "offline-loopback-accounting-v1",
            wire_digest,
            wire_size,
            token_projection,
            PreparedCacheUse::None,
            Some(output_limit),
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;
        // This request's own complete prompt, which is what a provider
        // reports as `input_tokens` for the operation it belongs to.
        let prepared_input_tokens = counted_tokens;
        let cache_affinity = record.projection().cache_affinity();
        self.credential_prepared.store(true, Ordering::Release);
        Ok(PreparedModelCall::new(record, move |call_context| {
            let cancellation = call_context.cancellation().clone();
            let round = self.next_round.fetch_add(1, Ordering::AcqRel);
            let dispatch_boundary = match round {
                1 => CrashBoundary::ProviderDispatchOne,
                _ => CrashBoundary::ProviderDispatchTwo,
            };
            Box::pin(async move {
                if is_summary_request {
                    self.controller
                        .evidence
                        .append_summary_dispatch(round, dispatch_boundary, wire_digest.to_string())
                        .map_err(model_evidence_error)?;
                }
                let pause = self
                    .controller
                    .announce_operation(
                        dispatch_boundary,
                        EvidenceKind::ProviderDispatched,
                        Some(round),
                        Some(wire_digest.to_string()),
                    )
                    .map_err(model_evidence_error)?;
                self.controller
                    .pause_at_selected_boundary_until_cancelled(pause, &cancellation)
                    .await;
                if let LoopbackScenario::Scripted(script)
                | LoopbackScenario::ScriptedSummary(script, _) = &self.scenario
                    && is_summary_request
                {
                    match script.fold {
                        FoldDisposition::Fail => {
                            return Err(ModelError::new(
                                ModelErrorKind::RateLimited,
                                "scripted fold failure",
                            ));
                        }
                        FoldDisposition::Stall(milliseconds)
                            if stall(
                                milliseconds,
                                &cancellation,
                                script.stall_ignores_cancellation,
                            )
                            .await =>
                        {
                            return Err(ModelError::new(
                                ModelErrorKind::Cancelled,
                                "the scripted fold stall was cancelled",
                            ));
                        }
                        _ => {}
                    }
                }
                if let LoopbackScenario::TerminalFailure(kind) = self.scenario {
                    return Err(ModelError::new(kind, "injected terminal model failure"));
                }
                if let LoopbackScenario::Scripted(script) = &self.scenario
                    && let Some(gate) = script.step(round).and_then(|step| step.gate.as_ref())
                {
                    crate::gate::wait_gate(gate, &cancellation, script.stall_ignores_cancellation)
                        .await?;
                }
                if let LoopbackScenario::Scripted(script) = &self.scenario
                    && let Some(step) = script.step(round)
                    && stall(
                        step.stall_ms,
                        &cancellation,
                        script.stall_ignores_cancellation,
                    )
                    .await
                {
                    return Err(ModelError::new(
                        ModelErrorKind::Cancelled,
                        "the scripted stall was cancelled",
                    ));
                }
                let scripted_fault = match &self.scenario {
                    LoopbackScenario::Scripted(script) => {
                        script.step(round).and_then(|step| step.fault.clone())
                    }
                    _ => None,
                };
                if let Some(fault) = &scripted_fault {
                    match fault.at {
                        ScriptFaultPhase::PreStream => {
                            return Err(scripted_error(fault, &self.descriptor));
                        }
                        ScriptFaultPhase::Stalled => {
                            if stall(
                                fault.stall_ms,
                                &cancellation,
                                scripted_stall_is_deaf(&self.scenario),
                            )
                            .await
                            {
                                return Err(ModelError::new(
                                    ModelErrorKind::Cancelled,
                                    "the scripted stall was cancelled",
                                ));
                            }
                            return Err(scripted_error(fault, &self.descriptor));
                        }
                        ScriptFaultPhase::InBand => {}
                    }
                }
                let response = if is_native_compaction {
                    native_summary_response(
                        round,
                        &plan,
                        self.identity_namespace,
                        &self.descriptor,
                        self.authority.account_binding(),
                    )?
                } else {
                    loopback_response(
                        round,
                        &plan,
                        &self.scenario,
                        self.identity_namespace,
                        prepared_input_tokens,
                    )?
                };
                let response = if self.seal_final_artifact {
                    seal_fixture_artifact(&plan, &response, cache_affinity)?
                } else {
                    response
                };
                let controller = self.controller.clone();
                let stream_cancellation = cancellation.clone();
                let outcome_boundary = match round {
                    1 => CrashBoundary::ProviderOutcomeOne,
                    _ => CrashBoundary::ProviderOutcomeTwo,
                };
                let events: ModelStream = Box::pin(stream::once(async move {
                    let pause = controller
                        .announce(outcome_boundary, EvidenceKind::ProviderOutcome, Some(round))
                        .map_err(model_evidence_error)?;
                    controller
                        .pause_at_selected_boundary_until_cancelled(pause, &cancellation)
                        .await;
                    Ok(ModelEvent::ResponseCompleted(response))
                }));
                if let Some(fault) = scripted_fault
                    && fault.at == ScriptFaultPhase::InBand
                {
                    // An in-band error frame: the stream commits deltas the
                    // face has already shown, and only then reports the fault.
                    let deltas = (0..fault.after_events)
                        .map(|ordinal| {
                            Ok(ModelEvent::TextDelta(plakous::model::TextDelta::new(
                                plakous::model::OutputLocation::new(0, 0),
                                format!("scripted delta {ordinal}\n").into_bytes(),
                            )))
                        })
                        .collect::<Vec<Result<ModelEvent, ModelError>>>();
                    let failure = scripted_error(&fault, &self.descriptor);
                    return Ok(Box::pin(
                        stream::iter(deltas).chain(stream::once(async move { Err(failure) })),
                    ) as ModelStream);
                }
                if let LoopbackScenario::Scripted(script) = &self.scenario
                    && let Some(step) = script.step(round)
                    && !step.stream.is_empty()
                {
                    let leading = scripted_stream(
                        step,
                        stream_cancellation,
                        script.stall_ignores_cancellation,
                    )?;
                    return Ok(Box::pin(leading.chain(events)) as ModelStream);
                }
                let prefix = match &self.scenario {
                    LoopbackScenario::Text { prefix, .. } => Some((*prefix).to_owned()),
                    LoopbackScenario::Scripted(script) => script
                        .step(round)
                        .and_then(|step| step.prefix.clone())
                        .filter(|prefix| !prefix.is_empty()),
                    _ => None,
                };
                if let Some(prefix) = prefix {
                    let prefix_event = ModelEvent::TextDelta(plakous::model::TextDelta::new(
                        plakous::model::OutputLocation::new(0, 0),
                        prefix.into_bytes(),
                    ));
                    return Ok(
                        Box::pin(stream::once(async move { Ok(prefix_event) }).chain(events))
                            as ModelStream,
                    );
                }
                Ok(events)
            })
        }))
    }
}

impl ToolExecutor for LoopbackEffect {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        let dispatch = self.controller.announce_operation(
            CrashBoundary::EffectDispatch,
            EvidenceKind::EffectDispatched,
            None,
            Some(invocation.call_id().as_uuid().to_string()),
        );
        Box::pin(async move {
            let pause = dispatch.map_err(tool_evidence_error)?;
            let cancellation = context.cancellation().clone();
            self.controller
                .pause_at_selected_boundary_until_cancelled(pause, &cancellation)
                .await;
            if pause && cancellation.is_cancelled() {
                // A running effect that observes the interrupt joins it with its
                // own classification (spec 4.3). The runtime records that as a
                // definite cancelled terminal, never as an unknown outcome; an
                // ungated crash cut above still parks until the child is killed.
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Cancelled,
                    "loopback effect joined the cancel",
                ));
            }
            // A tool *error* is a failed observation the model reads and
            // answers, not a runtime failure that abandons the turn.
            let outcome = if self.fail {
                ToolOutcome::failure_text("loopback effect refused")?
            } else if let Some(bytes) = self.body_bytes {
                ToolOutcome::success_text("x".repeat(bytes))?
            } else {
                ToolOutcome::success_text("loopback effect committed")?
            };
            let pause = self
                .controller
                .announce(
                    CrashBoundary::EffectOutcome,
                    EvidenceKind::EffectOutcome,
                    None,
                )
                .map_err(tool_evidence_error)?;
            self.controller.pause_at_selected_boundary(pause).await;
            Ok(outcome)
        })
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "map_err transfers sole ownership of the source I/O error"
)]
fn tool_evidence_error(error: io::Error) -> ToolRuntimeError {
    ToolRuntimeError::new(
        ToolRuntimeErrorKind::Execution,
        format!("loopback evidence persistence failed: {error}"),
    )
}
