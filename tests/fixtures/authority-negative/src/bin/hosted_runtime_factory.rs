#![allow(dead_code, unused_variables, unused_imports)]
use plakous::sessions::{SessionInputWindow, SessionLocator, SessionRuntimeBinding};
use plakous_hosted::{
    CliResult,
    agent::{AttachedCodingSession, CodingSessionOpenFailure, ProductAgentSpec, SeededHistory},
    runtime::{HostConfig, RuntimeFactory, RuntimeFuture, RuntimeRole, Sender},
    sessions::SessionRecord,
};
use std::sync::Arc;
struct ExternalFactory;
impl RuntimeFactory<plakous_host_protocol::Core> for ExternalFactory {
    fn runtime<'a>(
        &'a self,
        config: &'a HostConfig<plakous_host_protocol::Core>,
        role: RuntimeRole,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> {
        unreachable!()
    }
    fn open_head<'a>(
        &'a self,
        config: &'a mut HostConfig<plakous_host_protocol::Core>,
        head: SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> {
        unreachable!()
    }
    fn create<'a>(
        &'a self,
        config: &'a mut HostConfig<plakous_host_protocol::Core>,
        seed: Option<SeededHistory>,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<AttachedCodingSession>> {
        unreachable!()
    }
    fn open<'a>(
        &'a self,
        config: &'a HostConfig<plakous_host_protocol::Core>,
        record: &'a SessionRecord,
        head: SessionLocator,
        role: RuntimeRole,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> {
        unreachable!()
    }
    fn open_rebind_target<'a>(
        &'a self,
        config: &'a HostConfig<plakous_host_protocol::Core>,
        record: &'a SessionRecord,
        spec: Option<&'a ProductAgentSpec>,
        locator: SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, CodingSessionOpenFailure>> {
        unreachable!()
    }
    fn fork_runtime<'a>(
        &'a self,
        config: &'a HostConfig<plakous_host_protocol::Core>,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> {
        unreachable!()
    }
    fn rebind_runtime<'a>(
        &'a self,
        config: &'a HostConfig<plakous_host_protocol::Core>,
        spec: Option<&'a ProductAgentSpec>,
        attach: plakous_hosted::topology_runtime::TopologyHubAttach,
        native_target: bool,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, (CliResult<SessionRuntimeBinding>, Option<String>)> {
        unreachable!()
    }
    fn input_window(
        &self,
        config: &HostConfig<plakous_host_protocol::Core>,
    ) -> CliResult<SessionInputWindow> {
        unreachable!()
    }
    fn clear<'a>(
        &'a self,
        config: &'a mut HostConfig<plakous_host_protocol::Core>,
        opened: &'a AttachedCodingSession,
        target_product_session_id: &'a str,
        create_operation_id: &'a str,
        stderr_buf: &'a mut Vec<u8>,
        events: &'a Sender<plakous_host_protocol::Core>,
        started: std::time::Instant,
    ) -> RuntimeFuture<
        'a,
        CliResult<(
            AttachedCodingSession,
            Option<Arc<plakous::subscriptions::ResolvedProvider>>,
        )>,
    > {
        unreachable!()
    }
    fn api_runtime<'a>(
        &'a self,
        config: &'a HostConfig<plakous_host_protocol::Core>,
        record: &'a SessionRecord,
        opening_durable_session: bool,
        topology_attach: plakous_hosted::topology_runtime::TopologyHubAttach,
        tree_id: Option<&'a str>,
        attached_tree: &'a mut Option<String>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> {
        unreachable!()
    }
}

fn main() {}
