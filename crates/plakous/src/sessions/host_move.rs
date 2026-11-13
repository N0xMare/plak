//! Unsupported host-only move authority; not a supported embedding API or wire codec.
use super::{AgentSessions, AgentSessionsError, BranchHold, store_error};
pub use plakous_agent::internal::SessionWorkflowId as MoveOperation;
use plakous_agent::internal::host_move::PrepareMove;
pub use plakous_agent::internal::host_move::{
    ImportedCompactionFacts, ImportedCompactionSelection, MoveBranch, MoveDisposition, MoveInstall,
    MoveInterpreter, MoveLine, MoveSession, ProcessSessionId as MoveProcessId,
    ProcessSessionOwnerGeneration as MoveProcessGeneration, ReconstructedMoveHead,
};
pub use plakous_agent::internal::{
    ForkLineage as MoveFork, JobLedger as MoveJobs, JobRow as MoveJob,
    JobTerminal as MoveJobTerminal, JobTerminalStatus as MoveJobStatus,
    LineJobTerminal as MoveLineTerminal, LineOrigin as MoveLineOrigin,
    LineRecord as MoveLineRecord, RuntimeCompactionStrategy as MoveCompactionStrategy,
    RuntimeDurability as MoveDurability, SessionHead as MoveSessionHeader,
    SessionRef as MoveSessionRef, SessionRevision as MoveSessionRevision,
};
use plakous_model::Digest;
#[doc(hidden)]
pub use plakous_tools::NestedToolEligibility as MoveToolNesting;

/// Unsupported target-owned semantic compilation request; contains no source grant.
#[doc(hidden)]
pub struct MoveHeadRequest {
    pub operation: MoveOperation,
    pub source: super::SessionLocator,
    pub source_revision: u64,
    pub source_fork: Option<plakous_agent::internal::ForkLineage>,
    pub fence_floor: u64,
    pub line: plakous_model::LineId,
    pub generation: u64,
    pub binding: MoveBinding,
    pub history: super::PortableSessionHistory,
    pub jobs: MoveJobs,
    pub compaction: Option<ImportedCompactionFacts>,
    pub interpreter: Option<MoveInterpreter>,
}

/// Source-selected configuration facts, never historical admission evidence.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MoveBinding {
    pub protocol: plakous_model::ProtocolId,
    pub route: plakous_model::RouteId,
    pub model: plakous_model::ModelId,
    pub selected: plakous_model::SelectedProviderConfigurationId,
    pub effective: plakous_model::EffectiveProviderConfigurationId,
    pub account: String,
}

/// Source-owned semantic facts. Never serialize the native checkpoint supplied here.
#[doc(hidden)]
pub struct MoveHeadFacts {
    pub binding: MoveBinding,
    pub history: super::PortableSessionHistory,
    pub compaction: Option<ImportedCompactionFacts>,
    pub jobs: MoveJobs,
    pub presentation: super::SemanticSessionPresentation,
    pub interpreter: Option<MoveInterpreter>,
}

impl MoveHeadFacts {
    /// Projects the old-key producer's checkpoint into the approved semantic families.
    ///
    /// # Errors
    /// Refuses incomplete source presentation, nonterminal jobs or malformed named state.
    pub fn capture(
        checkpoint: &plakous_agent::internal::RuntimeCheckpoint,
        terminals: &[MoveLineTerminal],
    ) -> Result<Self, AgentSessionsError> {
        let binding = MoveBinding::from_checkpoint(checkpoint);
        let manifest = checkpoint.manifest();
        let values = manifest
            .named_state()
            .values()
            .iter()
            .map(|(key, bytes)| {
                serde_json::from_slice(bytes)
                    .map(|value| (key.clone(), value))
                    .map_err(|_| invalid_source("invalid source named state"))
            })
            .collect::<Result<_, _>>()?;
        let history = super::PortableSessionHistory::new(
            manifest.canonical().history().segments().to_vec(),
            binding.account.clone(),
            binding.model.clone(),
        )
        .with_named_state(values)?;
        let jobs = manifest
            .jobs()
            .with_line_terminals(terminals)
            .map_err(|_| invalid_source("invalid source job evidence"))?;
        if jobs.rows().iter().any(|job| job.terminal.is_none()) {
            return Err(invalid_source("source still owns nonterminal jobs"));
        }
        let compaction = manifest
            .canonical()
            .context()
            .export_compaction_selection(checkpoint.id(), manifest.canonical().history())
            .map_err(|_| invalid_source("invalid source compaction selection"))?;
        let presentation =
            super::FrozenSessionPresentation::from_checkpoint(checkpoint)?.semantic()?;
        Ok(Self {
            binding,
            history,
            compaction,
            jobs,
            presentation,
            interpreter: manifest
                .interpreter_observation()
                .map(|state| MoveInterpreter {
                    generation: state.generation(),
                    affected_cell: state.affected_cell().map(str::to_owned),
                }),
        })
    }
}

impl MoveBinding {
    /// Projects non-secret semantic identities from the producer's own checkpoint.
    #[must_use]
    pub fn from_checkpoint(checkpoint: &plakous_agent::internal::RuntimeCheckpoint) -> Self {
        let manifest = checkpoint.manifest();
        Self {
            protocol: manifest.binding().protocol().clone(),
            route: manifest.binding().route().clone(),
            model: manifest.binding().requested_model().clone(),
            selected: manifest.provider_configuration().selected_identity(),
            effective: manifest.provider_configuration().effective_identity(),
            account: manifest.binding().account().subject().to_owned(),
        }
    }

    pub(super) fn from_runtime(runtime: &super::SessionRuntimeBinding) -> Self {
        let (protocol, route, model, selected, effective, account) =
            plakous_agent::internal::facade_support::agent_builder_target_authority(
                &runtime.builder,
            );
        Self {
            protocol,
            route,
            model,
            selected,
            effective,
            account: plakous_model::hex_lower(account.bytes()),
        }
    }
}

impl AgentSessions {
    /// A host-only, memory-backed compiler with no persistent store or execution driver.
    /// The requested durability must still match the freshly compiled configuration.
    ///
    /// # Errors
    /// Refuses an unavailable memory backend or invalid runtime construction.
    #[doc(hidden)]
    pub fn move_preflight_compiler(durability: MoveDurability) -> Result<Self, AgentSessionsError> {
        let store =
            plakous_store::MemorySessionStore::new(plakous_store::MemoryStoreLimits::default())
                .map_err(store_error)?;
        Self::new(super::SessionBackend::from_store(
            std::sync::Arc::new(store),
            durability,
        ))
    }

    /// Host-only withdrawal of a Published group before Ready and before any driver open.
    /// The caller holds its durable never-Ready receipt lock for the whole call.
    ///
    /// # Errors
    /// Refuses stale/incomplete holds, changed heads, or any execution-open marker.
    #[doc(hidden)]
    pub async fn abandon_unopened_move(
        &self,
        operation: MoveOperation,
        digest: Digest,
        holds: &[&BranchHold],
    ) -> Result<(), AgentSessionsError> {
        self.backend
            .abandon_unopened_move(plakous_agent::internal::host_move::AbandonUnopenedMove {
                operation,
                digest,
                leases: holds.iter().map(|hold| hold.inner.move_lease()).collect(),
            })
            .await
            .map_err(store_error)
    }
    /// Rebuilds a dormant head under exact target replay scope and fresh authority.
    /// No driver, store mutation, lease, provider dispatch or tool dispatch is created.
    ///
    /// # Errors
    /// Refuses binding/replay drift, invalid facts, overflowing floors or live jobs.
    #[doc(hidden)]
    pub async fn reconstruct_move_head(
        &self,
        runtime: super::SessionRuntimeBinding,
        request: MoveHeadRequest,
    ) -> Result<ReconstructedMoveHead, AgentSessionsError> {
        let current = MoveBinding::from_runtime(&runtime);
        if request.binding != current {
            let changed = [
                ("protocol", request.binding.protocol != current.protocol),
                ("route", request.binding.route != current.route),
                ("model", request.binding.model != current.model),
                (
                    "selected configuration",
                    request.binding.selected != current.selected,
                ),
                (
                    "effective configuration",
                    request.binding.effective != current.effective,
                ),
                ("account", request.binding.account != current.account),
            ]
            .into_iter()
            .filter_map(|(field, changed)| changed.then_some(field))
            .collect::<Vec<_>>()
            .join(", ");
            return Err(AgentSessionsError::new(
                super::AgentSessionsErrorKind::InvalidInput,
                format!("host move source binding changed: {changed}"),
            ));
        }
        if !request.history.retains_replay_for(&runtime) {
            return Err(AgentSessionsError::new(
                super::AgentSessionsErrorKind::InvalidInput,
                "host move requires the exact source replay account and model",
            ));
        }
        plakous_agent::internal::facade_support::validate_seed_replay(
            &runtime.builder,
            request.history.segments(),
        )
        .await
        .map_err(|error| super::runtime_error(&error))?;
        let named_state = request.history.named_state.clone();
        let history = request.history.into_canonical(true)?;
        self.runtime
            .reconstruct_move_head(
                runtime.builder,
                plakous_agent::internal::host_move::MoveHeadSeed {
                    operation: request.operation,
                    source: request.source.session_ref(),
                    source_checkpoint: request.source.checkpoint_id(),
                    source_fork: request.source_fork,
                    source_revision: plakous_agent::internal::BranchRevision::from_store(
                        request.source_revision,
                    ),
                    fence_floor: request.fence_floor,
                    line: request.line,
                    generation: request.generation,
                    history,
                    named_state,
                    jobs: request.jobs,
                    compaction: request.compaction,
                    interpreter: request.interpreter,
                },
            )
            .await
            .map_err(|error| super::runtime_error(&error))
    }

    /// Recover a frozen batch after acknowledgement loss or restart, without a new grant.
    ///
    /// # Errors
    /// Refuses unknown, disposed or mismatching batches.
    #[doc(hidden)]
    pub async fn recover_move(
        &self,
        operation: MoveOperation,
    ) -> Result<MoveInstall, AgentSessionsError> {
        self.backend
            .recover_move(operation)
            .await
            .map_err(store_error)
    }
    /// Freeze exact held branches and return semantic state without creating a driver.
    ///
    /// # Errors
    /// Refuses incomplete holds, unresolved runtime boundaries or incompatible identities.
    #[doc(hidden)]
    pub async fn prepare_move(
        &self,
        operation: MoveOperation,
        holds: &[&BranchHold],
    ) -> Result<MoveInstall, AgentSessionsError> {
        self.backend
            .prepare_move(PrepareMove {
                operation,
                leases: holds.iter().map(|h| h.inner.move_lease()).collect(),
            })
            .await
            .map_err(store_error)
    }

    /// Stage a reconstructed batch invisibly, with imported floors and no lease.
    ///
    /// # Errors
    /// Refuses conflicting identities, invalid semantics or exhausted destination capacity.
    #[doc(hidden)]
    pub async fn stage_move(
        &self,
        operation: MoveOperation,
        install: MoveInstall,
    ) -> Result<(), AgentSessionsError> {
        self.backend
            .stage_move(operation, install)
            .await
            .map_err(store_error)
    }

    /// Resolve only the matching operation/digest. Committed retirement cannot abort.
    ///
    /// # Errors
    /// Refuses mismatching operations/digests and invalid or irreversible transitions.
    #[doc(hidden)]
    pub async fn finish_move(
        &self,
        operation: MoveOperation,
        digest: Digest,
        disposition: MoveDisposition,
    ) -> Result<(), AgentSessionsError> {
        self.backend
            .finish_move(operation, digest, disposition)
            .await
            .map_err(store_error)
    }
}

fn invalid_source(message: &str) -> AgentSessionsError {
    AgentSessionsError::new(super::AgentSessionsErrorKind::InvalidInput, message)
}
