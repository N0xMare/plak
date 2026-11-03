use super::{
    Arc, FutureExt, HOST_RETIRE_JOIN_TIMEOUT, LiveChildHandle, LiveChildTable, RETIRE_JOIN_EXPIRED,
    Retirement, RetirementFaultSource, TreeDriver, TurnId, publish_release,
    record_retirement_fault, retirement_needs_recovery,
};

impl LiveChildTable {
    /// An active child; retiring children cannot accept new work.
    pub fn get(&self, agent_id: &str) -> Option<&(dyn LiveChildHandle + 'static)> {
        self.children.get(agent_id).map(Box::as_ref)
    }

    /// The active child under its admission lock.
    pub fn get_mut(&mut self, agent_id: &str) -> Option<&mut (dyn LiveChildHandle + 'static)> {
        self.children.get_mut(agent_id).map(Box::as_mut)
    }

    /// Whether an active or retiring child owns this agent slot.
    pub fn contains_key(&self, agent_id: &str) -> bool {
        self.children.contains_key(agent_id)
            || self.retiring.contains_key(agent_id)
            || self.starting.contains_key(agent_id)
    }

    /// All active and retiring agent slots.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.children
            .keys()
            .chain(self.retiring.keys())
            .chain(self.starting.keys())
    }

    /// Whether every active and retiring slot is vacant.
    pub fn is_empty(&self) -> bool {
        self.children.is_empty() && self.retiring.is_empty() && self.starting.is_empty()
    }

    #[allow(
        clippy::too_many_lines,
        reason = "reserve the exact child slot and install its independently owned release worker atomically"
    )]
    pub(super) fn start_retire(
        &mut self,
        live: Arc<tokio::sync::Mutex<Self>>,
        driver: TreeDriver,
        agent_id: &str,
        expected: Option<TurnId>,
        cancel: bool,
    ) -> Option<Retirement> {
        if let Some(retirement) = self.retiring.get(agent_id) {
            return expected
                .is_none_or(|turn| turn == retirement.turn)
                .then(|| retirement.clone());
        }
        let turn = self.children.get(agent_id)?.turn_id();
        if expected.is_some_and(|expected| expected != turn) {
            return None;
        }
        let child = self.children.remove(agent_id)?;
        let agent = child.agent();
        let agent_id = agent_id.to_owned();
        let worker_id = agent_id.clone();
        let expected_attempt = driver
            .read(|tree| {
                tree.attempt(&worker_id)
                    .map(|attempt| attempt.attempt_id.clone())
            })
            .ok()
            .flatten();
        let retirement_attempt = expected_attempt.clone();
        let recovery = self.retirement_recovery.clone();
        let (dispatched, dispatch) = tokio::sync::oneshot::channel();
        // Spawn before yielding the admission lock: no caller cancellation can
        // strand an extracted child between reservation and task ownership.
        let owners = driver.release_owners();
        let task = owners.spawn(async move {
            let started = tokio::time::Instant::now();
            let mut release_returned = false;
            let mut cleanup_failure = None;
            let mut fault_recorded = false;
            let release_driver = driver.clone();
            let release = async move {
                if cancel {
                    child
                        .cancel_and_release(release_driver, Some(dispatched))
                        .await
                } else {
                    let _ = dispatched.send(());
                    child.release(release_driver).await
                }
            };
            let result = std::panic::AssertUnwindSafe(async {
                tokio::pin!(release);
                let released = tokio::select! {
                    released = &mut release => released,
                    () = tokio::time::sleep(HOST_RETIRE_JOIN_TIMEOUT) => {
                        tracing::warn!(target: "plak.topology", agent_id = %worker_id,
                            "slow child retirement; release owner retained");
                        record_retirement_fault(&driver, &worker_id, expected_attempt.as_ref(), RetirementFaultSource::Owner, RETIRE_JOIN_EXPIRED).await;
                        fault_recorded = true;
                        release.await
                    }
                };
                release_returned = true;
                cleanup_failure.clone_from(&released.failure);
                publish_release(&driver, &worker_id, released).await
            })
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err("child retirement worker panicked".to_owned()));
            if let Err(error) = &result {
                record_retirement_fault(
                    &driver,
                    &worker_id,
                    expected_attempt.as_ref(),
                    if fault_recorded {
                        RetirementFaultSource::OwnerAlreadyReported
                    } else {
                        RetirementFaultSource::Owner
                    },
                    error,
                )
                .await;
                fault_recorded = true;
            }
            {
                let mut live = live.lock().await;
                if live
                    .retiring
                    .get(&worker_id)
                    .is_some_and(|retirement| retirement.turn == turn)
                {
                    live.retiring.remove(&worker_id);
                    let (writer, agent, released) =
                        (driver.clone(), worker_id.clone(), result.is_ok());
                    let _ = driver
                        .mutate(move |tree| {
                            tree.release_turn_owner(&agent);
                            if released
                                && tree.node(&agent).is_ok_and(|node| {
                                    node.status == crate::protocol::NodeStatus::Closed
                                })
                            {
                                tree.try_emit_tree_node_detached(
                                    writer.observer(),
                                    &agent,
                                    crate::tree::NodeDetach::Closed,
                                );
                            }
                        })
                        .await;
                }
            }
            tracing::info!(target: "plak.topology", agent_id = %worker_id,
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                release_returned, cleanup_failure = ?cleanup_failure,
                success = result.is_ok(), retirement_failure = ?result.as_ref().err(),
                "child retirement returned");
            if (fault_recorded || retirement_needs_recovery(&driver, &worker_id))
                && let Some(recovery) = recovery
            {
                recovery();
            }
            result
        });
        let retirement = Retirement {
            agent,
            turn,
            expected_attempt: retirement_attempt,
            recovery: self.retirement_recovery.clone(),
            dispatched: dispatch.map(|_| ()).boxed().shared(),
            abort: task.abort_handle(),
            completion: task
                .map(|result| {
                    result.unwrap_or_else(|error| {
                        Err(if error.is_panic() {
                            "child retirement worker panicked".to_owned()
                        } else {
                            "child retirement worker was aborted".to_owned()
                        })
                    })
                })
                .boxed()
                .shared(),
        };
        self.retiring.insert(agent_id, retirement.clone());
        Some(retirement)
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Installs a test child without runtime admission.
    pub fn insert_for_test(&mut self, agent_id: String, child: Box<dyn LiveChildHandle>) {
        self.children.insert(agent_id, child);
    }
}
