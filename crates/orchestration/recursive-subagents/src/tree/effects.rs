use super::{
    AdoptFailure, AdoptLanding, AdoptRequest, AdoptTicket, Arc, Completed, Effects, MintRequest,
    MintTicket, MintedCopy, ProductAgentTree, SealTicket, Settled, TopologyError, UnmintTicket,
    VcsError, VcsErrorKind, VcsOps, VcsSnapshot, WorkspaceMode, diverged_from_base,
    topology_error_from_vcs,
};

impl ProductAgentTree {
    pub(crate) fn defer_unavailable_seals(&mut self, effects: &mut Effects) {
        let (ready, deferred): (Vec<_>, Vec<_>) = std::mem::take(&mut effects.seals)
            .into_iter()
            .partition(|ticket| self.vcs.0.placement_ready(&ticket.path).unwrap_or(true));
        effects.seals = ready;
        self.effects.seals.extend(deferred);
    }

    /// Drains what the mutations so far owe the VCS, excluding reopen-owned
    /// base-pin reconciliation, which the driver drains during attachment. The
    /// caller runs them with the mutex released and hands the answers back
    /// to [`Self::complete`].
    pub fn take_effects(&mut self) -> Effects {
        let (seals, deferred) = std::mem::take(&mut self.effects.seals)
            .into_iter()
            .partition(|ticket| self.vcs.0.placement_ready(&ticket.path).unwrap_or(true));
        let base_pins = std::mem::take(&mut self.effects.base_pins);
        let (attach_refreshes, refreshes): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.effects.refreshes)
                .into_iter()
                .partition(|ticket| ticket.attach);
        self.effects.refreshes = refreshes;
        let mut effects = std::mem::take(&mut self.effects);
        effects.seals = seals;
        self.effects.seals = deferred;
        self.effects.base_pins = base_pins;
        self.effects.refreshes = attach_refreshes;
        effects.launch_checkout.clone_from(&self.launch_checkout);
        effects
    }

    /// Whether [`Self::take_attach_effects`] would take anything.
    pub(crate) fn owes_attach_effects(&self) -> bool {
        !self.effects.refreshes.is_empty() || !self.effects.base_pins.is_empty()
    }

    pub(crate) fn take_attach_effects(&mut self) -> Effects {
        Effects {
            refreshes: std::mem::take(&mut self.effects.refreshes),
            base_pins: std::mem::take(&mut self.effects.base_pins),
            ..Effects::default()
        }
    }

    /// Whether `agent_id` owns a queued or already dispatched seal.
    pub(super) fn seal_pending(&self, agent_id: &str) -> bool {
        self.outstanding_seals.contains_key(agent_id)
    }

    /// Re-lock half of an effects batch: records each seal, each adopt, and
    /// each failed hook. Adopt receipts come back in ticket order. A batch
    /// that recorded anything bumps the revision, so the publish that
    /// follows is a real change to the store's compare-and-swap.
    pub fn complete(&mut self, completed: Completed) -> Settled {
        let Completed {
            refreshes,
            base_pins,
            dormancy,
            seals,
            adopts,
            hooks,
            unmints,
            stages,
        } = completed;
        // An absent copy's base pin was never reconciliation work.
        let recorded_anything = !stages.is_empty()
            || !refreshes.is_empty()
            || base_pins
                .iter()
                .any(|(_, result)| !matches!(result, Ok(false)))
            || !dormancy.is_empty()
            || !seals.is_empty()
            || !adopts.is_empty()
            || !unmints.is_empty()
            || hooks.iter().any(|(_, ran)| ran.is_err());
        let before = self.revision;
        let mut settled = Settled::default();
        for (ticket, result) in base_pins {
            self.finish_base_pin_recovery(ticket, result);
        }
        for (ticket, answer) in refreshes {
            if let Err(error) = self.complete_refresh(ticket, answer) {
                settled.cleanup_failures.push(error);
            }
        }
        for (ticket, answer) in dormancy {
            if let Err(error) = self.complete_dormancy(ticket, answer) {
                settled.cleanup_failures.push(error);
            }
        }
        for (ticket, sealed) in seals {
            let failure = sealed
                .as_ref()
                .err()
                .filter(|_| {
                    self.outstanding_seals
                        .get(&ticket.agent_id)
                        .is_some_and(|owner| Arc::ptr_eq(owner, &ticket.ownership))
                })
                .map(|error| {
                    let error = topology_error_from_vcs(error.clone());
                    TopologyError::new(
                        error.code,
                        format!(
                            "{}'s copy could not be sealed: {}",
                            ticket.agent_id, error.message,
                        ),
                    )
                });
            self.finish_seal_with(ticket, sealed);
            if let Some(error) = failure {
                settled.seal_failures.push(error);
            }
        }
        for (ticket, landed) in adopts {
            settled.adopts.push(self.finish_adopt(&ticket, landed));
        }
        for (hook, ran) in hooks {
            if let Err(error) = ran {
                settled.hook_failures.push(self.fail_mint_hook(
                    &hook.caller_id,
                    &hook.request,
                    hook.operation_id.as_deref(),
                    &hook.agent_id,
                    &error.detail,
                ));
            }
        }
        for (ticket, removed) in unmints {
            if let Err(error) = self.finish_unmint(ticket, removed) {
                settled.cleanup_failures.push(error);
            }
        }
        // Each effect's answer is recorded above, in this revision: its stage
        // is done, or settles by its class when a step's outcome was lost.
        for stage in &stages {
            self.settle_vcs_stage(stage);
        }
        // A seal or adopt that just finished may be what a closed copy was
        // waiting on.
        self.prune_closed_copies();
        if recorded_anything && self.revision == before {
            self.bump();
        }
        settled
    }

    /// Runs the queued effects through the tree's own port, blocking this
    /// thread on each, and records them: what a test does where the runtime
    /// would release the mutex in between. Not for production callers.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(
        clippy::expect_used,
        reason = "test driver refuses malformed close fixtures"
    )]
    pub fn settle_for_test(&mut self) -> Settled {
        let attach = self.take_attach_effects();
        if !attach.is_empty() {
            let completed = super::block_on_for_test(attach.run(self.vcs()));
            self.complete(completed);
        }
        let mut settled = Settled::default();
        loop {
            self.fold_reserved_closes()
                .expect("test close fold must succeed");
            self.refresh_epoch_after_close();
            self.prune_closed_copies();
            self.publish_workspace_generations();
            let effects = self.take_effects();
            if effects.is_empty() {
                return settled;
            }
            let completed = super::block_on_for_test(effects.run(self.vcs()));
            let more = self.complete(completed);
            settled.adopts.extend(more.adopts);
            settled.hook_failures.extend(more.hook_failures);
            settled.seal_failures.extend(more.seal_failures);
            settled.cleanup_failures.extend(more.cleanup_failures);
        }
    }
}

impl SealTicket {
    /// The VCS half of the seal. Takes no tree reference precisely so it can
    /// be called with the tree mutex released.
    ///
    /// A copy that never diverged from its base yields no candidate, and the
    /// pin `seal_candidate` placed is dropped here, in the same unlocked
    /// phase. An unpin refusal is a recoverable seal failure, never an empty
    /// candidate success.
    pub async fn seal(&self, vcs: &dyn VcsOps) -> Result<Option<VcsSnapshot>, VcsError> {
        let mut sealed = vcs
            .seal(
                &self.path,
                &self.namespace,
                self.candidate_id.as_str(),
                &self.base_commit,
            )
            .await?;
        sealed.history_left_base = vcs
            .history_left_base(&self.path, &self.base_commit, &sealed.commit)
            .await?;
        if !diverged_from_base(&sealed, &self.base_content) {
            vcs.unpin(&self.path, &self.namespace, self.candidate_id.as_str())
                .await?;
            return Ok(None);
        }
        if let Some(carry) = &self.carry {
            Box::pin(super::cross_runner::seal_back(vcs, self, carry, &sealed)).await?;
        }
        Ok(Some(sealed))
    }
}

impl AdoptTicket {
    /// The subprocess half of the adopt. Holds no tree
    /// reference so it can run with the tree mutex released.
    pub async fn land(&self, vcs: &dyn VcsOps) -> Result<AdoptLanding, AdoptFailure> {
        let candidate = VcsSnapshot {
            history_left_base: false,
            commit: self.candidate.commit.clone(),
            content: self.candidate.content.clone(),
            change_id: self.candidate.change_id.clone(),
        };
        vcs.adopt(&AdoptRequest {
            resolution_refusal: self.resolution_refusal.as_deref(),
            revert: self.revert.as_ref().map(|plan| &plan.range),
            accept_conflicts: self.accept_conflicts,
            conflict_pin: self.conflict_pin.as_ref(),
            resolver_paths: &self.resolver_paths,
            resolved_source: self
                .source
                .as_ref()
                .filter(|_| self.revert.is_none())
                .map(|source| plakous_workspace::occupancy_vcs::ResolvedSource {
                    candidate_id: source.candidate_id.as_str(),
                    agent_id: self.source_copy_agent.as_deref(),
                    commit: &source.commit,
                }),
            target_path: &self.target_path,
            child_path: &self.child_path,
            base_commit: &self.candidate.base_commit,
            base_change_id: self.candidate.base_change_id.as_deref(),
            candidate: &candidate,
            expected_target: self.expected_target.as_deref(),
            namespace: &self.namespace,
            candidate_id: self.candidate.candidate_id.as_str(),
            source_agent_id: self
                .advance_copy_base
                .then_some(self.candidate.producing_agent_id.as_str()),
        })
        .await
    }
}

impl MintTicket {
    /// The copy shape the plan chose.
    #[must_use]
    pub(super) const fn mode(&self) -> WorkspaceMode {
        self.mode
    }

    /// The subprocess half of the mint. Holds no tree reference so it can
    /// run with the tree mutex released.
    pub async fn mint(&self, vcs: &dyn VcsOps) -> Result<MintedCopy, VcsError> {
        if let Some(carry) = &self.carry {
            // Boxed: the carry is rare and its future large.
            return Box::pin(super::cross_runner::mint_across(vcs, self, carry)).await;
        }
        let mut minted = vcs
            .mint_from(&MintRequest {
                launch: &self.launch_checkout,
                agent_id: &self.agent_id,
                namespace: &self.namespace,
                parent_cwd: &self.caller_path,
                mode: self.mode,
                base: self.input_base.as_ref(),
            })
            .await?;
        if let Some(base) = &self.merge_base {
            if let Err(error) = vcs
                .restore_copy_base(&minted.path, &self.namespace, &self.agent_id, &base.commit)
                .await
            {
                let _ = vcs
                    .unmint(&self.launch_checkout, &minted.path, &self.namespace)
                    .await;
                return Err(error);
            }
            minted.base.clone_from(base);
        }
        Ok(minted)
    }
}

impl Effects {
    /// Folds a later batch into this one, so a caller that mutated twice
    /// settles once.
    pub fn absorb(&mut self, other: Self) {
        let Self {
            refreshes,
            base_pins,
            seals,
            dormancy,
            adopts,
            hooks,
            unmints,
            launch_checkout,
        } = other;
        self.refreshes.extend(refreshes);
        self.base_pins.extend(base_pins);
        self.seals.extend(seals);
        self.dormancy.extend(dormancy);
        self.adopts.extend(adopts);
        self.hooks.extend(hooks);
        self.unmints.extend(unmints);
        if self.launch_checkout.as_os_str().is_empty() {
            self.launch_checkout = launch_checkout;
        }
    }

    /// Whether this batch owes the VCS nothing, so the caller can skip the
    /// unlocked phase entirely.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.refreshes.is_empty()
            && self.base_pins.is_empty()
            && self.dormancy.is_empty()
            && self.seals.is_empty()
            && self.adopts.is_empty()
            && self.hooks.is_empty()
            && self.unmints.is_empty()
    }

    pub(crate) fn refuse_attach(self, reason: String) -> Completed {
        Completed {
            refreshes: self
                .refreshes
                .into_iter()
                .map(|ticket| {
                    (
                        ticket,
                        Err(VcsError::new(
                            VcsErrorKind::RecoveryRequired,
                            reason.clone(),
                        )),
                    )
                })
                .collect(),
            base_pins: self
                .base_pins
                .into_iter()
                .map(|ticket| {
                    (
                        ticket,
                        Err(VcsError::new(
                            VcsErrorKind::RecoveryRequired,
                            reason.clone(),
                        )),
                    )
                })
                .collect(),
            ..Completed::default()
        }
    }

    /// Runs every queued effect against `vcs`, one awaited port call at a
    /// time and in a fixed order: base pins, refreshes, seals, adopts, hooks,
    /// unmints, dormancy. Holds no tree reference, so it cannot be called
    /// with the tree mutex held by construction of its arguments; a `FakeVcs`
    /// in tests records whether it was anyway.
    pub async fn run(self, vcs: &dyn VcsOps) -> Completed {
        self.run_staged(vcs, super::stages::EffectStages::default())
            .await
    }

    /// [`Self::run`], each ticket as its recorded stage in `stages`, which
    /// the completion settles.
    pub(crate) async fn run_staged(
        self,
        vcs: &dyn VcsOps,
        plan: super::stages::EffectStages,
    ) -> Completed {
        use super::stages::EffectStages;
        use plakous_workspace::occupancy_vcs::stage;
        let Self {
            refreshes,
            base_pins,
            seals,
            dormancy,
            adopts,
            hooks,
            unmints,
            launch_checkout,
        } = self;
        let mut completed = Completed::default();
        for (index, ticket) in base_pins.into_iter().enumerate() {
            let staged = EffectStages::at(&plan.base_pins, index);
            let result = stage::run(staged, ticket.run(vcs)).await;
            completed.base_pins.push((ticket, result));
        }
        for (index, ticket) in refreshes.into_iter().enumerate() {
            let staged = EffectStages::at(&plan.refreshes, index);
            let answer = stage::run(staged, ticket.run(vcs)).await;
            completed.refreshes.push((ticket, answer));
        }
        for (index, ticket) in seals.into_iter().enumerate() {
            let staged = EffectStages::at(&plan.seals, index);
            let sealed = stage::run(staged, ticket.seal(vcs)).await;
            completed.seals.push((ticket, sealed));
        }
        for (index, ticket) in adopts.into_iter().enumerate() {
            let staged = EffectStages::at(&plan.adopts, index);
            let landed = stage::run(staged, ticket.land(vcs)).await;
            completed.adopts.push((ticket, landed));
        }
        for (index, hook) in hooks.into_iter().enumerate() {
            let staged = EffectStages::at(&plan.hooks, index);
            let ran = stage::run(
                staged,
                vcs.run_mint_hook(&hook.cwd, &hook.command, &hook.env),
            )
            .await;
            completed.hooks.push((hook, ran));
        }
        for (index, ticket) in unmints.into_iter().enumerate() {
            let staged = EffectStages::at(&plan.unmints, index);
            let removed = stage::run(staged, async {
                let removed = vcs
                    .unmint(&launch_checkout, &ticket.path, &ticket.namespace)
                    .await;
                if removed.is_ok() {
                    for candidate_id in &ticket.candidate_ids {
                        // A surviving pin keeps explicitly relinquished
                        // content reachable; the copy is gone.
                        let _ = vcs
                            .unpin(&launch_checkout, &ticket.namespace, candidate_id)
                            .await;
                    }
                }
                removed
            })
            .await;
            completed.unmints.push((ticket, removed));
        }
        for (index, mut ticket) in dormancy.into_iter().enumerate() {
            let staged = EffectStages::at(&plan.dormancy, index);
            let answer = stage::run(staged, ticket.run(vcs, &launch_checkout)).await;
            completed.dormancy.push((ticket, answer));
        }
        completed.stages = [
            plan.base_pins,
            plan.refreshes,
            plan.seals,
            plan.adopts,
            plan.hooks,
            plan.unmints,
            plan.dormancy,
        ]
        .concat();
        completed
    }
}

impl Completed {
    pub(crate) fn refuse_unpublished_unmints(&mut self, tickets: Vec<UnmintTicket>) {
        self.unmints.extend(tickets.into_iter().map(|ticket| {
            (
                ticket,
                Err(VcsError::new(
                    VcsErrorKind::RecoveryRequired,
                    "removal preparation was not durably published; no removal dispatched",
                )),
            )
        }));
    }
}
