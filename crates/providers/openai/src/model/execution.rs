#[cfg(feature = "native-auth")]
use super::{
    Arc, CapacityRule, CodexDispatchTarget, CountCertificateKind, CountObservation, Duration,
    LanguageModel, ModelFamilyId, ModelPreparationContext, ModelPreparationError,
    ModelPreparationErrorKind, OPENAI_RESPONSES_ADAPTER_REVISION, OpenAiLanguageModel,
    PreparedCacheUse, PreparedCallRecord, PreparedModelCall, PreparedModelProjection,
    PreparedOutputLimit, PreparedResponsesModelExecution, PreparedTokenProjection, ProtocolId,
    RESPONSES_CANONICAL_PROJECTION_REVISION, RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION,
    RESPONSES_FAMILY_ID, ResponsesArtifactReplayBridge, ResponsesCanonicalDialectData,
    RuntimeModelAuthorityError, RuntimeModelAuthorityOperation, Timestamp, ValidatedRequestPlan,
    canonical_local_tools, codex_artifact_replay_bridge, codex_dispatch_service,
    codex_reference_client_input_tokens, codex_request_authority, is_native_compaction_request,
    preparation_rejection, prepare_responses_attempt, start_prepared_responses_model_call,
    usage_limit, validate_codex_preparation_authority,
};
#[cfg(all(feature = "native-auth", test))]
use super::{ContextEvidenceClass, NonZeroU32, TEST_OUTPUT_HARD_CAP};

#[cfg(feature = "native-auth")]
impl LanguageModel for OpenAiLanguageModel {
    fn reachability_probe(&self) -> Option<plakous_model::ReachabilityProbe> {
        let origin = plakous_http::Origin::parse(
            self.current_target().ok()?.target.route().origin().as_str(),
        )
        .ok()?;
        Some(plakous_responses::bind_reachability_probe(
            self.client.clone(),
            origin,
            self.descriptor.route().clone(),
        ))
    }

    fn revalidate_runtime_authority(
        self: Arc<Self>,
        checked_at: Timestamp,
    ) -> RuntimeModelAuthorityOperation {
        Box::pin(async move {
            let target = self.current_target()?;
            if checked_at < self.admitted_from || checked_at >= self.admitted_until {
                return Err(RuntimeModelAuthorityError::Stale);
            }
            let checked = match self
                .receipt_renewal
                .ensure_credential_ready(&target.target)
                .await
            {
                Ok(()) => self.revalidate_target(&target, checked_at).await,
                Err(error) => Err(error),
            };
            let result = match checked {
                // The fence moved (Unavailable) or the access token entered
                // its refresh lead (Stale): renew the receipt.
                Err(
                    RuntimeModelAuthorityError::Stale | RuntimeModelAuthorityError::Unavailable,
                ) => self.renew(Arc::clone(&target), checked_at).await,
                result => result,
            };
            if result.is_err() {
                self.receipt_renewal
                    .ensure_credential_ready(&target.target)
                    .await
                    .or_else(|error| {
                        if error == RuntimeModelAuthorityError::Stale {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })?;
            }
            result
        })
    }

    fn wait_for_runtime_authority(
        self: Arc<Self>,
    ) -> plakous_model::RuntimeModelAuthorityWaitOperation {
        Box::pin(async move {
            let stale = self.current_target()?;
            let mut delay = Duration::from_millis(250);
            let mut previous = None;
            loop {
                if !Arc::ptr_eq(&self.current_target()?, &stale) {
                    return Ok(());
                }
                let observation = self.receipt_renewal.ready_selection(&stale.target).await;
                if previous.as_ref() != Some(&observation) {
                    delay = Duration::from_millis(250);
                }
                previous = Some(observation);
                match observation {
                    Ok(selection) => {
                        let _renewal = self.renewal.lock().await;
                        if !Arc::ptr_eq(&self.current_target()?, &stale) {
                            return Ok(());
                        }
                        self.renewal_floor.note(stale.target.clock().now());
                        let result = self
                            .receipt_renewal
                            .renew_selection(
                                &stale.target,
                                self.admitted_until,
                                &self.renewal_floor,
                                selection,
                            )
                            .await;
                        self.renewal_floor.clear();
                        match result {
                            Ok(target) => {
                                let service = codex_dispatch_service(
                                    &target,
                                    &self.client,
                                    &self.attempt_data,
                                )
                                .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
                                *self
                                    .target
                                    .lock()
                                    .map_err(|_| RuntimeModelAuthorityError::Unavailable)? =
                                    Arc::new(CodexDispatchTarget { target, service });
                                return Ok(());
                            }
                            Err(RuntimeModelAuthorityError::AwaitingCredential) => {}
                            Err(error) => return Err(error),
                        }
                    }
                    Err(
                        RuntimeModelAuthorityError::AwaitingCredential
                        | RuntimeModelAuthorityError::Stale,
                    ) => {}
                    Err(error) => return Err(error),
                }
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
            }
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Seal the Codex authority, replay artifact, wire measurement and prepared execution under the same immutable target."
    )]
    fn prepare_call(
        self: Arc<Self>,
        plan: ValidatedRequestPlan,
        context: ModelPreparationContext,
    ) -> Result<PreparedModelCall, ModelPreparationError> {
        let current = self.current_target()?;
        let target = &current.target;
        if let Some(projection) = plan.request().tool_projection() {
            let target = projection.target();
            if target.protocol() != self.descriptor.protocol().as_str()
                || target.integration_profile() != self.descriptor.profile().as_str()
                || target.family()
                    != self
                        .descriptor
                        .model_family()
                        .map_or("", ModelFamilyId::as_str)
                || target.requested_model() != self.descriptor.requested_model().as_str()
                || target.effective_model() != self.descriptor.requested_model().as_str()
            {
                return Err(preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "Codex compiled tool projection does not match the admitted exact target",
                ));
            }
        }
        target.validate_current_route_evidence().map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Codex route evidence is not current",
            )
        })?;
        target.validate_plan(&plan).map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidRequest,
                "Codex plan belongs to another exact target",
            )
        })?;
        self.descriptor
            .validate_plan_admission(plan.request(), &plan)?;
        target
            .validate_request_intent(plan.request())
            .map_err(|_| {
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidRequest,
                    "Codex request intent is not admitted",
                )
            })?;
        validate_codex_preparation_authority(target, context.authority(), context.prepared_at())?;
        let cache_affinity = context.cache_affinity().ok_or_else(|| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidRequest,
                "Codex OAuth requires branch cache affinity",
            )
        })?;
        let affinity_identity = cache_affinity.identity();

        let authority =
            codex_request_authority(target, &plan, Some(affinity_identity)).map_err(|()| {
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidRequest,
                    "Codex request authority could not be constructed",
                )
            })?;
        let replay_bridge = codex_artifact_replay_bridge(
            context.active_family_artifact(),
            plan.request(),
            authority.clone(),
            context.prepared_at(),
        )
        .map_err(Self::artifact_preparation_error)?;
        let prepared_attempt = prepare_responses_attempt(
            authority,
            plan.request(),
            replay_bridge
                .as_ref()
                .map(ResponsesArtifactReplayBridge::context),
            &self.attempt_data,
        )
        .map_err(|error| {
            preparation_rejection(
                error.preparation_error_kind(),
                format!("Codex Responses request could not be prepared: {error}"),
            )
        })?;
        let cache_use = match (replay_bridge.as_ref(), context.active_family_artifact()) {
            (Some(_), Some(active)) => PreparedCacheUse::ActiveFamilyArtifactAndRuntimeAffinity {
                artifact: active.integrity(),
                affinity: affinity_identity,
            },
            (None, _) => PreparedCacheUse::RuntimeAffinity {
                affinity: affinity_identity,
            },
            (Some(_), None) => {
                return Err(preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "Codex replay bridge omitted its active family artifact",
                ));
            }
        };

        let wire_digest = prepared_attempt.wire_digest();
        let wire_size = u64::try_from(prepared_attempt.wire_size()).map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::ResourceLimit,
                "Codex prepared wire size is not representable",
            )
        })?;
        let count_observation = self
            .descriptor
            .context_contract()
            .and_then(|contract| contract.measurements().first())
            .map(|certificate| {
                let measured_tokens = if matches!(
                    certificate.kind(),
                    CountCertificateKind::ReferenceClientEnvelopeUpperBound { .. }
                ) {
                    codex_reference_client_input_tokens(prepared_attempt.body().as_ref())
                        .ok_or_else(|| {
                            preparation_rejection(
                                ModelPreparationErrorKind::ResourceLimit,
                                "Codex complete-input upper bound is not representable",
                            )
                        })?
                } else {
                    wire_size
                };
                CountObservation::new(
                    certificate.certificate_id(),
                    certificate.revision(),
                    measured_tokens,
                )
                .map_err(|_| {
                    preparation_rejection(
                        ModelPreparationErrorKind::InvalidConfiguration,
                        "Codex test measurement fixture is invalid",
                    )
                })
            })
            .transpose()?;
        let token_projection =
            PreparedTokenProjection::new(wire_digest, wire_size, count_observation)?
                .with_input_parts(plakous_model::ContextInputParts::measure_wire(
                    prepared_attempt.body().as_ref(),
                ))
                .with_wire_input(
                    prepared_attempt.body().as_ref(),
                    Some(plakous_model::CONSERVATIVE_IMAGE_MAX_TOKENS),
                    &context,
                )?;
        let protocol_family = ProtocolId::new(RESPONSES_FAMILY_ID).map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Responses family identity is invalid",
            )
        })?;
        let output_limit = prepared_attempt
            .enforced_output_tokens()
            .map(|tokens| {
                PreparedOutputLimit::enforced_wire_maximum(
                    tokens,
                    "max_output_tokens",
                    wire_digest,
                    OPENAI_RESPONSES_ADAPTER_REVISION,
                )
            })
            .transpose()?;
        let output_limit = if self.descriptor.context_contract().is_some_and(|contract| {
            matches!(
                contract.capacity().rule(),
                CapacityRule::ProviderReservedInput { .. }
            )
        }) {
            None
        } else {
            output_limit
        };
        #[cfg(test)]
        let output_limit = match output_limit {
            Some(output_limit) => Some(output_limit),
            None => self
                .descriptor
                .context_contract()
                .filter(|contract| {
                    matches!(
                        contract.capacity().evidence_class(),
                        ContextEvidenceClass::ReleaseVerified
                    )
                })
                .map(|contract| {
                    PreparedOutputLimit::release_verified_hard_cap(
                        NonZeroU32::new(TEST_OUTPUT_HARD_CAP).ok_or_else(|| {
                            preparation_rejection(
                                ModelPreparationErrorKind::InvalidConfiguration,
                                "Codex test output cap is invalid",
                            )
                        })?,
                        contract.capacity().identity(),
                        &self.descriptor,
                        &context,
                        &protocol_family,
                        OPENAI_RESPONSES_ADAPTER_REVISION,
                        RESPONSES_CANONICAL_PROJECTION_REVISION,
                        RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION,
                    )
                    .map_err(ModelPreparationError::from)
                })
                .transpose()?,
        };
        let projection = PreparedModelProjection::new(
            &self.descriptor,
            &plan,
            &context,
            protocol_family,
            OPENAI_RESPONSES_ADAPTER_REVISION,
            RESPONSES_CANONICAL_PROJECTION_REVISION,
            RESPONSES_EXACT_WIRE_ACCOUNTING_REVISION,
            wire_digest,
            wire_size,
            token_projection,
            cache_use,
            output_limit,
        )?;
        let record =
            PreparedCallRecord::for_prepared(&self.descriptor, &plan, projection, &context)?;

        let canonical_tools = canonical_local_tools(plan.request());
        let expected_model = self
            .descriptor
            .reported_model()
            .unwrap_or_else(|| self.descriptor.requested_model())
            .clone();
        let canonical = {
            let canonical = ResponsesCanonicalDialectData::codex(expected_model, canonical_tools)
                .map_err(|_| {
                preparation_rejection(
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "Codex canonical projection facts are invalid",
                )
            })?;
            let canonical = if target.codex_responses_lite() {
                canonical.with_codex_lite_namespace()
            } else {
                canonical
            };
            if is_native_compaction_request(plan.request()) {
                let producing = prepared_attempt.authority().artifact_binding();
                canonical.with_native_compaction(plakous_model::CompactionReplayBinding::new(
                    producing.dialect().clone(),
                    producing.model_family().cloned(),
                    producing.model().clone(),
                    producing.account(),
                ))
            } else {
                canonical
            }
        };
        let artifact_binding = prepared_attempt.authority().artifact_binding().clone();
        let clock = target.clock();
        let content_filter_retry = !is_native_compaction_request(plan.request());
        let prepared_execution = PreparedResponsesModelExecution::new(
            prepared_attempt,
            canonical,
            plakous_responses::CanonicalIdSources::with_clock(Arc::clone(&clock)),
            plakous_responses::CanonicalIdSources::with_clock(Arc::clone(&clock)),
            replay_bridge
                .as_ref()
                .map(|bridge| bridge.hosted_programs().clone())
                .unwrap_or_default(),
            plan,
            artifact_binding,
            Arc::clone(&clock),
            self.descriptor.clone(),
            self.config.event_channel_capacity(),
            Some(usage_limit::model_error),
        )
        .map_err(|_| {
            preparation_rejection(
                ModelPreparationErrorKind::InvalidConfiguration,
                "Codex prepared model execution binding is invalid",
            )
        })?;
        let prepared_execution = prepared_execution.with_content_filter_retry(content_filter_retry);
        let config = self.config;
        Ok(PreparedModelCall::new(record, move |call_context| {
            Box::pin(async move {
                let service = Arc::clone(
                    &self
                        .current_target()
                        .map_err(ModelPreparationError::from)
                        .map_err(plakous_model::ModelError::from)?
                        .service,
                );
                let now = std::time::Instant::now();
                let deadline = now.checked_add(config.operation_timeout()).unwrap_or(now);
                start_prepared_responses_model_call(
                    prepared_execution,
                    service,
                    call_context,
                    deadline,
                )
                .await
            })
        }))
    }
}
