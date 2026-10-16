//! Inspect and measure the current authoritative context.

use crate::runtime::PromptCommand;
use crate::runtime::admission::validate_configuration_runtime_authority;
use crate::runtime::context::{context_status_projection, context_status_request_ids};
use crate::runtime::driver::Driver;
use crate::runtime::driver::turn_position::turn_position;
use crate::{AgentContextStatusRequest, AgentSessionEndpointError, RuntimeLifecycle};
use plakous_model::{
    ContextStatus, ConversationView, ModelPreparationContext, ModelRequest, RequestMetadata,
    ValidationMode,
};
use std::sync::Arc;
impl Driver {
    pub(super) async fn inspect_context_status(
        &mut self,
        active: Option<&PromptCommand>,
        request: AgentContextStatusRequest,
    ) -> Result<ContextStatus, AgentSessionEndpointError> {
        let checkpoint = self.authority.checkpoint();
        let manifest = checkpoint.manifest();
        let topology = manifest.topology();
        if (
            topology.session_id,
            topology.branch_id,
            topology.checkpoint_id,
        ) != (request.session_id, request.branch_id, request.checkpoint_id)
        {
            return Err(AgentSessionEndpointError::Stale {
                session_id: topology.session_id,
                branch_id: topology.branch_id,
                checkpoint_id: topology.checkpoint_id,
            });
        }
        if active.is_some() || !matches!(manifest.lifecycle(), RuntimeLifecycle::Quiescent) {
            return Err(AgentSessionEndpointError::Busy);
        }
        let pending = request.pending_user_input.as_ref();
        let notice = if pending.is_some() {
            self.process_notices()
                .map_err(|_| AgentSessionEndpointError::Unavailable)?
                .map(|(segment, _)| segment)
        } else {
            None
        };
        let input = context_status_projection(&checkpoint, pending, notice)?;
        // The measured request is the pending prompt's turn, so its position
        // counts that prompt.
        let position = turn_position(input.history());
        let (turn_id, attempt_id) = context_status_request_ids(request.checkpoint_id, pending)?;
        let prepared_at = self.clock.now();
        let model_request = ModelRequest::from_parts(
            self.stable_request_prefix.clone(),
            input,
            RequestMetadata::for_turn(self.run_id, turn_id, attempt_id, prepared_at),
        )
        .map_err(|_| AgentSessionEndpointError::InvalidInput)?;
        let descriptor = self.provider_configuration.effective().descriptor().clone();
        let plan = descriptor
            .validate_request(model_request, ValidationMode::Strict)
            .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        descriptor
            .preflight_context_admission_requirement(plan.request().context_admission_requirement())
            .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        let authority = Arc::clone(&self.model)
            .revalidate_runtime_authority(prepared_at)
            .await
            .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        validate_configuration_runtime_authority(
            &self.provider_configuration,
            &descriptor,
            &authority,
            prepared_at,
        )
        .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        let (active_family_artifact, cache_affinity) = self
            .active_model_preparation_state()
            .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        let mut preparation_context = ModelPreparationContext::new(authority, prepared_at)
            .with_cache_affinity(cache_affinity);
        preparation_context = self
            .with_previous_response_metadata(&plan, preparation_context)
            .map_err(|_| AgentSessionEndpointError::Unavailable)?
            .with_turn_position(position);
        if let Some(artifact) = active_family_artifact {
            preparation_context = preparation_context.with_active_family_artifact(artifact);
        }
        let prepared = match Arc::clone(&self.model).prepare_call(plan.clone(), preparation_context)
        {
            Ok(prepared) => prepared,
            Err(error) => {
                return error
                    .context_status()
                    .cloned()
                    .map(|status| status.with_context_anchor(self.context_anchor))
                    .ok_or(AgentSessionEndpointError::Unavailable);
            }
        };
        let record = prepared.record();
        let status = descriptor
            .context_status(&plan, record.projection(), prepared_at)
            .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        if status.identity() != record.context_status_identity() {
            return Err(AgentSessionEndpointError::Unavailable);
        }
        // The byte split of the body the measurement counted and the anchor
        // occupancy is measured from ride along for presentation; both are
        // outside the identity just checked.
        Ok(status
            .with_input_parts(record.projection().token_projection().input_parts())
            .with_context_anchor(self.context_anchor))
    }

    pub(super) async fn measure_live_context_status(
        &mut self,
    ) -> Result<ContextStatus, AgentSessionEndpointError> {
        let checkpoint = self.authority.checkpoint();
        let input = context_status_projection(&checkpoint, None, None)?;
        self.measure_context_input(input).await
    }

    /// Measures `input` exactly as a turn's request over it would be checked.
    pub(super) async fn measure_context_input(
        &mut self,
        input: ConversationView,
    ) -> Result<ContextStatus, AgentSessionEndpointError> {
        let position = turn_position(input.history());
        let (turn_id, attempt_id) =
            context_status_request_ids(self.authority.checkpoint().id(), None)?;
        let prepared_at = self.clock.now();
        let model_request = ModelRequest::from_parts(
            self.stable_request_prefix.clone(),
            input,
            RequestMetadata::for_turn(self.run_id, turn_id, attempt_id, prepared_at),
        )
        .map_err(|_| AgentSessionEndpointError::InvalidInput)?;
        let descriptor = self.provider_configuration.effective().descriptor().clone();
        let plan = descriptor
            .validate_request(model_request, ValidationMode::Strict)
            .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        descriptor
            .preflight_context_admission_requirement(plan.request().context_admission_requirement())
            .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        let authority = Arc::clone(&self.model)
            .revalidate_runtime_authority(prepared_at)
            .await
            .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        validate_configuration_runtime_authority(
            &self.provider_configuration,
            &descriptor,
            &authority,
            prepared_at,
        )
        .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        let (active_family_artifact, cache_affinity) = self
            .active_model_preparation_state()
            .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        let mut preparation_context = ModelPreparationContext::new(authority, prepared_at)
            .with_cache_affinity(cache_affinity);
        preparation_context = self
            .with_previous_response_metadata(&plan, preparation_context)
            .map_err(|_| AgentSessionEndpointError::Unavailable)?
            .with_turn_position(position);
        if let Some(artifact) = active_family_artifact {
            preparation_context = preparation_context.with_active_family_artifact(artifact);
        }
        let prepared = match Arc::clone(&self.model).prepare_call(plan.clone(), preparation_context)
        {
            Ok(prepared) => prepared,
            Err(error) => {
                return error
                    .context_status()
                    .cloned()
                    .ok_or(AgentSessionEndpointError::Unavailable);
            }
        };
        let record = prepared.record();
        let status = descriptor
            .context_status(&plan, record.projection(), prepared_at)
            .map_err(|_| AgentSessionEndpointError::Unavailable)?;
        if status.identity() != record.context_status_identity() {
            return Err(AgentSessionEndpointError::Unavailable);
        }
        // The byte split of the body the measurement counted rides along for
        // presentation; it is outside the identity just checked.
        Ok(status
            .with_input_parts(record.projection().token_projection().input_parts())
            .with_context_anchor(self.context_anchor))
    }
}
