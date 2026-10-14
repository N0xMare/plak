//! The durable registration identity of a standard coding capability.
//!
//! `plakous.standard.<suffix>` and `plakous.standard.<suffix>.v3` are persisted
//! in session descriptors, and `durable.rs` rebuilds the expected registration
//! to compare against a reopened one. That makes the *shape* of a standard
//! registration — its id, its executor binding, the definition's version and
//! output contract — core business, even though the executors that carry it
//! live in the pack crates.
//!
//! `update_plan` is here in full because `durable.rs` normalizes the
//! per-session plan authority: it compares the whole persisted registration,
//! description and schema included, so both are durable format.

use std::sync::Arc;

use plakous_model::{
    ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
    ToolResourceKey,
};
use serde_json::{Value, json};

use crate::capabilities::{STANDARD_CODING_TOOL_VERSION, UPDATE_PLAN_CAPABILITY_NAME};
use crate::schema::object_schema;
use crate::{
    RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorBindingIdentity, ToolExecutorId,
    ToolFuture, ToolInvocation, ToolRegistration, ToolRuntimeError, ToolRuntimeErrorKind,
};

/// Standard `update_plan` tool description. Durable: it is compared field for
/// field when a persisted plan registration is normalized.
pub const PLAKOUS_TOOL_UPDATE_PLAN_DESCRIPTION: &str =
    "Replace the bounded task plan and return its new authoritative revision and statuses.";

/// The frozen canonical `update_plan` input schema.
#[must_use]
pub fn update_plan_input_schema() -> Value {
    object_schema(
        json!({
            "plan": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "step": {"type": "string"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                    },
                    "required": ["step", "status"],
                    "additionalProperties": false
                }
            }
        }),
        &["plan"],
    )
}

/// The canonical `update_plan` definition.
///
/// # Errors
///
/// Returns an invalid-definition error when a built-in invariant cannot freeze.
pub fn update_plan_definition() -> Result<ToolDefinition, ToolRuntimeError> {
    standard_definition(
        UPDATE_PLAN_CAPABILITY_NAME,
        PLAKOUS_TOOL_UPDATE_PLAN_DESCRIPTION,
        update_plan_input_schema(),
        workspace_concurrency(&["harness.plan"])?,
        ToolEffect::Mutating,
    )
}

/// The exact `update_plan` registration one plan authority would enroll.
///
/// The executor refuses: this is the registration `durable.rs` compares a
/// persisted one against, never one that dispatches.
///
/// # Errors
///
/// Returns an invalid-definition error when a built-in invariant cannot freeze.
pub fn expected_update_plan_registration(
    executor_id: &ToolExecutorId,
) -> Result<ToolRegistration, ToolRuntimeError> {
    standard_registration_with_executor(
        "update-plan",
        update_plan_definition()?,
        Arc::new(UnboundPlanExecutor),
        executor_id,
    )
}

#[derive(Debug)]
struct UnboundPlanExecutor;

impl ToolExecutor for UnboundPlanExecutor {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async {
            Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "this plan registration exists only to compare durable identity",
            ))
        })
    }
}

/// One standard coding registration under its exact durable identity.
///
/// `stable_suffix` is the persisted half of `plakous.standard.<suffix>` and
/// `plakous.standard.<suffix>.v3`; it is durable-format state, not a label.
///
/// # Errors
///
/// Returns the pack's own definition error, or an invalid id.
pub fn standard_registration<T: ToolExecutor + 'static>(
    stable_suffix: &str,
    parts: Result<(ToolDefinition, T), ToolRuntimeError>,
) -> Result<ToolRegistration, ToolRuntimeError> {
    let (definition, executor) = parts?;
    let executor_id = standard_executor_id(stable_suffix)?;
    standard_registration_with_executor(stable_suffix, definition, Arc::new(executor), &executor_id)
}

/// The exact durable executor identity of one standard coding capability.
///
/// # Errors
///
/// Returns an invalid-identity error when the suffix is not a valid id.
pub fn standard_executor_id(stable_suffix: &str) -> Result<ToolExecutorId, ToolRuntimeError> {
    ToolExecutorId::new(format!("plakous.standard.{stable_suffix}.v3"))
}

/// One standard coding registration over an executor another registration owns.
///
/// # Errors
///
/// Returns an invalid-identity error when an id or binding cannot freeze.
pub fn standard_registration_with_executor(
    stable_suffix: &str,
    definition: ToolDefinition,
    executor: Arc<dyn ToolExecutor>,
    executor_id: &ToolExecutorId,
) -> Result<ToolRegistration, ToolRuntimeError> {
    Ok(ToolRegistration::new(
        RegisteredToolId::new(format!("plakous.standard.{stable_suffix}"))?,
        executor_id.clone(),
        ToolExecutorBindingIdentity::new(
            4,
            &[b"standard-coding-tool", executor_id.as_str().as_bytes()],
        )?,
        definition,
        executor,
    )
    .nested_eligible())
}

/// One standard coding capability definition under the shared version and
/// output contract every standard tool carries.
///
/// # Errors
///
/// Returns an invalid-definition error when a built-in invariant cannot freeze.
pub fn standard_definition(
    canonical_name: &'static str,
    description: &'static str,
    schema: Value,
    concurrency: ToolConcurrency,
    effect: ToolEffect,
) -> Result<ToolDefinition, ToolRuntimeError> {
    let name = ToolName::new(canonical_name.to_owned()).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "standard tool name is invalid",
            &error,
        )
    })?;
    ToolDefinition::new(
        name,
        STANDARD_CODING_TOOL_VERSION,
        description,
        ToolInputContract::structured(schema).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "standard tool input contract is invalid",
                &error,
            )
        })?,
        ToolOutputContract::model_content(128, 1_048_576).map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "standard tool output contract is invalid",
                &error,
            )
        })?,
        concurrency,
        effect,
    )
    .map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidDefinition,
            "standard tool definition is invalid",
            &error,
        )
    })
}

/// Exclusive concurrency over the named workspace resources.
///
/// # Errors
///
/// Returns an invalid-definition error when a resource key is not valid.
pub fn workspace_concurrency(keys: &[&str]) -> Result<ToolConcurrency, ToolRuntimeError> {
    keys.iter()
        .map(|key| ToolResourceKey::new((*key).to_owned()))
        .collect::<Result<Vec<_>, _>>()
        .map(ToolConcurrency::ResourceExclusive)
        .map_err(|error| {
            ToolRuntimeError::from_sensitive_source(
                ToolRuntimeErrorKind::InvalidDefinition,
                "standard tool resource identity is invalid",
                &error,
            )
        })
}

/// Stable identity of one concrete plan-state authority configuration.
///
/// The identity is frozen into the update-plan executor registration, so an
/// decision for one state owner cannot be replayed after the owner changes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PlanStateIdentity {
    authority: Arc<str>,
    revision: u64,
}

impl PlanStateIdentity {
    /// Creates a portable nonzero authority revision.
    ///
    /// # Errors
    ///
    /// Returns a registration error for invalid authority text or revision.
    pub fn new(authority: impl Into<Arc<str>>, revision: u64) -> Result<Self, ToolRuntimeError> {
        let authority = authority.into();
        let portable = !authority.is_empty()
            && authority.len() <= 72
            && authority.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
            });
        if !portable || revision == 0 {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Registration,
                "plan-state authority identity is invalid",
            ));
        }
        Ok(Self {
            authority,
            revision,
        })
    }

    /// Returns the stable authority name.
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// Returns the authority configuration revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// The durable executor identity this authority registers `update_plan` under.
    ///
    /// # Errors
    ///
    /// Returns an invalid-identity error when the composed id is not valid.
    #[allow(
        clippy::missing_panics_doc,
        reason = "the format is a valid id by construction"
    )]
    pub fn executor_id(&self) -> Result<ToolExecutorId, ToolRuntimeError> {
        ToolExecutorId::new(format!(
            "plakous.standard.update-plan.v1.{authority}.r{revision}",
            authority = self.authority,
            revision = self.revision,
        ))
    }
}
