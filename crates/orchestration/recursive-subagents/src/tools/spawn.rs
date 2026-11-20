//! Spawning a child (the bind request, the minting phases) and landing its candidate.

use super::*;

/// Everything the host needs to bind a spawned child's session and start its
/// attempt turn: the receipt plus the node's bind identity and brief, read
/// from the tree so a replayed receipt carries the same start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnStart {
    /// The spawn receipt the tree produced.
    pub spawned: SpawnAgentOutput,
    /// The one line naming the child's job.
    pub description: String,
    /// The child's brief, which its first turn runs.
    pub prompt: String,
    /// The schema the child's `submit_result` must satisfy.
    pub output_schema_json: String,
    /// The token that first turn must submit under.
    pub turn_token: u64,
    /// The checkout the child works in.
    pub path: String,
    /// Its effective tier after the template clip.
    pub tier: Tier,
    /// The account the child's card binds.
    pub account: String,
    /// The exact model the child's card binds.
    pub tuple_id: String,
    /// Reasoning effort, when the card named one.
    pub effort: Option<String>,
}

/// A bound child session. `turn_id` is the started assignment turn; a binder
/// that only materializes a session (tests) leaves it `None`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnBound {
    /// The binder already durably published the locator and turn before live
    /// admission. The caller must not restore an already completed child.
    pub durably_registered: bool,
    /// The child session the binder opened.
    pub session_locator: String,
    /// The assignment turn it started; `None` when the binder only
    /// materialised a session.
    pub turn_id: Option<String>,
}

/// A refused bind or a startup whose admitted work has not settled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnBindError {
    /// No session or turn remains owned by the binder.
    Refused(plakous_host_protocol::status::HostFailure),
    /// Ownership remains with an independent worker; sealing is forbidden.
    RecoveryRequired {
        /// The last observed startup stage.
        stage: crate::hub::ChildStartupStage,
        /// Actionable diagnostic without provider or credential payloads.
        message: String,
    },
}

impl From<String> for SpawnBindError {
    fn from(message: String) -> Self {
        Self::Refused(message.into())
    }
}

impl From<&str> for SpawnBindError {
    fn from(message: &str) -> Self {
        Self::Refused(message.into())
    }
}

/// Where [`TopologyTool::spawn_admit`]'s first locked phase left the spawn:
/// a receipt (replayed, or an `inherit` bind that minted nothing), or a
/// reservation waiting on the mint that runs with the mutex released.
enum SpawnPhase {
    Done(SpawnAgentOutput, bool),
    /// The mint still owed, and the stage it runs as on a runner.
    Minting(
        Box<SpawnReservation>,
        MintTicket,
        Option<Arc<plakous_workspace::occupancy_vcs::stage::VcsStage>>,
    ),
}

struct SpawnBinding {
    parent_composition: Option<plakous_host_protocol::selection::CompositionSelection>,
    resolved_account: Option<String>,
    runner: Option<String>,
}

struct SpawnPlacement {
    runner: Option<String>,
    jj_copy_dirs: Result<std::collections::BTreeSet<String>, VcsError>,
}

impl TopologyTool {
    /// Spends one unit of the caller cell's agent budget, or refuses.
    ///
    /// A direct model call has no cell identity and is never budgeted, so the
    /// root agent's own `spawn_agent` is unaffected. The refusal is an ordinary
    /// tool failure, so the cell observes it as a failed call and can keep
    /// running.
    ///
    /// The check runs before `prepare_operation`, so an operation the tree
    /// would have *replayed* still spends a unit. That costs nothing today —
    /// L0's `CompositionChild` record is not replay-complete, so no nested
    /// request is ever re-offered. Any future replay support
    /// must define whether replay spends a unit.
    pub(super) fn refuse_over_cell_agent_budget(
        &self,
        lineage: &ToolCallerLineage,
        value: &Value,
    ) -> Option<Result<ToolOutcome, ToolRuntimeError>> {
        if !self.consumes_cell_agent_budget_for(value) {
            return None;
        }
        let caller = self.caller_id.lock().map_or_default(|guard| guard.clone());
        let cell = CellIdentity::from_lineage(lineage, &caller)?;
        self.cell_budget
            .admit_agent(&cell)
            .err()
            .map(|exhausted| ToolOutcome::failure_text(exhausted.to_failure_json().to_string()))
    }

    /// Whether *this* call spends a unit of the cell's agent budget.
    ///
    ///
    /// Beyond the kinds that always do, a `send_agent_message` carrying
    /// `purpose=followup` does: it re-tasks an idle finished child on a fresh
    /// attempt, which is a new agent turn the cell asked for. Ordinary mail on
    /// an already-running child stays free, so an exhausted cell can still
    /// steer, await, and close what it has. A request that does not decode is
    /// not charged; the kind's own handler is what reports the shape error.
    pub(super) fn consumes_cell_agent_budget_for(&self, value: &Value) -> bool {
        if self.kind.consumes_cell_agent_budget() {
            return true;
        }
        matches!(self.kind, TopologyKind::Send)
            && decode::<SendAgentMessageRequest>(value)
                .is_ok_and(|request| request.purpose == Some(MessagePurpose::Followup))
    }

    /// `adopt_candidate` in three phases: authority and the
    /// `Adopting` mark under the lock, the `merge-tree`/`apply` (or jj
    /// rebase/squash) subprocesses with the lock released, then the receipt
    /// under the lock again. Every other tool call and TUI read proceeds while
    /// the landing runs.
    #[allow(
        clippy::too_many_lines,
        reason = "adoption preserves the prepared receipt across unlocked landing and acknowledged tree publication"
    )]
    pub(super) async fn adopt_with(
        &self,
        operation_id: &str,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self.caller_id.lock().map_or_default(|guard| guard.clone());
        let running = self.running_jobs(&caller);
        let warning = if running.is_empty() {
            String::new()
        } else {
            format!(
                "running processes {} still use this checkout",
                running.join(", ")
            )
        };
        let request = match decode::<AdoptCandidateRequest>(value) {
            Ok(request) => request,
            Err(error) => return Self::topology_failure(&error),
        };
        let lock_poisoned = || {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "topology tree lock is poisoned",
            )
        };
        // What the admission asks the port is answered here, outside the
        // writer; an unanswered question refuses the adopt where it is needed.
        let facts = match self.driver.read(|tree| tree.adopt_probe(&caller, &request)) {
            Ok(probe) => probe.run(self.driver.vcs().as_ref()).await,
            Err(error) => return Self::topology_failure(&error),
        };
        let effects = {
            let driver = self.driver.clone();
            let (operation_id, caller, request) =
                (operation_id.to_owned(), caller.clone(), request.clone());
            let prepared = self
                .driver
                .write(move |tree| {
                    let prepared = tree
                        .prepare_operation(&operation_id, &caller, "adopt_candidate", &request)
                        .and_then(|prior| match prior {
                            Some(prior) => Ok(Some(prior)),
                            None => driver.save_guarded(tree).map(|()| None),
                        });
                    match prepared {
                        Ok(Some(prior)) => {
                            if let Some(files_changed) = serde_json::from_str::<Value>(&prior)
                                .ok()
                                .and_then(|receipt| receipt.get("files_changed")?.as_u64())
                                .and_then(|files| u32::try_from(files).ok())
                            {
                                crate::call_facts::leave(
                                    &operation_id,
                                    crate::call_facts::TopologyCallFacts::Adopted { files_changed },
                                );
                            }
                            return Err(Box::new(ToolOutcome::success_text(prior)));
                        }
                        Ok(None) => {}
                        Err(error) => return Err(Box::new(Self::topology_failure(&error))),
                    }
                    if let Err(error) = driver.fold_and_commit_guarded(tree, |tree| {
                        tree.begin_adopt(&caller, &request, &facts)
                    }) {
                        // Prepare persisted a non-terminal receipt; close it.
                        if let Err(persist) = tree
                            .remember_failure(
                                &operation_id,
                                &caller,
                                "adopt_candidate",
                                &request,
                                error.code,
                            )
                            .and_then(|()| driver.save_guarded(tree))
                        {
                            return Err(Box::new(Self::topology_failure(&persist)));
                        }
                        return Err(Box::new(Self::topology_failure(&error)));
                    }
                    Ok::<_, Answered>(driver.hold_effects(tree.take_effects()))
                })
                .await
                .map_err(|_| lock_poisoned())?;
            match prepared {
                Ok(effects) => effects,
                Err(done) => return *done,
            }
        };
        let landed = match self.driver.settle(effects).await {
            Ok(settled) => settled.adopts.into_iter().next().unwrap_or_else(|| {
                Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "the adopt ticket was not completed",
                ))
            }),
            Err(error) => return Self::topology_failure(&error),
        };
        let driver = self.driver.clone();
        let operation_id = operation_id.to_owned();
        self.driver
            .write(move |tree| {
                let conflict = tree.take_adopt_conflict(request.candidate_id.as_str());
                match landed {
                    Ok(outcome) => {
                        crate::call_facts::leave(
                            &operation_id,
                            crate::call_facts::TopologyCallFacts::Adopted {
                                files_changed: outcome.files_changed,
                            },
                        );
                        let mut outcome = json!(outcome);
                        if !warning.is_empty() {
                            outcome["warning"] = json!(warning);
                        }
                        if let Err(error) = tree
                            .remember_success(
                                &operation_id,
                                &caller,
                                "adopt_candidate",
                                &request,
                                &outcome,
                            )
                            .and_then(|()| driver.save_guarded(tree))
                        {
                            return Self::topology_failure(&error);
                        }
                        ToolOutcome::success_text(outcome.to_string())
                    }
                    Err(error) => {
                        if let Some((paths, candidate)) =
                            conflict.filter(|_| error.code == TopologyErrorCode::MergeConflict)
                        {
                            crate::call_facts::leave(
                                &operation_id,
                                crate::call_facts::TopologyCallFacts::AdoptConflict {
                                    paths,
                                    candidate,
                                },
                            );
                        }
                        if let Err(persist) = tree
                            .remember_failure(
                                &operation_id,
                                &caller,
                                "adopt_candidate",
                                &request,
                                error.code,
                            )
                            .and_then(|()| driver.save_guarded(tree))
                        {
                            return Self::topology_failure(&persist);
                        }
                        Self::topology_failure(&error)
                    }
                }
            })
            .await
            .map_err(|_| lock_poisoned())?
    }

    async fn spawn_target(
        &self,
        caller: &str,
        request: &SpawnAgentRequest,
    ) -> Result<Option<String>, TopologyError> {
        let selection = self.driver.read(|tree| {
            if tree.replay_request_key(caller, request)?.is_some() {
                return Ok(None);
            }
            let node = tree.node(caller)?;
            let card = tree.caller_epoch(caller)?.binding(&request.template);
            let name = request.runner.as_ref().or_else(|| card?.runner.as_ref());
            Ok::<_, TopologyError>(
                name.filter(|name| name.as_str() != "local")
                    .map(|name| (name.clone(), node.placement.runner_id.clone())),
            )
        })??;
        let Some((name, parent)) = selection else {
            return Ok(None);
        };
        let binder = self.binder().ok_or_else(|| {
            TopologyError::new(
                TopologyErrorCode::RunnerNotConfigured,
                format!("runner {name:?} is not paired on this host"),
            )
        })?;
        let id = binder.resolve_runner(&name)?;
        let crosses =
            ProductAgentTree::cross_runner_spawn_source(parent.as_deref(), request, &id)?.is_some();
        binder.require_runner_ready(&id).await?;
        if !crosses {
            return Ok(None);
        }
        self.driver
            .vcs()
            .prepare_transfer(&id)
            .await
            .map_err(|error| {
                TopologyError::new(TopologyErrorCode::CrossRunnerCopyUnavailable, error.detail)
            })?;
        Ok(Some(id))
    }

    async fn spawn_placement(
        &self,
        caller: &str,
        request: &SpawnAgentRequest,
    ) -> Result<SpawnPlacement, TopologyError> {
        let runner = self.spawn_target(caller, request).await?;
        let (launch, path) = self.driver.read(|tree| {
            tree.node(caller)
                .map(|node| (tree.launch_checkout.clone(), node.path.clone()))
        })??;
        let vcs = self.driver.vcs();
        // The copy root is listed before reservation so a crashed JJ mint's
        // directory keeps its id. Transferred copies use Git only.
        let jj_copy_dirs = if runner.is_some() {
            Ok(std::collections::BTreeSet::new())
        } else if vcs.path_runner(&path).as_deref() == vcs.stage_runner() {
            vcs.jj_copy_dirs(&launch).await
        } else {
            vcs.jj_copy_dirs(&path).await
        };
        Ok(SpawnPlacement {
            runner,
            jj_copy_dirs,
        })
    }

    fn spawn_tool_receipt(
        &self,
        output: &SpawnAgentOutput,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let runner = match self.driver.read(|tree| {
            tree.node(&output.agent_id)
                .map(|node| node.placement.runner_id.clone())
        }) {
            Ok(Ok(runner)) => runner,
            Ok(Err(error)) | Err(error) => return Self::topology_failure(&error),
        };
        let mut value = json!(output);
        value["runner"] = json!(runner.as_deref().unwrap_or("local"));
        ToolOutcome::success_text(value.to_string())
    }

    /// `spawn_agent` in three phases, the same shape the
    /// adopt uses: admission, the reserved child id, and the prepared
    /// operation under the lock; the mint — a `git worktree add` or a
    /// `jj workspace add`, seconds of subprocess — with the lock released;
    /// the node, attempt, grant, and workspace rows and the receipt under the
    /// lock again. Every other tool call and TUI read proceeds while the mint
    /// runs, and until it answers no row names the reserved id, so a wait, a
    /// followup, or a close for it is refused exactly as one for an id that
    /// was never spawned.
    async fn spawn_admit(
        &self,
        caller: &str,
        operation_id: &str,
        request: &SpawnAgentRequest,
        resolved_account: Option<String>,
    ) -> Result<(Result<(SpawnAgentOutput, bool), TopologyError>, HeldEffects), ToolRuntimeError>
    {
        let tool = self.clone();
        let (caller, operation_id, request) =
            (caller.to_owned(), operation_id.to_owned(), request.clone());
        let placement = match self.spawn_placement(&caller, &request).await {
            Ok(placement) => placement,
            Err(error) => return Ok((Err(error), self.driver.hold_effects(Effects::default()))),
        };
        let binding = SpawnBinding {
            parent_composition: self
                .binder()
                .and_then(|binder| binder.spawn_parent_composition(&caller)),
            resolved_account,
            runner: placement.runner,
        };
        let (phase, effects) = self
            .driver
            .mutate({
                let (caller, operation_id, request) =
                    (caller.clone(), operation_id.clone(), request.clone());
                move |tree| {
                    tool.spawn_reserve_locked(
                        tree,
                        &caller,
                        &operation_id,
                        &request,
                        &placement.jj_copy_dirs,
                        binding,
                    )
                }
            })
            .await
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        let (reservation, ticket, stage) = match phase {
            Ok(SpawnPhase::Done(output, replayed)) => {
                return Ok((Ok((output, replayed)), effects));
            }
            Ok(SpawnPhase::Minting(reservation, ticket, stage)) => (reservation, ticket, stage),
            Err(error) => return Ok((Err(error), effects)),
        };
        // One release owner runs the mint and the writer that commits or
        // rolls back its reservation, as settle does: a caller dropped
        // mid-mint never strands the reservation. The mint itself is a task
        // of its own inside it, so once started it runs to its end.
        let driver = self.driver.clone();
        let (result, more) = self
            .driver
            .owned(async move {
                let port = driver.vcs();
                let staged = stage.clone();
                let minted = tokio::spawn(driver.with_stage_ledger(async move {
                    plakous_workspace::occupancy_vcs::stage::run(staged, ticket.mint(port.as_ref()))
                        .await
                }))
                .await
                .unwrap_or_else(|error| {
                    Err(VcsError::mint_failed(format!("mint task failed: {error}")))
                });
                let writer = driver.clone();
                driver
                    .mutate_owed(move |tree| {
                        // The mint's answer is recorded in this revision with
                        // its stage settled.
                        if let Some(stage) = &stage {
                            tree.settle_vcs_stage(stage);
                        }
                        match minted {
                            Ok(minted) => match tree.spawn_commit(*reservation, Some(minted)) {
                                Ok(output) => {
                                    writer.save(tree)?;
                                    Ok((output, false))
                                }
                                Err(error) => {
                                    tree.remember_failure(
                                        &operation_id,
                                        &caller,
                                        "spawn_agent",
                                        &request,
                                        error.code,
                                    )?;
                                    writer.save(tree)?;
                                    Err(error)
                                }
                            },
                            Err(error) => {
                                let error =
                                    tree.spawn_abort(*reservation, topology_error_from_vcs(error));
                                writer.save(tree)?;
                                Err(error)
                            }
                        }
                    })
                    .await
            })
            .await
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        let mut effects = effects;
        effects.absorb(more);
        Ok((result, effects))
    }

    /// The locked half of [`Self::spawn_admit`]: the operation ledger and the
    /// admission checks, with no subprocess.
    fn spawn_reserve_locked(
        &self,
        tree: &mut ProductAgentTree,
        caller: &str,
        operation_id: &str,
        request: &SpawnAgentRequest,
        jj_copy_dirs: &Result<std::collections::BTreeSet<String>, VcsError>,
        binding: SpawnBinding,
    ) -> Result<SpawnPhase, TopologyError> {
        if let Some(prior) = tree.prepare_operation(operation_id, caller, "spawn_agent", request)? {
            let output = serde_json::from_str(&prior).map_err(|_| {
                TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "prior topology receipt could not be reconstructed",
                )
            })?;
            return Ok(SpawnPhase::Done(output, true));
        }
        self.driver.save(tree)?;
        let admitted = tree.spawn_reserve_on(
            caller,
            request,
            Some(operation_id),
            jj_copy_dirs,
            binding.runner.as_deref(),
        );
        match admitted {
            Ok(SpawnAdmission::Replayed(output)) => {
                self.driver.save(tree)?;
                Ok(SpawnPhase::Done(output, false))
            }
            // An `inherit` bind mints nothing, so its rows and its receipt
            // land in this same lock.
            Ok(SpawnAdmission::Reserved(mut reservation, None)) => {
                reservation.set_parent_composition(binding.parent_composition);
                reservation.set_resolved_account(binding.resolved_account);
                match tree.spawn_commit(*reservation, None) {
                    Ok(output) => {
                        self.driver.save(tree)?;
                        Ok(SpawnPhase::Done(output, false))
                    }
                    Err(error) => {
                        tree.remember_failure(
                            operation_id,
                            caller,
                            "spawn_agent",
                            request,
                            error.code,
                        )?;
                        self.driver.save(tree)?;
                        Err(error)
                    }
                }
            }
            Ok(SpawnAdmission::Reserved(mut reservation, Some(ticket))) => {
                reservation.set_parent_composition(binding.parent_composition);
                reservation.set_resolved_account(binding.resolved_account);
                // On a runner the mint is a recorded stage, published before
                // it runs (spec 03 §14).
                let runner = tree.node(caller)?.placement.runner_id.clone();
                let stage = match runner.as_deref() {
                    Some(runner) => {
                        let stage = tree.record_mint_stage(&ticket, None, runner)?;
                        self.driver.save(tree)?;
                        Some(stage)
                    }
                    None => None,
                };
                Ok(SpawnPhase::Minting(reservation, ticket, stage))
            }
            Err(error) => {
                // Prepare already persisted a non-terminal receipt. Failures
                // must close it or reopen treats the op as RecoveryRequired.
                tree.remember_failure(operation_id, caller, "spawn_agent", request, error.code)?;
                self.driver.save(tree)?;
                Err(error)
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "spawn coordinates reservation, mint, bind, startup ownership and receipt completion without replaying effects"
    )]
    pub(super) async fn spawn_with(
        &self,
        operation_id: String,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self.caller_id.lock().map_or_default(|guard| guard.clone());
        let request = match decode::<SpawnAgentRequest>(value) {
            Ok(request) => request,
            Err(error) => {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
        };
        // A completed operation is already authorized by its exact durable
        // receipt. Account readiness may change before replay, so inspect the
        // recorded request before consulting the current account state.
        let replay = {
            let (operation_id, caller, request) =
                (operation_id.clone(), caller.clone(), request.clone());
            let replay = self
                .driver
                .write(move |tree| {
                    if let Some(prior) = tree.operations().get(&operation_id) {
                        if prior.caller_agent_id != caller {
                            return Err(TopologyError::new(
                                TopologyErrorCode::OperationRecoveryRequired,
                                "retry caller conflicts with the prepared operation",
                            ));
                        }
                        Ok(Some(tree.prepare_operation(
                            &operation_id,
                            &caller,
                            "spawn_agent",
                            &request,
                        )))
                    } else {
                        Ok(None)
                    }
                })
                .await
                .map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "topology tree lock is poisoned",
                    )
                })?;
            match replay {
                Ok(replay) => replay,
                Err(error) => return Self::topology_failure(&error),
            }
        };
        let replay = match replay {
            Some(Ok(Some(prior))) => {
                let output = serde_json::from_str::<SpawnAgentOutput>(&prior).map_err(|_| {
                    TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "prior topology receipt could not be reconstructed",
                    )
                });
                let output = match output {
                    Ok(output) => output,
                    Err(error) => return Self::topology_failure(&error),
                };
                Some(output)
            }
            Some(Err(error)) => return Self::topology_failure(&error),
            Some(Ok(None)) => {
                return Self::topology_failure(&TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "prepared spawn operation has no terminal receipt yet",
                ));
            }
            None => None,
        };
        // The caller's menu is checked under the tree lock, before either
        // binder admission. Product binders re-lock the hub snapshot, so the
        // lock is released before `admit_spawn` / `admit_binding`; holding it
        // across those calls deadlocks live spawn.
        let mut resolved_account = None;
        if replay.is_none()
            && let Some(binder) = self.binder()
        {
            let binding = {
                let (caller, request) = (caller.clone(), request.clone());
                let binding = self
                    .driver
                    .write(move |tree| {
                        let caller_template = tree.node(&caller)?.template.clone();
                        // Spawn authority is the caller's epoch.
                        let epoch = tree.caller_epoch(&caller)?;
                        if epoch.binding(&request.template).is_none() {
                            return Err(TopologyError::new(
                                TopologyErrorCode::TemplateUnknown,
                                format!(
                                    "template {} is not in this tree epoch's roster",
                                    request.template
                                ),
                            ));
                        }
                        if !epoch.menu_contains(
                            caller_template.as_ref().map(TemplateName::as_str),
                            &request.template,
                        ) {
                            return Err(TopologyError::new(
                                TopologyErrorCode::TemplateNotSpawnable,
                                format!(
                                    "template {} is not on this node's spawn menu: the root spawns the working \
                                     set, every other node its own template's spawns list minus itself",
                                    request.template
                                ),
                            ));
                        }
                        Ok(epoch.binding(&request.template).cloned())
                    })
                    .await
                    .map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "topology tree lock is poisoned",
                        )
                    })?;
                match binding {
                    Ok(binding) => binding,
                    Err(error) => return Self::topology_failure(&error),
                }
            };
            if let Err(error) = binder.admit_spawn(&caller, &request) {
                return Self::topology_failure(&error);
            }
            if let Some(mut binding) = binding {
                match binder.resolve_account(&binding.account).await {
                    Ok(Some(account)) => {
                        resolved_account = Some(account.clone());
                        binding.account = account;
                    }
                    Ok(None) => {}
                    Err(error) => return Self::topology_failure(&error),
                }
                if let Err(error) = binder.admit_binding(&request.template, &binding).await {
                    return Self::topology_failure(&error);
                }
            }
        }
        let (output, effects) = if let Some(output) = replay {
            (
                Ok((output, true)),
                self.driver.hold_effects(Effects::default()),
            )
        } else {
            self.spawn_admit(&caller, &operation_id, &request, resolved_account)
                .await?
        };
        let (output, replayed) = match output {
            Ok(output) => output,
            Err(error) => {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
        };
        // the hook runs once per mint, after the copy exists and
        // with the lock released, before any session is bound to the copy. A
        // non-zero exit or a timeout unminted the copy and failed the spawn
        // closed under the lock that recorded it.
        let settled = self.settle(effects).await?;
        if let Some(error) = settled.hook_failures.into_iter().next() {
            return Self::topology_failure(&error);
        }
        let poisoned = || {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Execution,
                "topology tree lock is poisoned",
            )
        };
        if !replayed {
            let observer = self.observer.clone();
            let (agent_id, caller, template) = (
                output.agent_id.clone(),
                caller.clone(),
                request.template.clone(),
            );
            self.driver
                .write(move |tree| {
                    if let Some(observer) = observer.as_ref() {
                        observer.node_spawned(
                            &tree.tree_id,
                            &agent_id,
                            Some(caller.as_str()),
                            template.as_str(),
                        );
                    }
                })
                .await
                .map_err(|_| poisoned())?;
        }
        let already_bound = {
            let agent_id = output.agent_id.clone();
            let bound = self
                .driver
                .write(move |tree| {
                    if replayed {
                        let node = tree.node(&agent_id).map_err(|error| {
                            Ok(ToolRuntimeError::new(
                                ToolRuntimeErrorKind::Execution,
                                error.message,
                            ))
                        })?;
                        let unresolved = tree.attempt(&agent_id).is_some_and(|attempt| {
                            attempt.has_unresolved_session_failure()
                                || (node.active_turn && attempt.turn_id.is_none())
                        });
                        if unresolved {
                            return Err(Err(()));
                        }
                    }
                    Ok(tree
                        .node(&agent_id)
                        .ok()
                        .and_then(|node| node.session_locator.clone()))
                })
                .await
                .map_err(|_| poisoned())?;
            match bound {
                Ok(bound) => bound,
                Err(Ok(error)) => return Err(error),
                Err(Err(())) => {
                    return ToolOutcome::failure_text(json!({ "code": TopologyErrorCode::OperationRecoveryRequired, "message": "child startup or session outcome remains unresolved; exact replay cannot dispatch another start" }).to_string());
                }
            }
        };
        if already_bound.is_some() {
            let driver = self.driver.clone();
            let agent_id = output.agent_id.clone();
            let recorded = self
                .driver
                .write(move |tree| {
                    if let Err(error) = tree.record_session_bound_after_failure(&agent_id) {
                        return BoundAfterFailure::Refused(error);
                    }
                    if let Err(error) = driver.save_guarded(tree) {
                        return BoundAfterFailure::Unpersisted(error);
                    }
                    match driver.fold_and_commit_guarded(tree, |tree| {
                        tree.fold_due_in_place(&agent_id).map(|_| ())
                    }) {
                        Ok(()) => BoundAfterFailure::Folded,
                        Err(error) => BoundAfterFailure::FoldRefused(error),
                    }
                })
                .await
                .map_err(|_| poisoned())?;
            return match recorded {
                BoundAfterFailure::Refused(error) => ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                ),
                BoundAfterFailure::Unpersisted(error) => {
                    self.fail_closed_session(
                        &output.agent_id,
                        format!("bound locator could not be persisted: {}", error.message),
                    )
                    .await
                }
                BoundAfterFailure::FoldRefused(error) => Self::topology_failure(&error),
                BoundAfterFailure::Folded => self.spawn_tool_receipt(&output),
            };
        }
        if let Some(binder) = self.binder() {
            let start = {
                let spawned = output.clone();
                self.driver
                    .write(move |tree| {
                        let node = tree.node(&spawned.agent_id).map_err(|error| {
                            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                        })?;
                        Ok::<_, ToolRuntimeError>(SpawnStart {
                            description: node.description.clone(),
                            prompt: node.prompt.clone(),
                            output_schema_json: node.output_schema_json.clone(),
                            turn_token: tree
                                .attempt(&spawned.agent_id)
                                .map(|attempt| attempt.turn_token)
                                .unwrap_or(1),
                            path: node.path.display().to_string(),
                            tier: node.tier,
                            account: node.account.clone(),
                            tuple_id: node.tuple_id.clone(),
                            effort: node.effort.clone(),
                            spawned,
                        })
                    })
                    .await
                    .map_err(|_| poisoned())??
            };
            match binder.bind(start).await {
                Ok(bound) => {
                    if bound.durably_registered {
                        return self.spawn_tool_receipt(&output);
                    }
                    let driver = self.driver.clone();
                    let agent_id = output.agent_id.clone();
                    let recorded = self
                        .driver
                        .write(move |tree| {
                            let bind = match tree
                                .node(&agent_id)
                                .ok()
                                .and_then(|node| node.session_locator.as_deref())
                            {
                                Some(existing) if existing == bound.session_locator => {
                                    tree.record_child_session(&agent_id, &bound.session_locator)
                                }
                                Some(_) => Err(TopologyError::new(
                                    TopologyErrorCode::OperationRecoveryRequired,
                                    "bound session locator differs from startup publication",
                                )),
                                None => {
                                    tree.record_child_session(&agent_id, &bound.session_locator)
                                }
                            };
                            if let Err(error) = bind {
                                return BoundSession::Refused(error);
                            }
                            if let Err(error) = driver.save_guarded(tree) {
                                return BoundSession::Unpersisted(format!(
                                    "bound session intent could not be persisted: {}",
                                    error.message
                                ));
                            }
                            let Ok(locator) = bound
                                .session_locator
                                .parse::<plakous::sessions::SessionLocator>()
                            else {
                                return BoundSession::InvalidAllocation;
                            };
                            if let Err(error) = tree.project_feed(
                                &agent_id,
                                &locator.session_id().to_string(),
                                &[],
                                &crate::tree::TreeFeedReads::default(),
                            ) {
                                return BoundSession::ProjectionRefused(TopologyError::from(error));
                            }
                            if let Some(turn_id) = bound.turn_id {
                                tree.bind_turn_owner(&agent_id, turn_id);
                            }
                            if let Err(error) = driver.save_guarded(tree) {
                                return BoundSession::Unpersisted(format!(
                                    "bound locator could not be persisted: {}",
                                    error.message
                                ));
                            }
                            BoundSession::Recorded
                        })
                        .await
                        .map_err(|_| poisoned())?;
                    return match recorded {
                        BoundSession::Refused(error) => ToolOutcome::failure_text(
                            json!({ "code": error.code, "message": error.message }).to_string(),
                        ),
                        BoundSession::Unpersisted(message) => {
                            self.fail_closed_session(&output.agent_id, message).await
                        }
                        BoundSession::InvalidAllocation => Err(ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "bound session allocation is invalid",
                        )),
                        BoundSession::ProjectionRefused(error) => Self::topology_failure(&error),
                        BoundSession::Recorded => self.spawn_tool_receipt(&output),
                    };
                }
                Err(SpawnBindError::Refused(message)) => {
                    let driver = self.driver.clone();
                    let observer = self.observer.clone();
                    let (caller, request, operation_id, agent_id, diagnostic) = (
                        caller.clone(),
                        request.clone(),
                        operation_id.clone(),
                        output.agent_id.clone(),
                        message.diagnostic.as_str().to_owned(),
                    );
                    let (persisted, effects) = self
                        .driver
                        .mutate(move |tree| {
                            let error = tree.fail_spawn_refused(
                                &caller,
                                &request,
                                Some(&operation_id),
                                &agent_id,
                                &diagnostic,
                            );
                            let status = tree
                                .node(&agent_id)
                                .ok()
                                .and_then(|node| NodeDetach::from_node_status(node.status))
                                .unwrap_or(NodeDetach::RecoveryRequired);
                            tree.try_emit_tree_node_detached(observer.as_ref(), &agent_id, status);
                            driver.save(tree).map(|()| error)
                        })
                        .await
                        .map_err(|error| {
                            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                        })?;
                    self.settle(effects).await?;
                    return match persisted {
                        Ok(error) | Err(error) => {
                            Self::topology_failure(&error.with_failure(&message))
                        }
                    };
                }
                Err(SpawnBindError::RecoveryRequired { stage, message }) => {
                    let driver = self.driver.clone();
                    let (agent_id, attempt_id, reason) = (
                        output.agent_id.clone(),
                        output.attempt_id.clone(),
                        format!("startup {stage:?}: {message}"),
                    );
                    let (persisted, effects) = self
                        .driver
                        .mutate(move |tree| {
                            let recorded = tree
                                .record_host_retirement_for_attempt(
                                    &agent_id,
                                    Some(&attempt_id),
                                    reason,
                                )
                                .and_then(|value| {
                                    driver.apply_recorded(tree, &agent_id).map(|()| value)
                                })?;
                            driver.save(tree)?;
                            Ok::<_, TopologyError>(recorded)
                        })
                        .await
                        .map_err(|error| {
                            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                        })?;
                    self.driver.settle(effects).await.map_err(|error| {
                        ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                    })?;
                    match persisted {
                        Err(error) => return Self::topology_failure(&error),
                        Ok(true) => self.driver.request_retirement_recovery(),
                        Ok(false) => {}
                    }
                    return ToolOutcome::failure_text(json!({ "code": TopologyErrorCode::OperationRecoveryRequired, "stage": format!("{stage:?}"), "message": message }).to_string());
                }
            }
        }
        self.spawn_tool_receipt(&output)
    }
}

/// What recording a session bound after a failed startup came to.
enum BoundAfterFailure {
    Refused(TopologyError),
    Unpersisted(TopologyError),
    FoldRefused(TopologyError),
    Folded,
}

/// What recording a freshly bound child session came to.
enum BoundSession {
    Refused(TopologyError),
    Unpersisted(String),
    InvalidAllocation,
    ProjectionRefused(TopologyError),
    Recorded,
}
