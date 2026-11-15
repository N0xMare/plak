//! Route construction and lifecycle observation share one hosted port.
use std::{any::Any, future::Future, pin::Pin, sync::Arc};

use super::{HostBoundary, HostConfig, HostWorkflow, RuntimeRole};
use crate::{
    CliResult,
    agent::{AttachedCodingSession, CodingSessionOpenFailure, ProductAgentSpec, SeededHistory},
    sessions::SessionRecord,
};
use plakous::sessions::{
    SessionInputWindow, SessionLocator, SessionRuntimeBinding, SessionTargetBinding,
};

#[path = "move_compilation.rs"]
mod move_compilation;

#[cfg(any(test, feature = "test-support"))]
pub(super) mod test_rebind;
#[cfg(not(any(test, feature = "test-support")))]
#[path = "factory/test_rebind_unavailable.rs"]
pub(super) mod test_rebind;

/// A construction operation borrows the host only until its result is ready.
pub type RuntimeFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub(super) mod sealed {
    pub trait Sealed {}
}

/// A per-turn lifecycle observer, selected at admission without retaining the prompt.
pub trait TurnObserver: Send {
    /// Observe a lifecycle fact without retaining model input or taking ownership
    /// of the driver. The callback must not replay an effect at a crash cut.
    fn observe(&self, boundary: HostBoundary);
}

/// The host-level construction port is sealed, not an embedder extension point.
/// Only the production factory and the cfg'd scripted test kit implement it.
///
/// ```
/// use plakous_hosted::runtime::{ProductionRuntimeFactory, RuntimeFactory};
/// let _: &dyn RuntimeFactory<plakous_host_protocol::Core> = &ProductionRuntimeFactory;
/// ```
///
/// ```compile_fail,E0277
/// # #![allow(dead_code, unused_variables, unused_imports)]
/// # use std::sync::Arc;
/// # use plakous_hosted::{CliResult, agent::{AttachedCodingSession, CodingSessionOpenFailure, ProductAgentSpec, SeededHistory}, runtime::{RuntimeFactory, RuntimeFuture, RuntimeRole, HostConfig, Sender}, sessions::SessionRecord};
/// # use plakous::sessions::{SessionInputWindow, SessionLocator, SessionRuntimeBinding};
/// struct ExternalFactory;
/// impl RuntimeFactory<plakous_host_protocol::Core> for ExternalFactory {
/// # fn runtime<'a>( &'a self, config: &'a HostConfig<plakous_host_protocol::Core>, role: RuntimeRole, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> { unreachable!() }
/// # fn open_head<'a>( &'a self, config: &'a mut HostConfig<plakous_host_protocol::Core>, head: SessionLocator, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> { unreachable!() }
/// # fn verify_head<'a>( &'a self, config: &'a mut HostConfig<plakous_host_protocol::Core>, head: SessionLocator, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, Result<(), CodingSessionOpenFailure>> { unreachable!() }
/// # fn create<'a>( &'a self, config: &'a mut HostConfig<plakous_host_protocol::Core>, seed: Option<SeededHistory>, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, CliResult<AttachedCodingSession>> { unreachable!() }
/// # fn open<'a>( &'a self, config: &'a HostConfig<plakous_host_protocol::Core>, record: &'a SessionRecord, head: SessionLocator, role: RuntimeRole, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> { unreachable!() }
/// # fn open_rebind_target<'a>( &'a self, config: &'a HostConfig<plakous_host_protocol::Core>, record: &'a SessionRecord, spec: Option<&'a ProductAgentSpec>, locator: SessionLocator, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> { unreachable!() }
/// # fn fork_runtime<'a>( &'a self, config: &'a HostConfig<plakous_host_protocol::Core>, tree_id: &'a str, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> { unreachable!() }
/// # fn rebind_runtime<'a>( &'a self, config: &'a HostConfig<plakous_host_protocol::Core>, spec: Option<&'a ProductAgentSpec>, attach: plakous_hosted::topology_runtime::TopologyHubAttach, native_target: bool, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, (CliResult<SessionRuntimeBinding>, Option<String>)> { unreachable!() }
/// # fn input_window(&self, config: &HostConfig<plakous_host_protocol::Core>) -> CliResult<SessionInputWindow> { unreachable!() }
/// # fn clear<'a>( &'a self, config: &'a mut HostConfig<plakous_host_protocol::Core>, opened: &'a AttachedCodingSession, target_product_session_id: &'a str, create_operation_id: &'a str, stderr_buf: &'a mut Vec<u8>, events: &'a Sender<plakous_host_protocol::Core>, started: std::time::Instant, ) -> RuntimeFuture<'a, CliResult<( AttachedCodingSession, Option<Arc<plakous::subscriptions::ResolvedProvider>>, )>> { unreachable!() }
/// # fn api_runtime<'a>( &'a self, config: &'a HostConfig<plakous_host_protocol::Core>, record: &'a SessionRecord, opening_durable_session: bool, topology_attach: plakous_hosted::topology_runtime::TopologyHubAttach, tree_id: Option<&'a str>, attached_tree: &'a mut Option<String>, ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> { unreachable!() }
/// }
/// ```
pub trait RuntimeFactory<P: plakous_host_protocol::ProtocolExtension>:
    sealed::Sealed + Any + Send + Sync
{
    /// Unsupported host-move compilation against detached semantic topology.
    #[doc(hidden)]
    fn move_runtime<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        _tree: Option<&'a crate::agent_tree::ProductAgentTree>,
        _plan_authority: &'a str,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> {
        self.runtime(config, RuntimeRole::Rebind, stderr)
    }
    /// Compile the exact saved target for this role, without creating a second
    /// turn owner. Construction failures remain errors, never fallback routes.
    fn runtime<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        role: RuntimeRole,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>>;
    /// Revalidate and attach the exact canonical head. Failure retains the
    /// typed open/refusal facts rather than selecting another session.
    fn open_head<'a>(
        &'a self,
        config: &'a mut HostConfig<P>,
        head: SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>>;
    /// Answers whether [`Self::open_head`] of `head` would attach, without
    /// owning, attaching or recovering it.
    fn verify_head<'a>(
        &'a self,
        config: &'a mut HostConfig<P>,
        head: SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<(), CodingSessionOpenFailure>>;
    /// Create a canonical attachment, consuming optional portable history once.
    /// A failed create does not authorize publishing a product locator.
    fn create<'a>(
        &'a self,
        config: &'a mut HostConfig<P>,
        seed: Option<SeededHistory>,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<AttachedCodingSession>>;
    /// Open the supplied record's exact locator under the requested role;
    /// credentials and binding facts are revalidated by the runtime.
    fn open<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        record: &'a SessionRecord,
        head: SessionLocator,
        role: RuntimeRole,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>>;
    /// Open an explicitly selected successor tuple and locator, retaining its
    /// own topology lease rather than reusing the source's execution authority.
    fn open_rebind_target<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        record: &'a SessionRecord,
        spec: Option<&'a ProductAgentSpec>,
        locator: SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>>;
    /// The runtime a fork compiles against its own tree `tree_id`.
    fn fork_runtime<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        tree_id: &'a str,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>>;
    /// Compile a successor against the supplied topology attachment. The
    /// optional returned tree id is construction evidence, not a commit.
    fn rebind_runtime<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        spec: Option<&'a ProductAgentSpec>,
        attach: crate::topology_runtime::TopologyHubAttach,
        native_target: bool,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, (CliResult<SessionRuntimeBinding>, Option<String>)>;
    /// Resolve the exact target's admitted input window, without an inference call.
    ///
    /// # Errors
    /// Refuses missing or inconsistent model facts or an unavailable capability.
    fn input_window(&self, config: &HostConfig<P>) -> CliResult<SessionInputWindow>;
    /// Create the explicit empty successor of a clear operation; the source
    /// attachment remains borrowed until construction returns. The operation id
    /// binds retries to this create, not to a second admitted turn.
    #[allow(
        clippy::too_many_arguments,
        reason = "The runtime factory clear seam receives both the source session authority and the complete new-session construction facts."
    )]
    fn clear<'a>(
        &'a self,
        config: &'a mut HostConfig<P>,
        opened: &'a AttachedCodingSession,
        target_product_session_id: &'a str,
        create_operation_id: &'a str,
        stderr_buf: &'a mut Vec<u8>,
        events: &'a super::Sender<P>,
        started: std::time::Instant,
    ) -> RuntimeFuture<
        'a,
        CliResult<(
            AttachedCodingSession,
            Option<Arc<plakous::subscriptions::ResolvedProvider>>,
        )>,
    >;
    /// Compile an API target with exact account credentials and its own topology
    /// attachment. Opening a durable session revalidates its frozen evidence.
    fn api_runtime<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        record: &'a SessionRecord,
        opening_durable_session: bool,
        topology_attach: crate::topology_runtime::TopologyHubAttach,
        tree_id: Option<&'a str>,
        attached_tree: &'a mut Option<String>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>>;
    /// Scripted target identity, when the factory supplies one. Production
    /// construction derives its identity from the admitted route instead.
    fn target_binding(
        &self,
    ) -> Option<(plakous::subscriptions::NativeProvider, SessionTargetBinding)> {
        None
    }
    /// Observe a workflow cut; the production default has no side effects.
    fn observe(&self, _workflow: HostWorkflow, _boundary: HostBoundary) {}
    /// Select one observer at turn admission; no prompt is retained by default.
    fn turn_observer(&self, _prompt: &str) -> Option<Box<dyn TurnObserver>> {
        None
    }
    /// Install factory-owned construction facts before opening the runtime.
    fn configure(&self, _config: &mut HostConfig<P>) {}
    /// Notify the factory after exact attachment, before the host drives work.
    fn attached(&self, _config: &HostConfig<P>, _opened: &mut AttachedCodingSession) {}
}

/// Registers the production route builders, including their topology leases.
#[derive(Debug)]
pub struct ProductionRuntimeFactory;
impl sealed::Sealed for ProductionRuntimeFactory {}
impl<P: plakous_host_protocol::ProtocolExtension> RuntimeFactory<P> for ProductionRuntimeFactory {
    fn move_runtime<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        tree: Option<&'a crate::agent_tree::ProductAgentTree>,
        plan_authority: &'a str,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> {
        Box::pin(move_compilation::runtime(
            config,
            tree,
            plan_authority,
            stderr,
        ))
    }
    fn configure(&self, config: &mut HostConfig<P>) {
        config.state_roots = config
            .state_roots
            .clone()
            .with_child_runtime_factory(Arc::new(
                crate::agent::children::runtime_factory::ProductionChildRuntimeFactory,
            ));
    }

    fn clear<'a>(
        &'a self,
        config: &'a mut HostConfig<P>,
        opened: &'a AttachedCodingSession,
        target_product_session_id: &'a str,
        create_operation_id: &'a str,
        stderr_buf: &'a mut Vec<u8>,
        events: &'a super::Sender<P>,
        started: std::time::Instant,
    ) -> RuntimeFuture<
        'a,
        CliResult<(
            AttachedCodingSession,
            Option<Arc<plakous::subscriptions::ResolvedProvider>>,
        )>,
    > {
        Box::pin(super::clear::production_build_clear_candidate(
            config,
            opened,
            target_product_session_id,
            create_operation_id,
            stderr_buf,
            events,
            started,
        ))
    }
    fn api_runtime<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        record: &'a SessionRecord,
        opening_durable_session: bool,
        topology_attach: crate::topology_runtime::TopologyHubAttach,
        tree_id: Option<&'a str>,
        attached_tree: &'a mut Option<String>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> {
        Box::pin(super::startup::production_api_runtime_for_record(
            config,
            record,
            opening_durable_session,
            topology_attach,
            tree_id,
            attached_tree,
        ))
    }
    fn runtime<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        role: RuntimeRole,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> {
        Box::pin(super::startup::production_host_runtime_binding(
            config, stderr, role,
        ))
    }
    fn open_head<'a>(
        &'a self,
        config: &'a mut HostConfig<P>,
        head: SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> {
        Box::pin(super::startup::production_open_head(config, head, stderr))
    }
    fn verify_head<'a>(
        &'a self,
        config: &'a mut HostConfig<P>,
        head: SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<(), CodingSessionOpenFailure>> {
        Box::pin(super::startup::production_verify_head(config, head, stderr))
    }
    fn create<'a>(
        &'a self,
        config: &'a mut HostConfig<P>,
        seed: Option<SeededHistory>,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<AttachedCodingSession>> {
        Box::pin(super::startup::production_create_session(
            config, seed, stderr,
        ))
    }
    fn open<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        record: &'a SessionRecord,
        head: SessionLocator,
        role: RuntimeRole,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> {
        Box::pin(super::startup::production_open_session_for_host(
            config, record, head, stderr, role,
        ))
    }
    fn open_rebind_target<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        record: &'a SessionRecord,
        spec: Option<&'a ProductAgentSpec>,
        locator: SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> {
        Box::pin(async move {
            if let Some(spec) = spec {
                crate::agent::open_coding_session(spec.clone(), locator, stderr).await
            } else {
                self.open(config, record, locator, RuntimeRole::Rebind, stderr)
                    .await
            }
        })
    }
    fn fork_runtime<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        tree_id: &'a str,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> {
        Box::pin(super::fork::production_fork_runtime(
            config, tree_id, stderr,
        ))
    }
    fn rebind_runtime<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        spec: Option<&'a ProductAgentSpec>,
        attach: crate::topology_runtime::TopologyHubAttach,
        _native_target: bool,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, (CliResult<SessionRuntimeBinding>, Option<String>)> {
        Box::pin(async move {
            if let Some(spec) = spec {
                let mut attached = None;
                let binding = crate::agent::coding_session_runtime_binding_with_attach(
                    spec.clone(),
                    stderr,
                    attach,
                    &mut attached,
                )
                .await;
                (binding, attached)
            } else {
                (
                    self.runtime(config, RuntimeRole::Rebind, stderr).await,
                    None,
                )
            }
        })
    }
    fn input_window(&self, config: &HostConfig<P>) -> CliResult<SessionInputWindow> {
        super::production_carry_input_window(config)
    }
}

pub(super) fn observe_boundary<P: plakous_host_protocol::ProtocolExtension>(
    factory: &dyn RuntimeFactory<P>,
    workflow: HostWorkflow,
    boundary: HostBoundary,
) {
    factory.observe(workflow, boundary);
}
