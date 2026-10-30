//! Object addressing, store reconstruction and canonical-value validation.

#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

#[cfg(test)]
use super::{ADDRESS_VALIDATIONS, OBJECT_HASHES, SEGMENT_DECODES, SEGMENT_HASHES};

use plakous_model::{
    CompiledProviderConfiguration, Digest, DigestAlgorithm, DigestDomain, HistorySegment,
    StableRequestPrefix,
};

use crate::manifest::{
    DurableCommandRecord, FamilyArtifact, ModelRouteBinding, OperationRecordWire,
    ReconstructiveComponent, RuntimeContextState, RuntimeManifestError, ToolRuntimeManifest,
    decode_bundle_binding, decode_bundle_command_record, decode_bundle_component,
    decode_bundle_family_artifact, decode_bundle_history_segment, decode_bundle_metadata,
    decode_bundle_provider_configuration, decode_bundle_stable_prefix, decode_bundle_tools,
    encode_bundle_binding, encode_bundle_command_record, encode_bundle_component,
    encode_bundle_family_artifact, encode_bundle_metadata_value,
    encode_bundle_provider_configuration, encode_bundle_stable_prefix, encode_bundle_tools,
};

use super::wire::{
    parse_canonical, parse_checkpoint_root, parse_ledger, parse_manifest_root,
    parse_operation_node, parse_sequence_node, parse_working,
};
use super::{
    CheckpointBundleError, OBJECT_ADDRESS_BYTES, OBJECT_ADDRESS_DOMAIN, RUNTIME_FORMAT_EPOCH,
    RuntimeObject, RuntimeObjectAddress, RuntimeObjectKind,
};

impl RuntimeObjectKind {
    /// Decodes the closed persisted kind tag without delegating codec logic to a backend.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointBundleError::UnknownObjectKind`] for an unknown tag.
    pub fn from_store_tag(tag: u8) -> Result<Self, CheckpointBundleError> {
        match tag {
            1 => Ok(Self::CheckpointRoot),
            2 => Ok(Self::ManifestRoot),
            3 => Ok(Self::ManifestMetadata),
            4 => Ok(Self::StablePrefix),
            5 => Ok(Self::HistorySegment),
            6 => Ok(Self::HistoryNode),
            7 => Ok(Self::WorkingProjection),
            8 => Ok(Self::ReconstructiveComponent),
            9 => Ok(Self::CanonicalState),
            10 => Ok(Self::ModelRouteBinding),
            11 => Ok(Self::ToolRuntime),
            12 => Ok(Self::CommandRecord),
            13 => Ok(Self::CommandNode),
            14 => Ok(Self::OperationRecord),
            15 => Ok(Self::OperationNode),
            16 => Ok(Self::OperationLedger),
            17 => Ok(Self::FamilyArtifact),
            18 => Ok(Self::ContextState),
            19 => Ok(Self::ProviderConfiguration),
            20 => Ok(Self::NamedStateValue),
            _ => Err(CheckpointBundleError::UnknownObjectKind(tag)),
        }
    }
}

impl RuntimeObjectAddress {
    /// Reconstructs and validates one address read from normalized backend columns.
    ///
    /// # Errors
    ///
    /// Returns an error unless the codec revision and digest algorithm, domain,
    /// and version are exactly those of this fresh object format.
    pub fn from_store_parts(
        kind: RuntimeObjectKind,
        codec_revision: u32,
        digest: Digest,
    ) -> Result<Self, CheckpointBundleError> {
        if codec_revision != RUNTIME_FORMAT_EPOCH
            || digest.algorithm() != DigestAlgorithm::Sha256
            || digest.domain() != DigestDomain::Blob
            || digest.version() != RUNTIME_FORMAT_EPOCH
        {
            return Err(CheckpointBundleError::UnsupportedRevision);
        }
        Ok(Self {
            kind,
            codec_revision,
            digest,
        })
    }

    /// Returns the closed object kind.
    #[must_use]
    pub const fn kind(self) -> RuntimeObjectKind {
        self.kind
    }

    /// Returns the exact object codec revision.
    #[must_use]
    pub const fn codec_revision(self) -> u32 {
        self.codec_revision
    }

    /// Returns the domain-separated content digest.
    #[must_use]
    pub const fn digest(self) -> Digest {
        self.digest
    }
}

impl Ord for RuntimeObjectAddress {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.kind, self.codec_revision, self.digest.bytes()).cmp(&(
            other.kind,
            other.codec_revision,
            other.digest.bytes(),
        ))
    }
}

impl PartialOrd for RuntimeObjectAddress {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for RuntimeObjectAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeObjectAddress")
            .field("kind", &self.kind)
            .field("codec_revision", &self.codec_revision)
            .field("digest", &self.digest)
            .finish()
    }
}

impl PartialEq for RuntimeObject {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
            && self.canonical_bytes == other.canonical_bytes
            && self.children == other.children
    }
}

impl Eq for RuntimeObject {}

impl RuntimeObject {
    // Only BundleBuilder may bypass reconstruction: it just encoded these
    // bytes and their ordered edges and computed their address.
    pub(super) fn new_trusted(
        address: RuntimeObjectAddress,
        canonical_bytes: Vec<u8>,
        children: Vec<RuntimeObjectAddress>,
    ) -> Self {
        let canonical_bytes: Arc<[u8]> = canonical_bytes.into();
        Self {
            address,
            addressed_bytes: Arc::clone(&canonical_bytes),
            canonical_bytes,
            children: children.into(),
            decoded_segment: None,
        }
    }

    /// Reconstructs one object read from normalized backend rows.
    ///
    /// This validates the address against the exact canonical bytes. The closed
    /// ordered child mirror is validated with the other objects by
    /// [`super::CheckpointObjectBundle::from_store`], where child kinds and presence
    /// are available.
    ///
    /// # Errors
    ///
    /// Returns an error for a foreign revision/digest shape or bytes whose
    /// recomputed address differs from the stored address.
    pub fn from_store(
        address: RuntimeObjectAddress,
        canonical_bytes: Vec<u8>,
        ordered_children: Vec<RuntimeObjectAddress>,
    ) -> Result<Self, CheckpointBundleError> {
        RuntimeObjectAddress::from_store_parts(
            address.kind,
            address.codec_revision,
            address.digest,
        )?;
        #[cfg(test)]
        ADDRESS_VALIDATIONS.update(|count| count + 1);
        if object_address(address.kind, &canonical_bytes)? != address {
            return Err(CheckpointBundleError::InvalidAddress);
        }
        let decoded_segment = if address.kind == RuntimeObjectKind::HistorySegment {
            Some(canonical_history_segment(&canonical_bytes)?)
        } else {
            None
        };
        let expected_children = if decoded_segment.is_some() {
            Vec::new()
        } else {
            canonical_child_specs(address.kind, &canonical_bytes)?
        }
        .into_iter()
        .map(|(child, _)| child)
        .collect::<Vec<_>>();
        if expected_children != ordered_children {
            return Err(CheckpointBundleError::NonCanonical);
        }
        let canonical_bytes: Arc<[u8]> = canonical_bytes.into();
        Ok(Self {
            address,
            addressed_bytes: Arc::clone(&canonical_bytes),
            canonical_bytes,
            children: ordered_children.into(),
            decoded_segment,
        })
    }

    /// Returns the verified content address.
    #[must_use]
    pub const fn address(&self) -> RuntimeObjectAddress {
        self.address
    }

    /// Returns exact canonical bytes. Digests never substitute for these bytes.
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    /// Returns the ordered child-address mirror committed by the canonical bytes.
    #[must_use]
    pub fn children(&self) -> &[RuntimeObjectAddress] {
        &self.children
    }

    /// Returns conservative retained payload and edge bytes for benchmark counters.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.canonical_bytes
            .len()
            .saturating_add(self.children.len().saturating_mul(OBJECT_ADDRESS_BYTES))
    }
}

impl fmt::Debug for RuntimeObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeObject")
            .field("address", &self.address)
            .field("canonical_bytes_len", &self.canonical_bytes.len())
            .field("children", &self.children)
            .finish_non_exhaustive()
    }
}

pub(crate) fn object_address(
    kind: RuntimeObjectKind,
    canonical_bytes: &[u8],
) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
    #[cfg(test)]
    {
        OBJECT_HASHES.update(|count| count + 1);
        if kind == RuntimeObjectKind::HistorySegment {
            SEGMENT_HASHES.update(|count| count + 1);
        }
    }
    let codec_revision = RUNTIME_FORMAT_EPOCH;
    let digest = Digest::sha256(
        DigestDomain::Blob,
        codec_revision,
        &[
            OBJECT_ADDRESS_DOMAIN,
            &[kind as u8],
            &codec_revision.to_be_bytes(),
            canonical_bytes,
        ],
    )
    .map_err(|_| CheckpointBundleError::InvalidAddress)?;
    Ok(RuntimeObjectAddress {
        kind,
        codec_revision,
        digest,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "The canonical object graph enumerates every manifest child in stable role and ordinal order"
)]
pub(super) fn canonical_child_specs(
    kind: RuntimeObjectKind,
    canonical_bytes: &[u8],
) -> Result<Vec<(RuntimeObjectAddress, RuntimeObjectKind)>, CheckpointBundleError> {
    let children = match kind {
        RuntimeObjectKind::CheckpointRoot => {
            let (_, _, manifest) = parse_checkpoint_root(canonical_bytes)?;
            vec![(manifest, RuntimeObjectKind::ManifestRoot)]
        }
        RuntimeObjectKind::ManifestRoot => {
            let root = parse_manifest_root(canonical_bytes)?;
            let mut children = vec![
                (root.metadata, RuntimeObjectKind::ManifestMetadata),
                (root.harness, RuntimeObjectKind::ReconstructiveComponent),
                (
                    root.provider_configuration,
                    RuntimeObjectKind::ProviderConfiguration,
                ),
                (root.canonical, RuntimeObjectKind::CanonicalState),
                (root.binding, RuntimeObjectKind::ModelRouteBinding),
                (root.tools, RuntimeObjectKind::ToolRuntime),
                (root.ledger, RuntimeObjectKind::OperationLedger),
            ];
            children.extend(
                root.family
                    .map(|address| (address, RuntimeObjectKind::FamilyArtifact)),
            );
            children.extend(
                root.named_state
                    .into_values()
                    .map(|address| (address, RuntimeObjectKind::NamedStateValue)),
            );
            children
        }
        RuntimeObjectKind::NamedStateValue => {
            crate::manifest::named_state::validate_state_value(canonical_bytes)
                .map_err(RuntimeManifestError::from)?;
            Vec::new()
        }
        RuntimeObjectKind::ManifestMetadata => {
            canonical_metadata(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::StablePrefix => {
            canonical_stable_prefix(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::HistorySegment => {
            canonical_history_segment(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::HistoryNode => {
            let node = parse_sequence_node(canonical_bytes)?;
            let mut children = Vec::with_capacity(2);
            children.extend(
                node.previous
                    .map(|address| (address, RuntimeObjectKind::HistoryNode)),
            );
            children.push((node.value, RuntimeObjectKind::HistorySegment));
            children
        }
        RuntimeObjectKind::WorkingProjection => parse_working(canonical_bytes)?
            .segments
            .into_iter()
            .map(|address| (address, RuntimeObjectKind::HistorySegment))
            .collect(),
        RuntimeObjectKind::ReconstructiveComponent => {
            canonical_component(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::ContextState => {
            RuntimeContextState::decode(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::ProviderConfiguration => {
            canonical_provider_configuration(canonical_bytes)?;
            Vec::new()
        }
        RuntimeObjectKind::CanonicalState => {
            let canonical = parse_canonical(canonical_bytes)?;
            let mut children = vec![(canonical.prefix, RuntimeObjectKind::StablePrefix)];
            children.extend(
                canonical
                    .history_head
                    .map(|address| (address, RuntimeObjectKind::HistoryNode)),
            );
            children.extend([
                (canonical.working, RuntimeObjectKind::WorkingProjection),
                (canonical.context, RuntimeObjectKind::ContextState),
            ]);
            children
        }
        RuntimeObjectKind::ModelRouteBinding => {
            let binding = canonical_binding(canonical_bytes)?;
            [binding.descriptor(), binding.admission().evidence()]
                .into_iter()
                .map(|component| {
                    component_address(component)
                        .map(|address| (address, RuntimeObjectKind::ReconstructiveComponent))
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        RuntimeObjectKind::ToolRuntime => {
            let tools = canonical_tools(canonical_bytes)?;
            [
                tools.registry(),
                tools.exposure(),
                tools.environment(),
                tools.effect_policy(),
            ]
            .into_iter()
            .map(|component| {
                component_address(component)
                    .map(|address| (address, RuntimeObjectKind::ReconstructiveComponent))
            })
            .collect::<Result<Vec<_>, _>>()?
        }
        RuntimeObjectKind::CommandRecord => {
            let record = canonical_command(canonical_bytes)?;
            vec![(
                component_address(record.command_component())?,
                RuntimeObjectKind::ReconstructiveComponent,
            )]
        }
        RuntimeObjectKind::CommandNode => {
            let node = parse_sequence_node(canonical_bytes)?;
            let mut children = Vec::with_capacity(2);
            children.extend(
                node.previous
                    .map(|address| (address, RuntimeObjectKind::CommandNode)),
            );
            children.push((node.value, RuntimeObjectKind::CommandRecord));
            children
        }
        RuntimeObjectKind::OperationRecord => canonical_operation_wire(canonical_bytes)?
            .component_digests()
            .into_iter()
            .map(|digest| {
                component_object_address(digest)
                    .map(|address| (address, RuntimeObjectKind::ReconstructiveComponent))
            })
            .collect::<Result<Vec<_>, _>>()?,
        RuntimeObjectKind::OperationNode => {
            let node = parse_operation_node(canonical_bytes)?;
            let mut children = Vec::with_capacity(3);
            children.extend(
                node.left
                    .map(|address| (address, RuntimeObjectKind::OperationNode)),
            );
            children.push((node.record, RuntimeObjectKind::OperationRecord));
            children.extend(
                node.right
                    .map(|address| (address, RuntimeObjectKind::OperationNode)),
            );
            children
        }
        RuntimeObjectKind::OperationLedger => {
            let ledger = parse_ledger(canonical_bytes)?;
            let mut children = Vec::with_capacity(2);
            children.extend(
                ledger
                    .command_head
                    .map(|address| (address, RuntimeObjectKind::CommandNode)),
            );
            children.extend(
                ledger
                    .operation_head
                    .map(|address| (address, RuntimeObjectKind::OperationNode)),
            );
            children
        }
        RuntimeObjectKind::FamilyArtifact => {
            let artifact = canonical_family_artifact(canonical_bytes)?;
            vec![(
                component_address(artifact.binding_component())?,
                RuntimeObjectKind::ReconstructiveComponent,
            )]
        }
    };
    Ok(children)
}

fn component_address(
    component: &ReconstructiveComponent,
) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
    object_address(
        RuntimeObjectKind::ReconstructiveComponent,
        &encode_bundle_component(component)?,
    )
}

/// Content digest of the reconstructive-component object holding `component`;
/// an operation record names its components by this digest.
pub(crate) fn component_object_digest(
    component: &ReconstructiveComponent,
) -> Result<Digest, RuntimeManifestError> {
    component_address(component)
        .map(|address| address.digest)
        .map_err(|error| match error {
            CheckpointBundleError::Manifest(error) => error,
            _ => RuntimeManifestError::InvalidDigest,
        })
}

pub(super) fn component_object_address(
    digest: Digest,
) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
    RuntimeObjectAddress::from_store_parts(
        RuntimeObjectKind::ReconstructiveComponent,
        RUNTIME_FORMAT_EPOCH,
        digest,
    )
}

pub(super) fn canonical_operation_wire(
    bytes: &[u8],
) -> Result<OperationRecordWire, CheckpointBundleError> {
    let wire = OperationRecordWire::decode(bytes)?;
    if wire.encode()? != bytes {
        return Err(CheckpointBundleError::NonCanonical);
    }
    Ok(wire)
}

fn canonical_metadata(bytes: &[u8]) -> Result<(), CheckpointBundleError> {
    let metadata = decode_bundle_metadata(bytes)?;
    if encode_bundle_metadata_value(&metadata)? != bytes {
        return Err(CheckpointBundleError::NonCanonical);
    }
    Ok(())
}

macro_rules! canonical_value {
    ($name:ident, $ty:ty, $decode:ident, $encode:ident) => {
        pub(super) fn $name(bytes: &[u8]) -> Result<$ty, CheckpointBundleError> {
            let value = $decode(bytes)?;
            if $encode(&value)? != bytes {
                return Err(CheckpointBundleError::NonCanonical);
            }
            Ok(value)
        }
    };
}

canonical_value!(
    canonical_stable_prefix,
    StableRequestPrefix,
    decode_bundle_stable_prefix,
    encode_bundle_stable_prefix
);
pub(super) fn canonical_history_segment(
    bytes: &[u8],
) -> Result<HistorySegment, CheckpointBundleError> {
    #[cfg(test)]
    SEGMENT_DECODES.update(|count| count + 1);
    Ok(decode_bundle_history_segment(bytes)?)
}
canonical_value!(
    canonical_component,
    ReconstructiveComponent,
    decode_bundle_component,
    encode_bundle_component
);
canonical_value!(
    canonical_provider_configuration,
    CompiledProviderConfiguration,
    decode_bundle_provider_configuration,
    encode_bundle_provider_configuration
);
canonical_value!(
    canonical_binding,
    ModelRouteBinding,
    decode_bundle_binding,
    encode_bundle_binding
);
canonical_value!(
    canonical_tools,
    ToolRuntimeManifest,
    decode_bundle_tools,
    encode_bundle_tools
);
canonical_value!(
    canonical_command,
    DurableCommandRecord,
    decode_bundle_command_record,
    encode_bundle_command_record
);
canonical_value!(
    canonical_family_artifact,
    FamilyArtifact,
    decode_bundle_family_artifact,
    encode_bundle_family_artifact
);
