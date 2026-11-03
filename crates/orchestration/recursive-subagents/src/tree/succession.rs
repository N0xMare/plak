//! Live succession of the root node: the root-tuple view a line-head
//! advance moves, and the `epoch_pending` publish-or-validate pair of a
//! profile switch (spec 03 §1.2, §14.3).
//!
//! A switch records the successor epoch's exact bytes before the runtime
//! publication that compiles against them. Once the publication is known to
//! have landed, [`ProductAgentTree::settle_epoch_pending`] appends exactly
//! those bytes; when it is known not to have landed,
//! [`ProductAgentTree::withdraw_epoch_pending`] removes the row. Nothing else
//! reads the row.

use super::{
    EpochPending, OrchestrationSnapshot, ProductAgentTree, TopologyError, TopologyErrorCode,
};

/// The `agent_operations` key of `workflow_id`'s pending epoch.
fn operation_id(workflow_id: &str) -> String {
    format!("plak:epoch_pending:{workflow_id}")
}

impl ProductAgentTree {
    /// Moves the tree's view of the root binding to what the line head now
    /// runs (a rebind that keeps the roster). No epoch changes, so live
    /// descendants and the admission state are untouched.
    pub fn record_root_bind(&mut self, account: &str, tuple_id: &str, effort: Option<&str>) {
        let root_id = self.root_agent_id.clone();
        if let Some(root) = self.nodes.get_mut(&root_id) {
            if !account.is_empty() {
                account.clone_into(&mut root.account);
            }
            tuple_id.clone_into(&mut root.tuple_id);
            root.effort = effort.map(str::to_owned);
        }
        tuple_id.clone_into(&mut self.selected_tuple);
        self.allowed_tuples.insert(tuple_id.to_owned());
        self.bump();
    }

    /// Records `snapshot` as the epoch `workflow_id`'s publication installs.
    /// Recording the same bytes again returns the recorded row.
    ///
    /// # Errors
    ///
    /// Refuses a snapshot whose digest does not match its contents, a root
    /// spawn mid-reservation, an append the epoch bound would refuse, another
    /// workflow's pending epoch, and different bytes under this workflow.
    pub fn record_epoch_pending(
        &mut self,
        workflow_id: &str,
        snapshot: &OrchestrationSnapshot,
    ) -> Result<EpochPending, TopologyError> {
        let epoch_no = self.epochs().current.checked_add(1).ok_or_else(|| {
            TopologyError::new(TopologyErrorCode::BoundExceeded, "epoch number overflow")
        })?;
        let pending = EpochPending::of(epoch_no, workflow_id, snapshot)?;
        if let Some(recorded) = self.epoch_pending()?.into_iter().next() {
            if recorded == pending {
                return Ok(recorded);
            }
            let message = if recorded.workflow_id == workflow_id {
                "this profile switch already recorded another successor epoch"
            } else {
                "another profile switch is pending on this tree"
            };
            let code = if recorded.workflow_id == workflow_id {
                TopologyErrorCode::OperationRecoveryRequired
            } else {
                TopologyErrorCode::GrantDenied
            };
            return Err(TopologyError::new(code, message));
        }
        // An append that would be refused afterwards is refused now, before
        // the runtime publishes a successor compiled against these bytes.
        self.check_appendable(snapshot)?;
        let row =
            pending.to_operation(operation_id(workflow_id), &self.root_agent_id.clone(), None)?;
        self.operations.insert(row.operation_id.clone(), row);
        self.bump();
        Ok(pending)
    }

    /// Every pending epoch this tree records, oldest first. There is at most
    /// one.
    ///
    /// # Errors
    ///
    /// Refuses a malformed row.
    pub fn epoch_pending(&self) -> Result<Vec<EpochPending>, TopologyError> {
        let mut pending = Vec::new();
        for operation in self.operations.values() {
            if let Some(row) = EpochPending::from_operation(operation)? {
                pending.push(row);
            }
        }
        Ok(pending)
    }

    /// Installs the epoch `workflow_id`'s published successor compiled
    /// against: appends the recorded bytes and removes the row in the same
    /// revision. Returns the installed epoch's number, or `None` when no row
    /// is pending for this workflow (already settled, or never recorded).
    ///
    /// # Errors
    ///
    /// Refuses a malformed row, a row numbered for another epoch than the
    /// next one, and any refusal of [`Self::append_epoch`].
    pub fn settle_epoch_pending(
        &mut self,
        workflow_id: &str,
    ) -> Result<Option<u64>, TopologyError> {
        let key = operation_id(workflow_id);
        let Some(operation) = self.operations.get(&key) else {
            return Ok(None);
        };
        let Some(pending) = EpochPending::from_operation(operation)? else {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "a pending epoch key holds another operation",
            ));
        };
        if Some(pending.epoch_no) != self.epochs().current.checked_add(1) {
            return Err(TopologyError::new(
                TopologyErrorCode::OperationRecoveryRequired,
                "the pending epoch is not this tree's next epoch",
            ));
        }
        let installed = self.append_epoch(pending.snapshot()?)?;
        self.operations.remove(&key);
        Ok(Some(installed))
    }

    /// Removes `workflow_id`'s pending epoch, whose publication is known not
    /// to have landed. Returns whether a row was removed.
    pub fn withdraw_epoch_pending(&mut self, workflow_id: &str) -> bool {
        let removed = self.operations.remove(&operation_id(workflow_id)).is_some();
        if removed {
            self.bump();
        }
        removed
    }

    /// Removes every pending epoch but `keep`'s: a switch that did not
    /// publish onto the head now opening. Returns the withdrawn workflows.
    ///
    /// # Errors
    ///
    /// Refuses a malformed row.
    pub fn withdraw_epoch_pending_except(
        &mut self,
        keep: Option<&str>,
    ) -> Result<Vec<String>, TopologyError> {
        let stale: Vec<String> = self
            .epoch_pending()?
            .into_iter()
            .map(|pending| pending.workflow_id)
            .filter(|workflow| Some(workflow.as_str()) != keep)
            .collect();
        for workflow in &stale {
            self.withdraw_epoch_pending(workflow);
        }
        Ok(stale)
    }
}
