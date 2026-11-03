//! The product's one child controller over the hosted child services.

use std::pin::Pin;

use super::super::recovery::ProductChildRecoverer;
use super::{ProductChildSessionBinder, ProductLiveCanceller, ProductMailDeliverer};

/// The product's child controller: each child service's hosted
/// implementation behind the one port the tree's tools hold.
pub(in crate::agent) struct ProductChildController {
    pub(in crate::agent) binder: ProductChildSessionBinder,
    pub(in crate::agent) deliverer: ProductMailDeliverer,
    pub(in crate::agent) canceller: ProductLiveCanceller,
    pub(in crate::agent) recoverer: ProductChildRecoverer,
    /// A root runtime built against the hub without a session attachment
    /// keeps the tree resident while its tools live.
    pub(in crate::agent) _residency: Option<crate::topology_runtime::HubResidency>,
}

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl crate::agent_tree_tools::ChildController for ProductChildController {
    fn services(&self) -> crate::agent_tree_tools::ChildServices {
        crate::agent_tree_tools::ChildServices::ALL
    }

    fn admit_spawn(
        &self,
        caller_id: &str,
        request: &crate::agent_tree_protocol::SpawnAgentRequest,
    ) -> Result<(), crate::agent_tree::TopologyError> {
        self.binder.admit_spawn(caller_id, request)
    }

    fn admit_binding<'a>(
        &'a self,
        template: &'a crate::agent_tree_protocol::TemplateName,
        binding: &'a crate::orchestration::RosterBinding,
    ) -> Boxed<'a, Result<(), crate::agent_tree::TopologyError>> {
        self.binder.admit_binding(template, binding)
    }

    fn bind(
        &self,
        start: crate::agent_tree_tools::SpawnStart,
    ) -> Boxed<
        '_,
        Result<crate::agent_tree_tools::SpawnBound, crate::agent_tree_tools::SpawnBindError>,
    > {
        self.binder.bind(start)
    }

    fn deliver(
        &self,
        request: crate::agent_tree_tools::MailDeliveryRequest,
    ) -> Boxed<
        '_,
        Result<
            crate::agent_tree_tools::MailDeliveryAck,
            crate::agent_tree_tools::MailDeliveryFailure,
        >,
    > {
        self.deliverer.deliver(request)
    }

    fn withdraw_followup(
        &self,
        driver: crate::agent_tree_tools::TreeDriver,
        agent_id: String,
        message_id: String,
        receipt: Option<(String, crate::agent_tree_protocol::TopologyErrorCode)>,
    ) -> Boxed<'_, Result<(), crate::agent_tree::TopologyError>> {
        self.deliverer
            .withdraw_followup(driver, agent_id, message_id, receipt)
    }

    fn cancel_agents(
        &self,
        agent_ids: Vec<String>,
        signalled: tokio::sync::oneshot::Sender<()>,
    ) -> Boxed<
        '_,
        Result<
            crate::agent_tree_tools::LiveCancelOutcome,
            crate::agent_tree_tools::LiveCancelFailure,
        >,
    > {
        self.canceller.cancel_agents(agent_ids, signalled)
    }

    fn recover(
        &self,
        agent_id: String,
        expected_attempt_id: Option<crate::agent_tree_protocol::AttemptId>,
    ) -> Boxed<'_, Result<crate::agent_tree::RecoverAgentOutput, crate::agent_tree::TopologyError>>
    {
        self.recoverer.recover(agent_id, expected_attempt_id)
    }
}
