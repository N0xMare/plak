//! The shared catalog inputs of a resolver and its models.

use super::catalog::{contract_drift, raw_contract_drift};
use super::{
    ACCESS_REFRESH_SKEW, Arc, BTreeMap, BTreeSet, CATALOG_TTL_SECONDS,
    CODEX_NATIVE_COMPACTION_NAMESPACE, Capability, CapabilityConstraints, CapabilitySet,
    CatalogDocument, CatalogInputModality, CatalogModelWire, CatalogRenewal, CatalogShellType,
    Client, Clock, CodexCatalogApiSupport, CodexCatalogCapacityFacts, CodexCatalogEntry,
    CodexCatalogToolMode, CodexModelCatalog, CodexReasoningEffortSet, CodexResponsesWireMode,
    CodexSubscriptionCredentialManager, CredentialStatus, Deserialize, Duration, FactProvenance,
    FactSource, FeatureSupport, HeaderName, Instant, MAX_CATALOG_MODELS, MAX_MODEL_ID_BYTES,
    Method, ModelAdapterProfileId, ModelFamilyId, ModelId, NamespaceId, NativeCodexAuthError,
    OPENAI_CODEX_COMPATIBILITY_PROFILE_ID, OPENAI_CODEX_MODELS_PATH, OpenAiRoute,
    PROFILE_EXPIRES_AT, RESPONSES_FAMILY_ID, ReasoningEffort, RefreshForce, ResolvedCatalog,
    ResolvedModelDescriptor, RouteId, RuntimeModelAuthority, StatusCode, Support, Timestamp,
    WitnessedDescriptor, bearer_headers, catalog_content_filter_guidance, catalog_enum_fingerprint,
    catalog_service_tiers, credential_selection_epoch, executable_codex_catalog_model,
    execute_catalog_bounded, parse_catalog_json, parse_stored,
    reference_client_envelope_context_contract, release_enrolled_codex_tool_mode,
    require_json_media_type, require_status, resolution_route_fingerprint, sensitive_header_value,
    validate_identifier,
};

/// The exact catalog inputs a resolver and its models share, so a running
/// model renews its receipt through the same resolution that admitted it.
#[derive(Clone)]
pub(crate) struct CodexCatalogSource {
    pub(super) catalog_retries: bool,
    pub(super) manager: Arc<CodexSubscriptionCredentialManager>,
    pub(super) route: OpenAiRoute,
    pub(super) client: Client,
    pub(super) clock: Arc<dyn Clock>,
}

impl CodexCatalogSource {
    #[allow(
        clippy::too_many_lines,
        reason = "catalog admission keeps exact lease, wire, enum, evidence, and witness checks together"
    )]
    pub(super) async fn fetch_catalog(
        &self,
        renewal: Option<&CatalogRenewal<'_>>,
    ) -> Result<ResolvedCatalog, NativeCodexAuthError> {
        let _lifecycle = self.manager.lifecycle.lock().await;
        // A renewal already ran its own detached refresh (see
        // `refresh_detached`); this fetch only reads the result.
        if renewal.is_none() {
            self.manager
                .refresh_locked_with_force(
                    Instant::now() + Duration::from_secs(30),
                    None,
                    RefreshForce::IfDue,
                    None,
                )
                .await
                .map_err(|error| match error {
                    NativeCodexAuthError::Validation => NativeCodexAuthError::CatalogRefresh,
                    other => other,
                })?;
        }
        let fence = self.manager.load_fence().await?;
        if fence.status() != CredentialStatus::Ready
            || fence.account_binding() != &self.manager.selected_account()?
        {
            return Err(NativeCodexAuthError::Unavailable);
        }
        let lease = self.manager.acquire_ready_lease(fence.clone()).await?;
        let credential = parse_stored(lease.payload())?;
        if credential.access_expires_at
            <= self
                .clock
                .now()
                .seconds()
                .saturating_add(ACCESS_REFRESH_SKEW)
            || credential.account_id != fence.account_binding().as_str()
        {
            return Err(NativeCodexAuthError::Unavailable);
        }
        let selection_epoch = credential_selection_epoch(
            self.manager.route_account_digest(),
            self.manager.authority_binding,
            &credential.selection_epoch,
        )?;
        if let Some(renewal) = renewal {
            renewal.check(fence.account_binding(), selection_epoch)?;
        }
        let requested_at = self.clock.now();
        // The backend versions the catalog projection from the exact native
        // client release. A fabricated future version can hide otherwise
        // admitted models, so this must remain bound to the reviewed profile.
        let path = format!(
            "{OPENAI_CODEX_MODELS_PATH}?client_version={}",
            crate::OPENAI_CODEX_REFERENCE_CLIENT_VERSION
        );
        let mut headers =
            bearer_headers(&credential.access_token, false).map_err(|error| match error {
                NativeCodexAuthError::Validation => {
                    NativeCodexAuthError::CatalogAuthorizationHeader
                }
                other => other,
            })?;
        let account = sensitive_header_value(credential.account_id.as_bytes().to_vec())
            .map_err(|_| NativeCodexAuthError::CatalogAccountHeader)?;
        headers.insert(HeaderName::from_static("chatgpt-account-id"), account);
        let reply = execute_catalog_bounded(
            &self.client,
            &self.manager.endpoints.chatgpt_origin,
            Method::GET,
            &path,
            headers,
            Vec::new(),
            if renewal.is_some() || !self.catalog_retries {
                plakous_exchange::lookup::LookupBudget::single_attempt(
                    Instant::now() + Duration::from_secs(30),
                )
            } else {
                plakous_exchange::lookup::LookupBudget::from_deadline(
                    Instant::now() + Duration::from_secs(30),
                )
            },
            self.manager.request_gate.clone(),
        )
        .await;
        drop(credential);
        drop(lease);
        let reply = reply?;
        require_status(&reply, StatusCode::OK)?;
        require_json_media_type(&reply).map_err(|error| match error {
            NativeCodexAuthError::Validation => NativeCodexAuthError::CatalogMediaType,
            other => other,
        })?;
        let document: CatalogDocument =
            parse_catalog_json(&reply.body).map_err(|error| match error {
                NativeCodexAuthError::Validation => NativeCodexAuthError::CatalogDocument,
                other => other,
            })?;
        if document.models.is_empty() || document.models.len() > MAX_CATALOG_MODELS {
            return Err(NativeCodexAuthError::Catalog);
        }
        // Catalog evidence lasts a fixed window from its observation, as
        // SuperGrok's does (spec 01 §7.4), independent of the access token:
        // runtime authority is separately bounded by the access expiry, and a
        // running model renews its credential receipt inside this window.
        let observed = renewal.map_or_else(
            || Ok(self.clock.now()),
            |renewal| renewal.observed_at(requested_at),
        )?;
        let expiry_seconds = observed
            .seconds()
            .saturating_add(CATALOG_TTL_SECONDS)
            .min(PROFILE_EXPIRES_AT);
        let expires =
            Timestamp::new(expiry_seconds, 0).map_err(|_| NativeCodexAuthError::Catalog)?;
        if expires <= self.clock.now() {
            return Err(NativeCodexAuthError::Catalog);
        }
        let provenance = FactProvenance::new(FactSource::RouteEvidence, observed, Some(expires));
        let release_provenance = FactProvenance::new(FactSource::BuiltIn, observed, None);
        let account_fingerprint = self
            .route
            .account_binding()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        let credential_authority_binding = self
            .route
            .credential_authority_binding()
            .map_err(|_| NativeCodexAuthError::Catalog)?;
        let context_authority_binding = RuntimeModelAuthority::authority_class_for(
            account_fingerprint,
            credential_authority_binding,
        )
        .map_err(|_| NativeCodexAuthError::Catalog)?;
        #[derive(Deserialize)]
        struct CatalogSlug {
            slug: String,
        }
        let mut slug_counts = BTreeMap::<String, usize>::new();
        for raw in &document.models {
            if let Ok(row) = parse_catalog_json::<CatalogSlug>(raw.get().as_bytes()) {
                *slug_counts.entry(row.slug).or_default() += 1;
            }
        }
        let ambiguous_slugs = slug_counts
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(slug, _)| slug.clone())
            .collect::<BTreeSet<_>>();
        let mut entries = Vec::new();
        let mut descriptors = BTreeMap::new();
        let mut drifted_models = BTreeMap::new();
        let mut drifted_visibility = BTreeMap::new();
        for raw in document.models {
            let Ok(model) = parse_catalog_json::<CatalogModelWire>(raw.get().as_bytes()) else {
                if let Ok(row) = parse_catalog_json::<CatalogSlug>(raw.get().as_bytes())
                    && executable_codex_catalog_model(&row.slug)
                    && !ambiguous_slugs.contains(&row.slug)
                    && let Some(reason) = raw_contract_drift(raw.get(), &row.slug)
                {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw.get())
                        && let Some(value) = value.get("visibility")
                        && let Ok(visibility) = serde_json::from_value::<
                            super::catalog::CatalogVisibility,
                        >(value.clone())
                        && let Some(visibility) = visibility.recognized()
                    {
                        drifted_visibility.insert(row.slug.clone(), visibility);
                    }
                    drifted_models.insert(row.slug, reason);
                }
                continue;
            };
            if ambiguous_slugs.contains(&model.slug)
                || validate_identifier(&model.slug, MAX_MODEL_ID_BYTES).is_err()
                || !executable_codex_catalog_model(&model.slug)
            {
                continue;
            }
            if let Some(reason) = contract_drift(&model) {
                if let Some(visibility) = model.visibility.recognized() {
                    drifted_visibility.insert(model.slug.clone(), visibility);
                }
                drifted_models.insert(model.slug.clone(), reason);
                continue;
            }
            let Some(visibility) = model.visibility.recognized() else {
                continue;
            };
            let Ok(capacity_facts) = CodexCatalogCapacityFacts::from_wire(&model) else {
                continue;
            };
            if model.input_modalities.iter().collect::<BTreeSet<_>>().len()
                != model.input_modalities.len()
                || model
                    .supported_reasoning_levels
                    .iter()
                    .map(|level| &level.effort)
                    .collect::<BTreeSet<_>>()
                    .len()
                    != model.supported_reasoning_levels.len()
            {
                continue;
            }
            let text = model.input_modalities.contains(&CatalogInputModality::Text);
            if !text {
                continue;
            }
            let tools = matches!(
                model.shell_type,
                CatalogShellType::Default
                    | CatalogShellType::Local
                    | CatalogShellType::UnifiedExec
                    | CatalogShellType::ShellCommand
            );
            // Codex 0.166.0 omits parallel tool calls for Responses Lite rows.
            // The standard request builder can still enable them with tools.
            let parallel_tools = tools && !model.use_responses_lite;
            // The descriptor records the coarse existence of reasoning. The
            // provider compiler separately admits the exact requested effort
            // against this witnessed set, so a partial list grants no tiers.
            let reasoning_efforts =
                CodexReasoningEffortSet::from_catalog(&model.supported_reasoning_levels);
            let reasoning_effort_options: Vec<ReasoningEffort> = model
                .supported_reasoning_levels
                .iter()
                .filter_map(|level| level.effort.portable())
                .filter(|effort| *effort != ReasoningEffort::Ultra)
                .collect();
            let default_reasoning_effort = match model.default_reasoning_level.as_ref() {
                None => None,
                Some(effort) => match effort.portable() {
                    Some(ReasoningEffort::Ultra) => None,
                    Some(effort) if reasoning_effort_options.contains(&effort) => Some(effort),
                    Some(_) | None => continue,
                },
            };
            let Ok((service_tiers, default_service_tier)) = catalog_service_tiers(&model) else {
                continue;
            };
            let catalog_enum_fingerprint = catalog_enum_fingerprint(&model)?;
            let tool_mode = model.tool_mode.recognized();
            let reasoning = reasoning_efforts.supports_named_reasoning();
            if let Some(expected_tool_mode) = release_enrolled_codex_tool_mode(&model.slug) {
                if !tools || !reasoning {
                    continue;
                }
                let tool_mode_matches = match expected_tool_mode {
                    CodexCatalogToolMode::CodeModeOnly => matches!(
                        tool_mode,
                        Some(CodexCatalogToolMode::CodeModeOnly)
                            | Some(CodexCatalogToolMode::Direct)
                    ),
                    _ => tool_mode == Some(expected_tool_mode),
                };
                if !tool_mode_matches {
                    continue;
                }
            }
            let mut features = BTreeMap::new();
            features.insert(
                Capability::Streaming,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            );
            if tools {
                features.insert(
                    Capability::Tools,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        provenance,
                    ),
                );
            }
            // The live Codex catalog row's `input_modalities` is the evidence,
            // on Standard and Responses Lite rows alike.
            if model
                .input_modalities
                .contains(&CatalogInputModality::Image)
            {
                features.insert(
                    Capability::ImageInput,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        provenance,
                    ),
                );
            }
            if parallel_tools {
                features.insert(
                    Capability::ParallelTools,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        release_provenance,
                    ),
                );
            }
            if reasoning {
                features.insert(
                    Capability::Reasoning,
                    FeatureSupport::new(
                        Support::Native,
                        CapabilityConstraints::default(),
                        provenance,
                    ),
                );
            }
            let requested = ModelId::new(&model.slug).map_err(|_| NativeCodexAuthError::Catalog)?;
            let compaction_namespace = NamespaceId::new(CODEX_NATIVE_COMPACTION_NAMESPACE)
                .map_err(|_| NativeCodexAuthError::Catalog)?;
            let mut native_namespaces = BTreeMap::new();
            native_namespaces.insert(
                compaction_namespace,
                FeatureSupport::new(
                    Support::Native,
                    CapabilityConstraints::default(),
                    provenance,
                ),
            );
            let descriptor_without_context = ResolvedModelDescriptor::new(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                self.route.dialect().protocol().clone(),
                RouteId::new(crate::OPENAI_CODEX_SUBSCRIPTION_ROUTE_ID)
                    .map_err(|_| NativeCodexAuthError::Catalog)?,
                requested.clone(),
                Some(requested),
                Some(
                    ModelFamilyId::new(RESPONSES_FAMILY_ID)
                        .map_err(|_| NativeCodexAuthError::Catalog)?,
                ),
                ModelAdapterProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
                    .map_err(|_| NativeCodexAuthError::Catalog)?,
                CapabilitySet::new(features, native_namespaces),
            );
            let catalog_receipt = resolution_route_fingerprint(
                &self.route,
                &descriptor_without_context,
                account_fingerprint,
                credential_authority_binding,
                capacity_facts,
                reasoning_efforts,
                catalog_enum_fingerprint,
                model.use_responses_lite,
            )?;
            let descriptor = descriptor_without_context.with_context_contract(
                reference_client_envelope_context_contract(
                    context_authority_binding,
                    capacity_facts,
                    observed,
                    expires,
                    catalog_receipt,
                )?,
            );
            let witness = self.manager.issue_catalog_witness(
                &descriptor,
                &fence,
                selection_epoch,
                observed,
                expires,
                model.use_responses_lite,
            )?;
            descriptors.insert(
                model.slug.clone(),
                WitnessedDescriptor {
                    descriptor,
                    witness,
                    capacity_facts,
                    reasoning_efforts,
                    tool_mode,
                    responses_lite: model.use_responses_lite,
                    content_filter_guidance: catalog_content_filter_guidance(&model)
                        .map(str::to_owned),
                    catalog_enum_fingerprint,
                    catalog_receipt,
                    observed_at: observed,
                    expires_at: expires,
                },
            );
            entries.push(CodexCatalogEntry {
                model_id: model.slug,
                visibility: Some(visibility),
                priority: model.priority,
                api_support: if model.supported_in_api {
                    CodexCatalogApiSupport::PlatformEligible
                } else {
                    CodexCatalogApiSupport::SubscriptionOnly
                },
                text,
                tools,
                parallel_tools,
                wire_mode: if model.use_responses_lite {
                    CodexResponsesWireMode::Lite
                } else {
                    CodexResponsesWireMode::Standard
                },
                tool_mode,
                reasoning_effort_options,
                default_reasoning_effort,
                reasoning_efforts,
                service_tiers,
                default_service_tier,
                capacity_facts,
                observed_at: observed,
                expires_at: expires,
            });
        }
        entries.sort_by_key(CodexCatalogEntry::priority);
        if entries.is_empty() {
            if let Some((_, reason)) = drifted_models.into_iter().next() {
                return Err(NativeCodexAuthError::CompatibilityDrift(reason));
            }
            return Err(NativeCodexAuthError::Catalog);
        }
        Ok(ResolvedCatalog {
            public: CodexModelCatalog {
                entries,
                profile_id: OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
                source_revision: crate::config::OPENAI_CODEX_CATALOG_SOURCE_REVISION,
                drifted_models,
                drifted_visibility,
            },
            descriptors,
            ambiguous_slugs,
            credential_fence: fence,
        })
    }
}
