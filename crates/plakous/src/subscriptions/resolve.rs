//! Gated native model and provider resolution.

use super::{
    Arc, ConfiguredLanguageModel, MODEL_CATALOG_DEADLINE, ModelId, NativeModelResolutionError,
    NativeModelSelector, NativeProvider, NativeSubscriptionError, NativeSubscriptions,
    NativeToolMode, ResolvedNativeModel, ResolvedProvider, ResolvedProviderAuthorityBinding,
    RuntimeModelAuthority, SelectedProviderConfiguration, SessionLineage, SubscriptionRequestClass,
    SubscriptionRequestGate, SubscriptionRequestLimits, SubscriptionResolutionEvidence,
    SystemSubscriptionGateClock, TurnScopedSubscriptionRequestGate,
    apply_turn_scoped_provider_gate, catalog_operation_deadline, codex_native_http_config,
    codex_native_tool_mode, first_codex_compatible_model, guidance, map_claude_error,
    map_codex_error, map_xai_error, native_provider_route_selection, timestamp_now,
    xai_inference_client, xai_native_http_config,
};

/// The pinned native provider whose exact route `selected` names, with the
/// exact model selector for it.
fn gated_provider_selection(
    selected: &SelectedProviderConfiguration,
) -> Result<(NativeProvider, NativeModelSelector), NativeModelResolutionError> {
    let requested_model = selected.route().requested_model().clone();
    let provider = [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ]
    .into_iter()
    .find(|provider| {
        native_provider_route_selection(*provider, requested_model.clone())
            .is_ok_and(|route| &route == selected.route())
    })
    .ok_or_else(|| {
        NativeModelResolutionError::after_selection(
            requested_model.clone(),
            NativeSubscriptionError::ProviderConfigurationRejected,
        )
    })?;
    let selector = NativeModelSelector::Exact(requested_model.clone());
    Ok((provider, selector))
}

/// Compiles one resolved native model and its runtime authority into the
/// provider's configured model, the step both gated resolvers share.
fn configure_gated_provider(
    selected: &SelectedProviderConfiguration,
    provider: NativeProvider,
    resolved: ResolvedNativeModel,
    authority: RuntimeModelAuthority,
) -> Result<ResolvedProvider, NativeModelResolutionError> {
    let requested_model = selected.route().requested_model().clone();
    let native_tool_mode = resolved
        .evidence
        .codex_exact
        .as_ref()
        .and_then(codex_native_tool_mode);
    let configuration = match provider {
        NativeProvider::Codex => {
            let evidence = resolved.evidence.codex_exact.as_ref().ok_or_else(|| {
                NativeModelResolutionError::after_selection(
                    requested_model.clone(),
                    NativeSubscriptionError::Internal,
                )
            })?;
            plakous_openai::compile_codex_subscription_configuration(selected, &authority, evidence)
        }
        NativeProvider::Xai => {
            let evidence = resolved.evidence.xai_exact.as_ref().ok_or_else(|| {
                NativeModelResolutionError::after_selection(
                    requested_model.clone(),
                    NativeSubscriptionError::Internal,
                )
            })?;
            plakous_xai::compile_supergrok_subscription_configuration(
                selected, &authority, evidence,
            )
        }
        NativeProvider::Claude => {
            return configure_claude_provider(selected, resolved, authority, native_tool_mode);
        }
    }
    .map_err(|error| {
        let error = match error {
            plakous_openai::ResponsesProviderConfigurationError::PolicyMismatch(field) => {
                NativeSubscriptionError::ProviderConfigurationPolicyMismatch(field)
            }
            plakous_openai::ResponsesProviderConfigurationError::Identity => {
                NativeSubscriptionError::ProviderConfigurationIdentityRejected
            }
            plakous_openai::ResponsesProviderConfigurationError::ProviderConfiguration(error) => {
                NativeSubscriptionError::ProviderConfigurationCompileRejected(error)
            }
            _ => NativeSubscriptionError::ProviderConfigurationRejected,
        };
        NativeModelResolutionError::after_selection(requested_model, error)
    })?;
    let configured = ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        resolved.model(),
        configuration,
        authority,
    )
    .map_err(|_| {
        NativeModelResolutionError::after_selection(
            resolved.selected_model.clone(),
            NativeSubscriptionError::ConfiguredAggregateRejected,
        )
    })?;
    Ok(ResolvedProvider {
        model: resolved,
        configured,
        native_tool_mode,
        reresolution: None,
    })
}

/// The Claude OAuth branch of [`configure_gated_provider`]: Messages policy
/// compilation bound to the resolved descriptor and authority.
fn configure_claude_provider(
    selected: &SelectedProviderConfiguration,
    resolved: ResolvedNativeModel,
    authority: RuntimeModelAuthority,
    native_tool_mode: Option<NativeToolMode>,
) -> Result<ResolvedProvider, NativeModelResolutionError> {
    let requested_model = selected.route().requested_model().clone();
    let beta = plakous_anthropic::claude_oauth_beta_header(requested_model.as_str());
    let evidence = plakous_model::ProviderSemanticEvidence::new(
        1,
        b"plakous-anthropic/claude-oauth-semantic-evidence-v1".to_vec(),
    )
    .map_err(|_| {
        NativeModelResolutionError::after_selection(
            requested_model.clone(),
            NativeSubscriptionError::ProviderConfigurationIdentityRejected,
        )
    })?;
    let binding = ResolvedProviderAuthorityBinding::new(&resolved.descriptor, &authority, evidence)
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                requested_model.clone(),
                NativeSubscriptionError::ProviderConfigurationIdentityRejected,
            )
        })?;
    let configuration = plakous_anthropic::compile_messages_oauth_configuration(
        selected,
        resolved.descriptor.clone(),
        binding,
        plakous_anthropic::ANTHROPIC_API_VERSION,
        beta,
    )
    .map_err(|error| {
        let error = match error {
            plakous_anthropic::MessagesProviderConfigurationError::Identity => {
                NativeSubscriptionError::ProviderConfigurationIdentityRejected
            }
            plakous_anthropic::MessagesProviderConfigurationError::PolicyMismatch
            | plakous_anthropic::MessagesProviderConfigurationError::Provider(_) => {
                NativeSubscriptionError::ProviderConfigurationRejected
            }
        };
        NativeModelResolutionError::after_selection(requested_model.clone(), error)
    })?;
    let configured = ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        resolved.model(),
        configuration,
        authority,
    )
    .map_err(|_| {
        NativeModelResolutionError::after_selection(
            requested_model.clone(),
            NativeSubscriptionError::ConfiguredAggregateRejected,
        )
    })?;
    Ok(ResolvedProvider {
        model: resolved,
        configured,
        native_tool_mode,
        reresolution: None,
    })
}

impl NativeSubscriptions {
    /// Resolves one exact native model under a classified physical-request gate.
    ///
    /// This is the library **admit** seam for any caller that needs the
    /// fail-closed catalog + transport gate. It is not a general uncapped model
    /// client: the same gate is installed on auth refresh, catalog discovery, and
    /// every resolved inference transport.
    ///
    /// # Errors
    ///
    /// Returns [`NativeModelResolutionError`] when compatibility locks, online
    /// admission, catalog selection, or evidence binding fail. `selected_model`
    /// is set only after a concrete id was chosen.
    #[allow(
        clippy::too_many_lines,
        reason = "the provider branches keep exact selection and pre/post-selection evidence explicit"
    )]
    pub(crate) async fn resolve_gated_native_model(
        &self,
        provider: NativeProvider,
        model_selector: &NativeModelSelector,
        gate: Arc<dyn SubscriptionRequestGate>,
        lineage: SessionLineage,
    ) -> Result<ResolvedNativeModel, NativeModelResolutionError> {
        self.resolve_gated_native_model_phased(provider, model_selector, gate, false, lineage)
            .await
            .map(|(model, _)| model)
    }

    async fn resolve_gated_native_model_for_attach(
        &self,
        provider: NativeProvider,
        model_selector: &NativeModelSelector,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Result<(ResolvedNativeModel, RuntimeModelAuthority), NativeModelResolutionError> {
        let (model, compile_authority) = self
            .resolve_gated_native_model_phased(
                provider,
                model_selector,
                gate,
                true,
                SessionLineage::Root,
            )
            .await?;
        let compile_authority = compile_authority.ok_or_else(|| {
            NativeModelResolutionError::after_selection(
                model.selected_model.clone(),
                NativeSubscriptionError::Internal,
            )
        })?;
        Ok((model, compile_authority))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the provider branches keep exact selection and pre/post-selection evidence explicit"
    )]
    async fn resolve_gated_native_model_phased(
        &self,
        provider: NativeProvider,
        model_selector: &NativeModelSelector,
        gate: Arc<dyn SubscriptionRequestGate>,
        attach: bool,
        lineage: SessionLineage,
    ) -> Result<(ResolvedNativeModel, Option<RuntimeModelAuthority>), NativeModelResolutionError>
    {
        model_selector
            .validate_for_provider(provider)
            .map_err(|_| {
                NativeModelResolutionError::before_selection(
                    NativeSubscriptionError::ModelUnavailable,
                )
            })?;
        let _lock = self
            .acquire_compatibility_lock(provider)
            .await
            .map_err(NativeModelResolutionError::before_selection)?;
        match provider {
            NativeProvider::Claude => {
                let NativeModelSelector::Exact(selected_model) = model_selector else {
                    return Err(NativeModelResolutionError::before_selection(
                        NativeSubscriptionError::ModelUnavailable,
                    ));
                };
                let selected_model = selected_model.clone();
                let online = self
                    .claude_online_with_gate(Some(Arc::clone(&gate)), true)
                    .await
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(selected_model.clone(), error)
                    })?;
                let client = plakous_http::Client::new_http1(
                    plakous_http::Limits::for_streaming_inference(),
                )
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                let client = client
                    .pooled()
                    .with_observer(self.exchange_observer.clone());
                if !attach {
                    online.manager.refresh_if_needed().await.map_err(|error| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            map_claude_error(error),
                        )
                    })?;
                }
                let mut permit = gate
                    .acquire(SubscriptionRequestClass::Catalog)
                    .map_err(|_| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            NativeSubscriptionError::Internal,
                        )
                    })?;
                permit.authorize_dispatch().map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                let session_id = plakous_model::SessionId::generate(
                    &plakous_model::SystemClock,
                    &plakous_model::SystemIdGenerator,
                )
                .map_err(|_| {
                    NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::Internal,
                    )
                })?;
                let (model, authority, descriptor, retrieval) = online
                    .manager
                    .resolve_model_with_retrieval(
                        selected_model.as_str(),
                        client,
                        lineage,
                        session_id,
                    )
                    .await
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            map_claude_error(error),
                        )
                    })?;
                drop(permit);
                let catalog_expires_at = authority.valid_until().ok_or_else(|| {
                    NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::ResolutionEvidenceRejected,
                    )
                })?;
                if authority.observed_at() >= catalog_expires_at {
                    return Err(NativeModelResolutionError::after_selection(
                        selected_model.clone(),
                        NativeSubscriptionError::ResolutionEvidenceRejected,
                    ));
                }
                let evidence = SubscriptionResolutionEvidence {
                    account_fingerprint: authority.account_binding().to_string(),
                    credential_authority_binding: authority
                        .credential_authority_binding()
                        .to_string(),
                    credential_generation: authority.credential_generation(),
                    credential_selection_epoch: "primary".to_owned(),
                    route_fingerprint: plakous_anthropic::CLAUDE_OAUTH_ROUTE_ID.to_owned(),
                    descriptor_digest: descriptor.binding_digest().map_or_else(
                        |_| "claude-oauth-descriptor".to_owned(),
                        |digest| digest.to_string(),
                    ),
                    dialect: descriptor.protocol().as_str().to_owned(),
                    adapter_revision: plakous_anthropic::CLAUDE_OAUTH_ADAPTER_REVISION.to_owned(),
                    catalog_observed_at: authority.observed_at(),
                    catalog_expires_at,
                    runtime_admission_witness: None,
                    profile_revision: plakous_anthropic::CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID
                        .to_owned(),
                    codex_exact: None,
                    xai_exact: None,
                };
                let resolved = ResolvedNativeModel {
                    selected_model,
                    descriptor,
                    model,
                    evidence,
                    retrieval: Some(Arc::new(
                        crate::anthropic_retrieval::AnthropicRetrievalPort::new(retrieval),
                    )),
                    image_gen: None,
                };
                let compile_authority = attach.then_some(authority);
                Ok((resolved, compile_authority))
            }
            NativeProvider::Codex => {
                let exact_selection = model_selector.exact_model().cloned();
                let online = self
                    .codex_online_with_gate(Some(gate), true)
                    .await
                    .map_err(|error| match exact_selection.clone() {
                        Some(selected) => {
                            NativeModelResolutionError::after_selection(selected, error)
                        }
                        None => NativeModelResolutionError::before_selection(error),
                    })?;
                let resolver = plakous_openai::CodexModelResolver::new(
                    Arc::clone(&online.manager),
                    online.client.with_observer(self.exchange_observer.clone()),
                    codex_native_http_config().map_err(|error| {
                        exact_selection.clone().map_or_else(
                            || NativeModelResolutionError::before_selection(error),
                            |selected| NativeModelResolutionError::after_selection(selected, error),
                        )
                    })?,
                )
                .map_err(|error| {
                    let error = match error {
                        plakous_openai::NativeCodexAuthError::Validation => {
                            NativeSubscriptionError::ProviderClientConstructionRejected
                        }
                        other => map_codex_error(other),
                    };
                    exact_selection.clone().map_or_else(
                        || NativeModelResolutionError::before_selection(error),
                        |selected| NativeModelResolutionError::after_selection(selected, error),
                    )
                })?
                .with_inference_observer(self.exchange_observer.clone());
                let resolver = if self.catalog_retries {
                    resolver
                } else {
                    resolver.without_catalog_retries()
                };
                let catalog = resolver.refresh_catalog().await.map_err(|error| {
                    let error = match error {
                        plakous_openai::NativeCodexAuthError::Validation => {
                            NativeSubscriptionError::CatalogValidationRejected
                        }
                        other => map_codex_error(other),
                    };
                    exact_selection.clone().map_or_else(
                        || NativeModelResolutionError::before_selection(error),
                        |selected| NativeModelResolutionError::after_selection(selected, error),
                    )
                })?;
                let selected_model = match model_selector {
                    NativeModelSelector::Exact(model) => model.clone(),
                    NativeModelSelector::CodexFirstCompatible => {
                        let selected =
                            first_codex_compatible_model(catalog.entries().iter().map(|entry| {
                                (
                                    entry.model_id(),
                                    entry.supports_text(),
                                    entry.supports_tools(),
                                    entry.supports_reasoning(),
                                )
                            }))
                            .ok_or_else(|| {
                                NativeModelResolutionError::before_selection(
                                    NativeSubscriptionError::ModelUnavailable,
                                )
                            })?;
                        ModelId::new(selected.to_owned()).map_err(|_| {
                            NativeModelResolutionError::before_selection(
                                NativeSubscriptionError::ModelUnavailable,
                            )
                        })?
                    }
                };
                let (model, resolution, retrieval, images) = resolver
                    .resolve_with_retrieval(selected_model.as_str())
                    .map_err(|error| {
                        let error = match error {
                            plakous_openai::NativeCodexAuthError::Validation => {
                                NativeSubscriptionError::ResolutionEvidenceRejected
                            }
                            other => map_codex_error(other),
                        };
                        NativeModelResolutionError::after_selection(selected_model.clone(), error)
                    })?;
                let model = guidance::with_content_filter_guidance(
                    model,
                    resolution.content_filter_guidance(),
                );
                let descriptor = resolution
                    .validated_descriptor()
                    .map_err(|_| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            NativeSubscriptionError::ResolutionEvidenceRejected,
                        )
                    })?
                    .clone();
                let compile_authority = if attach {
                    Some(Self::attach_compile_authority_from_codex(
                        &selected_model,
                        &descriptor,
                        &resolution,
                    )?)
                } else {
                    None
                };
                let evidence = SubscriptionResolutionEvidence {
                    account_fingerprint: resolution.account_fingerprint().to_string(),
                    credential_authority_binding: resolution
                        .credential_authority_binding()
                        .to_string(),
                    credential_generation: resolution.selected_credential_generation(),
                    credential_selection_epoch: resolution.credential_selection_epoch().to_string(),
                    route_fingerprint: resolution.route_fingerprint().to_string(),
                    descriptor_digest: resolution.descriptor_digest().to_string(),
                    dialect: descriptor.protocol().as_str().to_owned(),
                    adapter_revision: resolution.adapter_revision().to_owned(),
                    catalog_observed_at: resolution.catalog_observed_at(),
                    catalog_expires_at: resolution.catalog_expires_at(),
                    runtime_admission_witness: None,
                    profile_revision: resolution.profile_revision().to_owned(),
                    codex_exact: Some(resolution),
                    xai_exact: None,
                };
                Ok((
                    ResolvedNativeModel {
                        selected_model,
                        descriptor,
                        model,
                        evidence,
                        retrieval: Some(Arc::new(
                            crate::responses_retrieval::CodexRetrievalPort::new(retrieval),
                        )),
                        image_gen: Some(Arc::new(crate::responses_images::CodexImagePort::new(
                            images,
                        ))),
                    },
                    compile_authority,
                ))
            }
            NativeProvider::Xai => {
                let NativeModelSelector::Exact(selected_model) = model_selector else {
                    return Err(NativeModelResolutionError::before_selection(
                        NativeSubscriptionError::ModelUnavailable,
                    ));
                };
                let selected_model = selected_model.clone();
                let online =
                    self.xai_online_with_gate(Some(gate), true)
                        .await
                        .map_err(|error| {
                            NativeModelResolutionError::after_selection(
                                selected_model.clone(),
                                error,
                            )
                        })?;
                let config = xai_native_http_config().map_err(|error| {
                    NativeModelResolutionError::after_selection(selected_model.clone(), error)
                })?;
                let inference_client = xai_inference_client(self.exchange_observer.clone())
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(selected_model.clone(), error)
                    })?;
                let (model, resolution, retrieval, images) = online
                    .manager
                    .resolve_model_with_retrieval(selected_model.as_str(), inference_client, config)
                    .await
                    .map_err(|error| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            map_xai_error(error),
                        )
                    })?;
                let descriptor = resolution
                    .validated_descriptor()
                    .map_err(|_| {
                        NativeModelResolutionError::after_selection(
                            selected_model.clone(),
                            NativeSubscriptionError::ResolutionEvidenceRejected,
                        )
                    })?
                    .clone();
                let compile_authority = if attach {
                    Some(Self::attach_compile_authority_from_xai(
                        &selected_model,
                        &descriptor,
                        &resolution,
                    )?)
                } else {
                    None
                };
                let evidence = SubscriptionResolutionEvidence {
                    account_fingerprint: resolution.account_fingerprint().to_string(),
                    credential_authority_binding: resolution
                        .credential_authority_binding()
                        .to_string(),
                    credential_generation: resolution.selected_credential_generation(),
                    credential_selection_epoch: resolution.credential_selection_epoch().to_owned(),
                    route_fingerprint: resolution.route_fingerprint().to_owned(),
                    descriptor_digest: resolution.descriptor_digest().to_string(),
                    dialect: descriptor.protocol().as_str().to_owned(),
                    adapter_revision: resolution.adapter_revision().to_owned(),
                    catalog_observed_at: resolution.catalog_observed_at(),
                    catalog_expires_at: resolution.catalog_expires_at(),
                    runtime_admission_witness: Some(resolution.runtime_admission_witness().clone()),
                    profile_revision: resolution.profile_revision().to_owned(),
                    codex_exact: None,
                    xai_exact: Some(resolution),
                };
                Ok((
                    ResolvedNativeModel {
                        selected_model,
                        descriptor,
                        model,
                        evidence,
                        retrieval: Some(Arc::new(
                            crate::responses_retrieval::XaiRetrievalPort::new(retrieval),
                        )),
                        image_gen: Some(Arc::new(crate::responses_images::XaiImagePort::new(
                            images,
                        ))),
                    },
                    compile_authority,
                ))
            }
        }
    }

    /// Resolves one complete explicit native provider configuration.
    ///
    /// The literal provider route, model, credential selection, behavior, and
    /// product policy are supplied by the caller. Resolution accepts only an
    /// exact pinned native route, obtains fresh authority from the same model
    /// object that will execute, and compiles through that provider's actual
    /// Responses policy table.
    ///
    /// # Errors
    ///
    /// Returns a typed native resolution failure when the selected route is not
    /// exact, live authority cannot be revalidated, catalog facts reject the
    /// requested behavior, or family compilation fails.
    pub async fn resolve_gated_provider(
        &self,
        selected: SelectedProviderConfiguration,
        gate: Arc<dyn SubscriptionRequestGate>,
        lineage: SessionLineage,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let (provider, selector) = gated_provider_selection(&selected)?;
        let resolved = self
            .resolve_gated_native_model(provider, &selector, gate, lineage)
            .await?;
        let requested_model = selected.route().requested_model().clone();
        let checked_at = timestamp_now().map_err(|error| {
            NativeModelResolutionError::after_selection(requested_model.clone(), error)
        })?;
        let authority = resolved
            .model()
            .revalidate_runtime_authority(checked_at)
            .await
            .map_err(|_| {
                NativeModelResolutionError::after_selection(
                    requested_model,
                    NativeSubscriptionError::ProviderAuthorityRevalidationRejected,
                )
            })?;
        configure_gated_provider(&selected, provider, resolved, authority)
    }

    /// Local-first attach variant of [`Self::resolve_gated_provider`].
    ///
    /// Skips attach-time `revalidate_runtime_authority` and Claude OAuth
    /// `refresh_if_needed`. Codex and Grok still perform authenticated catalog
    /// discovery. First dispatch still revalidates.
    pub async fn resolve_gated_provider_for_attach(
        &self,
        selected: SelectedProviderConfiguration,
        gate: Arc<dyn SubscriptionRequestGate>,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let (provider, selector) = gated_provider_selection(&selected)?;
        let (resolved, authority) = self
            .resolve_gated_native_model_for_attach(provider, &selector, gate)
            .await?;
        configure_gated_provider(&selected, provider, resolved, authority)
    }

    /// Resolves an ordinary native provider with one nonrenewable inference
    /// budget per admitted logical root.
    ///
    /// Authentication and catalog requests share the supplied process-level
    /// limits. Every root turn or session workflow receives an isolated copy of
    /// the inference allowance and retains it until the runtime publishes that
    /// root's terminal result.
    pub async fn resolve_turn_scoped_provider(
        &self,
        selected: SelectedProviderConfiguration,
        limits: SubscriptionRequestLimits,
        lineage: SessionLineage,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let turn_gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
            limits,
            Arc::new(SystemSubscriptionGateClock::default()),
        ));
        let gate: Arc<dyn SubscriptionRequestGate> = turn_gate.clone();
        let resolved = tokio::time::timeout(
            MODEL_CATALOG_DEADLINE,
            self.resolve_gated_provider(selected.clone(), gate, lineage),
        )
        .await
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                selected.route().requested_model().clone(),
                catalog_operation_deadline(turn_gate.as_ref()),
            )
        })??;
        let resolved = apply_turn_scoped_provider_gate(resolved, Arc::clone(&turn_gate))?;
        Ok(self.with_reresolution(resolved, selected, lineage, turn_gate))
    }

    /// Local-first attach variant of [`Self::resolve_turn_scoped_provider`].
    ///
    /// Same turn gate and signature as dispatch resolution. Attach skips Claude
    /// `refresh_if_needed` and runtime authority revalidation; Codex/Grok catalog
    /// HTTP may still run. First dispatch still revalidates.
    pub async fn resolve_turn_scoped_provider_for_attach(
        &self,
        selected: SelectedProviderConfiguration,
        limits: SubscriptionRequestLimits,
    ) -> Result<ResolvedProvider, NativeModelResolutionError> {
        let turn_gate = Arc::new(TurnScopedSubscriptionRequestGate::new(
            limits,
            Arc::new(SystemSubscriptionGateClock::default()),
        ));
        let gate: Arc<dyn SubscriptionRequestGate> = turn_gate.clone();
        let resolved = tokio::time::timeout(
            MODEL_CATALOG_DEADLINE,
            self.resolve_gated_provider_for_attach(selected.clone(), gate),
        )
        .await
        .map_err(|_| {
            NativeModelResolutionError::after_selection(
                selected.route().requested_model().clone(),
                catalog_operation_deadline(turn_gate.as_ref()),
            )
        })??;
        let resolved = apply_turn_scoped_provider_gate(resolved, Arc::clone(&turn_gate))?;
        Ok(self.with_reresolution(resolved, selected, SessionLineage::Root, turn_gate))
    }
}
