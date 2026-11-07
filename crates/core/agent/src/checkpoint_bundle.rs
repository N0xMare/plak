//! Backend-neutral content-addressed representation of runtime checkpoints.

use std::collections::BTreeMap;
use std::sync::Arc;

use plakous_model::{CheckpointId, Digest, DigestDomain, HistorySegment, WorkingProjectionLimits};
use thiserror::Error;

use crate::manifest::{RuntimeManifestError, decode_bundle_digest, encode_bundle_digest};
use crate::{MAX_RUNTIME_CHECKPOINT_BYTES, MAX_RUNTIME_RECORDS, RuntimeCheckpoint};

mod builder;
mod codec;
mod index;
mod object;
mod operation_tree;
mod wire;

pub(crate) use object::{component_object_digest, object_address};

/// Format epoch of the complete persisted checkpoint object graph. Every
/// object address carries it and nothing from another epoch decodes. Any
/// pre-release change to a persisted runtime encoding bumps this one number;
/// the per-component revision constants inside the graph are frozen.
/// Epoch 37 authenticates the root-bound count of retired local-tool attempts.
pub const RUNTIME_FORMAT_EPOCH: u32 = 37;

const OBJECT_ADDRESS_BYTES: usize = 1 + 4 + 32;
const MAX_CHECKPOINT_OBJECTS: usize =
    MAX_RUNTIME_RECORDS * 8 + 64 + crate::manifest::named_state::MAX_STATE_KEYS;
// A long session retains operation evidence outside its compacted conversation.
// This bound must cover the reachable graph, not just a few flat manifest encodings.
const MAX_CHECKPOINT_OBJECT_BYTES: usize = 1024 * 1024 * 1024;
const OBJECT_ADDRESS_DOMAIN: &[u8] = b"plakous-agent/runtime-object-v3";

/// Closed semantic kind of one immutable checkpoint object.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum RuntimeObjectKind {
    /// Small checkpoint envelope naming the manifest root and semantic integrity.
    CheckpointRoot = 1,
    /// Small manifest root naming every reconstruction group.
    ManifestRoot = 2,
    /// Provenance, topology, lifecycle, and durability scalars.
    ManifestMetadata = 3,
    /// Exact immutable stable request prefix.
    StablePrefix = 4,
    /// One exact canonical history segment.
    HistorySegment = 5,
    /// One append-sharing persistent history-vector node.
    HistoryNode = 6,
    /// Exact bounded working projection over referenced segments.
    WorkingProjection = 7,
    /// One exact externally owned reconstruction component.
    ReconstructiveComponent = 8,
    /// Stable prefix, history head, working projection, and context binding.
    CanonicalState = 9,
    /// Exact model/route/account/profile binding.
    ModelRouteBinding = 10,
    /// Exact tool/exposure/environment/effect-policy binding.
    ToolRuntime = 11,
    /// One exact durable command ledger record.
    CommandRecord = 12,
    /// One append-sharing persistent command-vector node.
    CommandNode = 13,
    /// One exact durable physical-operation record.
    OperationRecord = 14,
    /// One structurally shared authenticated operation-map node.
    OperationNode = 15,
    /// Ordered command and operation vector heads.
    OperationLedger = 16,
    /// Exact family-native artifact and binding.
    FamilyArtifact = 17,
    /// Exact typed runtime context state.
    ContextState = 18,
    /// Reconstruction-complete selected and effective provider configuration.
    ProviderConfiguration = 19,
    /// One bounded canonical JSON value in session-owned named state.
    NamedStateValue = 20,
}

/// Complete content address of one immutable runtime object.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct RuntimeObjectAddress {
    kind: RuntimeObjectKind,
    codec_revision: u32,
    digest: Digest,
}

/// One exact immutable object and its indexed ordered child mirror.
#[derive(Clone)]
pub struct RuntimeObject {
    address: RuntimeObjectAddress,
    canonical_bytes: Arc<[u8]>,
    addressed_bytes: Arc<[u8]>,
    children: Arc<[RuntimeObjectAddress]>,
    decoded_segment: Option<HistorySegment>,
}

/// Sharing and retained-growth observations for one publication ancestry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CheckpointBundleStats {
    object_count: usize,
    retained_bytes: usize,
    reused_object_count: usize,
    reused_retained_bytes: usize,
    new_object_count: usize,
    new_retained_bytes: usize,
    visited_object_count: usize,
    new_index_node_count: usize,
}

/// Small root plus the exact closed object set required to materialize a checkpoint.
#[derive(Clone)]
pub struct CheckpointObjectBundle {
    checkpoint_id: CheckpointId,
    checkpoint_integrity: Digest,
    base_root: Option<RuntimeObjectAddress>,
    root: RuntimeObjectAddress,
    objects: Arc<[RuntimeObject]>,
    layers: Arc<ObjectLayer>,
    cas: Option<Arc<CasNode>>,
    checkpoint: Option<Arc<RuntimeCheckpoint>>,
    reuse: Option<Arc<BundleReuse>>,
    stats: CheckpointBundleStats,
    /// Layers chained since the bundle was last built from one closure.
    depth: usize,
    /// Objects the root reached when the bundle was last built from one
    /// closure; [`CheckpointObjectBundle::trimmed`] waits for twice as many.
    trim_floor: usize,
}

/// Layers a bundle may chain before a trim is due whatever its size.
const MAX_LAYER_DEPTH: usize = 256;

#[derive(Eq, PartialEq)]
struct ObjectLayer {
    prior: Option<Arc<Self>>,
    objects: Arc<[RuntimeObject]>,
}

impl Drop for ObjectLayer {
    fn drop(&mut self) {
        let mut prior = self.prior.take();
        while let Some(link) = prior {
            let Some(mut node) = Arc::into_inner(link) else {
                break;
            };
            prior = node.prior.take();
        }
    }
}

/// Persistent Patricia index used only to prove CAS membership/reversion
/// without scanning checkpoint ancestry. Its depth is bounded by the fixed
/// 296-bit address width, so each supplied object adds constant-bounded metadata.
#[derive(Clone)]
enum CasNode {
    Leaf(RuntimeObject),
    Branch {
        differing_bit: u16,
        zero: Arc<Self>,
        one: Arc<Self>,
    },
}

#[derive(Debug, Eq, PartialEq)]
struct BundleReuse {
    canonical: RuntimeObjectAddress,
    harness: RuntimeObjectAddress,
    provider_configuration: RuntimeObjectAddress,
    prefix: RuntimeObjectAddress,
    history_head: Option<RuntimeObjectAddress>,
    working: RuntimeObjectAddress,
    context: RuntimeObjectAddress,
    binding: RuntimeObjectAddress,
    tools: RuntimeObjectAddress,
    command_count: usize,
    command_head: Option<RuntimeObjectAddress>,
    operation_count: usize,
    operation_head: Option<RuntimeObjectAddress>,
    ledger: RuntimeObjectAddress,
    family: Option<RuntimeObjectAddress>,
    named_state: BTreeMap<String, RuntimeObjectAddress>,
}

/// Sole agent-owned persisted codec for the closed checkpoint object graph.
#[derive(Clone, Copy, Debug, Default)]
pub struct RuntimeCheckpointCodec;

/// Read-only diagnostic metadata, not a materialized or recovery-authorizing checkpoint.
#[derive(Clone, Debug)]
pub struct RuntimeCheckpointInspection {
    /// Exact topology bound by the manifest metadata address.
    pub topology: crate::manifest::RuntimeTopology,
    /// Recorded runtime lifecycle.
    pub lifecycle: crate::manifest::RuntimeLifecycle,
    /// Recorded durability.
    pub durability: crate::manifest::RuntimeDurability,
    /// Operation ids and canonical request identities named by recovery obligations.
    pub requests: Vec<(u64, Digest)>,
}

/// Typed fail-closed bundle construction or validation error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum CheckpointBundleError {
    /// An embedded semantic value failed its owning canonical codec.
    #[error("checkpoint object semantic value is invalid: {0}")]
    Manifest(#[from] RuntimeManifestError),
    /// A closed object-kind tag is unknown.
    #[error("checkpoint object kind tag {0} is unknown")]
    UnknownObjectKind(u8),
    /// An object or address uses a codec revision this codec does not implement.
    #[error("checkpoint object revision is unsupported")]
    UnsupportedRevision,
    /// An address does not match its exact kind, revision, and canonical bytes.
    #[error("checkpoint object content address is invalid")]
    InvalidAddress,
    /// Two values attempted to occupy one content address.
    #[error("checkpoint object address collision or disagreement")]
    AddressConflict,
    /// A required root or child object is absent.
    #[error("checkpoint object graph is missing a required object")]
    MissingObject,
    /// The supplied set contains an object unreachable from the sole root.
    #[error("checkpoint object graph contains an extra object")]
    ExtraObject,
    /// A child occupied the wrong closed semantic role.
    #[error("checkpoint object child has the wrong kind")]
    WrongKind,
    /// Canonical bytes or their ordered child mirror are not the sole encoding.
    #[error("checkpoint object encoding is noncanonical")]
    NonCanonical,
    /// A persistent vector count, order, or topology is invalid.
    #[error("checkpoint object graph topology is invalid")]
    InvalidTopology,
    /// A cycle was observed in the required object graph.
    #[error("checkpoint object graph contains a cycle")]
    Cycle,
    /// Object count, depth, or aggregate canonical bytes exceed fixed bounds.
    #[error("checkpoint object graph exceeds a fixed bound")]
    BoundExceeded,
    /// The root identities disagree with the materialized semantic checkpoint.
    #[error("checkpoint object graph disagrees with the semantic checkpoint")]
    SemanticDisagreement,
    /// The requested ordinary advance is not an exact append over the supplied base.
    #[error("checkpoint object incremental base does not match the semantic append")]
    IncrementalBaseMismatch,
    /// A custom bundle object is truncated or malformed.
    #[error("checkpoint object encoding is malformed")]
    Malformed,
}

struct BundleBuilder {
    objects: BTreeMap<RuntimeObjectAddress, RuntimeObject>,
    visited: usize,
}

struct BuiltCheckpoint {
    root: RuntimeObjectAddress,
    reuse: BundleReuse,
}

struct ObjectIndex<'a> {
    objects: BTreeMap<RuntimeObjectAddress, &'a RuntimeObject>,
    decoded: BTreeMap<RuntimeObjectAddress, HistorySegment>,
}

#[cfg(test)]
thread_local! {
    static OBJECT_HASHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ADDRESS_VALIDATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SEGMENT_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SEGMENT_HASHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "negative bundle tests preserve exact mutation sites and diagnostics"
)]
pub(crate) mod tests;
