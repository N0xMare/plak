//! Host-only semantic presentation data; no descriptor envelope or executable crosses.
use super::{
    Arc, FrozenToolRegistry, FrozenToolRegistryDescriptor, NestedToolEligibility, RegisteredToolId,
    ToolBindingError, ToolDefinition, ToolDescriptorError, ToolExecutorBindingIdentity,
    ToolExecutorBindings, ToolExecutorId, ToolRegistration, ToolRegistryBuilder,
};

/// Unsupported host reconstruction record. The export document owns its wire encoding.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticToolRegistration {
    /// Stable canonical capability identifier, not an invocation grant.
    pub registered_id: String,
    /// Current executable must accept this release-owned binding identity.
    pub executor_id: String,
    /// Expected executable semantics, never executable code or a process handle.
    pub executor_binding: ToolExecutorBindingIdentity,
    /// Frozen name, description and portable input/output schemas.
    pub definition: ToolDefinition,
    /// Descriptive source nesting; exposure still requires fresh qualification.
    pub nesting: NestedToolEligibility,
}

impl FrozenToolRegistryDescriptor {
    /// Projects semantic records without native descriptor bytes or registry hashes.
    #[doc(hidden)]
    #[must_use]
    pub fn semantic_registrations(&self) -> Vec<SemanticToolRegistration> {
        self.wire
            .registrations
            .iter()
            .map(|registration| SemanticToolRegistration {
                registered_id: registration.registered_tool_id.clone(),
                executor_id: registration.executor_id.clone(),
                executor_binding: ToolExecutorBindingIdentity::from_digest(
                    registration.executor_binding_identity,
                ),
                definition: registration.definition.clone(),
                nesting: registration.nested_eligibility,
            })
            .collect()
    }

    /// Rebuilds under current registry limits and current executable capabilities.
    ///
    /// # Errors
    /// Refuses missing/revised executors, duplicate identities, or invalid source schemas.
    #[doc(hidden)]
    pub fn restore_semantic_registrations(
        records: &[SemanticToolRegistration],
        live: &FrozenToolRegistry,
    ) -> Result<FrozenToolRegistry, ToolBindingError> {
        let bindings = ToolExecutorBindings::capture(live);
        let mut builder = ToolRegistryBuilder::new(live.limits().clone());
        for record in records {
            let tool = RegisteredToolId::new(record.registered_id.clone())
                .map_err(|_| ToolDescriptorError::InvalidSemantics)?;
            let executor = ToolExecutorId::new(record.executor_id.clone())
                .map_err(|_| ToolDescriptorError::InvalidSemantics)?;
            let binding =
                bindings
                    .by_id
                    .get(&executor)
                    .ok_or_else(|| ToolBindingError::MissingBinding {
                        id: executor.clone(),
                    })?;
            if !binding.identity.accepts(&executor, record.executor_binding) {
                return Err(ToolBindingError::BindingIdentityMismatch {
                    id: executor,
                    expected: record.executor_binding,
                    found: binding.identity,
                });
            }
            let live_identity = live
                .identities()
                .find(|identity| identity.tool_id() == &tool)
                .ok_or(ToolDescriptorError::InvalidSemantics)?;
            if live_identity.executor_id() != &executor {
                return Err(ToolDescriptorError::InvalidSemantics.into());
            }
            let claims = live_identity.claims_presented_name();
            let owner = live_identity.owner().clone();
            let restored = ToolRegistration::new(
                tool,
                executor,
                record.executor_binding,
                record.definition.clone(),
                Arc::clone(&binding.executor),
            )
            .with_nested_eligibility(record.nesting)
            .with_owner(owner);
            builder
                .register(if claims {
                    restored.presented_name_claimer()
                } else {
                    restored
                })
                .map_err(|_| ToolDescriptorError::InvalidSemantics)?;
        }
        Ok(builder.freeze())
    }
}
