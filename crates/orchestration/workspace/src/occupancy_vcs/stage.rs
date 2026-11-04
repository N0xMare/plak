//! The tree stage a VCS effect runs as (spec 03 §14).
//!
//! A port whose effects run on a runner ([`super::VcsOps::stage_runner`])
//! sends each effect as one step of a tree operation, keyed by the stage the
//! tree recorded before it ran. The tree runs each effect inside its stage
//! with [`run`]; the port reads it with [`current`] and reports a step whose
//! outcome the link could not learn with [`VcsStage::mark_unknown`]. An
//! effect that deliberately runs as no stage says so with [`unstaged`]; a
//! runner port treats an effect in neither scope as a caller that forgot its
//! stage.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// One recorded stage: the operation id its steps are keyed under, and
/// whether one of them ended with its outcome unknown.
#[derive(Debug)]
pub struct VcsStage {
    operation_id: String,
    next_step: AtomicU32,
    unknown: AtomicBool,
}

tokio::task_local! {
    static STAGE: Arc<VcsStage>;
    static UNSTAGED: &'static str;
}

impl VcsStage {
    /// A stage recorded under `operation_id`.
    #[must_use]
    pub fn new(operation_id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            operation_id: operation_id.into(),
            next_step: AtomicU32::new(0),
            unknown: AtomicBool::new(false),
        })
    }

    /// The operation id the tree recorded the stage under.
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// The key of the stage's next effect step: the operation id and the
    /// step's number within the stage, so a stage that runs several effects
    /// (an unmint and its unpins) keys each once.
    #[must_use]
    pub fn next_step(&self) -> String {
        let step = self.next_step.fetch_add(1, Ordering::Relaxed);
        format!("{}/{step}", self.operation_id)
    }

    /// Records that a step of this stage ended without its outcome: the
    /// link was lost past its window, or the runner restarted under it.
    pub fn mark_unknown(&self) {
        self.unknown.store(true, Ordering::Release);
    }

    /// Records that the stage's outcome, once unknown, was checked and is
    /// the one it expected.
    pub fn mark_confirmed(&self) {
        self.unknown.store(false, Ordering::Release);
    }

    /// Whether a step ended without its outcome.
    #[must_use]
    pub fn outcome_unknown(&self) -> bool {
        self.unknown.load(Ordering::Acquire)
    }
}

/// Runs `work` as `stage`'s effects; `None` runs it outside any stage.
pub async fn run<F: Future>(stage: Option<Arc<VcsStage>>, work: F) -> F::Output {
    match stage {
        Some(stage) => STAGE.scope(stage, work).await,
        None => work.await,
    }
}

/// The stage the calling task runs, if any.
#[must_use]
pub fn current() -> Option<Arc<VcsStage>> {
    STAGE.try_with(Arc::clone).ok()
}

/// Runs `work` as effects that are deliberately no stage, for `reason`:
/// each re-derived by its next attach or settle, so a step of it keyed
/// afresh is safe.
pub fn unstaged<F: Future>(reason: &'static str, work: F) -> impl Future<Output = F::Output> {
    UNSTAGED.scope(reason, work)
}

/// Why the calling task runs effects as no stage, when it says so.
#[must_use]
pub fn unstaged_reason() -> Option<&'static str> {
    UNSTAGED.try_with(|reason| *reason).ok()
}
