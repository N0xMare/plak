use super::{Future, Stage};

type Hook = std::pin::Pin<Box<dyn Future<Output = ()>>>;

/// Test seams of one host's preparation, carried by the host itself so they
/// hold on whichever thread polls its steps.
#[derive(Default)]
pub(in crate::runtime::host_loop) struct Hooks {
    /// Every stage preparation entered, in order.
    pub(in crate::runtime::host_loop) called: Vec<Stage>,
    /// Runs once, before the step of its stage.
    pub(in crate::runtime::host_loop) hook: Option<(Stage, Hook)>,
    /// Replaces the next mail selection with this failure.
    pub(in crate::runtime::host_loop) mail_error: Option<String>,
}

impl Hooks {
    pub(in crate::runtime::host_loop) fn new() -> Self {
        Self::default()
    }
}
