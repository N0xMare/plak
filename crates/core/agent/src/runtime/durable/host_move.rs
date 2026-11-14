//! Compile a new immutable head from semantic facts without opening a driver.

use super::{DurableGenesisIdentities, DurableSessionRuntime};
use crate::runtime::admission::sealed_runtime_admission;
use crate::runtime::continuation::runtime_manifest_error;
use crate::runtime::durable::bootstrap::{lowercase_hex, prepare_durable_genesis};
use crate::runtime::durable::branch::validate_durable_builder;
use crate::session::host_move::{MoveBranch, MoveHeadSeed, ReconstructedMoveHead};
use crate::{
    AgentBuilder, AgentError, AgentErrorKind, RuntimeCanonicalState, RuntimeCheckpoint,
    RuntimeLifecycle, RuntimeManifest, RuntimeOperationLedger,
};
use plakous_model::WorkingProjection;

impl DurableSessionRuntime {
    /// Compiles under fresh destination binding/admission, without a lease or dispatch.
    /// Retrying an installed move recovers its staged batch rather than recompiling it.
    ///
    /// # Errors
    /// Refuses invalid/overflowing floors, jobs outside the cut, or incompatible inputs.
    #[doc(hidden)]
    pub async fn reconstruct_move_head(
        &self,
        builder: AgentBuilder,
        seed: MoveHeadSeed,
    ) -> Result<ReconstructedMoveHead, AgentError> {
        validate_durable_builder(&builder)?;
        let revision = seed.source_revision.checked_next().ok_or_else(invalid)?;
        if seed.fence_floor == 0
            || seed.fence_floor == u64::MAX
            || seed.generation == 0
            || seed.generation == u64::MAX
            || builder.line_carry.is_some()
            || !seed.job_cut.permits(&seed.jobs)
            || seed.jobs.rows().iter().any(|row| {
                row.owner.line != seed.line
                    || row.owner.generation != seed.generation
                    || row.owner.session_id != seed.source.session_id()
                    || row.owner.branch_id != seed.source.branch_id()
                    || row.owner.fence > seed.fence_floor
            })
        {
            return Err(invalid());
        }
        let operation = format!(
            "host-move:{}:{}:{}:{}",
            lowercase_hex(seed.operation.as_bytes()),
            seed.source.session_id(),
            seed.source.branch_id(),
            seed.source_checkpoint
        );
        let mut ids = DurableGenesisIdentities::new(&operation)?;
        ids.session_id = seed.source.session_id();
        ids.branch_id = seed.source.branch_id();
        let admission = sealed_runtime_admission(&builder).await?;
        let fresh = prepare_durable_genesis(
            &builder,
            ids,
            admission,
            self.durability,
            seed.history.clone(),
        )?;
        let checkpoint = reconstruct(&fresh, &seed, revision.get())?;
        Ok(ReconstructedMoveHead {
            branch: MoveBranch {
                session: seed.source,
                revision,
                checkpoint: checkpoint.id(),
                fence_floor: seed.fence_floor,
            },
            checkpoint,
        })
    }
}

fn reconstruct(
    fresh: &RuntimeCheckpoint,
    seed: &MoveHeadSeed,
    revision: u64,
) -> Result<RuntimeCheckpoint, AgentError> {
    let manifest = fresh.manifest();
    let mut topology = manifest.topology();
    topology.parent_checkpoint_id = Some(seed.source_checkpoint);
    topology.fork = seed.source_fork;
    topology.branch_revision = revision;
    topology.line = seed.line;
    topology.line_generation = seed.generation;
    topology.lease_fence = seed.fence_floor;
    let mut context = manifest.canonical().context().clone();
    if let Some(compaction) = &seed.compaction {
        context = context
            .with_imported_compaction(compaction.clone(), &seed.history)
            .map_err(runtime_manifest_error)?;
    } else if seed
        .history
        .segments()
        .iter()
        .any(|segment| segment.kind() == plakous_model::HistorySegmentKind::Compaction)
    {
        return Err(invalid());
    }
    let canonical = RuntimeCanonicalState::new(
        manifest.canonical().stable_prefix().clone(),
        seed.history.clone(),
        WorkingProjection::default(),
        context,
    )
    .map_err(runtime_manifest_error)?;
    let rebuilt = RuntimeManifest::new(
        manifest.provenance().clone(),
        topology,
        RuntimeLifecycle::Quiescent,
        manifest.harness_plan_component().clone(),
        manifest.provider_configuration().clone(),
        canonical,
        manifest.binding().clone(),
        manifest.tools().clone(),
        RuntimeOperationLedger::new(Vec::new(), Vec::new()).map_err(runtime_manifest_error)?,
        None,
        seed.interpreter
            .as_ref()
            .map(crate::session::host_move::MoveInterpreter::observation)
            .transpose()
            .map_err(runtime_manifest_error)?,
        seed.jobs.clone(),
        seed.named_state.clone(),
        manifest.durability(),
    )
    .map_err(runtime_manifest_error)?;
    RuntimeCheckpoint::new(rebuilt).map_err(runtime_manifest_error)
}

fn invalid() -> AgentError {
    AgentError::new(
        AgentErrorKind::InvalidInput,
        "move cannot reconstruct these identities, floors, compaction facts or job cut",
    )
}
