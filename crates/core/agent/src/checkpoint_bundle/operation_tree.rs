//! Balanced operation-record tree inside the ledger object graph.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use super::object::canonical_operation_wire;
use super::wire::{BundleWriter, OperationNodeSpec, parse_operation_node};
use super::{
    BundleBuilder, CheckpointBundleError, CheckpointObjectBundle, ObjectIndex, RuntimeObject,
    RuntimeObjectAddress, RuntimeObjectKind,
};

impl BundleBuilder {
    pub(super) fn operation_tree_upsert(
        &mut self,
        root: Option<RuntimeObjectAddress>,
        key: u64,
        record: RuntimeObjectAddress,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<(RuntimeObjectAddress, bool), CheckpointBundleError> {
        let Some(root) = root else {
            return self
                .operation_tree_node(key, None, record, None, prior)
                .map(|address| (address, true));
        };
        let node = self.operation_tree_spec(root, prior)?;
        match key.cmp(&node.key) {
            Ordering::Less => {
                let (left, inserted) = self.operation_tree_upsert(node.left, key, record, prior)?;
                let address = self.operation_tree_balance(
                    node.key,
                    Some(left),
                    node.record,
                    node.right,
                    prior,
                )?;
                self.objects.remove(&root);
                Ok((address, inserted))
            }
            Ordering::Greater => {
                let (right, inserted) =
                    self.operation_tree_upsert(node.right, key, record, prior)?;
                let address = self.operation_tree_balance(
                    node.key,
                    node.left,
                    node.record,
                    Some(right),
                    prior,
                )?;
                self.objects.remove(&root);
                Ok((address, inserted))
            }
            Ordering::Equal => self
                .operation_tree_node(key, node.left, record, node.right, prior)
                .map(|address| (address, false)),
        }
    }

    fn operation_tree_balance(
        &mut self,
        key: u64,
        left: Option<RuntimeObjectAddress>,
        record: RuntimeObjectAddress,
        right: Option<RuntimeObjectAddress>,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let left_height = self.operation_tree_height(left, prior)?;
        let right_height = self.operation_tree_height(right, prior)?;
        let skew = i16::from(left_height) - i16::from(right_height);
        if skew > 1 {
            let pivot_address = left.ok_or(CheckpointBundleError::InvalidTopology)?;
            let pivot = self.operation_tree_spec(pivot_address, prior)?;
            if self.operation_tree_height(pivot.left, prior)?
                >= self.operation_tree_height(pivot.right, prior)?
            {
                let new_right = self.operation_tree_node(key, pivot.right, record, right, prior)?;
                self.objects.remove(&pivot_address);
                return self.operation_tree_node(
                    pivot.key,
                    pivot.left,
                    pivot.record,
                    Some(new_right),
                    prior,
                );
            }
            let middle_address = pivot.right.ok_or(CheckpointBundleError::InvalidTopology)?;
            let middle = self.operation_tree_spec(middle_address, prior)?;
            let new_left =
                self.operation_tree_node(pivot.key, pivot.left, pivot.record, middle.left, prior)?;
            let new_right = self.operation_tree_node(key, middle.right, record, right, prior)?;
            self.objects.remove(&pivot_address);
            self.objects.remove(&middle_address);
            return self.operation_tree_node(
                middle.key,
                Some(new_left),
                middle.record,
                Some(new_right),
                prior,
            );
        }
        if skew < -1 {
            let pivot_address = right.ok_or(CheckpointBundleError::InvalidTopology)?;
            let pivot = self.operation_tree_spec(pivot_address, prior)?;
            if self.operation_tree_height(pivot.right, prior)?
                >= self.operation_tree_height(pivot.left, prior)?
            {
                let new_left = self.operation_tree_node(key, left, record, pivot.left, prior)?;
                self.objects.remove(&pivot_address);
                return self.operation_tree_node(
                    pivot.key,
                    Some(new_left),
                    pivot.record,
                    pivot.right,
                    prior,
                );
            }
            let middle_address = pivot.left.ok_or(CheckpointBundleError::InvalidTopology)?;
            let middle = self.operation_tree_spec(middle_address, prior)?;
            let new_left = self.operation_tree_node(key, left, record, middle.left, prior)?;
            let new_right = self.operation_tree_node(
                pivot.key,
                middle.right,
                pivot.record,
                pivot.right,
                prior,
            )?;
            self.objects.remove(&pivot_address);
            self.objects.remove(&middle_address);
            return self.operation_tree_node(
                middle.key,
                Some(new_left),
                middle.record,
                Some(new_right),
                prior,
            );
        }
        self.operation_tree_node(key, left, record, right, prior)
    }

    fn operation_tree_node(
        &mut self,
        key: u64,
        left: Option<RuntimeObjectAddress>,
        record: RuntimeObjectAddress,
        right: Option<RuntimeObjectAddress>,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let left_spec = left
            .map(|address| self.operation_tree_spec(address, prior))
            .transpose()?;
        let right_spec = right
            .map(|address| self.operation_tree_spec(address, prior))
            .transpose()?;
        let count = left_spec
            .map_or(0, |node| node.count)
            .saturating_add(right_spec.map_or(0, |node| node.count))
            .saturating_add(1);
        let height = left_spec
            .map_or(0, |node| node.height)
            .max(right_spec.map_or(0, |node| node.height))
            .saturating_add(1);
        let mut writer = BundleWriter::default();
        writer.u64(key);
        writer.count(count)?;
        writer.u8(height);
        writer.optional_address(left);
        writer.address(record);
        writer.optional_address(right);
        let mut children = Vec::with_capacity(3);
        children.extend(left);
        children.push(record);
        children.extend(right);
        self.insert(RuntimeObjectKind::OperationNode, writer.finish(), children)
    }

    fn operation_tree_height(
        &self,
        address: Option<RuntimeObjectAddress>,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<u8, CheckpointBundleError> {
        address
            .map(|address| {
                self.operation_tree_spec(address, prior)
                    .map(|node| node.height)
            })
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn operation_tree_spec(
        &self,
        address: RuntimeObjectAddress,
        prior: Option<&CheckpointObjectBundle>,
    ) -> Result<OperationNodeSpec, CheckpointBundleError> {
        let object = self
            .objects
            .get(&address)
            .or_else(|| prior.and_then(|bundle| bundle.lookup(address)))
            .ok_or(CheckpointBundleError::MissingObject)?;
        if object.address.kind != RuntimeObjectKind::OperationNode {
            return Err(CheckpointBundleError::WrongKind);
        }
        parse_operation_node(&object.canonical_bytes)
    }
}

pub(super) fn materialize_operation_tree<'a>(
    index: &'a ObjectIndex<'a>,
    count: usize,
    root: Option<RuntimeObjectAddress>,
) -> Result<Vec<&'a RuntimeObject>, CheckpointBundleError> {
    if (count == 0) != root.is_none() {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    let Some(root) = root else {
        return Ok(Vec::new());
    };
    let mut summaries = BTreeMap::<RuntimeObjectAddress, (usize, u8, u64, u64)>::new();
    let mut stack = vec![(root, false)];
    while let Some((address, expanded)) = stack.pop() {
        let object = index.object(address, RuntimeObjectKind::OperationNode)?;
        let node = parse_operation_node(&object.canonical_bytes)?;
        if !expanded {
            stack.push((address, true));
            if let Some(right) = node.right {
                stack.push((right, false));
            }
            if let Some(left) = node.left {
                stack.push((left, false));
            }
            continue;
        }
        let left = node
            .left
            .map(|child| {
                summaries
                    .get(&child)
                    .copied()
                    .ok_or(CheckpointBundleError::InvalidTopology)
            })
            .transpose()?;
        let right = node
            .right
            .map(|child| {
                summaries
                    .get(&child)
                    .copied()
                    .ok_or(CheckpointBundleError::InvalidTopology)
            })
            .transpose()?;
        if left.is_some_and(|(_, _, _, maximum)| maximum >= node.key)
            || right.is_some_and(|(_, _, minimum, _)| minimum <= node.key)
        {
            return Err(CheckpointBundleError::InvalidTopology);
        }
        let left_count = left.map_or(0, |summary| summary.0);
        let right_count = right.map_or(0, |summary| summary.0);
        let left_height = left.map_or(0, |summary| summary.1);
        let right_height = right.map_or(0, |summary| summary.1);
        let expected_count = left_count.saturating_add(right_count).saturating_add(1);
        let expected_height = left_height.max(right_height).saturating_add(1);
        if node.count != expected_count
            || node.height != expected_height
            || left_height.abs_diff(right_height) > 1
        {
            return Err(CheckpointBundleError::InvalidTopology);
        }
        let record = index.object(node.record, RuntimeObjectKind::OperationRecord)?;
        if canonical_operation_wire(&record.canonical_bytes)?.operation_id() != node.key {
            return Err(CheckpointBundleError::InvalidTopology);
        }
        summaries.insert(
            address,
            (
                expected_count,
                expected_height,
                left.map_or(node.key, |summary| summary.2),
                right.map_or(node.key, |summary| summary.3),
            ),
        );
    }
    if summaries.get(&root).map(|summary| summary.0) != Some(count) {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    let mut records = Vec::with_capacity(count);
    let mut nodes = Vec::new();
    let mut current = Some(root);
    while current.is_some() || !nodes.is_empty() {
        while let Some(address) = current {
            nodes.push(address);
            current = parse_operation_node(
                &index
                    .object(address, RuntimeObjectKind::OperationNode)?
                    .canonical_bytes,
            )?
            .left;
        }
        let address = nodes.pop().ok_or(CheckpointBundleError::InvalidTopology)?;
        let node = parse_operation_node(
            &index
                .object(address, RuntimeObjectKind::OperationNode)?
                .canonical_bytes,
        )?;
        records.push(index.object(node.record, RuntimeObjectKind::OperationRecord)?);
        current = node.right;
    }
    Ok(records)
}
