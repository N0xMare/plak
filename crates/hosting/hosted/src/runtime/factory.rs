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

/// A construction operation borrows the host only until its result is ready.
pub type RuntimeFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub(super) mod sealed {
    pub trait Sealed {}
}

/// A per-turn lifecycle observer, selected at admission without retaining the prompt.
pub trait TurnObserver: Send {
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
/// # fn runtime( &self, config: &HostConfig<plakous_host_protocol::Core>, role: RuntimeRole, stderr: &mut Vec<u8>, ) -> CliResult<SessionRuntimeBinding> { unreachable!() }
/// # fn open_head<'a>( &'a self, config: &'a mut HostConfig<plakous_host_protocol::Core>, head: SessionLocator, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> { unreachable!() }
/// # fn create<'a>( &'a self, config: &'a mut HostConfig<plakous_host_protocol::Core>, seed: Option<SeededHistory>, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, CliResult<AttachedCodingSession>> { unreachable!() }
/// # fn open<'a>( &'a self, config: &'a HostConfig<plakous_host_protocol::Core>, record: &'a SessionRecord, head: SessionLocator, role: RuntimeRole, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> { unreachable!() }
/// # fn open_rebind_target<'a>( &'a self, config: &'a HostConfig<plakous_host_protocol::Core>, record: &'a SessionRecord, spec: Option<&'a ProductAgentSpec>, locator: SessionLocator, stderr: &'a mut Vec<u8>, ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> { unreachable!() }
/// # fn fork_runtime( &self, executor: &plakous::application::ApplicationExecutor, config: &HostConfig<plakous_host_protocol::Core>, stderr: &mut Vec<u8>, ) -> CliResult<SessionRuntimeBinding> { unreachable!() }
/// # fn rebind_runtime( &self, config: &HostConfig<plakous_host_protocol::Core>, spec: Option<&ProductAgentSpec>, attach: plakous_hosted::topology_runtime::TopologyHubAttach, native_target: bool, stderr: &mut Vec<u8>, ) -> (CliResult<SessionRuntimeBinding>, Option<String>) { unreachable!() }
/// # fn input_window(&self, config: &HostConfig<plakous_host_protocol::Core>) -> CliResult<SessionInputWindow> { unreachable!() }
/// # fn clear( &self, executor: &plakous::application::ApplicationExecutor, config: &mut HostConfig<plakous_host_protocol::Core>, opened: &AttachedCodingSession, target_product_session_id: &str, create_operation_id: &str, stderr_buf: &mut Vec<u8>, events: &Sender<plakous_host_protocol::Core>, started: std::time::Instant, ) -> CliResult<( AttachedCodingSession, Option<Arc<plakous::subscriptions::ResolvedProvider>>, )> { unreachable!() }
/// # fn api_runtime<'a>( &'a self, config: &'a HostConfig<plakous_host_protocol::Core>, record: &'a SessionRecord, opening_durable_session: bool, topology_attach: plakous_hosted::topology_runtime::TopologyHubAttach, tree_id: Option<&'a str>, attached_tree: &'a mut Option<String>, ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> { unreachable!() }
/// }
/// ```
pub trait RuntimeFactory<P: plakous_host_protocol::ProtocolExtension>:
    sealed::Sealed + Any + Send + Sync
{
    fn runtime(
        &self,
        config: &HostConfig<P>,
        role: RuntimeRole,
        stderr: &mut Vec<u8>,
    ) -> CliResult<SessionRuntimeBinding>;
    fn open_head<'a>(
        &'a self,
        config: &'a mut HostConfig<P>,
        head: SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>>;
    fn create<'a>(
        &'a self,
        config: &'a mut HostConfig<P>,
        seed: Option<SeededHistory>,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<AttachedCodingSession>>;
    fn open<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        record: &'a SessionRecord,
        head: SessionLocator,
        role: RuntimeRole,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>>;
    fn open_rebind_target<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        record: &'a SessionRecord,
        spec: Option<&'a ProductAgentSpec>,
        locator: SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>>;
    fn fork_runtime(
        &self,
        executor: &plakous::application::ApplicationExecutor,
        config: &HostConfig<P>,
        stderr: &mut Vec<u8>,
    ) -> CliResult<SessionRuntimeBinding>;
    fn rebind_runtime(
        &self,
        config: &HostConfig<P>,
        spec: Option<&ProductAgentSpec>,
        attach: crate::topology_runtime::TopologyHubAttach,
        native_target: bool,
        stderr: &mut Vec<u8>,
    ) -> (CliResult<SessionRuntimeBinding>, Option<String>);
    fn input_window(&self, config: &HostConfig<P>) -> CliResult<SessionInputWindow>;
    fn clear(
        &self,
        executor: &plakous::application::ApplicationExecutor,
        config: &mut HostConfig<P>,
        opened: &AttachedCodingSession,
        target_product_session_id: &str,
        create_operation_id: &str,
        stderr_buf: &mut Vec<u8>,
        events: &super::Sender<P>,
        started: std::time::Instant,
    ) -> CliResult<(
        AttachedCodingSession,
        Option<Arc<plakous::subscriptions::ResolvedProvider>>,
    )>;
    fn api_runtime<'a>(
        &'a self,
        config: &'a HostConfig<P>,
        record: &'a SessionRecord,
        opening_durable_session: bool,
        topology_attach: crate::topology_runtime::TopologyHubAttach,
        tree_id: Option<&'a str>,
        attached_tree: &'a mut Option<String>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>>;
    fn target_binding(
        &self,
    ) -> Option<(plakous::subscriptions::NativeProvider, SessionTargetBinding)> {
        None
    }
    fn observe(&self, _workflow: HostWorkflow, _boundary: HostBoundary) {}
    fn turn_observer(&self, _prompt: &str) -> Option<Box<dyn TurnObserver>> {
        None
    }
    fn configure(&self, _config: &mut HostConfig<P>) {}
    fn attached(&self, _config: &HostConfig<P>, _opened: &mut AttachedCodingSession) {}
}

/// Registers the production route builders, including their topology leases.
pub struct ProductionRuntimeFactory;
impl sealed::Sealed for ProductionRuntimeFactory {}
impl<P: plakous_host_protocol::ProtocolExtension> RuntimeFactory<P> for ProductionRuntimeFactory {
    fn configure(&self, config: &mut HostConfig<P>) {
        config.state_roots = config
            .state_roots
            .clone()
            .with_child_runtime_factory(Arc::new(
                crate::agent::children::runtime_factory::ProductionChildRuntimeFactory,
            ));
    }

    fn clear(
        &self,
        executor: &plakous::application::ApplicationExecutor,
        config: &mut HostConfig<P>,
        opened: &AttachedCodingSession,
        target_product_session_id: &str,
        create_operation_id: &str,
        stderr_buf: &mut Vec<u8>,
        events: &super::Sender<P>,
        started: std::time::Instant,
    ) -> CliResult<(
        AttachedCodingSession,
        Option<Arc<plakous::subscriptions::ResolvedProvider>>,
    )> {
        super::clear::production_build_clear_candidate(
            executor,
            config,
            opened,
            target_product_session_id,
            create_operation_id,
            stderr_buf,
            events,
            started,
        )
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
    fn runtime(
        &self,
        config: &HostConfig<P>,
        role: RuntimeRole,
        stderr: &mut Vec<u8>,
    ) -> CliResult<SessionRuntimeBinding> {
        super::startup::production_host_runtime_binding(config, stderr, role)
    }
    fn open_head<'a>(
        &'a self,
        config: &'a mut HostConfig<P>,
        head: SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> {
        Box::pin(super::startup::production_open_head(config, head, stderr))
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
    fn fork_runtime(
        &self,
        executor: &plakous::application::ApplicationExecutor,
        config: &HostConfig<P>,
        stderr: &mut Vec<u8>,
    ) -> CliResult<SessionRuntimeBinding> {
        super::fork::production_fork_runtime(executor, config, stderr)
    }
    fn rebind_runtime(
        &self,
        config: &HostConfig<P>,
        spec: Option<&ProductAgentSpec>,
        attach: crate::topology_runtime::TopologyHubAttach,
        _native_target: bool,
        stderr: &mut Vec<u8>,
    ) -> (CliResult<SessionRuntimeBinding>, Option<String>) {
        if let Some(spec) = spec {
            let mut attached = None;
            let binding = crate::agent::coding_session_runtime_binding_with_attach(
                spec.clone(),
                stderr,
                attach,
                &mut attached,
            );
            (binding, attached)
        } else {
            (self.runtime(config, RuntimeRole::Rebind, stderr), None)
        }
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
