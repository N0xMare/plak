//! The append-sharing operation ledger: authenticated command and operation
//! records and the persistent maps that index them.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the manifest module that shares its vocabulary"
)]

use super::*;

/// One append-sharing authenticated command-ledger node.
#[derive(Debug)]
pub(super) struct LedgerNode<T> {
    pub(super) record: T,
    pub(super) previous: Option<Arc<LedgerNode<T>>>,
    pub(super) identity: Digest,
}

/// One node in the persistent exact command-identity membership index.
#[derive(Debug)]
pub(super) struct CommandIdNode {
    pub(super) command_id: Arc<str>,
    pub(super) left: Option<Arc<Self>>,
    pub(super) right: Option<Arc<Self>>,
    pub(super) height: u8,
    pub(super) identity: Digest,
}

#[derive(Clone, Debug)]
pub(super) struct AuthenticatedOperationRecord {
    pub(super) record: Arc<DurableOperationRecord>,
    pub(super) identity: Digest,
}

impl AuthenticatedOperationRecord {
    pub(super) fn new(record: DurableOperationRecord) -> Result<Self, RuntimeManifestError> {
        let bytes = encode_bundle_operation_record(&record)?;
        let identity = Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"plakous-agent/runtime-operation-record-v1", &bytes],
        )
        .map_err(|_| RuntimeManifestError::InvalidDigest)?;
        Ok(Self {
            record: Arc::new(record),
            identity,
        })
    }
}

/// One authenticated node in the persistent operation AVL map.
#[derive(Debug)]
pub(super) struct OperationMapNode {
    pub(super) entry: AuthenticatedOperationRecord,
    pub(super) left: Option<Arc<Self>>,
    pub(super) right: Option<Arc<Self>>,
    pub(super) height: u8,
    pub(super) count: usize,
    pub(super) unresolved_count: usize,
    pub(super) identity: Digest,
}

/// One immutable operation-ledger mutation and its resulting map identity.
#[derive(Debug)]
pub(super) struct OperationLedgerMutation {
    pub(super) previous: Option<Arc<Self>>,
    pub(super) inserted: bool,
    pub(super) entry: AuthenticatedOperationRecord,
    pub(super) operation_identity: Digest,
}

/// A borrowed operation mutation used by incremental checkpoint persistence.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OperationLedgerMutationRef<'a> {
    pub(super) inserted: bool,
    pub(super) record: &'a DurableOperationRecord,
}

impl<'a> OperationLedgerMutationRef<'a> {
    pub(crate) const fn inserted(self) -> bool {
        self.inserted
    }

    pub(crate) const fn record(self) -> &'a DurableOperationRecord {
        self.record
    }
}

/// Exact command and physical-operation ledger.
#[derive(Clone, Debug)]
pub struct RuntimeOperationLedger {
    pub(super) command_tail: Option<Arc<LedgerNode<DurableCommandRecord>>>,
    pub(super) command_ids: Option<Arc<CommandIdNode>>,
    pub(super) operation_root: Option<Arc<OperationMapNode>>,
    pub(super) operation_version: Option<Arc<OperationLedgerMutation>>,
    pub(super) command_count: usize,
    pub(super) operation_count: usize,
}

impl RuntimeOperationLedger {
    /// Creates a canonically ordered exact ledger for cold decode/import.
    pub fn new(
        mut commands: Vec<DurableCommandRecord>,
        mut operations: Vec<DurableOperationRecord>,
    ) -> Result<Self, RuntimeManifestError> {
        if commands.len() > MAX_RUNTIME_RECORDS || operations.len() > MAX_RUNTIME_RECORDS {
            return Err(RuntimeManifestError::RecordLimit);
        }
        commands.sort_by_key(|record| record.sequence);
        operations.sort_by_key(DurableOperationRecord::operation_id);
        let command_ids = commands
            .iter()
            .map(|record| record.command_id.as_str())
            .collect::<BTreeSet<_>>();
        if command_ids.len() != commands.len()
            || commands
                .windows(2)
                .any(|pair| pair[0].sequence == pair[1].sequence)
            || operations
                .windows(2)
                .any(|pair| pair[0].operation_id == pair[1].operation_id)
        {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        let mut ledger = Self {
            command_tail: None,
            command_ids: None,
            operation_root: None,
            operation_version: None,
            command_count: 0,
            operation_count: 0,
        };
        for record in commands {
            ledger = ledger.append_command(record)?;
        }
        for record in operations {
            ledger = ledger.append_operation(record)?;
        }
        Ok(ledger)
    }

    /// Returns ordered command records for cold encoding/inspection.
    #[must_use]
    pub fn commands(&self) -> Vec<&DurableCommandRecord> {
        collect_ledger_records(self.command_tail.as_deref(), self.command_count)
    }

    /// Returns ordered physical-operation records for cold encoding/inspection.
    #[must_use]
    pub fn operations(&self) -> Vec<&DurableOperationRecord> {
        let mut records = Vec::with_capacity(self.operation_count);
        collect_operation_records(self.operation_root.as_deref(), &mut records);
        records
    }

    pub(crate) fn operation(&self, operation_id: u64) -> Option<&DurableOperationRecord> {
        operation_map_get(self.operation_root.as_deref(), operation_id)
    }

    pub(crate) fn newest_operations(&self, max_records: usize) -> Vec<&DurableOperationRecord> {
        fn collect<'a>(
            node: Option<&'a OperationMapNode>,
            max: usize,
            records: &mut Vec<&'a DurableOperationRecord>,
        ) {
            if records.len() >= max {
                return;
            }
            let Some(node) = node else {
                return;
            };
            collect(node.right.as_deref(), max, records);
            if records.len() < max {
                records.push(&node.entry.record);
                collect(node.left.as_deref(), max, records);
            }
        }
        let mut records = Vec::new();
        collect(self.operation_root.as_deref(), max_records, &mut records);
        records
    }

    pub(crate) const fn command_count(&self) -> usize {
        self.command_count
    }

    pub(crate) const fn operation_count(&self) -> usize {
        self.operation_count
    }

    pub(crate) fn authority_identity(&self) -> Result<Digest, RuntimeManifestError> {
        let command_count = u64::try_from(self.command_count)
            .map_err(|_| RuntimeManifestError::RecordLimit)?
            .to_be_bytes();
        let operation_count = u64::try_from(self.operation_count)
            .map_err(|_| RuntimeManifestError::RecordLimit)?
            .to_be_bytes();
        let command = self
            .command_tail
            .as_ref()
            .map_or(&[][..], |tail| tail.identity.bytes());
        let operation = self
            .operation_root
            .as_ref()
            .map_or(&[][..], |root| root.identity.bytes());
        Digest::sha256(
            DigestDomain::ProviderOperation,
            4,
            &[
                b"plakous-agent/runtime-ledger-authority-v1",
                &command_count,
                command,
                &operation_count,
                operation,
            ],
        )
        .map_err(|_| RuntimeManifestError::InvalidDigest)
    }

    pub(super) fn unresolved_operation_count(&self) -> usize {
        self.operation_root
            .as_ref()
            .map_or(0, |node| node.unresolved_count)
    }

    pub(crate) fn command_delta_since<'a>(
        &'a self,
        ancestor: &Self,
    ) -> Option<Vec<&'a DurableCommandRecord>> {
        ledger_delta_since(
            self.command_tail.as_deref(),
            self.command_count,
            ancestor.command_tail.as_deref(),
            ancestor.command_count,
        )
        .map(|(_, records)| records)
    }

    pub(crate) fn operation_delta_since<'a>(
        &'a self,
        ancestor: &Self,
    ) -> Option<Vec<OperationLedgerMutationRef<'a>>> {
        let ancestor_identity = ancestor.operation_root.as_ref().map(|node| node.identity);
        let ancestor_version = ancestor.operation_version.as_deref();
        let mut version = self.operation_version.as_deref();
        let mut reverse = Vec::new();
        loop {
            let pointer_match = match (version, ancestor_version) {
                (Some(current), Some(ancestor)) => std::ptr::eq(current, ancestor),
                (None, None) => true,
                (None, Some(_)) | (Some(_), None) => false,
            };
            let semantic_match = version.map(|item| item.operation_identity) == ancestor_identity;
            if pointer_match || semantic_match {
                reverse.reverse();
                return Some(reverse);
            }
            let mutation = version?;
            reverse.push(OperationLedgerMutationRef {
                inserted: mutation.inserted,
                record: &mutation.entry.record,
            });
            version = mutation.previous.as_deref();
        }
    }

    /// Appends one ordered command record while sharing the complete prefix.
    pub fn append_command(
        &self,
        record: DurableCommandRecord,
    ) -> Result<Self, RuntimeManifestError> {
        if self.command_count >= MAX_RUNTIME_RECORDS
            || self
                .command_tail
                .as_ref()
                .is_some_and(|tail| tail.record.sequence >= record.sequence)
            || command_id_contains(self.command_ids.as_deref(), &record.command_id)
        {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        let command_ids = command_id_insert(
            self.command_ids.as_ref(),
            Arc::<str>::from(record.command_id.as_str()),
        )?;
        let identity = ledger_prefix_identity(
            b"command",
            self.command_tail.as_ref().map(|tail| tail.identity),
            &encode_command_record_body(&record)?,
        )?;
        Ok(Self {
            command_tail: Some(Arc::new(LedgerNode {
                record,
                previous: self.command_tail.clone(),
                identity,
            })),
            command_ids: Some(command_ids),
            operation_root: self.operation_root.clone(),
            operation_version: self.operation_version.clone(),
            command_count: self.command_count.saturating_add(1),
            operation_count: self.operation_count,
        })
    }

    /// Appends one ordered operation record while sharing the complete prefix.
    pub fn append_operation(
        &self,
        record: DurableOperationRecord,
    ) -> Result<Self, RuntimeManifestError> {
        if self.operation_count >= MAX_RUNTIME_RECORDS
            || operation_map_last(self.operation_root.as_deref())
                .is_some_and(|last| last.operation_id >= record.operation_id)
            || matches!(
                &record.kind,
                DurableOperationKind::ProcessSession { session_id, .. }
                    if operation_map_contains_process_session(
                        self.operation_root.as_deref(),
                        *session_id,
                    )
            )
        {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        let entry = AuthenticatedOperationRecord::new(record)?;
        let operation_root = operation_map_insert(self.operation_root.as_ref(), entry.clone())?;
        let operation_count = self
            .operation_count
            .checked_add(1)
            .ok_or(RuntimeManifestError::RecordLimit)?;
        Ok(Self {
            command_tail: self.command_tail.clone(),
            command_ids: self.command_ids.clone(),
            operation_version: Some(Arc::new(OperationLedgerMutation {
                previous: self.operation_version.clone(),
                inserted: true,
                entry,
                operation_identity: operation_root.identity,
            })),
            operation_root: Some(operation_root),
            command_count: self.command_count,
            operation_count,
        })
    }

    /// Replaces one operation while sharing all unaffected AVL subtrees.
    ///
    /// The replacement must preserve the exact kind, request, and complete
    /// prior transition prefix while appending at least one transition.
    pub fn replace_operation(
        &self,
        record: DurableOperationRecord,
    ) -> Result<Self, RuntimeManifestError> {
        let existing = operation_map_get(self.operation_root.as_deref(), record.operation_id)
            .ok_or(RuntimeManifestError::InvalidLedger)?;
        if existing.kind != record.kind
            || existing.request != record.request
            || record.transitions.len() <= existing.transitions.len()
            || !record.transitions.starts_with(&existing.transitions)
        {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        let entry = AuthenticatedOperationRecord::new(record)?;
        let operation_root = operation_map_replace(
            self.operation_root
                .as_ref()
                .ok_or(RuntimeManifestError::InvalidLedger)?,
            entry.clone(),
        )?;
        Ok(Self {
            command_tail: self.command_tail.clone(),
            command_ids: self.command_ids.clone(),
            operation_version: Some(Arc::new(OperationLedgerMutation {
                previous: self.operation_version.clone(),
                inserted: false,
                entry,
                operation_identity: operation_root.identity,
            })),
            operation_root: Some(operation_root),
            command_count: self.command_count,
            operation_count: self.operation_count,
        })
    }

    /// Appends one transition to the named operation.
    pub fn transition_operation(
        &self,
        operation_id: u64,
        transition: DurableOperationTransition,
    ) -> Result<Self, RuntimeManifestError> {
        let record = operation_map_get(self.operation_root.as_deref(), operation_id)
            .ok_or(RuntimeManifestError::InvalidLedger)?
            .append_transition(transition)?;
        self.replace_operation(record)
    }

    /// Appends several transitions to one operation and validates only the
    /// result, for a closure whose intermediate state is never committed.
    pub fn transition_operation_many(
        &self,
        operation_id: u64,
        transitions: impl IntoIterator<Item = DurableOperationTransition>,
    ) -> Result<Self, RuntimeManifestError> {
        let record = operation_map_get(self.operation_root.as_deref(), operation_id)
            .ok_or(RuntimeManifestError::InvalidLedger)?
            .append_transitions(transitions)?;
        self.replace_operation(record)
    }

    /// Drops finalized evidence the caller no longer needs.
    ///
    /// Commands declined by `keep_command` are dropped, and so is every
    /// operation `keep` declines, which must then be `Finalized`. The newest
    /// command and operation always stay, so the sequences a reopen derives
    /// from them never go back. The result is rebuilt from the retained
    /// records rather than appended to `self`.
    pub(crate) fn retained(
        &self,
        mut keep_command: impl FnMut(&DurableCommandRecord) -> bool,
        mut keep: impl FnMut(&DurableOperationRecord) -> bool,
    ) -> Result<Self, RuntimeManifestError> {
        let last_sequence = self.command_tail.as_ref().map(|tail| tail.record.sequence);
        let commands = self
            .commands()
            .into_iter()
            .filter(|record| keep_command(record) || Some(record.sequence) == last_sequence)
            .cloned()
            .collect();
        let last_operation =
            operation_map_last(self.operation_root.as_deref()).map(|record| record.operation_id);
        let mut operations = Vec::new();
        for record in self.operations() {
            if Some(record.operation_id) == last_operation || keep(record) {
                operations.push(record.clone());
            } else if !matches!(record.state(), DurableOperationState::Finalized(_)) {
                return Err(RuntimeManifestError::InvalidLedger);
            }
        }
        Self::new(commands, operations)
    }

    /// Whether every record here is either one of `prior`'s, unchanged or
    /// extended by transitions, or newer than all of `prior`'s: the shape
    /// [`Self::retained`] leaves, which a checkpoint bundle rebuilds instead of
    /// diffing.
    pub(crate) fn is_retained_successor_of(&self, prior: &Self) -> bool {
        let prior_commands = prior
            .commands()
            .into_iter()
            .map(|record| (record.sequence, record))
            .collect::<BTreeMap<_, _>>();
        let prior_last_sequence = prior_commands.keys().next_back().copied();
        let commands_fit = self.commands().into_iter().all(|record| {
            prior_commands.get(&record.sequence).map_or_else(
                || prior_last_sequence.is_none_or(|last| record.sequence > last),
                |prior| *prior == record,
            )
        });
        let prior_last_operation =
            operation_map_last(prior.operation_root.as_deref()).map(|record| record.operation_id);
        commands_fit
            && self.operations().into_iter().all(|record| {
                operation_map_get(prior.operation_root.as_deref(), record.operation_id).map_or_else(
                    || prior_last_operation.is_none_or(|last| record.operation_id > last),
                    |prior| {
                        prior.kind == record.kind
                            && prior.request == record.request
                            && record.transitions.starts_with(&prior.transitions)
                    },
                )
            })
    }

    /// Replaces the greatest operation identity as a compatibility convenience.
    pub fn replace_last_operation(
        &self,
        record: DurableOperationRecord,
    ) -> Result<Self, RuntimeManifestError> {
        if operation_map_last(self.operation_root.as_deref())
            .is_none_or(|last| last.operation_id != record.operation_id)
        {
            return Err(RuntimeManifestError::InvalidLedger);
        }
        self.replace_operation(record)
    }
}

impl PartialEq for RuntimeOperationLedger {
    fn eq(&self, other: &Self) -> bool {
        self.command_count == other.command_count
            && self.operation_count == other.operation_count
            && collect_ledger_records(self.command_tail.as_deref(), self.command_count)
                == collect_ledger_records(other.command_tail.as_deref(), other.command_count)
            && self.operations() == other.operations()
    }
}

pub(super) fn collect_ledger_records<T>(mut tail: Option<&LedgerNode<T>>, count: usize) -> Vec<&T> {
    let mut records = Vec::with_capacity(count);
    while let Some(node) = tail {
        records.push(&node.record);
        tail = node.previous.as_deref();
    }
    records.reverse();
    records
}

pub(super) fn command_id_contains(mut node: Option<&CommandIdNode>, command_id: &str) -> bool {
    while let Some(current) = node {
        match command_id.cmp(&current.command_id) {
            std::cmp::Ordering::Less => node = current.left.as_deref(),
            std::cmp::Ordering::Greater => node = current.right.as_deref(),
            std::cmp::Ordering::Equal => return true,
        }
    }
    false
}

pub(super) fn command_id_height(node: Option<&CommandIdNode>) -> u8 {
    node.map_or(0, |node| node.height)
}

pub(super) fn command_id_node(
    command_id: Arc<str>,
    left: Option<Arc<CommandIdNode>>,
    right: Option<Arc<CommandIdNode>>,
) -> Result<Arc<CommandIdNode>, RuntimeManifestError> {
    let height = command_id_height(left.as_deref())
        .max(command_id_height(right.as_deref()))
        .saturating_add(1);
    let command_id_length = u32::try_from(command_id.len())
        .map_err(|_| RuntimeManifestError::InvalidLedger)?
        .to_be_bytes();
    let left_present = [u8::from(left.is_some())];
    let right_present = [u8::from(right.is_some())];
    let identity = Digest::sha256(
        DigestDomain::Blob,
        1,
        &[
            b"plakous-agent/runtime-command-id-avl-v1",
            &command_id_length,
            command_id.as_bytes(),
            &left_present,
            left.as_ref().map_or(&[][..], |node| node.identity.bytes()),
            &right_present,
            right.as_ref().map_or(&[][..], |node| node.identity.bytes()),
            &[height],
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)?;
    Ok(Arc::new(CommandIdNode {
        command_id,
        left,
        right,
        height,
        identity,
    }))
}

pub(super) fn command_id_insert(
    node: Option<&Arc<CommandIdNode>>,
    command_id: Arc<str>,
) -> Result<Arc<CommandIdNode>, RuntimeManifestError> {
    let Some(node) = node else {
        return command_id_node(command_id, None, None);
    };
    match command_id.as_ref().cmp(&node.command_id) {
        std::cmp::Ordering::Less => command_id_balance(
            node.command_id.clone(),
            Some(command_id_insert(node.left.as_ref(), command_id)?),
            node.right.clone(),
        ),
        std::cmp::Ordering::Greater => command_id_balance(
            node.command_id.clone(),
            node.left.clone(),
            Some(command_id_insert(node.right.as_ref(), command_id)?),
        ),
        std::cmp::Ordering::Equal => Err(RuntimeManifestError::InvalidLedger),
    }
}

pub(super) fn command_id_balance(
    command_id: Arc<str>,
    left: Option<Arc<CommandIdNode>>,
    right: Option<Arc<CommandIdNode>>,
) -> Result<Arc<CommandIdNode>, RuntimeManifestError> {
    let balance = i16::from(command_id_height(left.as_deref()))
        - i16::from(command_id_height(right.as_deref()));
    if balance > 1 {
        let left_root = left.as_ref().ok_or(RuntimeManifestError::InvalidLedger)?;
        if command_id_height(left_root.left.as_deref())
            >= command_id_height(left_root.right.as_deref())
        {
            let new_right = command_id_node(command_id, left_root.right.clone(), right)?;
            return command_id_node(
                left_root.command_id.clone(),
                left_root.left.clone(),
                Some(new_right),
            );
        }
        let pivot = left_root
            .right
            .as_ref()
            .ok_or(RuntimeManifestError::InvalidLedger)?;
        let new_left = command_id_node(
            left_root.command_id.clone(),
            left_root.left.clone(),
            pivot.left.clone(),
        )?;
        let new_right = command_id_node(command_id, pivot.right.clone(), right)?;
        return command_id_node(pivot.command_id.clone(), Some(new_left), Some(new_right));
    }
    if balance < -1 {
        let right_root = right.as_ref().ok_or(RuntimeManifestError::InvalidLedger)?;
        if command_id_height(right_root.right.as_deref())
            >= command_id_height(right_root.left.as_deref())
        {
            let new_left = command_id_node(command_id, left, right_root.left.clone())?;
            return command_id_node(
                right_root.command_id.clone(),
                Some(new_left),
                right_root.right.clone(),
            );
        }
        let pivot = right_root
            .left
            .as_ref()
            .ok_or(RuntimeManifestError::InvalidLedger)?;
        let new_left = command_id_node(command_id, left, pivot.left.clone())?;
        let new_right = command_id_node(
            right_root.command_id.clone(),
            pivot.right.clone(),
            right_root.right.clone(),
        )?;
        return command_id_node(pivot.command_id.clone(), Some(new_left), Some(new_right));
    }
    command_id_node(command_id, left, right)
}

pub(super) fn collect_operation_records<'a>(
    node: Option<&'a OperationMapNode>,
    records: &mut Vec<&'a DurableOperationRecord>,
) {
    let Some(node) = node else {
        return;
    };
    collect_operation_records(node.left.as_deref(), records);
    records.push(&node.entry.record);
    collect_operation_records(node.right.as_deref(), records);
}

pub(super) fn operation_map_get(
    mut node: Option<&OperationMapNode>,
    operation_id: u64,
) -> Option<&DurableOperationRecord> {
    while let Some(current) = node {
        match operation_id.cmp(&current.entry.record.operation_id) {
            std::cmp::Ordering::Less => node = current.left.as_deref(),
            std::cmp::Ordering::Greater => node = current.right.as_deref(),
            std::cmp::Ordering::Equal => return Some(&current.entry.record),
        }
    }
    None
}

pub(super) fn operation_map_contains_process_session(
    node: Option<&OperationMapNode>,
    session_id: ProcessSessionId,
) -> bool {
    let Some(node) = node else {
        return false;
    };
    matches!(
        node.entry.record.kind(),
        DurableOperationKind::ProcessSession {
            session_id: existing,
            ..
        } if *existing == session_id
    ) || operation_map_contains_process_session(node.left.as_deref(), session_id)
        || operation_map_contains_process_session(node.right.as_deref(), session_id)
}

pub(super) fn operation_map_last(
    mut node: Option<&OperationMapNode>,
) -> Option<&DurableOperationRecord> {
    let mut last = None;
    while let Some(current) = node {
        last = Some(&*current.entry.record);
        node = current.right.as_deref();
    }
    last
}

pub(super) fn operation_map_height(node: Option<&OperationMapNode>) -> u8 {
    node.map_or(0, |node| node.height)
}

pub(super) fn operation_map_node(
    entry: AuthenticatedOperationRecord,
    left: Option<Arc<OperationMapNode>>,
    right: Option<Arc<OperationMapNode>>,
) -> Result<Arc<OperationMapNode>, RuntimeManifestError> {
    let height = operation_map_height(left.as_deref())
        .max(operation_map_height(right.as_deref()))
        .saturating_add(1);
    let count = left
        .as_ref()
        .map_or(0, |node| node.count)
        .checked_add(right.as_ref().map_or(0, |node| node.count))
        .and_then(|count| count.checked_add(1))
        .ok_or(RuntimeManifestError::RecordLimit)?;
    let unresolved_count = left
        .as_ref()
        .map_or(0, |node| node.unresolved_count)
        .checked_add(right.as_ref().map_or(0, |node| node.unresolved_count))
        .and_then(|count| {
            count.checked_add(usize::from(!matches!(
                entry.record.state(),
                DurableOperationState::Finalized(_)
            )))
        })
        .ok_or(RuntimeManifestError::RecordLimit)?;
    let operation_id = entry.record.operation_id.to_be_bytes();
    let height_bytes = [height];
    let count_bytes = u64::try_from(count)
        .map_err(|_| RuntimeManifestError::RecordLimit)?
        .to_be_bytes();
    let unresolved_count_bytes = u64::try_from(unresolved_count)
        .map_err(|_| RuntimeManifestError::RecordLimit)?
        .to_be_bytes();
    let left_present = [u8::from(left.is_some())];
    let right_present = [u8::from(right.is_some())];
    let identity = Digest::sha256(
        DigestDomain::Blob,
        1,
        &[
            b"plakous-agent/runtime-operation-avl-v1",
            &operation_id,
            entry.identity.bytes(),
            &left_present,
            left.as_ref().map_or(&[][..], |node| node.identity.bytes()),
            &right_present,
            right.as_ref().map_or(&[][..], |node| node.identity.bytes()),
            &height_bytes,
            &count_bytes,
            &unresolved_count_bytes,
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)?;
    Ok(Arc::new(OperationMapNode {
        entry,
        left,
        right,
        height,
        count,
        unresolved_count,
        identity,
    }))
}

pub(super) fn operation_map_insert(
    node: Option<&Arc<OperationMapNode>>,
    entry: AuthenticatedOperationRecord,
) -> Result<Arc<OperationMapNode>, RuntimeManifestError> {
    let Some(node) = node else {
        return operation_map_node(entry, None, None);
    };
    let operation_id = entry.record.operation_id;
    match operation_id.cmp(&node.entry.record.operation_id) {
        std::cmp::Ordering::Less => operation_map_balance(
            node.entry.clone(),
            Some(operation_map_insert(node.left.as_ref(), entry)?),
            node.right.clone(),
        ),
        std::cmp::Ordering::Greater => operation_map_balance(
            node.entry.clone(),
            node.left.clone(),
            Some(operation_map_insert(node.right.as_ref(), entry)?),
        ),
        std::cmp::Ordering::Equal => Err(RuntimeManifestError::InvalidLedger),
    }
}

pub(super) fn operation_map_replace(
    node: &Arc<OperationMapNode>,
    entry: AuthenticatedOperationRecord,
) -> Result<Arc<OperationMapNode>, RuntimeManifestError> {
    match entry
        .record
        .operation_id
        .cmp(&node.entry.record.operation_id)
    {
        std::cmp::Ordering::Less => operation_map_node(
            node.entry.clone(),
            Some(operation_map_replace(
                node.left
                    .as_ref()
                    .ok_or(RuntimeManifestError::InvalidLedger)?,
                entry,
            )?),
            node.right.clone(),
        ),
        std::cmp::Ordering::Greater => operation_map_node(
            node.entry.clone(),
            node.left.clone(),
            Some(operation_map_replace(
                node.right
                    .as_ref()
                    .ok_or(RuntimeManifestError::InvalidLedger)?,
                entry,
            )?),
        ),
        std::cmp::Ordering::Equal => {
            operation_map_node(entry, node.left.clone(), node.right.clone())
        }
    }
}

pub(super) fn operation_map_balance(
    entry: AuthenticatedOperationRecord,
    left: Option<Arc<OperationMapNode>>,
    right: Option<Arc<OperationMapNode>>,
) -> Result<Arc<OperationMapNode>, RuntimeManifestError> {
    let balance = i16::from(operation_map_height(left.as_deref()))
        - i16::from(operation_map_height(right.as_deref()));
    if balance > 1 {
        let left_root = left.as_ref().ok_or(RuntimeManifestError::InvalidLedger)?;
        if operation_map_height(left_root.left.as_deref())
            >= operation_map_height(left_root.right.as_deref())
        {
            let new_right = operation_map_node(entry, left_root.right.clone(), right)?;
            return operation_map_node(
                left_root.entry.clone(),
                left_root.left.clone(),
                Some(new_right),
            );
        }
        let pivot = left_root
            .right
            .as_ref()
            .ok_or(RuntimeManifestError::InvalidLedger)?;
        let new_left = operation_map_node(
            left_root.entry.clone(),
            left_root.left.clone(),
            pivot.left.clone(),
        )?;
        let new_right = operation_map_node(entry, pivot.right.clone(), right)?;
        return operation_map_node(pivot.entry.clone(), Some(new_left), Some(new_right));
    }
    if balance < -1 {
        let right_root = right.as_ref().ok_or(RuntimeManifestError::InvalidLedger)?;
        if operation_map_height(right_root.right.as_deref())
            >= operation_map_height(right_root.left.as_deref())
        {
            let new_left = operation_map_node(entry, left, right_root.left.clone())?;
            return operation_map_node(
                right_root.entry.clone(),
                Some(new_left),
                right_root.right.clone(),
            );
        }
        let pivot = right_root
            .left
            .as_ref()
            .ok_or(RuntimeManifestError::InvalidLedger)?;
        let new_left = operation_map_node(entry, left, pivot.left.clone())?;
        let new_right = operation_map_node(
            right_root.entry.clone(),
            pivot.right.clone(),
            right_root.right.clone(),
        )?;
        return operation_map_node(pivot.entry.clone(), Some(new_left), Some(new_right));
    }
    operation_map_node(entry, left, right)
}

pub(super) fn ledger_delta_since<'a, T>(
    mut tail: Option<&'a LedgerNode<T>>,
    total: usize,
    ancestor_tail: Option<&LedgerNode<T>>,
    ancestor_count: usize,
) -> Option<(usize, Vec<&'a T>)> {
    let delta = total.checked_sub(ancestor_count)?;
    let mut reverse = Vec::with_capacity(delta);
    for _ in 0..delta {
        let node = tail?;
        reverse.push(&node.record);
        tail = node.previous.as_deref();
    }
    if !match (tail, ancestor_tail) {
        (None, None) => true,
        (Some(left), Some(right)) => left.identity == right.identity,
        (None, Some(_)) | (Some(_), None) => false,
    } {
        return None;
    }
    reverse.reverse();
    Some((ancestor_count, reverse))
}
