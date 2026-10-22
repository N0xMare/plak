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

/// The route binder selected at the composition root. Real and scripted routes
/// enter the same lifecycle; neither admission nor publication selects a test path.
pub trait RuntimeFactory<P: plakous_host_protocol::ProtocolExtension>: Any + Send + Sync {
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
    fn observe_turn_boundary(&self, _prompt: &str, _boundary: HostBoundary) {}
    fn configure(&self, _config: &mut HostConfig<P>) {}
    fn attached(&self, _config: &HostConfig<P>, _opened: &mut AttachedCodingSession) {}
}

/// Registers the production route builders, including their topology leases.
pub struct ProductionRuntimeFactory;
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
