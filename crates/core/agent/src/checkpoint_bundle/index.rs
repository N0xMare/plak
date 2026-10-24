//! The bundle API, object layering, CAS membership and closed-graph validation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use plakous_model::{CheckpointId, Digest, DigestAlgorithm, DigestDomain};

use crate::RuntimeCheckpoint;

use super::builder::reuse_from_bundle;
use super::object::{canonical_child_specs, canonical_history_segment};
use super::{
    CasNode, CheckpointBundleError, CheckpointBundleStats, CheckpointObjectBundle,
    MAX_CHECKPOINT_OBJECT_BYTES, MAX_CHECKPOINT_OBJECTS, MAX_LAYER_DEPTH, ObjectIndex, ObjectLayer,
    RUNTIME_FORMAT_EPOCH, RuntimeCheckpointCodec, RuntimeObject, RuntimeObjectAddress,
    RuntimeObjectKind,
};

impl CheckpointBundleStats {
    /// Returns the total unique CAS object count retained through this publication.
    #[must_use]
    pub const fn object_count(self) -> usize {
        self.object_count
    }

    /// Returns total unique CAS object and edge bytes retained through this publication.
    #[must_use]
    pub const fn retained_bytes(self) -> usize {
        self.retained_bytes
    }

    /// Returns objects whose addresses existed in the supplied prior bundle.
    #[must_use]
    pub const fn reused_object_count(self) -> usize {
        self.reused_object_count
    }

    /// Returns conservatively retained bytes reused from the prior bundle.
    #[must_use]
    pub const fn reused_retained_bytes(self) -> usize {
        self.reused_retained_bytes
    }

    /// Returns objects newly retained relative to the prior bundle.
    #[must_use]
    pub const fn new_object_count(self) -> usize {
        self.new_object_count
    }

    /// Returns conservatively retained bytes newly added relative to the prior bundle.
    #[must_use]
    pub const fn new_retained_bytes(self) -> usize {
        self.new_retained_bytes
    }

    /// Returns semantic objects visited by the split path.
    ///
    /// Ordinary incremental publication keeps this proportional to the delta;
    /// cold full splitting intentionally visits the full reachable closure.
    #[must_use]
    pub const fn visited_object_count(self) -> usize {
        self.visited_object_count
    }

    /// Returns newly allocated persistent CAS-index nodes for this publication.
    ///
    /// Patricia depth is fixed by the address width, so this is a direct bound
    /// on otherwise-hidden publication address metadata.
    #[must_use]
    pub const fn new_index_node_count(self) -> usize {
        self.new_index_node_count
    }
}

impl CasNode {
    fn get(mut node: Option<&Self>, address: RuntimeObjectAddress) -> Option<&RuntimeObject> {
        while let Some(current) = node {
            match current {
                Self::Leaf(object) => return (object.address == address).then_some(object),
                Self::Branch {
                    differing_bit,
                    zero,
                    one,
                } => {
                    node = Some(if address_bit(address, *differing_bit) {
                        one
                    } else {
                        zero
                    });
                }
            }
        }
        None
    }

    fn insert(
        node: Option<&Arc<Self>>,
        object: RuntimeObject,
    ) -> Result<(Arc<Self>, bool, usize), CheckpointBundleError> {
        let Some(node) = node else {
            return Ok((Arc::new(Self::Leaf(object)), true, 1));
        };
        let existing = Self::leaf_for(node, object.address);
        if existing.address == object.address {
            if existing != &object {
                return Err(CheckpointBundleError::AddressConflict);
            }
            return Ok((node.clone(), false, 0));
        }
        let differing_bit = first_differing_address_bit(existing.address, object.address)
            .ok_or(CheckpointBundleError::AddressConflict)?;
        let (root, nodes_created) = Self::insert_at(node, differing_bit, object);
        Ok((root, true, nodes_created))
    }

    fn leaf_for(mut node: &Self, address: RuntimeObjectAddress) -> &RuntimeObject {
        loop {
            match node {
                Self::Leaf(object) => return object,
                Self::Branch {
                    differing_bit,
                    zero,
                    one,
                } => {
                    node = if address_bit(address, *differing_bit) {
                        one
                    } else {
                        zero
                    };
                }
            }
        }
    }

    fn insert_at(
        node: &Arc<Self>,
        differing_bit: u16,
        object: RuntimeObject,
    ) -> (Arc<Self>, usize) {
        if let Self::Branch {
            differing_bit: current_bit,
            zero,
            one,
        } = node.as_ref()
            && *current_bit < differing_bit
        {
            return if address_bit(object.address, *current_bit) {
                let (new_one, nodes_created) = Self::insert_at(one, differing_bit, object);
                (
                    Arc::new(Self::Branch {
                        differing_bit: *current_bit,
                        zero: zero.clone(),
                        one: new_one,
                    }),
                    nodes_created.saturating_add(1),
                )
            } else {
                let (new_zero, nodes_created) = Self::insert_at(zero, differing_bit, object);
                (
                    Arc::new(Self::Branch {
                        differing_bit: *current_bit,
                        zero: new_zero,
                        one: one.clone(),
                    }),
                    nodes_created.saturating_add(1),
                )
            };
        }
        let existing = node.clone();
        let object_address = object.address;
        let added = Arc::new(Self::Leaf(object));
        let (zero, one) = if address_bit(object_address, differing_bit) {
            (existing, added)
        } else {
            (added, existing)
        };
        (
            Arc::new(Self::Branch {
                differing_bit,
                zero,
                one,
            }),
            2,
        )
    }
}

fn first_differing_address_bit(
    left: RuntimeObjectAddress,
    right: RuntimeObjectAddress,
) -> Option<u16> {
    (0..296_u16).find(|bit| address_bit(left, *bit) != address_bit(right, *bit))
}

fn address_bit(address: RuntimeObjectAddress, bit: u16) -> bool {
    let byte_index = usize::from(bit / 8);
    let byte = match byte_index {
        0 => address.kind as u8,
        1..=4 => address.codec_revision.to_be_bytes()[byte_index.saturating_sub(1)],
        5..=36 => address.digest.bytes()[byte_index.saturating_sub(5)],
        _ => 0,
    };
    byte & (0x80_u8 >> u32::from(bit % 8)) != 0
}

impl fmt::Debug for CheckpointObjectBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CheckpointObjectBundle")
            .field("checkpoint_id", &self.checkpoint_id)
            .field("checkpoint_integrity", &self.checkpoint_integrity)
            .field("base_root", &self.base_root)
            .field("root", &self.root)
            .field("supplied_object_count", &self.objects.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl PartialEq for CheckpointObjectBundle {
    fn eq(&self, other: &Self) -> bool {
        self.checkpoint_id == other.checkpoint_id
            && self.checkpoint_integrity == other.checkpoint_integrity
            && self.base_root == other.base_root
            && self.root == other.root
            && self.objects == other.objects
            && self.stats == other.stats
    }
}

impl CheckpointObjectBundle {
    /// Reassembles a complete reachable object set loaded by a storage backend.
    ///
    /// The supplied objects must already be the exact canonical sorted closure;
    /// missing, extra, corrupt, or reordered objects are rejected immediately.
    ///
    /// # Errors
    ///
    /// Returns a typed error when any address, object, edge, semantic value, or
    /// semantic checkpoint identity fails exact validation.
    pub fn from_store(
        checkpoint_id: CheckpointId,
        checkpoint_integrity: Digest,
        root: RuntimeObjectAddress,
        objects: Vec<RuntimeObject>,
    ) -> Result<Self, CheckpointBundleError> {
        let retained_bytes = objects
            .iter()
            .map(RuntimeObject::retained_bytes)
            .fold(0_usize, usize::saturating_add);
        let objects: Arc<[RuntimeObject]> = objects.into();
        let (cas, _, index_nodes) = cas_insert_all(None, &objects)?;
        let bundle = Self {
            checkpoint_id,
            checkpoint_integrity,
            base_root: None,
            root,
            stats: CheckpointBundleStats {
                object_count: objects.len(),
                retained_bytes,
                new_object_count: objects.len(),
                new_retained_bytes: retained_bytes,
                new_index_node_count: index_nodes,
                ..CheckpointBundleStats::default()
            },
            depth: 1,
            trim_floor: objects.len(),
            objects: objects.clone(),
            layers: Arc::new(ObjectLayer {
                prior: None,
                objects,
            }),
            cas,
            checkpoint: None,
            reuse: None,
        };
        let mut bundle = bundle;
        let checkpoint = RuntimeCheckpointCodec::materialize(&bundle)?;
        bundle.checkpoint = Some(Arc::new(checkpoint));
        bundle.reuse = Some(Arc::new(reuse_from_bundle(&bundle)?));
        Ok(bundle)
    }

    /// Returns the checkpoint identity bound by the root.
    #[must_use]
    pub const fn checkpoint_id(&self) -> CheckpointId {
        self.checkpoint_id
    }

    /// Returns the compositional semantic checkpoint integrity bound by the root.
    #[must_use]
    pub const fn checkpoint_integrity(&self) -> Digest {
        self.checkpoint_integrity
    }

    /// Returns the exact prior checkpoint root this publication extends.
    ///
    /// Genesis and cold full-store reconstruction have no base root.
    #[must_use]
    pub const fn base_root(&self) -> Option<RuntimeObjectAddress> {
        self.base_root
    }

    /// Returns the small canonical checkpoint-root address.
    #[must_use]
    pub const fn root(&self) -> RuntimeObjectAddress {
        self.root
    }

    /// Returns the exact canonical sorted new CAS objects for this publication.
    ///
    /// Genesis returns the full reachable closure. Successor publications return
    /// only objects absent from the validated base publication ancestry.
    #[must_use]
    pub fn objects(&self) -> &[RuntimeObject] {
        &self.objects
    }

    /// Explicit alias for [`Self::objects`] used by storage backends.
    #[must_use]
    pub fn supplied_objects(&self) -> &[RuntimeObject] {
        &self.objects
    }

    /// Returns sharing and retained-growth counters relative to the supplied prior bundle.
    #[must_use]
    pub const fn stats(&self) -> CheckpointBundleStats {
        self.stats
    }

    /// Returns the sealed checkpoint validated when this publication was built or loaded.
    ///
    /// Storage commit paths use this value while validating only the supplied
    /// object delta against [`Self::base_root`]. Cold load still calls the full
    /// graph codec before this seal is installed.
    ///
    /// # Errors
    ///
    /// Returns an error only for an internally unsealed bundle under construction.
    pub fn validated_checkpoint(&self) -> Result<Arc<RuntimeCheckpoint>, CheckpointBundleError> {
        self.checkpoint
            .clone()
            .ok_or(CheckpointBundleError::SemanticDisagreement)
    }

    pub(super) fn lookup(&self, address: RuntimeObjectAddress) -> Option<&RuntimeObject> {
        CasNode::get(self.cas.as_deref(), address)
    }

    /// Forgets the objects this root no longer reaches, once they are due.
    ///
    /// [`RuntimeCheckpointCodec::advance`] dedupes against every object the
    /// bundle has seen, so without this an owner keeps every superseded
    /// object for its whole life. A trim is due when the chain is
    /// `MAX_LAYER_DEPTH` layers deep or holds twice what the root reached
    /// at the last trim, which keeps its cost proportional to what was
    /// published since. Call it only on a bundle whose commit was
    /// acknowledged: before that, its supplied objects are the commit's delta.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the root reaches an object the bundle lacks.
    pub fn trimmed(&self) -> Result<Self, CheckpointBundleError> {
        if self.depth < MAX_LAYER_DEPTH
            && self.stats.object_count < self.trim_floor.saturating_mul(2)
        {
            return Ok(self.clone());
        }
        self.current_root_closure()
    }

    /// Unconditionally forgets ancestral CAS membership before a store sweep.
    /// Unlike amortized trimming, this must run even below the size/depth
    /// thresholds: an old address may be reconstructed after pruning removed it.
    /// The returned closure is loaded/published state, never a pending
    /// publication delta. Retain the original delta until acknowledgement.
    ///
    /// # Errors
    /// Returns a typed error if the root closure is missing objects or exceeds bounds.
    #[doc(hidden)]
    pub fn current_root_closure(&self) -> Result<Self, CheckpointBundleError> {
        let mut closure = BTreeMap::new();
        let mut pending = vec![self.root];
        while let Some(address) = pending.pop() {
            if closure.contains_key(&address) {
                continue;
            }
            let object = self
                .lookup(address)
                .ok_or(CheckpointBundleError::MissingObject)?;
            pending.extend_from_slice(object.children());
            closure.insert(address, object.clone());
        }
        let objects: Arc<[RuntimeObject]> = closure.into_values().collect();
        let retained_bytes = objects
            .iter()
            .map(RuntimeObject::retained_bytes)
            .fold(0_usize, usize::saturating_add);
        if objects.len() > MAX_CHECKPOINT_OBJECTS || retained_bytes > MAX_CHECKPOINT_OBJECT_BYTES {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        let (cas, _, index_nodes) = cas_insert_all(None, &objects)?;
        Ok(Self {
            checkpoint_id: self.checkpoint_id,
            checkpoint_integrity: self.checkpoint_integrity,
            base_root: None,
            root: self.root,
            stats: CheckpointBundleStats {
                object_count: objects.len(),
                retained_bytes,
                new_object_count: objects.len(),
                new_retained_bytes: retained_bytes,
                new_index_node_count: index_nodes,
                ..CheckpointBundleStats::default()
            },
            depth: 1,
            trim_floor: objects.len(),
            layers: Arc::new(ObjectLayer {
                prior: None,
                objects: objects.clone(),
            }),
            objects,
            cas,
            checkpoint: self.checkpoint.clone(),
            reuse: self.reuse.clone(),
        })
    }
}

pub(super) fn cas_insert_all(
    mut root: Option<Arc<CasNode>>,
    objects: &[RuntimeObject],
) -> Result<(Option<Arc<CasNode>>, usize, usize), CheckpointBundleError> {
    let mut inserted = 0_usize;
    let mut nodes_created = 0_usize;
    for object in objects {
        let (next, was_inserted, object_nodes_created) =
            CasNode::insert(root.as_ref(), object.clone())?;
        root = Some(next);
        inserted = inserted.saturating_add(usize::from(was_inserted));
        nodes_created = nodes_created.saturating_add(object_nodes_created);
    }
    Ok((root, inserted, nodes_created))
}

impl<'a> ObjectIndex<'a> {
    pub(super) fn validate(
        bundle: &'a CheckpointObjectBundle,
    ) -> Result<Self, CheckpointBundleError> {
        if bundle.objects.is_empty() || bundle.objects.len() > MAX_CHECKPOINT_OBJECTS {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        let mut total = 0_usize;
        let mut objects = BTreeMap::new();
        let mut layer = Some(bundle.layers.as_ref());
        while let Some(current) = layer {
            if current
                .objects
                .windows(2)
                .any(|pair| pair[0].address >= pair[1].address)
            {
                return Err(CheckpointBundleError::NonCanonical);
            }
            for object in current.objects.iter() {
                total = total.saturating_add(object.retained_bytes());
                if total > MAX_CHECKPOINT_OBJECT_BYTES {
                    return Err(CheckpointBundleError::BoundExceeded);
                }
                if object.address.codec_revision != RUNTIME_FORMAT_EPOCH
                    || object.address.digest.algorithm() != DigestAlgorithm::Sha256
                    || object.address.digest.domain() != DigestDomain::Blob
                    || object.address.digest.version() != RUNTIME_FORMAT_EPOCH
                {
                    return Err(CheckpointBundleError::UnsupportedRevision);
                }
                // Both fields are construction-only: constructors either hash
                // these bytes or receive the builder's freshly computed address.
                // Pointer equality detects test-only byte replacement, not a memo.
                if !Arc::ptr_eq(&object.addressed_bytes, &object.canonical_bytes) {
                    return Err(CheckpointBundleError::InvalidAddress);
                }
                if objects.insert(object.address, object).is_some() {
                    return Err(CheckpointBundleError::NonCanonical);
                }
            }
            layer = current.prior.as_deref();
        }
        if objects.len() > MAX_CHECKPOINT_OBJECTS {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        let decoded = objects
            .iter()
            .filter(|(address, _)| address.kind == RuntimeObjectKind::HistorySegment)
            .map(|(address, object)| {
                let segment = match &object.decoded_segment {
                    Some(segment) => segment.clone(),
                    None => canonical_history_segment(&object.canonical_bytes)?,
                };
                Ok((*address, segment))
            })
            .collect::<Result<_, CheckpointBundleError>>()?;
        let index = Self { objects, decoded };
        index.object(bundle.root, RuntimeObjectKind::CheckpointRoot)?;
        for object in bundle.objects.iter() {
            let expected = index.expected_children(object)?;
            if expected.as_slice() != object.children() {
                return Err(CheckpointBundleError::NonCanonical);
            }
            for child in &expected {
                index
                    .objects
                    .get(child)
                    .ok_or(CheckpointBundleError::MissingObject)?;
            }
        }
        let reachable = index.validate_closed_acyclic(bundle.root)?;
        if bundle
            .objects
            .iter()
            .any(|object| !reachable.contains(&object.address))
        {
            return Err(CheckpointBundleError::ExtraObject);
        }
        Ok(index)
    }

    pub(super) fn object(
        &self,
        address: RuntimeObjectAddress,
        kind: RuntimeObjectKind,
    ) -> Result<&'a RuntimeObject, CheckpointBundleError> {
        let object = self
            .objects
            .get(&address)
            .copied()
            .ok_or(CheckpointBundleError::MissingObject)?;
        if object.address.kind != kind {
            return Err(CheckpointBundleError::WrongKind);
        }
        Ok(object)
    }

    fn expected_children(
        &self,
        object: &RuntimeObject,
    ) -> Result<Vec<RuntimeObjectAddress>, CheckpointBundleError> {
        if self.decoded.contains_key(&object.address) {
            return Ok(Vec::new());
        }
        let child_specs = canonical_child_specs(object.address.kind, &object.canonical_bytes)?;
        for (address, kind) in &child_specs {
            self.object(*address, *kind)?;
        }
        Ok(child_specs
            .into_iter()
            .map(|(address, _)| address)
            .collect())
    }

    fn validate_closed_acyclic(
        &self,
        root: RuntimeObjectAddress,
    ) -> Result<BTreeSet<RuntimeObjectAddress>, CheckpointBundleError> {
        let mut visiting = BTreeSet::new();
        let mut done = BTreeSet::new();
        let mut stack = vec![(root, false)];
        while let Some((address, expanded)) = stack.pop() {
            if expanded {
                visiting.remove(&address);
                done.insert(address);
                continue;
            }
            if done.contains(&address) {
                continue;
            }
            if !visiting.insert(address) {
                return Err(CheckpointBundleError::Cycle);
            }
            if stack.len() > MAX_CHECKPOINT_OBJECTS {
                return Err(CheckpointBundleError::BoundExceeded);
            }
            stack.push((address, true));
            let object = self
                .objects
                .get(&address)
                .ok_or(CheckpointBundleError::MissingObject)?;
            for child in object.children.iter().rev() {
                stack.push((*child, false));
            }
        }
        Ok(done)
    }
}
