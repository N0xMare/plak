//! Copies that cross runners (chapter 03 §14): a `fork` or `snapshot` child
//! placed on another runner than its parent's is minted there from a
//! bundle of its parent's state, and its candidate is carried back into its
//! parent's checkout the same way.
//!
//! Each step is a recorded stage, published before it runs. A mint runs
//! base snapshot, bundle create, relay and bundle apply, each reproducible,
//! then the mint itself into the tree's mirror on the target runner. A seal
//! on the child's runner is carried back by bundle create, relay and a
//! verifiable bundle apply, whose intent records the candidate reference
//! and commit it leaves: its rerun first asks whether the parent's
//! checkout already holds them. The host relays bundle chunks between the
//! runners and stores nothing.
//!
//! The tree records stages through the [`StageLedger`] the caller runs the
//! effect under ([`with_ledger`]); a carry with no ledger is unavailable.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous_workspace::occupancy_vcs::stage::{self, VcsStage};
use plakous_workspace::occupancy_vcs::{
    BUNDLE_CHUNK_BYTES, BundleFacts, MintRequest, MintedCopy, VcsError, VcsErrorKind, VcsFuture,
    VcsOps, VcsSnapshot, candidate_ref, transfer_ref,
};

use super::{
    AttemptId, MintTicket, ProductAgentTree, SealTicket, TopologyError, TopologyErrorCode,
    VcsExpectedOutcome, VcsStageClass, VcsStageIntent, VcsStageKind,
};
use crate::protocol::SpawnAgentRequest;

/// The runners a cross-runner copy is carried between.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Carry {
    /// The parent's runner, where the copy's base comes from.
    pub source: String,
    /// The runner the copy is minted on.
    pub target: String,
    /// The tree, whose mirror on the target holds the copy.
    pub tree_id: String,
    /// What the target already holds ([`ProductAgentTree::tree_runner_ancestors`]).
    pub ancestors: Vec<String>,
}

/// The runners a candidate sealed on another runner than its parent's is
/// carried back between.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealCarry {
    /// The child's runner, where the candidate is sealed.
    pub source: String,
    /// The parent's runner.
    pub target: String,
    /// The parent's checkout, whose repository takes the candidate.
    pub dest: PathBuf,
}

/// Records and settles the stages a carry runs as. The tree's driver
/// records each in a published revision before it runs.
pub trait StageLedger: Send + Sync {
    /// Records `intent` and returns its stage once it is published.
    fn record(&self, intent: VcsStageIntent) -> VcsFuture<'_, Result<Arc<VcsStage>, VcsError>>;
    /// Settles `stage` once its answer is known (or known to be unknown).
    fn settle(&self, stage: Arc<VcsStage>) -> VcsFuture<'_, ()>;
    /// Records that `runner` holds `commit`: a bundle with it as its tip
    /// was applied there.
    fn carried(&self, runner: &str, commit: &str) -> VcsFuture<'_, ()>;
    /// Records a `vcs_fault` for `agent`: the parent's checkout names
    /// another commit under the candidate reference `candidate` carries.
    fn conflicting(
        &self,
        agent: &str,
        attempt: Option<AttemptId>,
        candidate: &str,
    ) -> VcsFuture<'_, ()>;
}

tokio::task_local! {
    static LEDGER: Arc<dyn StageLedger>;
}

/// Runs `work` with `ledger` recording the stages of any carry in it.
pub fn with_ledger<F: Future>(
    ledger: Arc<dyn StageLedger>,
    work: F,
) -> impl Future<Output = F::Output> {
    LEDGER.scope(ledger, work)
}

fn ledger() -> Result<Arc<dyn StageLedger>, VcsError> {
    LEDGER
        .try_with(Arc::clone)
        .map_err(|_| VcsError::mint_unavailable("a copy crosses runners only as recorded stages"))
}

pub(crate) fn copy_unavailable(message: &str) -> TopologyError {
    TopologyError::new(TopologyErrorCode::CrossRunnerCopyUnavailable, message)
}

/// A stage of `kind` serving `agent`, carried from `source` to `target`.
fn moving(
    kind: VcsStageKind,
    class: VcsStageClass,
    agent: &str,
    source: &str,
    target: &str,
) -> VcsStageIntent {
    VcsStageIntent {
        attempt_id: None,
        stage: kind,
        class,
        agent_id: agent.to_owned(),
        candidate_id: None,
        source_runner: Some(source.to_owned()),
        target_runner: Some(target.to_owned()),
        base_commit: None,
        tip_commit: None,
        bundle_digest: None,
        expected: None,
    }
}

/// Runs `work` as the stage `intent` records.
async fn staged<T>(
    ledger: &dyn StageLedger,
    intent: VcsStageIntent,
    work: impl Future<Output = Result<T, VcsError>>,
) -> Result<T, VcsError> {
    let stage = ledger.record(intent).await?;
    let answer = stage::run(Some(Arc::clone(&stage)), work).await;
    ledger.settle(stage).await;
    answer
}

/// One bundle's way from `source_repo` (through the tree's port) to
/// `dest_repo` on `dest`.
struct Leg<'a> {
    source: &'a dyn VcsOps,
    source_repo: &'a Path,
    dest: &'a dyn VcsOps,
    dest_repo: &'a Path,
    /// The stage template: kind, runners, agent and candidate.
    intent: VcsStageIntent,
    reference: String,
    tip: String,
    ancestors: &'a [String],
}

impl Leg<'_> {
    /// Bundles the leg's reference, relays it and applies it with
    /// `fetch` and `pins`, as three stages; the apply records `expected`
    /// when it is verifiable.
    async fn carry(
        &self,
        ledger: &dyn StageLedger,
        namespace: &str,
        fetch: &[(String, String)],
        pins: &[(String, String)],
        expected: Option<VcsExpectedOutcome>,
    ) -> Result<(), VcsError> {
        // Only what the destination still holds is left out of the bundle.
        let mut exclude = Vec::new();
        for commit in self.ancestors {
            if self
                .dest
                .ref_commit(self.dest_repo, commit)
                .await?
                .is_some()
            {
                exclude.push(commit.clone());
            }
        }
        let mut create = self.intent.clone();
        create.stage = VcsStageKind::BundleCreate;
        create.tip_commit = Some(self.tip.clone());
        create.base_commit = exclude.first().cloned();
        let facts = staged(
            ledger,
            create.clone(),
            self.source
                .bundle_create(self.source_repo, &self.reference, &exclude),
        )
        .await?;
        let mut relay = create;
        relay.stage = VcsStageKind::Relay;
        relay.bundle_digest = Some(facts.digest.clone());
        staged(ledger, relay.clone(), self.relay(&facts)).await?;
        let mut apply = relay;
        apply.stage = VcsStageKind::BundleApply;
        if let Some(expected) = expected {
            apply.class = VcsStageClass::Verifiable;
            apply.expected = Some(expected);
        }
        let stage = ledger.record(apply.clone()).await?;
        let applied = stage::run(
            Some(Arc::clone(&stage)),
            self.dest.bundle_apply(
                self.dest_repo,
                &facts.digest,
                &facts.digest,
                namespace,
                fetch,
                pins,
            ),
        )
        .await;
        // A verifiable apply whose answer was lost is checked: the
        // expected reference naming the expected commit is its outcome.
        let applied = match (applied, &apply.expected) {
            (Err(error), Some(expected)) if error.kind == VcsErrorKind::RecoveryRequired => {
                if self.holds(expected).await? {
                    stage.mark_confirmed();
                    Ok(())
                } else {
                    Err(error)
                }
            }
            (applied, _) => applied,
        };
        ledger.settle(stage).await;
        if applied.is_ok() {
            ledger
                .carried(
                    self.intent.target_runner.as_deref().unwrap_or_default(),
                    &self.tip,
                )
                .await;
        }
        applied
    }

    /// The host's relay: each chunk read from the source and written to
    /// the destination's upload, which is named by the bundle's digest.
    /// The source's bundle file is removed once it is carried.
    async fn relay(&self, facts: &BundleFacts) -> Result<(), VcsError> {
        let mut offset = 0;
        while offset < facts.len {
            let chunk = self
                .source
                .bundle_read(self.source_repo, &facts.id, offset, BUNDLE_CHUNK_BYTES)
                .await?;
            if chunk.is_empty() {
                return Err(VcsError::mint_failed("the bundle ended before its length"));
            }
            self.dest
                .bundle_write(self.dest_repo, &facts.digest, offset, &chunk)
                .await?;
            offset += u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        }
        self.source.bundle_drop(self.source_repo, &facts.id).await
    }

    /// Whether the destination holds `expected` already.
    async fn holds(&self, expected: &VcsExpectedOutcome) -> Result<bool, VcsError> {
        Ok(self
            .dest
            .ref_commit(self.dest_repo, &expected.reference)
            .await?
            .as_deref()
            == Some(expected.commit.as_str()))
    }
}

/// Mints `ticket`'s copy on its carry's target runner: the parent's state
/// is pinned as a base snapshot on the parent's runner, carried into the
/// tree's mirror on the target, and the copy minted there over it.
pub(super) async fn mint_across(
    vcs: &dyn VcsOps,
    ticket: &MintTicket,
    carry: &Carry,
) -> Result<MintedCopy, VcsError> {
    let ledger = ledger()?;
    let target = vcs.transfer_port(&carry.target).ok_or_else(|| {
        VcsError::mint_unavailable("the tree's port does not reach the copy's runner")
    })?;
    let mirror = target.transfer_root(&carry.tree_id).await?;
    let agent = ticket.agent_id.as_str();
    let head = vcs
        .ref_commit(&ticket.caller_path, "HEAD")
        .await?
        .ok_or_else(|| VcsError::mint_unavailable("the parent's checkout has no commit"))?;
    let mut snapshot = moving(
        VcsStageKind::BaseSnapshot,
        VcsStageClass::Reproducible,
        agent,
        &carry.source,
        &carry.target,
    );
    snapshot.target_runner = None;
    snapshot.base_commit = Some(head);
    let base: VcsSnapshot = staged(
        ledger.as_ref(),
        snapshot,
        vcs.snapshot_base(&ticket.caller_path, &ticket.namespace, agent),
    )
    .await?;
    let reference = transfer_ref(&ticket.namespace, agent);
    Leg {
        source: vcs,
        source_repo: &ticket.caller_path,
        dest: target.as_ref(),
        dest_repo: &mirror,
        intent: moving(
            VcsStageKind::BundleCreate,
            VcsStageClass::Reproducible,
            agent,
            &carry.source,
            &carry.target,
        ),
        reference: reference.clone(),
        tip: base.commit.clone(),
        ancestors: &carry.ancestors,
    }
    .carry(
        ledger.as_ref(),
        &ticket.namespace,
        &[(reference.clone(), reference)],
        &[],
        None,
    )
    .await?;
    // The mint itself runs as the spawn's own stage, on the target.
    target
        .mint_from(&MintRequest {
            launch: &mirror,
            agent_id: agent,
            namespace: &ticket.namespace,
            parent_cwd: &mirror,
            mode: ticket.mode,
            base: Some(&base),
        })
        .await
}

/// Carries `sealed`, the candidate `ticket` sealed on its child's runner,
/// back into the parent's checkout as the same candidate pin. It is done
/// already when the parent's checkout names it.
pub(super) async fn seal_back(
    vcs: &dyn VcsOps,
    ticket: &SealTicket,
    carry: &SealCarry,
    sealed: &VcsSnapshot,
) -> Result<(), VcsError> {
    let ledger = ledger()?;
    let reference = candidate_ref(&ticket.namespace, ticket.candidate_id.as_str());
    let expected = VcsExpectedOutcome {
        reference: reference.clone(),
        commit: sealed.commit.clone(),
    };
    let mut intent = moving(
        VcsStageKind::BundleCreate,
        VcsStageClass::Reproducible,
        &ticket.agent_id,
        &carry.source,
        &carry.target,
    );
    intent.attempt_id.clone_from(&ticket.attempt_id);
    intent.candidate_id = Some(ticket.candidate_id.clone());
    let ancestors = [ticket.base_commit.clone()];
    let leg = Leg {
        source: vcs,
        source_repo: &ticket.path,
        dest: vcs,
        dest_repo: &carry.dest,
        intent,
        reference: reference.clone(),
        tip: sealed.commit.clone(),
        ancestors: &ancestors,
    };
    // Done already, or a decision: a reference that names another commit
    // is never moved.
    match vcs.ref_commit(&carry.dest, &reference).await? {
        Some(held) if held == expected.commit => return Ok(()),
        Some(_) => {
            ledger
                .conflicting(
                    &ticket.agent_id,
                    ticket.attempt_id.clone(),
                    ticket.candidate_id.as_str(),
                )
                .await;
            return Err(VcsError::new(
                VcsErrorKind::RecoveryRequired,
                "the parent's checkout already names another commit under this candidate's reference",
            ));
        }
        None => {}
    }
    let base_pin = (
        plakous_workspace::occupancy_vcs::candidate_refs(
            &ticket.namespace,
            ticket.candidate_id.as_str(),
        )[1]
        .clone(),
        ticket.base_commit.clone(),
    );
    leg.carry(
        ledger.as_ref(),
        &ticket.namespace,
        // The objects alone: the pins create the candidate reference, and
        // only where none is.
        &[(reference.clone(), String::new())],
        &[(reference, sealed.commit.clone()), base_pin],
        Some(expected),
    )
    .await
}

impl ProductAgentTree {
    /// The carry a spawn on `runner` needs: `None` on its parent's own
    /// runner.
    pub(super) fn spawn_carry(
        &self,
        caller_id: &str,
        request: &SpawnAgentRequest,
        runner: Option<&str>,
    ) -> Result<Option<Carry>, TopologyError> {
        let parent = self.node(caller_id)?.placement.runner_id.clone();
        let Some(runner) = runner.filter(|runner| parent.as_deref() != Some(*runner)) else {
            return Ok(None);
        };
        if request.workspace.is_inherit() {
            return Err(TopologyError::new(
                TopologyErrorCode::RunnerMismatch,
                "an inherit child works in its parent's checkout, so it runs on its parent's runner",
            ));
        }
        let Some(source) = parent else {
            return Err(copy_unavailable(
                "a copy crosses runners only from a parent on a runner",
            ));
        };
        if !request.input_ids().is_empty() {
            return Err(copy_unavailable(
                "a copy crosses runners only from its parent's live state, without candidate inputs",
            ));
        }
        if self.vcs_port().transfer_port(runner).is_none() {
            return Err(copy_unavailable(
                "the tree's port does not reach that runner, so a copy cannot be carried there",
            ));
        }
        Ok(Some(Carry {
            source,
            target: runner.to_owned(),
            tree_id: self.tree_id.clone(),
            ancestors: self.tree_runner_ancestors(runner),
        }))
    }

    /// The carry back of a candidate sealed by `agent_id`: `None` when it
    /// is on its parent's runner.
    pub(super) fn seal_carry(&self, agent_id: &str) -> Option<SealCarry> {
        let node = self.nodes.get(agent_id)?;
        let parent = self.nodes.get(node.parent_agent_id.as_deref()?)?;
        let source = node.placement.runner_id.clone()?;
        let target = parent.placement.runner_id.clone()?;
        (source != target).then(|| SealCarry {
            source,
            target,
            dest: parent.path.clone(),
        })
    }
}

#[cfg(test)]
mod tests;
