//! The host's one port onto child sessions: bind, mail and cancel.

use super::*;

/// Which of the child services a [`ChildController`] provides. A topology
/// tool whose service the controller does not provide runs exactly as it
/// does with no controller installed: it refuses, or it settles the tree
/// alone, rather than pretending.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChildServices {
    /// [`ChildController::bind`] and its admission hooks.
    pub bind: bool,
    /// [`ChildController::deliver`] and [`ChildController::withdraw_followup`].
    pub mail: bool,
    /// [`ChildController::cancel_agents`].
    pub cancel: bool,
}

impl ChildServices {
    /// Every child service.
    pub const ALL: Self = Self {
        bind: true,
        mail: true,
        cancel: true,
    };
    /// Binding spawned children only.
    pub const BIND: Self = Self {
        bind: true,
        ..Self::NONE
    };
    /// Mail delivery only.
    pub const MAIL: Self = Self {
        mail: true,
        ..Self::NONE
    };
    /// Live-turn cancellation only.
    pub const CANCEL: Self = Self {
        cancel: true,
        ..Self::NONE
    };
    const NONE: Self = Self {
        bind: false,
        mail: false,
        cancel: false,
    };
}

/// Opens, feeds and cancels the sessions behind a tree's children.
///
/// The tree decides; the controller acts on sessions, which the tree cannot
/// open. Each service is called with the tree mutex released, and the tree
/// records the answer afterwards. A service the controller does not declare
/// in [`Self::services`] is never called; its default answer is a refusal.
/// A controller that declares a service MUST override that service's
/// methods.
pub trait ChildController: Send + Sync {
    /// The services this controller provides. Constant for the
    /// controller's lifetime: a tool may consult it more than once per call.
    fn services(&self) -> ChildServices;

    /// Product admission of `caller_id`'s spawn request itself, before the
    /// tree resolves its template. The default admits everything.
    ///
    /// # Errors
    ///
    /// Refuses when the product will not admit this spawn.
    fn admit_spawn(
        &self,
        caller_id: &str,
        request: &SpawnAgentRequest,
    ) -> Result<(), TopologyError> {
        let _ = (caller_id, request);
        Ok(())
    }

    /// The composition the caller of a spawn runs, which the child's node
    /// freezes as its `parent_composition` (the one spawn-parent input that
    /// depends on the caller's profile). `None` when the controller does not
    /// know it; the child then reads its parent's live composition.
    fn spawn_parent_composition(
        &self,
        caller_id: &str,
    ) -> Option<plakous_host_protocol::selection::CompositionSelection> {
        let _ = caller_id;
        None
    }

    /// Product admission of the frozen card a spawn resolved to:
    /// account availability, and nothing about the wire. Called after
    /// [`Self::admit_spawn`], with the tree mutex released and before the
    /// tree admits a node, once the caller's menu contains
    /// `request.template`. A template the epoch does not carry, or one that
    /// is not on the caller's menu, never reaches here.
    /// A refusal here spends no node, attempt, or copy. The default admits
    /// everything. Wire-vocabulary rules (an OpenRouter card asking for
    /// `native-ptc`) live in the tree's own resolution, so a spawn whose
    /// controller does not declare bind (or with no controller) is held to
    /// them too.
    ///
    /// # Errors
    ///
    /// Refuses when the product will not admit this card's bind.
    fn admit_binding<'a>(
        &'a self,
        template: &'a TemplateName,
        binding: &'a RosterBinding,
    ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + 'a>> {
        let _ = (template, binding);
        Box::pin(async { Ok(()) })
    }

    /// Binds a spawned child to a real session **and starts its assignment
    /// turn**. Awaited with the tree mutex released; the spawn records the
    /// answer afterwards.
    ///
    /// `spawn_agent` carries the prompt (mail `delegate` is refused), so
    /// the child must be running its brief when spawn returns. Leaving the first
    /// turn to a later `send_agent_message` left children `running` with empty
    /// transcripts forever and every `wait_agent` burning its full timeout.
    fn bind(
        &self,
        start: SpawnStart,
    ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>> {
        let _ = start;
        Box::pin(async { Err(SpawnBindError::Refused(UNPROVIDED.into())) })
    }

    /// Starts, follows up, or steers the bound child session for one mail
    /// record, in whatever shape the tree's `disposition` already chose.
    ///
    /// A Start the controller admits is bound to the recipient's attempt and
    /// marked active by the controller, under the tree lock, before that turn
    /// can end. A follow-up or steer joins the turn the attempt already binds;
    /// a follow-up the runtime admits as the next turn is bound by the host's
    /// turn waiter when the turn before it ends. The send never binds or
    /// activates a turn or acknowledges mail: the controller records a
    /// command intent and projects its committed acceptance, including the
    /// exact envelope, before returning success. A no-fact refusal remains a
    /// keyed host decision.
    fn deliver(
        &self,
        request: MailDeliveryRequest,
    ) -> Pin<Box<dyn Future<Output = Result<MailDeliveryAck, MailDeliveryFailure>> + Send + '_>>
    {
        let _ = request;
        Box::pin(async { Err(MailDeliveryFailure::Refused(UNPROVIDED.into())) })
    }

    /// Withdraws only after a definitive Start refusal, holding startup and
    /// exact session ownership across an authoritative tail read and tree CAS.
    /// The opening mail and supplied send failure receipt settle in that CAS.
    fn withdraw_followup(
        &self,
        driver: TreeDriver,
        agent_id: String,
        message_id: String,
        receipt: Option<(String, TopologyErrorCode)>,
    ) -> Pin<Box<dyn Future<Output = Result<(), TopologyError>> + Send + '_>> {
        let _ = (driver, agent_id, message_id, receipt);
        Box::pin(async { Err(unprovided()) })
    }

    /// Cancels the live turns of these agents, after the tree has already
    /// recorded the interrupt or close durably. Sends `signalled` once exact
    /// owners are reserved and cancellation requested, before joining release.
    fn cancel_agents(
        &self,
        agent_ids: Vec<String>,
        signalled: tokio::sync::oneshot::Sender<()>,
    ) -> Pin<Box<dyn Future<Output = Result<LiveCancelOutcome, LiveCancelFailure>> + Send + '_>>
    {
        Box::pin(async move {
            let _ = signalled.send(());
            Err(LiveCancelFailure {
                message: UNPROVIDED.to_owned(),
                failed_ids: agent_ids,
                joiner_expired_ids: Vec::new(),
                owner_failures: Vec::new(),
                cancelled_ids: Vec::new(),
                not_live_ids: Vec::new(),
            })
        })
    }
}

const UNPROVIDED: &str = "the child controller does not provide this service";

fn unprovided() -> TopologyError {
    TopologyError::new(TopologyErrorCode::OperationRecoveryRequired, UNPROVIDED)
}

impl TopologyTool {
    fn controller_with(
        &self,
        provides: impl Fn(ChildServices) -> bool,
    ) -> Option<&Arc<dyn ChildController>> {
        self.controller
            .as_ref()
            .filter(|controller| provides(controller.services()))
    }

    /// The controller, when it binds spawned children.
    pub(super) fn binder(&self) -> Option<&Arc<dyn ChildController>> {
        self.controller_with(|services| services.bind)
    }

    /// The controller, when it delivers mail.
    pub(super) fn deliverer(&self) -> Option<&Arc<dyn ChildController>> {
        self.controller_with(|services| services.mail)
    }

    /// The controller, when it cancels live turns.
    pub(super) fn canceller(&self) -> Option<&Arc<dyn ChildController>> {
        self.controller_with(|services| services.cancel)
    }
}
