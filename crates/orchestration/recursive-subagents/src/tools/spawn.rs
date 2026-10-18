//! Spawning a child (the bind port, the minting phases) and landing its candidate.

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
    Refused(String),
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
        Self::Refused(message)
    }
}

impl From<&str> for SpawnBindError {
    fn from(message: &str) -> Self {
        Self::Refused(message.into())
    }
}

/// Binds a spawned child to a real session **and starts its assignment turn**.
///
/// `spawn_agent` carries the prompt (mail `delegate` is refused), so
/// the child must be running its brief when spawn returns. Leaving the first
/// turn to a later `send_agent_message` left children `running` with empty
/// transcripts forever and every `wait_agent` burning its full timeout.
pub trait SpawnedSessionBinder: Send + Sync {
    /// Product admission of the spawn request itself, before the tree
    /// resolves its template. The default admits everything.
    ///
    /// # Errors
    ///
    /// Refuses when the product will not admit this spawn.
    fn admit_spawn(&self, request: &SpawnAgentRequest) -> Result<(), TopologyError> {
        let _ = request;
        Ok(())
    }

    /// Product admission of the frozen card a spawn resolved to:
    /// account availability, and nothing about the wire. Called after
    /// [`Self::admit_spawn`], with the tree mutex released and before the
    /// tree admits a node, once the caller's menu contains
    /// `request.template`. A template the epoch does not carry, or one that
    /// is not on the caller's menu, never reaches here.
    /// A refusal here spends no node, attempt, or copy. The default admits
    /// everything. Wire-vocabulary rules (an OpenRouter card asking for
    /// `native-ptc`) live in the tree's own resolution, so a spawn with no
    /// binder installed is held to them too.
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

    /// Opens the child's session and starts its assignment turn. Awaited
    /// with the tree mutex released; the spawn records the answer afterwards.
    fn bind(
        &self,
        start: SpawnStart,
    ) -> Pin<Box<dyn Future<Output = Result<SpawnBound, SpawnBindError>> + Send + '_>>;
}

/// Where [`TopologyTool::spawn_admit`]'s first locked phase left the spawn:
/// a receipt (replayed, or an `inherit` bind that minted nothing), or a
/// reservation waiting on the mint that runs with the mutex released.
enum SpawnPhase {
    Done(SpawnAgentOutput, bool),
    Minting(Box<SpawnReservation>, MintTicket),
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
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
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
    pub(super) fn adopt_with(
        &self,
        operation_id: &str,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
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
        let effects = {
            let mut tree = self.driver.tree.lock().map_err(|_| lock_poisoned())?;
            let prepared = tree
                .prepare_operation(operation_id, &caller, "adopt_candidate", &request)
                .and_then(|prior| match prior {
                    Some(prior) => Ok(Some(prior)),
                    None => self.driver.save(&mut tree).map(|()| None),
                });
            match prepared {
                Ok(Some(prior)) => {
                    if let Some(files_changed) = serde_json::from_str::<Value>(&prior)
                        .ok()
                        .and_then(|receipt| receipt.get("files_changed")?.as_u64())
                        .and_then(|files| u32::try_from(files).ok())
                    {
                        crate::call_facts::leave(
                            operation_id,
                            crate::call_facts::TopologyCallFacts::Adopted { files_changed },
                        );
                    }
                    return ToolOutcome::success_text(prior);
                }
                Ok(None) => {}
                Err(error) => return Self::topology_failure(&error),
            }
            if let Err(error) = self
                .driver
                .fold_and_commit(&mut tree, |tree| tree.begin_adopt(&caller, &request))
            {
                // Prepare persisted a non-terminal receipt; close it.
                if let Err(persist) = tree
                    .remember_failure(
                        operation_id,
                        &caller,
                        "adopt_candidate",
                        &request,
                        error.code,
                    )
                    .and_then(|()| self.driver.save(&mut tree))
                {
                    return Self::topology_failure(&persist);
                }
                return Self::topology_failure(&error);
            }
            tree.take_effects()
        };
        let landed = match self.driver.settle(effects) {
            Ok(settled) => settled.adopts.into_iter().next().unwrap_or_else(|| {
                Err(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    "the adopt ticket was not completed",
                ))
            }),
            Err(error) => return Self::topology_failure(&error),
        };
        let mut tree = self.driver.tree.lock().map_err(|_| lock_poisoned())?;
        let conflict = tree.take_adopt_conflict(request.candidate_id.as_str());
        match landed {
            Ok(outcome) => {
                crate::call_facts::leave(
                    operation_id,
                    crate::call_facts::TopologyCallFacts::Adopted {
                        files_changed: outcome.files_changed,
                    },
                );
                let mut outcome = json!(outcome);
                if !warning.is_empty() {
                    outcome["warning"] = json!(warning);
                }
                if let Err(error) = tree
                    .remember_success(operation_id, &caller, "adopt_candidate", &request, &outcome)
                    .and_then(|()| self.driver.save(&mut tree))
                {
                    return Self::topology_failure(&error);
                }
                ToolOutcome::success_text(outcome.to_string())
            }
            Err(error) => {
                if let Some(paths) =
                    conflict.filter(|_| error.code == TopologyErrorCode::MergeConflict)
                {
                    crate::call_facts::leave(
                        operation_id,
                        crate::call_facts::TopologyCallFacts::AdoptConflict { paths },
                    );
                }
                if let Err(persist) = tree
                    .remember_failure(
                        operation_id,
                        &caller,
                        "adopt_candidate",
                        &request,
                        error.code,
                    )
                    .and_then(|()| self.driver.save(&mut tree))
                {
                    return Self::topology_failure(&persist);
                }
                Self::topology_failure(&error)
            }
        }
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
    ) -> Result<(Result<(SpawnAgentOutput, bool), TopologyError>, Effects), ToolRuntimeError> {
        let (phase, effects) = self
            .driver
            .mutate(|tree| self.spawn_reserve_locked(tree, caller, operation_id, request))
            .map_err(|error| {
                ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
            })?;
        let (reservation, ticket) = match phase {
            Ok(SpawnPhase::Done(output, replayed)) => {
                return Ok((Ok((output, replayed)), effects));
            }
            Ok(SpawnPhase::Minting(reservation, ticket)) => (reservation, ticket),
            Err(error) => return Ok((Err(error), effects)),
        };
        let port = self.driver.vcs();
        let minted = tokio::task::spawn_blocking(move || ticket.mint(port.as_ref()))
            .await
            .unwrap_or_else(|error| {
                Err(VcsError::mint_failed(format!("mint task failed: {error}")))
            });
        let (result, more) = self
            .driver
            .mutate(|tree| match minted {
                Ok(minted) => match tree.spawn_commit(*reservation, Some(minted)) {
                    Ok(output) => {
                        self.driver.save(tree)?;
                        Ok((output, false))
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
                },
                Err(error) => {
                    let error = tree.spawn_abort(*reservation, topology_error_from_vcs(error));
                    self.driver.save(tree)?;
                    Err(error)
                }
            })
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
        let admitted = tree.spawn_reserve(caller, request, Some(operation_id));
        match admitted {
            Ok(SpawnAdmission::Replayed(output)) => {
                self.driver.save(tree)?;
                Ok(SpawnPhase::Done(output, false))
            }
            // An `inherit` bind mints nothing, so its rows and its receipt
            // land in this same lock.
            Ok(SpawnAdmission::Reserved(reservation, None)) => {
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
            Ok(SpawnAdmission::Reserved(reservation, Some(ticket))) => {
                Ok(SpawnPhase::Minting(reservation, ticket))
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

    pub(super) async fn spawn_with(
        &self,
        operation_id: String,
        value: &Value,
    ) -> Result<ToolOutcome, ToolRuntimeError> {
        let caller = self
            .caller_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
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
            let mut tree = self.driver.tree.lock().map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?;
            if let Some(prior) = tree.operations().get(&operation_id) {
                if prior.caller_agent_id != caller {
                    return Self::topology_failure(&TopologyError::new(
                        TopologyErrorCode::OperationRecoveryRequired,
                        "retry caller conflicts with the prepared operation",
                    ));
                }
                Some(tree.prepare_operation(&operation_id, &caller, "spawn_agent", &request))
            } else {
                None
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
        if replay.is_none()
            && let Some(binder) = &self.binder
        {
            let binding = {
                let tree = self.driver.tree.lock().map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "topology tree lock is poisoned",
                    )
                })?;
                let caller_template = match tree.node(&caller) {
                    Ok(node) => node.template.clone(),
                    Err(error) => return Self::topology_failure(&error),
                };
                if tree.snapshot.binding(&request.template).is_none() {
                    return Self::topology_failure(&TopologyError::new(
                        TopologyErrorCode::TemplateUnknown,
                        format!(
                            "template {} is not in this tree epoch's roster",
                            request.template
                        ),
                    ));
                }
                if !tree.snapshot.menu_contains(
                    caller_template.as_ref().map(TemplateName::as_str),
                    &request.template,
                ) {
                    return Self::topology_failure(&TopologyError::new(
                        TopologyErrorCode::TemplateNotSpawnable,
                        format!(
                            "template {} is not on this node's spawn menu: the root spawns the working \
                             set, every other node its own template's spawns list minus itself",
                            request.template
                        ),
                    ));
                }
                tree.snapshot.binding(&request.template).cloned()
            };
            if let Err(error) = binder.admit_spawn(&request) {
                return Self::topology_failure(&error);
            }
            if let Some(binding) = binding
                && let Err(error) = binder.admit_binding(&request.template, &binding).await
            {
                return Self::topology_failure(&error);
            }
        }
        let (output, effects) = if let Some(output) = replay {
            (Ok((output, true)), Effects::default())
        } else {
            self.spawn_admit(&caller, &operation_id, &request).await?
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
        if !replayed {
            let tree = self.driver.tree.lock().map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?;
            if let Some(observer) = self.observer.as_ref() {
                observer.node_spawned(
                    &tree.tree_id,
                    &output.agent_id,
                    Some(caller.as_str()),
                    request.template.as_str(),
                );
            }
        }
        let already_bound = {
            let tree = self.driver.tree.lock().map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?;
            if replayed {
                let node = tree.node(&output.agent_id).map_err(|error| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                })?;
                let unresolved = tree.attempt(&output.agent_id).is_some_and(|attempt| {
                    attempt.has_unresolved_session_failure()
                        || (node.active_turn && attempt.turn_id.is_none())
                });
                if unresolved {
                    return ToolOutcome::failure_text(json!({ "code": TopologyErrorCode::OperationRecoveryRequired, "message": "child startup or session outcome remains unresolved; exact replay cannot dispatch another start" }).to_string());
                }
            }
            tree.node(&output.agent_id)
                .ok()
                .and_then(|node| node.session_locator.clone())
        };
        if already_bound.is_some() {
            let mut tree = self.driver.tree.lock().map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "topology tree lock is poisoned",
                )
            })?;
            if let Err(error) = tree.record_session_bound_after_failure(&output.agent_id) {
                return ToolOutcome::failure_text(
                    json!({ "code": error.code, "message": error.message }).to_string(),
                );
            }
            if let Err(error) = self.driver.save(&mut tree) {
                drop(tree);
                return self.fail_closed_session(
                    &output.agent_id,
                    format!("bound locator could not be persisted: {}", error.message),
                );
            }
            if let Err(error) = self.driver.fold_and_commit(&mut tree, |tree| {
                tree.fold_due_in_place(&output.agent_id).map(|_| ())
            }) {
                return Self::topology_failure(&error);
            }
            return ToolOutcome::success_text(json!(output).to_string());
        }
        if let Some(binder) = &self.binder {
            let start = {
                let tree = self.driver.tree.lock().map_err(|_| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Execution,
                        "topology tree lock is poisoned",
                    )
                })?;
                let node = tree.node(&output.agent_id).map_err(|error| {
                    ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                })?;
                SpawnStart {
                    spawned: output.clone(),
                    description: node.description.clone(),
                    prompt: node.prompt.clone(),
                    output_schema_json: node.output_schema_json.clone(),
                    turn_token: tree
                        .attempt(&output.agent_id)
                        .map(|attempt| attempt.turn_token)
                        .unwrap_or(1),
                    path: node.path.display().to_string(),
                    tier: node.tier,
                    account: node.account.clone(),
                    tuple_id: node.tuple_id.clone(),
                    effort: node.effort.clone(),
                }
            };
            match binder.bind(start).await {
                Ok(bound) => {
                    if bound.durably_registered {
                        return ToolOutcome::success_text(json!(output).to_string());
                    }
                    let mut tree = self.driver.tree.lock().map_err(|_| {
                        ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "topology tree lock is poisoned",
                        )
                    })?;
                    let bind = match tree
                        .node(&output.agent_id)
                        .ok()
                        .and_then(|node| node.session_locator.as_deref())
                    {
                        Some(existing) if existing == bound.session_locator => {
                            tree.record_child_session(&output.agent_id, &bound.session_locator)
                        }
                        Some(_) => Err(TopologyError::new(
                            TopologyErrorCode::OperationRecoveryRequired,
                            "bound session locator differs from startup publication",
                        )),
                        None => tree.record_child_session(&output.agent_id, &bound.session_locator),
                    };
                    if let Err(error) = bind {
                        return ToolOutcome::failure_text(
                            json!({ "code": error.code, "message": error.message }).to_string(),
                        );
                    }
                    if let Err(error) = self.driver.save(&mut tree) {
                        drop(tree);
                        return self.fail_closed_session(
                            &output.agent_id,
                            format!(
                                "bound session intent could not be persisted: {}",
                                error.message
                            ),
                        );
                    }
                    let locator = bound
                        .session_locator
                        .parse::<plakous::sessions::SessionLocator>()
                        .map_err(|_| {
                            ToolRuntimeError::new(
                                ToolRuntimeErrorKind::Execution,
                                "bound session allocation is invalid",
                            )
                        })?;
                    if let Err(error) = tree.project_feed(
                        &output.agent_id,
                        &locator.session_id().to_string(),
                        &[],
                        &crate::tree::TreeFeedReads::default(),
                    ) {
                        return Self::topology_failure(&TopologyError::from(error));
                    }
                    if let Some(turn_id) = bound.turn_id {
                        tree.bind_turn_owner(&output.agent_id, turn_id);
                    }
                    if let Err(error) = self.driver.save(&mut tree) {
                        drop(tree);
                        return self.fail_closed_session(
                            &output.agent_id,
                            format!("bound locator could not be persisted: {}", error.message),
                        );
                    }
                    return ToolOutcome::success_text(json!(output).to_string());
                }
                Err(SpawnBindError::Refused(message)) => {
                    let (persisted, effects) = self
                        .driver
                        .mutate(|tree| {
                            let error = tree.fail_spawn_refused(
                                &caller,
                                &request,
                                Some(&operation_id),
                                &output.agent_id,
                                &message,
                            );
                            let status = tree
                                .node(&output.agent_id)
                                .ok()
                                .and_then(|node| NodeDetach::from_node_status(node.status))
                                .unwrap_or(NodeDetach::RecoveryRequired);
                            tree.try_emit_tree_node_detached(
                                self.observer.as_ref(),
                                &output.agent_id,
                                status,
                            );
                            self.driver.save(tree).map(|()| error)
                        })
                        .map_err(|error| {
                            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                        })?;
                    self.settle(effects).await?;
                    return match persisted {
                        Ok(error) | Err(error) => Self::topology_failure(&error),
                    };
                }
                Err(SpawnBindError::RecoveryRequired { stage, message }) => {
                    let (persisted, effects) = self
                        .driver
                        .mutate(|tree| {
                            let recorded = tree
                                .record_host_retirement_for_attempt(
                                    &output.agent_id,
                                    Some(&output.attempt_id),
                                    format!("startup {stage:?}: {message}"),
                                )
                                .and_then(|value| {
                                    self.driver
                                        .apply_recorded(tree, &output.agent_id)
                                        .map(|()| value)
                                })?;
                            self.driver.save(tree)?;
                            Ok::<_, TopologyError>(recorded)
                        })
                        .map_err(|error| {
                            ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, error.message)
                        })?;
                    self.driver.settle(effects).map_err(|error| {
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
        ToolOutcome::success_text(json!(output).to_string())
    }
}
