//! One node's actor (design §3.1) and the role port its loops
//! consult (§3.2): the single driver of a resident node, whichever
//! its role. The root's port is `runtime::host_loop::RootRole`; a live
//! child's is `agent::live::ChildRole`.

use std::future::Future;
use std::time::Instant;

/// What one node's actor is for, as a value its single driver consults
/// rather than a second driver (design §3.2): which projections it
/// runs and what follows a turn that ends recovery-required.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RolePolicy {
    /// The product projector plus the root's tree passes (spec 03
    /// §4.4). A child runs the tree projector over its own feed instead.
    pub(crate) tree_passes: bool,
    pub(crate) recovery: RecoveryPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryPolicy {
    /// The node reopens itself in place as soon as it is idle (design §6.0).
    EagerInPlace,
    /// A turn its owner fenced is recycled by the child's waiter: the
    /// session is released and reopened, and the attempt ends as a failure
    /// the parent can re-task.
    RecycleFenced,
}

impl RolePolicy {
    /// The attached root's policy.
    pub(crate) const ROOT: Self = Self {
        tree_passes: true,
        recovery: RecoveryPolicy::EagerInPlace,
    };

    /// A live child's policy.
    pub(crate) const CHILD: Self = Self {
        tree_passes: false,
        recovery: RecoveryPolicy::RecycleFenced,
    };
}

/// One node's actor: what any resident node owns, and its role's port `R`.
pub(crate) struct NodeActor<R> {
    pub(crate) policy: RolePolicy,
    /// The line head's attachment.
    pub(crate) opened: crate::agent::AttachedCodingSession,
    pub(crate) quit_started_at: Option<Instant>,
    pub(crate) quit_abandoned_turn: bool,
    pub(crate) quit_job_stops: Option<crate::TaskHandle<Result<(), String>>>,
    pub(crate) role: R,
}

impl<R> NodeActor<R> {
    pub(crate) const fn new(
        policy: RolePolicy,
        opened: crate::agent::AttachedCodingSession,
        role: R,
    ) -> Self {
        Self {
            policy,
            opened,
            quit_started_at: None,
            quit_abandoned_turn: false,
            quit_job_stops: None,
            role,
        }
    }

    /// The node's one main loop: take the role's next input, urgent lane
    /// first, and serve it, until the role ends the node's life. `cx` is
    /// what the loop borrows from its caller for that life.
    pub(crate) async fn main_loop<C>(&mut self, cx: &mut C) -> R::Output
    where
        R: Role<C>,
    {
        loop {
            let input = match R::next(self, cx).await {
                Next::Input(input) => input,
                Next::Again => continue,
                Next::End(output) => return output,
            };
            if let Served::End(output) = R::serve(self, cx, input).await {
                return output;
            }
        }
    }
}

/// What the main loop takes next.
pub(crate) enum Next<I, O> {
    /// An input to serve.
    Input(I),
    /// Nothing to serve this time; ask again.
    Again,
    /// The node's life ends.
    End(O),
}

/// What serving one input left.
pub(crate) enum Served<O> {
    Continue,
    End(O),
}

/// A role's half of the main loop: where its inputs come from and how it
/// serves each one.
pub(crate) trait Role<C>: Sized {
    type Input;
    type Output;

    fn next(
        actor: &mut NodeActor<Self>,
        cx: &mut C,
    ) -> impl Future<Output = Next<Self::Input, Self::Output>>;

    fn serve(
        actor: &mut NodeActor<Self>,
        cx: &mut C,
        input: Self::Input,
    ) -> impl Future<Output = Served<Self::Output>>;
}

/// What follows a turn that reached its terminal.
pub(crate) enum AfterTerminal<T, S> {
    /// The node's next turn, driven the same way.
    Next(T),
    /// The turns are over.
    Settled(S),
}

/// A role's half of the turn loop: driving one turn to its terminal, and
/// what the role does with that terminal.
pub(crate) trait TurnRole<C> {
    type Turn;
    type Terminal;
    type Settled;

    fn drive(&mut self, cx: &mut C, turn: Self::Turn) -> impl Future<Output = Self::Terminal>;

    fn after_terminal(
        &mut self,
        cx: &mut C,
        terminal: Self::Terminal,
    ) -> impl Future<Output = AfterTerminal<Self::Turn, Self::Settled>>;
}

/// The node's one turn loop: drive each turn to its terminal, then let the
/// role say what follows.
pub(crate) async fn drive_turns<C, R: TurnRole<C>>(
    role: &mut R,
    cx: &mut C,
    first: R::Turn,
) -> R::Settled {
    let mut turn = first;
    loop {
        let terminal = role.drive(cx, turn).await;
        match role.after_terminal(cx, terminal).await {
            AfterTerminal::Next(next) => turn = next,
            AfterTerminal::Settled(settled) => return settled,
        }
    }
}
