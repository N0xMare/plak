use super::*;

pub(super) fn provenance_test_model(
    entry_base: u128,
    tool_first: bool,
) -> TestResult<Arc<ArtifactModel>> {
    provenance_test_model_at(entry_base, tool_first, Timestamp::new(1, 0)?)
}

fn provenance_test_model_at(
    entry_base: u128,
    tool_first: bool,
    observed_at: Timestamp,
) -> TestResult<Arc<ArtifactModel>> {
    let descriptor = durable_test_descriptor_at("test.route", observed_at)?;
    let mut model = ArtifactModel::new(
        descriptor.clone(),
        artifact_authority(&descriptor, 1)?,
        0,
        entry_base,
        false,
    );
    model.response_provenance = true;
    if tool_first {
        model = model.with_tool_first();
    }
    Ok(Arc::new(model))
}

fn provenance_test_builder(
    workspace: Arc<WorkspacePolicy>,
    model: &Arc<ArtifactModel>,
) -> TestResult<AgentBuilder> {
    let registry = one_tool_registry()?;
    let configured = configured_test_model(
        model.clone(),
        model.descriptor.clone(),
        model.authority.clone(),
        &registry,
        LocalDurabilityDisposition::DurableRequired,
        ContextAdmissionRequirement::AnyBounded,
    )?;
    Ok(AgentBuilder::new(configured, registry, workspace)
        .request_policy(AgentRequestPolicy::new(
            GenerationLimits::new(NonZeroU32::new(128), Vec::new())?,
            ReasoningPolicy::Disabled,
        ))
        .tool_policy(Arc::new(AlwaysAllowPolicy))
        .identity_sources(
            Arc::new(FixedClock::new(Timestamp::new(10, 0)?)),
            Arc::new(SystemIdGenerator),
        ))
}

#[tokio::test]
async fn response_provenance_survives_tool_round_followup_and_exact_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store,
        RuntimeDurability::Required,
        NonZeroU64::new(30).unwrap(),
    )?;
    let model = provenance_test_model(91_000, true)?;
    let agent = sessions
        .create(
            "create:provenance",
            provenance_test_builder(workspace.clone(), &model)?,
        )
        .await?;
    let first = agent.prompt("use the probe").await?.result().await?;
    assert!(
        matches!(first.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        first.outcome()
    );
    assert_eq!(first.tool_calls(), 1);
    let seen = lock_mutex(&model.observed_provenance).clone();
    assert!(seen[0].1.previous_response_metadata().is_none());
    assert_eq!(seen[0].1.previous_response_id(), None);
    assert!(seen.iter().skip(1).all(|(_, context)| {
        context
            .previous_response_metadata()
            .and_then(|m| m.provider_request_id())
            == Some("req_fixture_91000_0")
            && context.previous_response_id() == Some("msg_fixture_91000_0")
    }));
    // The tool continuation stays in the first turn.
    assert!(
        seen.iter()
            .all(|(_, context)| context.turn_position() == TurnPosition::new(1, 1))
    );
    let second = agent.prompt("second human prompt").await?.result().await?;
    assert!(matches!(second.outcome(), TurnResultOutcome::Completed(_)));
    let seen = lock_mutex(&model.observed_provenance).clone();
    let (plan, context) = seen.last().ok_or("missing followup preparation")?;
    assert_eq!(
        context
            .previous_response_metadata()
            .and_then(|m| m.provider_request_id()),
        Some("req_fixture_91000_1")
    );
    assert_eq!(context.previous_response_id(), Some("msg_fixture_91000_1"));
    assert_eq!(context.turn_position(), TurnPosition::new(2, 2));
    assert!(!format!("{context:?}").contains("req_fixture"));
    assert!(!format!("{context:?}").contains("msg_fixture"));
    // Measuring a pending prompt measures that prompt's turn.
    let _ = agent
        .session_endpoint()
        .context_status(
            agent.session_id(),
            agent.branch_id(),
            agent.checkpoint().id(),
            Some("third human prompt".to_owned()),
        )
        .await;
    let measured = lock_mutex(&model.observed_provenance).clone();
    let (_, measured) = measured.last().ok_or("missing measurement preparation")?;
    assert_eq!(measured.turn_position(), TurnPosition::new(3, 3));
    let ledger = first.checkpoint().manifest().ledger();
    assert_eq!(
        previous_response_metadata(ledger, plan, context.authority())
            .and_then(|m| m.provider_request_id().map(str::to_owned)),
        Some("req_fixture_91000_1".to_owned())
    );
    assert!(
        previous_response_metadata(
            &RuntimeOperationLedger::new(Vec::new(), Vec::new())?,
            plan,
            context.authority()
        )
        .is_none()
    );
    let foreign = RuntimeModelAuthority::new(
        &model.descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"foreign-account"])?,
        context.authority().credential_authority_binding(),
        1,
        Timestamp::new(9, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?;
    assert!(previous_response_metadata(ledger, plan, &foreign).is_none());
    // Refresh keeps the same account and credential authority; generation is not lineage.
    assert!(
        previous_response_metadata(ledger, plan, &artifact_authority(&model.descriptor, 2)?)
            .is_some()
    );
    let mut records: Vec<_> = ledger.operations().into_iter().cloned().collect();
    let next_id = records
        .iter()
        .map(DurableOperationRecord::operation_id)
        .max()
        .unwrap()
        + 1;
    // Duplicate the provider that actually owns the latest assistant, not an older call.
    let latest_id = plan
        .request()
        .input()
        .items()
        .into_iter()
        .filter(|i| i.actor() == Actor::Assistant)
        .last()
        .unwrap()
        .id();
    let owner = ledger
        .operations()
        .into_iter()
        .find(|record| match record.state() {
            DurableOperationState::Finalized(_) => {
                crate::runtime::operation_record::recorded_provider_response(record)
                    .ok()
                    .flatten()
                    .is_some_and(|r| r.output().iter().any(|i| i.id() == latest_id))
            }
            _ => false,
        })
        .ok_or("missing exact owner")?;
    records.push(DurableOperationRecord::new(
        next_id,
        owner.kind().clone(),
        owner.request().clone(),
        owner.transitions().to_vec(),
    )?);
    assert!(
        previous_response_metadata(
            &RuntimeOperationLedger::new(Vec::new(), records)?,
            plan,
            context.authority()
        )
        .is_none()
    );
    let latest = plan
        .request()
        .input()
        .items()
        .into_iter()
        .find(|item| item.id() == latest_id)
        .unwrap();
    let imported = ConversationItem::new(
        latest.id(),
        latest.actor(),
        latest.blocks().to_vec(),
        Provenance::Imported,
    )?;
    let changed_plan = |items| -> TestResult<ValidatedRequestPlan> {
        Ok(model.descriptor.validate_request(
            ModelRequest::from_parts(
                plan.request().stable_prefix().clone(),
                ConversationView::new(items),
                plan.request().metadata(),
            )?,
            ValidationMode::Strict,
        )?)
    };
    assert!(
        previous_response_metadata(ledger, &changed_plan(vec![imported])?, context.authority())
            .is_none()
    );
    assert!(
        previous_response_metadata(ledger, &changed_plan(Vec::new())?, context.authority())
            .is_none()
    );
    let copied = ConversationItem::new(
        EntryId::try_from(uuid::Uuid::from_u128(999_999))?,
        latest.actor(),
        latest.blocks().to_vec(),
        Provenance::ModelOutput,
    )?;
    assert!(
        previous_response_metadata(ledger, &changed_plan(vec![copied])?, context.authority())
            .is_none()
    );
    for change_selected in [true, false] {
        let selected = if change_selected {
            serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::SelectedProviderConfiguration,
                5,
                &[b"foreign-selected"],
            )?)?)?
        } else {
            plan.request().selected_provider_configuration_id()
        };
        let effective = if change_selected {
            plan.request().effective_provider_configuration_id()
        } else {
            serde_json::from_value(serde_json::to_value(Digest::sha256(
                DigestDomain::EffectiveProviderConfiguration,
                5,
                &[b"foreign-effective"],
            )?)?)?
        };
        let request = ModelRequest::builder(
            selected,
            effective,
            plan.request().input().clone(),
            plan.request().metadata(),
        )
        .tools(plan.request().tools().to_vec())
        .build()?;
        let foreign_plan = model
            .descriptor
            .validate_request(request, ValidationMode::Strict)?;
        assert!(previous_response_metadata(ledger, &foreign_plan, context.authority()).is_none());
    }
    let mismatched_response = ModelResponse::new(
        vec![latest.clone()],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"wrong-response-plan"])?,
    );
    let evidence = crate::runtime::operation_record::test_support::outcome_component(Ok(
        &mismatched_response,
    ))?;
    let checkpoints = [
        CheckpointId::try_from(uuid::Uuid::from_u128(900_001))?,
        CheckpointId::try_from(uuid::Uuid::from_u128(900_002))?,
        CheckpointId::try_from(uuid::Uuid::from_u128(900_003))?,
    ];
    let wrong_plan_owner = DurableOperationRecord::new(
        owner.operation_id(),
        owner.kind().clone(),
        owner.request().clone(),
        vec![
            DurableOperationTransition::new(checkpoints[0], DurableOperationState::Prepared),
            DurableOperationTransition::new(checkpoints[1], DurableOperationState::Dispatched),
            DurableOperationTransition::new(
                checkpoints[2],
                DurableOperationState::Completed(evidence),
            ),
        ],
    )?;
    assert!(
        previous_response_metadata(
            &RuntimeOperationLedger::new(Vec::new(), vec![wrong_plan_owner])?,
            plan,
            context.authority()
        )
        .is_none()
    );
    // A later empty refusal (or failed provider attempt) contributes no canonical
    // assistant entry. It cannot replace the retained entry's actual request ID.
    for outcome in [
        Ok(ModelResponse::new(
            Vec::new(),
            FinishDetails::new(FinishReason::Refusal, None),
            None,
            None,
            serde_json::from_slice::<PreparedCallRecord>(owner.request().bytes())?
                .projection()
                .plan(),
        )
        .with_metadata(plakous_model::ModelResponseMetadata::new(
            None,
            Some(plakous_model::ModelProviderRequestId::new(
                "req_refusal_without_entry",
            )?),
        ))),
        Err(AgentError::new(
            AgentErrorKind::Model,
            "fixture provider failure",
        )),
    ] {
        let evidence =
            crate::runtime::operation_record::test_support::outcome_component(outcome.as_ref())?;
        let mut records: Vec<_> = ledger.operations().into_iter().cloned().collect();
        records.push(DurableOperationRecord::new(
            next_id,
            owner.kind().clone(),
            owner.request().clone(),
            vec![
                DurableOperationTransition::new(checkpoints[0], DurableOperationState::Prepared),
                DurableOperationTransition::new(checkpoints[1], DurableOperationState::Dispatched),
                DurableOperationTransition::new(
                    checkpoints[2],
                    DurableOperationState::Completed(evidence),
                ),
            ],
        )?);
        assert_eq!(
            previous_response_metadata(
                &RuntimeOperationLedger::new(Vec::new(), records)?,
                plan,
                context.authority()
            )
            .and_then(|m| m.provider_request_id().map(str::to_owned)),
            Some("req_fixture_91000_1".to_owned())
        );
    }
    let other_descriptor = durable_test_descriptor("another.route")?;
    assert!(
        previous_response_metadata(ledger, plan, &artifact_authority(&other_descriptor, 1)?)
            .is_none()
    );
    let foreign_credential = RuntimeModelAuthority::new(
        &model.descriptor,
        context.authority().account_binding(),
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"foreign-credential-authority"],
        )?,
        1,
        Timestamp::new(9, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?;
    assert!(previous_response_metadata(ledger, plan, &foreign_credential).is_none());
    agent.shutdown().await?;
    let address = DurableSessionAddress::new(
        agent.session_id(),
        agent.branch_id(),
        agent.checkpoint().id(),
    );
    let reopened_model = provenance_test_model_at(92_000, false, Timestamp::new(2, 0)?)?;
    assert_ne!(
        model.descriptor.binding_digest()?,
        reopened_model.descriptor.binding_digest()?
    );
    assert_eq!(
        model.descriptor.semantic_binding_digest()?,
        reopened_model.descriptor.semantic_binding_digest()?
    );
    let refreshed_plan = reopened_model
        .descriptor
        .validate_request(plan.request().clone(), ValidationMode::Strict)?;
    assert!(previous_response_metadata(ledger, &refreshed_plan, context.authority()).is_none());
    assert!(previous_response_metadata(ledger, plan, &reopened_model.authority).is_none());
    assert!(
        previous_response_metadata(ledger, &refreshed_plan, &reopened_model.authority).is_some()
    );
    let opened = sessions
        .open(
            "open:provenance",
            address,
            provenance_test_builder(workspace, &reopened_model)?,
        )
        .await?;
    let DurableRuntimeOpen::Opened(opened) = opened else {
        return Err("unexpected recovery".into());
    };
    let third = opened
        .prompt("third human prompt after reopen")
        .await?
        .result()
        .await?;
    assert!(
        matches!(third.outcome(), TurnResultOutcome::Completed(_)),
        "{:?}",
        third.outcome()
    );
    assert_eq!(
        lock_mutex(&reopened_model.observed_provenance)
            .last()
            .unwrap()
            .1
            .previous_response_metadata()
            .and_then(|m| m.provider_request_id()),
        Some("req_fixture_91000_2")
    );
    let reopened_seen = lock_mutex(&reopened_model.observed_provenance).clone();
    let (_, reopened_context) = reopened_seen.last().ok_or("missing reopened preparation")?;
    // The ledger's retained response and canonical history carry both across reopen.
    assert_eq!(
        reopened_context.previous_response_id(),
        Some("msg_fixture_91000_2")
    );
    assert_eq!(reopened_context.turn_position(), TurnPosition::new(3, 3));
    opened.shutdown().await?;
    Ok(())
}

#[test]
fn every_builtin_tool_error_roundtrips_with_exact_canonical_bytes() -> TestResult {
    for kind in [
        ToolRuntimeErrorKind::Registration,
        ToolRuntimeErrorKind::InvalidDefinition,
        ToolRuntimeErrorKind::UnknownTool,
        ToolRuntimeErrorKind::InvalidArguments,
        ToolRuntimeErrorKind::Policy,
        ToolRuntimeErrorKind::UnsupportedCapability,
        ToolRuntimeErrorKind::EffectSafety,
        ToolRuntimeErrorKind::Cancelled,
        ToolRuntimeErrorKind::ExecutionTimeout,
        ToolRuntimeErrorKind::CleanupTimeout,
        ToolRuntimeErrorKind::WorkspacePolicy,
        ToolRuntimeErrorKind::Execution,
        ToolRuntimeErrorKind::ResourceLimit,
        ToolRuntimeErrorKind::ScriptExhausted,
    ] {
        let original = AgentError::from_tool(&ToolRuntimeError::new(kind, "bounded tool failure"));
        let durable = DurableAgentError::from_error(&original)?;
        let bytes = serde_json::to_vec(&durable)?;
        let restored: DurableAgentError = serde_json::from_slice(&bytes)?;
        assert_eq!(serde_json::to_vec(&restored)?, bytes);
        let restored = restored.into_error()?;
        assert_eq!(restored.tool_kind(), Some(kind));
        assert_eq!(restored.kind(), original.kind());
        assert_eq!(restored.message(), original.message());
    }
    assert!(tool_error_kind_from_durable("unknown tool category").is_err());
    Ok(())
}

#[test]
fn durable_store_invalid_fact_roundtrips() -> TestResult {
    let original = AgentError::from_store(SessionStoreError::Invalid(
        StoreInvalidReason::SessionNotArchived,
    ));
    let durable = DurableAgentError::from_error(&original)?;
    let restored = durable.into_error()?;
    assert_eq!(restored.store_error(), original.store_error());
    Ok(())
}

#[test]
fn every_closed_store_fact_roundtrips_with_exact_canonical_bytes() -> TestResult {
    let facts = [
        SessionStoreError::LeaseDomainMismatch,
        SessionStoreError::Invalid(StoreInvalidReason::BranchRevisionExhausted),
        SessionStoreError::Invalid(StoreInvalidReason::CheckpointCommitTopology),
        SessionStoreError::Invalid(StoreInvalidReason::CheckpointObjectBinding),
        SessionStoreError::Invalid(StoreInvalidReason::CheckpointObjectBundle),
        SessionStoreError::Invalid(StoreInvalidReason::NoncanonicalCheckpointObjects),
        SessionStoreError::Invalid(StoreInvalidReason::DisabledDurabilityRequiresMemoryStore),
        SessionStoreError::Invalid(StoreInvalidReason::ForkCheckpointSemantics),
        SessionStoreError::Invalid(StoreInvalidReason::ForkRequestDigestDomain),
        SessionStoreError::Invalid(StoreInvalidReason::ForkSourceNotQuiescent),
        SessionStoreError::Invalid(StoreInvalidReason::GenesisCheckpointTopology),
        SessionStoreError::Invalid(StoreInvalidReason::InvalidBranchContextTransition),
        SessionStoreError::Invalid(StoreInvalidReason::LeaseExpiry),
        SessionStoreError::Invalid(StoreInvalidReason::LeaseExpiryOverflow),
        SessionStoreError::Invalid(StoreInvalidReason::ManifestRootDelta),
        SessionStoreError::Invalid(StoreInvalidReason::OrdinaryForkChangedSemantics),
        SessionStoreError::Invalid(StoreInvalidReason::RebindConfigurationUnchanged),
        SessionStoreError::Invalid(StoreInvalidReason::SessionNotArchived),
        SessionStoreError::Invalid(StoreInvalidReason::UnauthorizedBranchManifestTransition),
        SessionStoreError::capacity(StoreCapacityBound::BranchRevision, 13, 12),
        SessionStoreError::capacity(StoreCapacityBound::SessionRevision, 13, 12),
        SessionStoreError::capacity(StoreCapacityBound::Checkpoints, 13, 12),
        SessionStoreError::capacity(StoreCapacityBound::LeaseFence, 13, 12),
        SessionStoreError::capacity(StoreCapacityBound::ObjectBytes, 13, 12),
        SessionStoreError::capacity(StoreCapacityBound::Objects, 13, 12),
        SessionStoreError::capacity(StoreCapacityBound::PageSize, 13, 12),
        SessionStoreError::capacity(StoreCapacityBound::Sessions, 13, 12),
        SessionStoreError::capacity(StoreCapacityBound::BranchesPerSession, 13, 12),
        SessionStoreError::capacity(StoreCapacityBound::SqliteInteger, 13, 12),
        SessionStoreError::admission_capacity(
            StoreCapacityBound::Checkpoints,
            57,
            64,
            StoreAdmission::Ordinary,
        ),
        SessionStoreError::admission_capacity(
            StoreCapacityBound::Objects,
            61,
            64,
            StoreAdmission::Settlement,
        ),
        SessionStoreError::admission_capacity(
            StoreCapacityBound::ObjectBytes,
            65,
            64,
            StoreAdmission::Recovery,
        ),
    ];
    for fact in facts.into_iter().chain([
        SessionStoreError::StorageFull,
        SessionStoreError::Corrupt(crate::session::StoreIntegrityReason::MissingBase),
        SessionStoreError::Corrupt(crate::session::StoreIntegrityReason::MissingRoot),
        SessionStoreError::Corrupt(crate::session::StoreIntegrityReason::MissingChild),
        SessionStoreError::corrupt(),
    ]) {
        let original = AgentError::from_store(fact);
        let durable = DurableAgentError::from_error(&original)?;
        let bytes = serde_json::to_vec(&durable)?;
        let decoded: DurableAgentError = serde_json::from_slice(&bytes)?;
        assert_eq!(serde_json::to_vec(&decoded)?, bytes);
        let restored = decoded.into_error()?;
        assert_eq!(restored.store_error(), Some(fact));
        assert_eq!(
            restored.is_storage_full(),
            fact == SessionStoreError::StorageFull
        );
    }
    assert_eq!(
        serde_json::to_string(&DurableSessionStoreError::from_error(
            SessionStoreError::Invalid(StoreInvalidReason::SessionNotArchived)
        ))?,
        r#"{"Invalid":"session is not archived"}"#
    );
    assert_eq!(
        serde_json::to_string(&DurableSessionStoreError::from_error(
            SessionStoreError::capacity(StoreCapacityBound::Checkpoints, 13, 12)
        ))?,
        r#"{"Capacity":{"bound":"checkpoints","current":13,"max":12}}"#
    );
    assert_eq!(
        serde_json::to_string(&DurableSessionStoreError::from_error(
            SessionStoreError::admission_capacity(
                StoreCapacityBound::Checkpoints,
                57,
                64,
                StoreAdmission::Ordinary,
            )
        ))?,
        r#"{"Capacity":{"bound":"checkpoints","current":57,"max":56,"admission":{"class":"Ordinary","limit":64}}}"#
    );
    assert!(
        serde_json::from_str::<DurableSessionStoreError>(r#"{"Invalid":"unknown future reason"}"#)
            .is_err()
    );
    assert!(
        serde_json::from_str::<DurableSessionStoreError>(
            r#"{"Capacity":{"bound":"unknown bound","current":1,"max":0}}"#
        )
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn durable_failed_turn_reopens_the_exact_closed_store_fact() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = Arc::new(WorkspacePolicy::host(directory.path(), ".")?);
    let store = Arc::new(TestCreateStore::default());
    let sessions = DurableSessionRuntime::current(
        store.clone(),
        RuntimeDurability::Required,
        NonZeroU64::new(30).ok_or("lease")?,
    )?;
    let model = artifact_test_model(0, 50_000)?;
    let agent = sessions
        .create(
            "create:closed-store-fact",
            durable_artifact_test_builder(Arc::clone(&workspace), model),
        )
        .await?;
    store
        .fail_provider_preparation
        .store(true, Ordering::Release);
    let command = CommandId::new("closed-store-fact:root")?;
    let first = agent
        .start(command.clone(), "refuse provider preparation")
        .await?
        .into_turn()
        .ok_or("admitted root")?
        .result()
        .await?;
    assert_eq!(
        first.error().and_then(AgentError::store_error),
        Some(SessionStoreError::Invalid(
            StoreInvalidReason::CheckpointCommitTopology
        ))
    );
    let address =
        DurableSessionAddress::new(agent.session_id(), agent.branch_id(), first.checkpoint_id());
    agent.shutdown().await?;
    let model = artifact_test_model(0, 60_000)?;
    let opened = sessions
        .open(
            "open:closed-store-fact",
            address,
            durable_artifact_test_builder(workspace, Arc::clone(&model)),
        )
        .await?;
    let DurableRuntimeOpen::Opened(opened) = opened else {
        return Err("failed terminal was not quiescent".into());
    };
    let restored = opened
        .start(command, "refuse provider preparation")
        .await?
        .into_result()
        .ok_or("historical result")?
        .result()
        .await?;
    assert_eq!(restored.outcome(), first.outcome());
    assert_eq!(restored.checkpoint_id(), first.checkpoint_id());
    assert_eq!(model.calls.load(Ordering::Acquire), 0);
    assert_eq!(model.prepares.load(Ordering::Acquire), 0);
    opened.shutdown().await?;
    Ok(())
}

#[test]
fn reopened_projection_reports_provider_target_drift() -> TestResult {
    let registry = fake_process_projection_registry()?;
    let projection = |evidence: &[u8]| -> TestResult<ToolProjectionMap> {
        let configuration = serde_json::from_value(serde_json::to_value(Digest::sha256(
            DigestDomain::EffectiveProviderConfiguration,
            5,
            &[evidence],
        )?)?)?;
        let target = ProviderModelProjectionIdentityV2::new(
            plakous_model::IntegrationClass::Standard,
            "test.responses",
            "test.route",
            "test.profile.r1",
            "responses-family",
            "exact-model",
            "exact-model",
            configuration,
            2,
        )?;
        Ok(ProcessToolProjectionKind::CodexUnifiedExec.projection_map(
            plakous_tools::JobConfig::default(),
            registry.clone(),
            target,
        )?)
    };
    let persisted = projection(b"original provider evidence")?;
    let provided = projection(b"changed provider evidence")?;
    assert_eq!(provided.registry_digest(), persisted.registry_digest());
    let presentation = ToolPresentation::V2(Arc::new(persisted.clone()));
    validate_reopened_projection(Some(&persisted), &presentation)?;
    validate_reopened_projection(None, &presentation)?;
    let error = validate_reopened_projection(Some(&provided), &presentation).unwrap_err();
    assert_eq!(error.kind(), AgentErrorKind::InvalidInput);
    assert!(
        error
            .to_string()
            .contains("builder projection target disagrees with durable provider configuration"),
        "{error}"
    );
    // Same target, different registry: a reopen never adopts drifted tools.
    let same_target = projection(b"original provider evidence")?;
    let drifted = ProcessToolProjectionKind::PiBash.projection_map(
        plakous_tools::JobConfig::default(),
        registry,
        same_target.target().clone(),
    )?;
    assert_eq!(drifted.target(), same_target.target());
    assert_ne!(drifted.digest(), same_target.digest());
    let sealed = ToolPresentation::V2(Arc::new(same_target));
    let error = validate_reopened_projection(Some(&drifted), &sealed).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("builder projection registry or exposure disagrees"),
        "{error}"
    );
    Ok(())
}

#[test]
fn nested_canonical_failure_is_catchable_and_success_strings_are_exact() -> TestResult {
    use uuid::Uuid;

    for (kind, body) in [
        (
            ToolResultKind::Success,
            "error: this is successful data\r\nno final newline",
        ),
        (
            ToolResultKind::Failure,
            "patch text is not a valid git-style unified diff",
        ),
    ] {
        let result = ToolResultBlock::new(
            CallId::try_from(Uuid::from_u128(7))?,
            ToolExecutionOwner::PlakLocal,
            kind,
            vec![ToolResultPart::Text(TextBlock::new(body, None)?)],
            ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)?,
        )?;
        let item = ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(8))?,
            Actor::Tool,
            vec![ContentBlock::ToolResult(result)],
            Provenance::ToolExecution,
        )?;
        let delivered = local_composition_nested_result_value(&item);
        if kind == ToolResultKind::Success {
            assert_eq!(delivered?, json!(body));
        } else {
            let error = delivered.expect_err("a failed tool must reject");
            assert_eq!(error.kind(), AgentErrorKind::Tool);
            assert_eq!(error.message(), body);
            assert!(!local_composition_recovery_authoritative(&error));
        }
    }
    let malformed = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(9))?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new("missing result", None)?)],
        Provenance::ModelOutput,
    )?;
    let error = local_composition_nested_result_value(&malformed)
        .expect_err("missing canonical tool result is an infrastructure failure");
    assert!(local_composition_recovery_authoritative(&error));
    Ok(())
}
