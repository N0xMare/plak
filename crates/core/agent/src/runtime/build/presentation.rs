use super::{
    AgentBuilder, AgentError, AgentErrorKind, Arc, Digest, DigestDomain, ExecutionEnvironment,
    FrozenToolRegistryDescriptor, PreparedToolInvocation, ProjectedToolPresentationV2,
    ReconstructiveComponent, StableRequestPrefix, TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION,
    TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION, ToolCallBlock, ToolExposureMap, ToolInputContract,
    ToolInvocationRequest, ToolName, ToolOutcome, ToolPresentation, ToolProjectionMap,
    ToolProjectionReceiptV2, ToolRuntimeError, ToolRuntimeErrorKind, tool_descriptor_error,
    validate_custom_process_presentation_v2,
};

pub(in crate::runtime) fn tool_registry_descriptor(
    component: &ReconstructiveComponent,
) -> Result<FrozenToolRegistryDescriptor, AgentError> {
    match component.codec_revision() {
        TOOL_REGISTRY_DESCRIPTOR_CODEC_REVISION => {
            FrozenToolRegistryDescriptor::decode_v1(component.bytes())
        }
        TOOL_REGISTRY_DESCRIPTOR_V2_CODEC_REVISION => {
            FrozenToolRegistryDescriptor::decode_v2(component.bytes())
        }
        _ => {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "rebind source tool-registry codec is unsupported",
            ));
        }
    }
    .map_err(tool_descriptor_error)
}

pub(in crate::runtime) fn validate_local_composition_binding_slot(
    already_bound: bool,
) -> Result<(), AgentError> {
    if already_bound {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "an agent may bind only one local composition profile",
        ));
    }
    Ok(())
}

pub(in crate::runtime) fn local_composition_binding_identity(
    attachment: &crate::local_composition::LocalCompositionAttachmentIdentity,
) -> Result<Digest, AgentError> {
    let profile = attachment.profile.identity_bytes();
    let artifact = attachment.artifact.identity_bytes();
    let engine_contract = attachment.engine_contract.identity_bytes();
    let evidence = attachment
        .evidence_receipt
        .map(|value| value.identity_bytes());
    let mut parts = vec![
        b"plakous.local-composition.attachment.v1".as_slice(),
        profile.as_slice(),
        artifact.as_slice(),
        engine_contract.as_slice(),
    ];
    if let Some(value) = evidence.as_ref() {
        parts.push(value.as_slice());
    }
    Digest::sha256(DigestDomain::ExecutionEnvironment, 1, &parts).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "local composition binding identity could not be validated",
        )
    })
}

pub(in crate::runtime) fn validate_local_composition_attachment_identity(
    attachment: &crate::local_composition::LocalCompositionAttachmentIdentity,
) -> Result<(), AgentError> {
    if attachment.binding != local_composition_binding_identity(attachment)? {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "local composition binding identity is invalid",
        ));
    }
    Ok(())
}

impl ToolPresentation {
    /// The canonical name a call's exposed name resolves to (spec 01 §8.2).
    ///
    /// An exposure whose operations share one canonical name resolves without
    /// the call's arguments. One that lowers to an operation by its arguments
    /// resolves through `prepare`, exactly as admission will, and to its first
    /// operation when the call does not prepare. A name nothing exposes
    /// resolves to itself.
    pub(in crate::runtime) fn canonical_name(
        &self,
        call: &ToolCallBlock,
        prepare: impl FnOnce(&Self) -> Result<PreparedToolInvocation, ToolRuntimeError>,
    ) -> ToolName {
        let exposed = call.exposed_name();
        let resolved = match self {
            Self::V1(exposure) => exposure
                .canonical_identity(exposed)
                .map(|identity| identity.name().clone()),
            Self::V2(projection) => projection.by_exposed(exposed).and_then(|entry| {
                let mut names = entry
                    .canonical_operations()
                    .values()
                    .map(plakous_tools::RegisteredToolIdentity::name);
                let first = names.next()?.clone();
                if names.all(|name| *name == first) {
                    return Some(first);
                }
                Some(prepare(self).map_or(first, |prepared| {
                    prepared.invocation.identity().name().clone()
                }))
            }),
        };
        resolved.unwrap_or_else(|| exposed.clone())
    }

    /// Whether `name` presents a composition `wait` marker, judged by the
    /// canonical identity it resolves to, never by its spelling.
    pub(in crate::runtime) fn is_cell_wait_marker(&self, name: &ToolName) -> bool {
        match self {
            Self::V1(exposure) => exposure
                .canonical_identity(name)
                .is_some_and(plakous_tools::is_cell_wait_marker),
            Self::V2(projection) => projection.by_exposed(name).is_some_and(|entry| {
                entry
                    .canonical_operations()
                    .values()
                    .all(plakous_tools::is_cell_wait_marker)
            }),
        }
    }

    pub(in crate::runtime) fn registry_arguments<'a>(
        &self,
        call: &'a ToolCallBlock,
    ) -> Result<&'a str, ToolRuntimeError> {
        let custom = match self {
            Self::V1(exposure) => exposure.definitions().iter().any(|definition| {
                definition.name() == call.exposed_name()
                    && matches!(definition.input(), ToolInputContract::Utf8(_))
            }),
            Self::V2(projection) => {
                projection
                    .by_exposed(call.exposed_name())
                    .is_some_and(|entry| {
                        matches!(
                            entry.presentation(),
                            ProjectedToolPresentationV2::CustomUtf8 { .. }
                        )
                    })
            }
        };
        if !custom {
            return Ok(call.raw_arguments());
        }
        if let Some(serde_json::Value::String(text)) = call.arguments() {
            Ok(text)
        } else {
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                "custom tool input is not a canonical JSON string",
            ))
        }
    }

    pub(in crate::runtime) fn prepare(
        &self,
        request: ToolInvocationRequest,
        environment: &ExecutionEnvironment,
    ) -> Result<PreparedToolInvocation, ToolRuntimeError> {
        match self {
            Self::V1(exposure) => {
                exposure
                    .prepare(request)
                    .map(|invocation| PreparedToolInvocation {
                        invocation,
                        projection_receipt: None,
                    })
            }
            Self::V2(projection) => {
                projection
                    .prepare_in_environment(request, environment)
                    .map(|prepared| {
                        let (invocation, receipt) = prepared.into_parts();
                        PreparedToolInvocation {
                            invocation,
                            projection_receipt: receipt,
                        }
                    })
            }
        }
    }

    pub(in crate::runtime) fn raise(
        &self,
        receipt: Option<&ToolProjectionReceiptV2>,
        outcome: ToolOutcome,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let prepared = plakous_tools::internal::tool_images_prepared(&outcome);
        let raised = match (self, receipt) {
            (Self::V2(projection), Some(receipt)) => projection.raise(receipt, outcome),
            (Self::V1(_) | Self::V2(_), None) => Ok(outcome),
            _ => Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "tool projection receipt disagrees with the runtime presentation mode",
            )),
        }?;
        Ok(if prepared {
            plakous_tools::internal::mark_tool_images_prepared(raised)
        } else {
            raised
        })
    }

    pub(in crate::runtime) async fn present_outcome(
        outcome: ToolOutcome,
        invocation: &plakous_tools::ToolInvocation,
        environment: &ExecutionEnvironment,
        cancellation: &plakous_model::CancellationToken,
    ) -> Result<(plakous_model::ToolResultBlock, ToolOutcome), ToolRuntimeError> {
        let outcome =
            plakous_tools::internal::prepare_tool_images(outcome, environment, cancellation)
                .await?;
        let result = outcome.to_model_result(invocation)?;
        Ok((result, outcome))
    }

    pub(in crate::runtime) fn request_prefix(
        &self,
        prefix: StableRequestPrefix,
    ) -> Result<StableRequestPrefix, AgentError> {
        match self {
            Self::V1(_) => Ok(prefix),
            Self::V2(projection) => projection
                .compile_request_view_v2()
                .and_then(|view| {
                    prefix.with_tool_projection(view).map_err(|error| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Registration,
                            format!(
                                "stable request prefix rejected the exact V2 projection ({error})"
                            ),
                        )
                    })
                })
                .map_err(|error| AgentError::from_tool(&error)),
        }
    }

    pub(in crate::runtime) fn registry_digest(&self) -> Digest {
        match self {
            Self::V1(exposure) => exposure.registry_digest(),
            Self::V2(projection) => projection.registry_digest(),
        }
    }

    pub(in crate::runtime) fn digest(&self) -> Digest {
        match self {
            Self::V1(exposure) => exposure.digest(),
            Self::V2(projection) => projection.digest(),
        }
    }
}

pub(in crate::runtime) fn validate_reopened_projection(
    provided: Option<&ToolProjectionMap>,
    presentation: &ToolPresentation,
) -> Result<(), AgentError> {
    let message = match (provided, presentation) {
        (Some(provided), ToolPresentation::V2(persisted))
            if provided.digest() == persisted.digest()
                && provided.registry_digest() == persisted.registry_digest() =>
        {
            return Ok(());
        }
        (None, _) => return Ok(()),
        (Some(provided), ToolPresentation::V2(persisted))
            if provided.target() != persisted.target() =>
        {
            "builder projection target disagrees with durable provider configuration"
        }
        (Some(_), ToolPresentation::V2(_)) => {
            "builder projection registry or exposure disagrees with durable tool semantics"
        }
        _ => "builder projection mode disagrees with durable tool semantics",
    };
    Err(AgentError::new(AgentErrorKind::InvalidInput, message)
        .with_binding_change(crate::BindingChange::ToolPresentation))
}

pub(in crate::runtime) fn construction_tool_presentation(
    builder: &AgentBuilder,
    exposure: ToolExposureMap,
) -> Result<ToolPresentation, AgentError> {
    match builder.tool_projection.as_ref() {
        Some(projection) => {
            let admission = if let Some(local_composition) = builder.local_composition.as_ref() {
                local_composition.integration.validate_configuration(
                    builder.job_config,
                    &local_composition.descriptor,
                    projection,
                    builder.tools.clone(),
                    &builder.provider_configuration,
                )
            } else if projection.target().integration_class()
                == plakous_model::IntegrationClass::Custom
            {
                validate_custom_process_presentation_v2(
                    projection,
                    &builder.tools,
                    &builder.provider_configuration,
                )
                .map_err(|error| error.to_string())
            } else {
                builder
                    .projection_admission
                    .as_ref()
                    .ok_or_else(|| {
                        "a standard-integration V2 projection needs a candidate admission from \
                         the composition root"
                            .to_owned()
                    })
                    .and_then(|admission| {
                        admission
                            .admit(
                                builder.job_config,
                                projection,
                                &builder.tools,
                                &builder.provider_configuration,
                            )
                            .map_err(|error| error.to_string())
                    })
            };
            admission.map_err(|error| {
                AgentError::new(
                    AgentErrorKind::InvalidInput,
                    format!(
                        "V2 projection is not qualified for the exact provider configuration: {error}"
                    ),
                )
            })?;
            Ok(ToolPresentation::V2(Arc::new(projection.clone())))
        }
        None => Ok(ToolPresentation::V1(exposure)),
    }
}
