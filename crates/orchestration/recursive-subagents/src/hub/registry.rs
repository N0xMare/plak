use super::{
    Arc, ChildStartup, FutureExt, HOST_RETIRE_JOIN_TIMEOUT, LiveChildHandle, LiveChildTable,
    RETIRE_JOIN_EXPIRED, Residency, Retirement, RetirementFaultSource, TreeDriver, TurnId,
    publish_release, record_retirement_fault, retirement_needs_recovery,
};

impl LiveChildTable {
    /// A resident child; an opening or releasing one takes no new work.
    pub fn get(&self, agent_id: &str) -> Option<&(dyn LiveChildHandle + 'static)> {
        match self.nodes.get(agent_id)? {
            Residency::Resident(child) => Some(child.as_ref()),
            Residency::Opening(_) | Residency::Releasing(_) => None,
        }
    }

    /// The resident child under its admission lock.
    pub fn get_mut(&mut self, agent_id: &str) -> Option<&mut (dyn LiveChildHandle + 'static)> {
        match self.nodes.get_mut(agent_id)? {
            Residency::Resident(child) => Some(child.as_mut()),
            Residency::Opening(_) | Residency::Releasing(_) => None,
        }
    }

    /// Whether this agent's node is not dormant: opening, resident or
    /// releasing.
    pub fn contains_key(&self, agent_id: &str) -> bool {
        self.nodes.contains_key(agent_id)
    }

    /// Every node that is not dormant.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.nodes.keys()
    }

    /// Whether every child node is dormant.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The resident children.
    pub(super) fn residents(&self) -> impl Iterator<Item = (&String, &dyn LiveChildHandle)> {
        self.nodes.iter().filter_map(|(id, node)| match node {
            Residency::Resident(child) => Some((id, child.as_ref())),
            Residency::Opening(_) | Residency::Releasing(_) => None,
        })
    }

    /// The opening children's startups.
    pub(super) fn openings(&self) -> impl Iterator<Item = (&String, &ChildStartup)> {
        self.nodes.iter().filter_map(|(id, node)| match node {
            Residency::Opening(startup) => Some((id, &**startup)),
            Residency::Resident(_) | Residency::Releasing(_) => None,
        })
    }

    /// The releasing children's retirements.
    pub(super) fn releasings(&self) -> impl Iterator<Item = (&String, &Retirement)> {
        self.nodes.iter().filter_map(|(id, node)| match node {
            Residency::Releasing(retirement) => Some((id, &**retirement)),
            Residency::Opening(_) | Residency::Resident(_) => None,
        })
    }

    /// The startup that owns this agent's opening node.
    pub(super) fn opening(&self, agent_id: &str) -> Option<&ChildStartup> {
        match self.nodes.get(agent_id)? {
            Residency::Opening(startup) => Some(&**startup),
            Residency::Resident(_) | Residency::Releasing(_) => None,
        }
    }

    /// The retirement of this agent's releasing node.
    pub(super) fn releasing(&self, agent_id: &str) -> Option<&Retirement> {
        match self.nodes.get(agent_id)? {
            Residency::Releasing(retirement) => Some(&**retirement),
            Residency::Opening(_) | Residency::Resident(_) => None,
        }
    }

    /// The node opens under `startup`.
    pub(super) fn open(&mut self, agent_id: String, startup: ChildStartup) {
        self.nodes
            .insert(agent_id, Residency::Opening(Box::new(startup)));
    }

    /// The node is resident with `child`'s turn, whatever it was.
    pub(super) fn reside(&mut self, agent_id: String, child: Box<dyn LiveChildHandle>) {
        self.nodes.insert(agent_id, Residency::Resident(child));
    }

    /// The node goes dormant.
    pub(super) fn vacate(&mut self, agent_id: &str) {
        self.nodes.remove(agent_id);
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
        if let Some(retirement) = self.releasing(agent_id) {
            return expected
                .is_none_or(|turn| turn == retirement.turn)
                .then(|| retirement.clone());
        }
        let turn = self.get(agent_id)?.turn_id();
        if expected.is_some_and(|expected| expected != turn) {
            return None;
        }
        let Some(Residency::Resident(child)) = self.nodes.remove(agent_id) else {
            unreachable!("a resident node was just read");
        };
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
                    .releasing(&worker_id)
                    .is_some_and(|retirement| retirement.turn == turn)
                {
                    live.vacate(&worker_id);
                    live.quiet.notify_one();
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
        self.nodes
            .insert(agent_id, Residency::Releasing(Box::new(retirement.clone())));
        Some(retirement)
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Installs a test child without runtime admission.
    pub fn insert_for_test(&mut self, agent_id: String, child: Box<dyn LiveChildHandle>) {
        self.reside(agent_id, child);
    }

    /// The retirement of a releasing node, for tests that join it.
    #[cfg(test)]
    pub(super) fn releasing_for_test(&self, agent_id: &str) -> Retirement {
        self.releasing(agent_id)
            .expect("the node is releasing")
            .clone()
    }
}
