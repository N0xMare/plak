use super::{
    ActorCommandEffect, AgentError, AgentErrorKind, Arc, BTreeMap, BindingReference, CommandAck,
    CommandId, CommandLedgerEntry, CompiledProviderConfiguration, Digest, DigestDomain,
    DurableCommandDisposition, DurableCommandEnvelope, DurableCommandRecord, DurableOperationKind,
    DurableOperationRecord, DurableOperationState, DurableTerminalResultRecord,
    FrozenToolExposureDescriptor, FrozenToolProjectionDescriptorV2, FrozenToolRegistry,
    FrozenToolRegistryDescriptor, LanguageModel, LocalCompositionConstruction, RuntimeCheckpoint,
    RuntimeComponentKind, RuntimeLifecycle, RuntimeManifest, RuntimeOperationLedger,
    RuntimeRevalidationWitness, RuntimeWorkingRoot, TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
    TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION, TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
    TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION, TerminalRecordRejection, Timestamp,
    ToolBindingError, ToolExecutorBindings, ToolPresentation, ToolRuntime, WorkspacePolicy,
    actor_command_effect, control_error_from_durable, read_terminal_result_record,
    runtime_component, runtime_component_revision, runtime_manifest_error,
    stable_runtime_environment_component, tool_descriptor_error,
};

#[allow(
    clippy::too_many_arguments,
    reason = "reopen must compare the complete live reconstruction authority tuple"
)]
pub(in crate::runtime) fn runtime_revalidation_witness(
    _model: &dyn LanguageModel,
    provider_configuration: &CompiledProviderConfiguration,
    tools: &ToolRuntime,
    presentation: &ToolPresentation,
    workspace: &WorkspacePolicy,
    local_composition: Option<&LocalCompositionConstruction>,
    admission: crate::RevalidationFact,
    checked_at: Timestamp,
) -> Result<RuntimeRevalidationWitness, AgentError> {
    let descriptor = provider_configuration.effective().descriptor();
    let semantic_descriptor_binding = descriptor.semantic_binding_digest().map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "resolved model descriptor could not be revalidated",
        )
    })?;
    let account = admission.reference().clone();
    let profile = BindingReference::new("model-adapter", descriptor.profile().as_str(), 1)
        .map_err(runtime_manifest_error)?;
    let registry_descriptor = tools.registry_descriptor().map_err(tool_descriptor_error)?;
    let (registry_revision, registry_bytes, exposure_revision, exposure_bytes) = match presentation
    {
        ToolPresentation::V1(exposure) => (
            TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
            registry_descriptor
                .encode_v1()
                .map_err(tool_descriptor_error)?,
            TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION,
            FrozenToolExposureDescriptor::capture(exposure)
                .and_then(|descriptor| descriptor.encode_v1())
                .map_err(tool_descriptor_error)?,
        ),
        ToolPresentation::V2(projection) => (
            TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION,
            registry_descriptor
                .encode_v2()
                .map_err(tool_descriptor_error)?,
            TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION,
            FrozenToolProjectionDescriptorV2::capture(projection)
                .and_then(|descriptor| descriptor.encode_v2())
                .map_err(tool_descriptor_error)?,
        ),
    };
    let registry = runtime_component_revision(
        RuntimeComponentKind::ToolRegistry,
        registry_revision,
        registry_bytes,
    )?;
    let exposure = runtime_component_revision(
        RuntimeComponentKind::ToolExposure,
        exposure_revision,
        exposure_bytes,
    )?;
    let environment = stable_runtime_environment_component(
        workspace,
        local_composition,
        provider_configuration,
        crate::runtime::build::live_job_config(tools)?,
    )?;
    let policy_revision = tools.policy_revision();
    let effect_policy = runtime_component(
        RuntimeComponentKind::EffectPolicy,
        format!(
            "{}:{}",
            policy_revision.policy_id(),
            policy_revision.revision()
        )
        .into_bytes(),
    )?;
    let policy = BindingReference::new(
        "tool-policy",
        policy_revision.policy_id(),
        policy_revision.revision(),
    )
    .map_err(runtime_manifest_error)?;
    Ok(RuntimeRevalidationWitness::new(
        checked_at,
        account,
        profile,
        semantic_descriptor_binding,
        semantic_descriptor_binding,
        admission,
        registry.integrity(),
        exposure.integrity(),
        environment.integrity(),
        effect_policy.integrity(),
        policy,
    ))
}

pub(in crate::runtime) fn rehydrate_runtime_indexes(
    authority: &RuntimeCheckpoint,
    feed_sequences: &BTreeMap<CommandId, crate::feed::FeedSeq>,
    durable: bool,
) -> Result<(BTreeMap<CommandId, CommandLedgerEntry>, u64, u64), AgentError> {
    let ledger = authority.manifest().ledger();
    let active_turn = match authority.manifest().lifecycle() {
        RuntimeLifecycle::Working {
            root: RuntimeWorkingRoot::Turn { turn_id, .. },
            ..
        } => Some(*turn_id),
        _ => None,
    };
    let mut commands = BTreeMap::new();
    let mut next_command = 1_u64;
    for record in ledger.commands() {
        next_command = next_command.max(record.sequence().checked_add(1).ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Session,
                "durable command sequence space was exhausted",
            )
        })?);
        let command_id = CommandId::new(record.command_id()).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Session,
                "durable command identity could not be reconstructed",
            )
        })?;
        let envelope: DurableCommandEnvelope =
            serde_json::from_slice(record.command_component().bytes()).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "durable command receipt bytes could not be reconstructed",
                )
            })?;
        let reconstructed = Digest::sha256(DigestDomain::Blob, 1, &[&envelope.canonical_input])
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Session,
                    "durable command binding could not be reconstructed",
                )
            })?;
        if envelope.version != 1 || reconstructed != record.binding() {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "durable command binding is inconsistent",
            ));
        }
        let effect = actor_command_effect(&envelope.effect)?;
        let (acknowledgement, effect) =
            match (record.disposition(), effect, envelope.rejection.as_ref()) {
                (DurableCommandDisposition::Accepted, ActorCommandEffect::Rejected, None) => {
                    return Err(AgentError::new(
                        AgentErrorKind::Session,
                        "accepted durable command carries a rejection effect",
                    ));
                }
                (DurableCommandDisposition::Accepted, effect, None) => (
                    CommandAck::accepted(command_id.clone(), record.sequence(), record.turn_id()),
                    effect,
                ),
                (
                    DurableCommandDisposition::Rejected,
                    ActorCommandEffect::Rejected,
                    Some(rejection),
                ) => (
                    CommandAck::rejected(
                        command_id.clone(),
                        record.sequence(),
                        record.turn_id(),
                        control_error_from_durable(rejection)?,
                    ),
                    ActorCommandEffect::Rejected,
                ),
                _ => {
                    return Err(AgentError::new(
                        AgentErrorKind::Session,
                        "durable command disposition disagrees with its exact receipt",
                    ));
                }
            };
        // Queued follow-up roots have receipts but no terminal yet; the reopen's
        // abandonment publisher closes them before ordinary admission resumes.
        let queued_followup = active_turn.is_some_and(|turn_id| {
            ledger.operations().into_iter().any(|operation| {
                operation.request().bytes() == record.command_component().bytes()
                    && crate::runtime::durable::composition_recovery::queued_root(
                        authority, operation, turn_id,
                    )
            })
        });
        let terminal = if matches!(effect, ActorCommandEffect::Root)
            && (record.turn_id() == active_turn || queued_followup)
        {
            None
        } else if matches!(effect, ActorCommandEffect::Root) {
            Some(terminal_record_for_command(ledger, record)?)
        } else {
            None
        };
        commands.insert(
            command_id.clone(),
            CommandLedgerEntry {
                binding: record.binding(),
                acknowledgement: acknowledgement
                    .with_feed_seq(feed_sequences.get(&command_id).copied()),
                admission_reclaimed: durable && !feed_sequences.contains_key(&command_id),
                effect,
                result: None,
                terminal,
            },
        );
    }
    let next_operation = ledger
        .operations()
        .into_iter()
        .map(DurableOperationRecord::operation_id)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Session,
                "durable operation identity space was exhausted",
            )
        })?;
    Ok((commands, next_command, next_operation))
}

pub(in crate::runtime) fn terminal_record_for_command(
    ledger: &RuntimeOperationLedger,
    command: &DurableCommandRecord,
) -> Result<DurableTerminalResultRecord, AgentError> {
    let turn_id = command.turn_id().ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::Session,
            "accepted root command omitted its durable turn binding",
        )
    })?;
    // A record that is present but re-encodes differently is a different
    // failure from one that is not there, and the refusal has to say which.
    // Reported as `None` alone, the 2026-09-18 resume blocker read as a
    // missing record when the record was intact and one optional field had
    // changed shape.
    let mut rejection = None;
    let terminal = ledger
        .operations()
        .into_iter()
        .filter(|operation| {
            matches!(
                operation.kind(),
                DurableOperationKind::Command {
                    turn_id: Some(operation_turn)
                } if *operation_turn == turn_id
            ) && operation.request().bytes() == command.command_component().bytes()
        })
        .find_map(|operation| match operation.state() {
            DurableOperationState::Finalized(evidence) => {
                match read_terminal_result_record(evidence.bytes()) {
                    Ok(terminal) if terminal.operation_id.0 == operation.operation_id() => {
                        Some(terminal)
                    }
                    Ok(_) => None,
                    Err(error) => {
                        // A round-trip failure is the diagnosable one, so it
                        // outranks an "absent" from some other operation.
                        if rejection.is_none()
                            || matches!(error, TerminalRecordRejection::RoundTrip(_))
                        {
                            rejection = Some(error);
                        }
                        None
                    }
                }
            }
            _ => None,
        })
        .ok_or_else(|| {
            // A record this build re-encodes differently was written by an
            // incompatible build; an absent one is a damaged session.
            let kind = if matches!(rejection, Some(TerminalRecordRejection::RoundTrip(_))) {
                AgentErrorKind::IncompatibleSession
            } else {
                AgentErrorKind::Session
            };
            AgentError::new(
                kind,
                format!(
                    "accepted root command has {}",
                    rejection.map_or_else(
                        || TerminalRecordRejection::Absent.detail(),
                        |rejection| rejection.detail(),
                    )
                ),
            )
        })?;
    if terminal.command_id != command.command_id() || terminal.turn_id != turn_id {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "terminal result record disagrees with its root command binding",
        ));
    }
    Ok(terminal)
}

pub(in crate::runtime) fn terminal_record_at_checkpoint(
    checkpoint: &RuntimeCheckpoint,
    command_id: &CommandId,
) -> Result<DurableTerminalResultRecord, AgentError> {
    let ledger = checkpoint.manifest().ledger();
    let command = ledger
        .commands()
        .into_iter()
        .find(|command| command.command_id() == command_id.as_str())
        .ok_or_else(|| {
            AgentError::new(
                AgentErrorKind::Session,
                "terminal checkpoint omitted its admitted root command",
            )
        })?;
    terminal_record_for_command(ledger, command)
}

pub(in crate::runtime) fn reconstruct_persisted_tools(
    manifest: &RuntimeManifest,
    bindings: &ToolExecutorBindings,
) -> Result<(FrozenToolRegistry, ToolPresentation), AgentError> {
    let tools = manifest.tools();
    let registry_revision = tools.registry().codec_revision();
    let exposure_revision = tools.exposure().codec_revision();
    if !matches!(
        (registry_revision, exposure_revision),
        (
            TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
            TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION
        ) | (
            TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION,
            TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION
        )
    ) {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "durable tools contain unsupported or mixed V1/V2 state",
        ));
    }
    let registry_descriptor = match registry_revision {
        TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION => {
            FrozenToolRegistryDescriptor::decode_v1(tools.registry().bytes())
        }
        TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION => {
            FrozenToolRegistryDescriptor::decode_v2(tools.registry().bytes())
        }
        _ => unreachable!("closed revision pair validated above"),
    }
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Session,
            "durable tool registry is unsupported or corrupt",
        )
    })?;
    let registry = registry_descriptor
        .resolve(bindings)
        .map_err(|error| match error {
            ToolBindingError::MissingBinding { id } | ToolBindingError::DuplicateBinding { id } => {
                AgentError::new(
                    AgentErrorKind::ExternalBindingUnavailable,
                    format!("a required tool executor binding is unavailable: {id}"),
                )
            }
            ToolBindingError::BindingIdentityMismatch { id, .. } => AgentError::new(
                AgentErrorKind::ExternalBindingUnavailable,
                format!("a required tool executor binding has a different identity: {id}"),
            ),
            ToolBindingError::Descriptor(_) => AgentError::new(
                AgentErrorKind::Session,
                "durable tool registry failed exact reconstruction",
            ),
            _ => AgentError::new(
                AgentErrorKind::Session,
                "durable tool registry binding failed closed",
            ),
        })?;
    let presentation = match exposure_revision {
        TOOL_EXPOSURE_DESCRIPTOR_CODEC_REVISION => {
            FrozenToolExposureDescriptor::decode_v1(tools.exposure().bytes(), &registry_descriptor)
                .and_then(|descriptor| descriptor.resolve(registry.clone()))
                .map(ToolPresentation::V1)
        }
        TOOL_EXPOSURE_DESCRIPTOR_V2_CODEC_REVISION => FrozenToolProjectionDescriptorV2::decode_v2(
            tools.exposure().bytes(),
            &registry_descriptor,
        )
        .and_then(|descriptor| descriptor.resolve(registry.clone()))
        .map(Arc::new)
        .map(ToolPresentation::V2),
        _ => unreachable!("closed revision pair validated above"),
    }
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Session,
            "durable tool presentation failed exact reconstruction",
        )
    })?;
    Ok((registry, presentation))
}
