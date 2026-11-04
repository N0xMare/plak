//! The driver's record of the stages a cross-runner carry runs as: each is
//! recorded and published before it runs, and settled by the writer once
//! its answer is known.

use std::future::Future;
use std::sync::Arc;

use plakous_workspace::occupancy_vcs::stage::VcsStage;
use plakous_workspace::occupancy_vcs::{VcsError, VcsFuture};

use super::{TopologyError, TreeDriver};
use crate::tree::VcsStageIntent;
use crate::tree::cross_runner::{StageLedger, with_ledger};

struct DriverLedger(TreeDriver);

impl StageLedger for DriverLedger {
    fn record(&self, intent: VcsStageIntent) -> VcsFuture<'_, Result<Arc<VcsStage>, VcsError>> {
        let driver = self.0.clone();
        Box::pin(async move {
            let writer = driver.clone();
            driver
                .write_owed(move |tree| {
                    let stage = tree.record_vcs_stage(&intent)?;
                    writer.save_guarded(tree)?;
                    Ok::<_, TopologyError>(stage)
                })
                .await
                .and_then(|recorded| recorded)
                .map_err(|error| VcsError::mint_unavailable(error.message))
        })
    }

    fn settle(&self, stage: Arc<VcsStage>) -> VcsFuture<'_, ()> {
        let driver = self.0.clone();
        Box::pin(async move {
            // Published with the effect's own answer; a stage left recorded
            // settles on reopen.
            let _ = driver
                .write_owed(move |tree| tree.settle_vcs_stage(&stage))
                .await;
        })
    }

    fn conflicting(
        &self,
        agent: &str,
        attempt: Option<crate::protocol::AttemptId>,
        candidate: &str,
    ) -> VcsFuture<'_, ()> {
        let (agent, candidate) = (agent.to_owned(), candidate.to_owned());
        self.write(move |tree| tree.fault_carried_candidate(&agent, &candidate, attempt))
    }

    fn carried(&self, runner: &str, commit: &str) -> VcsFuture<'_, ()> {
        let driver = self.0.clone();
        let (runner, commit) = (runner.to_owned(), commit.to_owned());
        Box::pin(async move {
            // Published with the effect's own answer: a lost record only
            // makes a later bundle carry more.
            let _ = driver
                .write_owed(move |tree| tree.record_runner_ancestor(&runner, &commit))
                .await;
        })
    }
}

impl DriverLedger {
    fn write(
        &self,
        work: impl FnOnce(&mut crate::tree::ProductAgentTree) + Send + 'static,
    ) -> VcsFuture<'_, ()> {
        let driver = self.0.clone();
        Box::pin(async move {
            let writer = driver.clone();
            let _ = driver
                .write_owed(move |tree| {
                    work(tree);
                    writer.save_guarded(tree)
                })
                .await;
        })
    }
}

/// How old a transfer file is before the reclamation pass removes it: no
/// carry runs this long, and a rerun writes its files again.
const TRANSFER_MAX_AGE_SECS: u64 = 24 * 60 * 60;

impl TreeDriver {
    /// Removes transfer files a carry that never finished left in the
    /// launch checkout and every runner's mirror. Best effort: a sweep that
    /// fails is tried again at the next pass.
    pub(super) async fn sweep_transfers(&self) {
        let Ok(launch) = self.read(|tree| tree.launch_checkout.clone()) else {
            return;
        };
        let _ = plakous_workspace::occupancy_vcs::stage::unstaged(
            "transfer sweep, repeated at every reclamation pass",
            self.vcs.sweep_transfers(&launch, TRANSFER_MAX_AGE_SECS),
        )
        .await;
    }

    /// Runs `work` with this driver recording the stages of any carry in it.
    pub(crate) fn with_stage_ledger<F: Future>(
        &self,
        work: F,
    ) -> impl Future<Output = F::Output> + use<F> {
        with_ledger(Arc::new(DriverLedger(self.clone())), work)
    }
}
