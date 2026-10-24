//! Tags, byte codecs, and receipt integrity.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::*;

pub(super) fn lease(
    session: SessionRef,
    owner: Option<&str>,
    fence: Option<&[u8]>,
    expires: Option<&[u8]>,
    nanoseconds: Option<i64>,
    duration: Option<&[u8]>,
) -> Result<Option<BranchLease>, SessionStoreError> {
    match (owner, fence, expires, nanoseconds, duration) {
        (None, None, None, None, None) => Ok(None),
        (Some(owner), Some(fence), Some(expires), Some(nanoseconds), Some(duration)) => {
            Ok(Some(BranchLease::from_store(
                session,
                parse(owner)?,
                nz(fence)?,
                lease_time(expires, nanoseconds)?,
                nz(duration)?,
            )))
        }
        _ => Err(SessionStoreError::corrupt()),
    }
}

pub(super) fn lineage(
    session: Option<&str>,
    branch: Option<&str>,
    checkpoint: Option<&str>,
    revision: Option<&[u8]>,
) -> Result<Option<ForkLineage>, SessionStoreError> {
    match (session, branch, checkpoint, revision) {
        (None, None, None, None) => Ok(None),
        (Some(session), Some(branch), Some(checkpoint), Some(revision)) => Ok(Some(ForkLineage {
            session_id: parse(session)?,
            branch_id: parse(branch)?,
            checkpoint_id: parse(checkpoint)?,
            branch_revision: read_u64(revision)?,
        })),
        _ => Err(SessionStoreError::corrupt()),
    }
}

pub(super) fn durability_tag(d: RuntimeDurability) -> Result<u8, SessionStoreError> {
    match d {
        RuntimeDurability::Required => Ok(1),
        RuntimeDurability::BestEffort => Ok(2),
        RuntimeDurability::Disabled => Err(SessionStoreError::Invalid(
            StoreInvalidReason::DisabledDurabilityRequiresMemoryStore,
        )),
    }
}

pub(super) fn decode_durability(v: i64) -> Result<RuntimeDurability, SessionStoreError> {
    match v {
        1 => Ok(RuntimeDurability::Required),
        2 => Ok(RuntimeDurability::BestEffort),
        _ => Err(SessionStoreError::corrupt()),
    }
}

pub(super) fn checkpoint_kind_tag(k: RuntimeCheckpointKind) -> i64 {
    match k {
        RuntimeCheckpointKind::Quiescent => 1,
        RuntimeCheckpointKind::Working => 2,
    }
}

pub(super) fn transition_tag(transition: BranchTransitionMode) -> i64 {
    match transition {
        BranchTransitionMode::OrdinaryFork => 1,
        BranchTransitionMode::Rebind => 2,
    }
}

pub(super) fn decode_transition(value: i64) -> Result<BranchTransitionMode, SessionStoreError> {
    match value {
        1 => Ok(BranchTransitionMode::OrdinaryFork),
        2 => Ok(BranchTransitionMode::Rebind),
        _ => Err(SessionStoreError::UnsupportedCodec),
    }
}

pub(super) fn decode_checkpoint_kind(v: i64) -> Result<RuntimeCheckpointKind, SessionStoreError> {
    match v {
        1 => Ok(RuntimeCheckpointKind::Quiescent),
        2 => Ok(RuntimeCheckpointKind::Working),
        _ => Err(SessionStoreError::corrupt()),
    }
}

pub(super) fn decode_address(
    k: i64,
    c: i64,
    d: Vec<u8>,
) -> Result<RuntimeObjectAddress, SessionStoreError> {
    let kind = u8::try_from(k)
        .ok()
        .and_then(|v| RuntimeObjectKind::from_store_tag(v).ok())
        .ok_or(SessionStoreError::UnsupportedCodec)?;
    let codec = u32::try_from(c).map_err(|_| SessionStoreError::UnsupportedCodec)?;
    let digest = Digest::from_sha256_bytes(DigestDomain::Blob, codec, array32(d)?)
        .map_err(|_| SessionStoreError::UnsupportedCodec)?;
    RuntimeObjectAddress::from_store_parts(kind, codec, digest).map_err(bundle_error)
}

pub(super) fn decode_digest(
    domain: DigestDomain,
    v: i64,
    b: Vec<u8>,
) -> Result<Digest, SessionStoreError> {
    Digest::from_sha256_bytes(
        domain,
        u32::try_from(v).map_err(|_| SessionStoreError::UnsupportedCodec)?,
        array32(b)?,
    )
    .map_err(|_| SessionStoreError::UnsupportedCodec)
}

pub(super) fn encode_address(out: &mut Vec<u8>, a: RuntimeObjectAddress) {
    out.push(kind_tag(a.kind()));
    out.extend_from_slice(&a.codec_revision().to_be_bytes());
    out.extend_from_slice(a.digest().bytes());
}

pub(super) fn kind_tag(k: RuntimeObjectKind) -> u8 {
    k as u8
}

// The deadline blob is domain epoch (16), scope (32), then seconds (8).
// Its nanoseconds live in the companion integer column. UTC records use the
// independent timestamp codec below and never pass through this decoder.
pub(super) fn domain_bytes(domain: LeaseDomain) -> Vec<u8> {
    [domain.epoch().as_slice(), domain.scope().as_slice()].concat()
}
pub(super) fn decode_domain(bytes: &[u8]) -> Result<LeaseDomain, SessionStoreError> {
    let bytes: &[u8; 48] = bytes.try_into().map_err(|_| SessionStoreError::corrupt())?;
    Ok(LeaseDomain::new(
        bytes[..16]
            .try_into()
            .map_err(|_| SessionStoreError::corrupt())?,
        bytes[16..]
            .try_into()
            .map_err(|_| SessionStoreError::corrupt())?,
    ))
}

pub(super) fn lease_time(
    bytes: &[u8],
    nanoseconds: i64,
) -> Result<LeaseInstant, SessionStoreError> {
    let bytes: &[u8; 56] = bytes.try_into().map_err(|_| SessionStoreError::corrupt())?;
    LeaseInstant::new(
        decode_domain(&bytes[..48])?,
        read_u64(&bytes[48..])?,
        u32::try_from(nanoseconds).map_err(|_| SessionStoreError::corrupt())?,
    )
    .map_err(|_| SessionStoreError::corrupt())
}

pub(super) fn lease_time_bytes(value: LeaseInstant) -> Vec<u8> {
    let mut bytes = domain_bytes(value.domain());
    bytes.extend_from_slice(&value.seconds().to_be_bytes());
    bytes
}

pub(super) fn lease_instant_bytes(value: LeaseInstant) -> Vec<u8> {
    framed(
        b"lease-instant",
        &[
            domain_bytes(value.domain()),
            value.seconds().to_be_bytes().to_vec(),
            value.nanoseconds().to_be_bytes().to_vec(),
        ],
    )
}

pub(super) fn timestamp(s: &[u8], ns: i64) -> Result<Timestamp, SessionStoreError> {
    Timestamp::new(
        read_u64(s)?,
        u32::try_from(ns).map_err(|_| SessionStoreError::corrupt())?,
    )
    .map_err(|_| SessionStoreError::corrupt())
}

pub(super) fn u64b(v: u64) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}

pub(super) fn read_u64(v: &[u8]) -> Result<u64, SessionStoreError> {
    <[u8; 8]>::try_from(v)
        .map(u64::from_be_bytes)
        .map_err(|_| SessionStoreError::corrupt())
}

pub(super) fn nz(v: &[u8]) -> Result<NonZeroU64, SessionStoreError> {
    NonZeroU64::new(read_u64(v)?).ok_or(SessionStoreError::corrupt())
}

fn array32(v: Vec<u8>) -> Result<[u8; 32], SessionStoreError> {
    v.try_into().map_err(|_| SessionStoreError::corrupt())
}

pub(super) fn workflow_id_bytes(value: SessionWorkflowId) -> Vec<u8> {
    value.as_bytes().to_vec()
}

pub(super) fn decode_workflow_id(value: &[u8]) -> Result<SessionWorkflowId, SessionStoreError> {
    SessionWorkflowId::new(<[u8; 32]>::try_from(value).map_err(|_| SessionStoreError::corrupt())?)
        .map_err(|_| SessionStoreError::corrupt())
}

pub(super) fn boolv(v: i64) -> Result<bool, SessionStoreError> {
    match v {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(SessionStoreError::corrupt()),
    }
}

pub(super) fn parse<T: FromStr>(v: &str) -> Result<T, SessionStoreError> {
    v.parse().map_err(|_| SessionStoreError::corrupt())
}

pub(super) fn id(v: impl fmt::Display) -> String {
    v.to_string()
}

pub(super) fn nonnegative(v: i64) -> Result<u64, SessionStoreError> {
    u64::try_from(v).map_err(|_| SessionStoreError::corrupt())
}

pub(super) fn i64n(v: u64) -> Result<i64, SessionStoreError> {
    i64::try_from(v)
        .map_err(|_| SessionStoreError::capacity(StoreCapacityBound::SqliteInteger, v, 0))
}

pub(super) fn exactly_one(changed: usize) -> Result<(), SessionStoreError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(SessionStoreError::corrupt())
    }
}

pub(super) fn u64n(v: usize) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

pub(super) const fn nonzero(v: usize) -> NonZeroUsize {
    match NonZeroUsize::new(v) {
        Some(v) => v,
        None => panic!("nonzero"),
    }
}

pub(super) fn create_receipt_integrity(
    receipt: &CreateReceipt,
) -> Result<Vec<u8>, SessionStoreError> {
    let operation = receipt.operation();
    receipt_integrity(
        b"create",
        &[
            session_ref_bytes(operation.session()),
            timestamp_bytes(operation.created_at()),
            text_bytes(operation.owner()),
            operation.lease_seconds().get().to_be_bytes().to_vec(),
            checkpoint_ref_bytes(operation.initial()),
            operation.checkpoint_commit_identity().identity_bytes(),
            session_head_bytes(receipt.session())?,
            branch_head_bytes(receipt.branch()),
            lease_bytes(receipt.lease()),
            receipt.feed_seq().get().to_be_bytes().to_vec(),
        ],
    )
}

pub(super) fn commit_receipt_integrity(
    committed: &CommittedCheckpoint,
) -> Result<Vec<u8>, SessionStoreError> {
    let operation = committed.operation();
    let receipt = committed.receipt();
    receipt_integrity(
        b"commit",
        &[
            lease_bytes(operation.lease()),
            branch_head_bytes(operation.expected()),
            checkpoint_ref_bytes(operation.checkpoint()),
            operation.checkpoint_commit_identity().identity_bytes(),
            checkpoint_ref_bytes(receipt.checkpoint()),
            branch_head_bytes(receipt.previous()),
            branch_head_bytes(receipt.head()),
            receipt.feed_seq().get().to_be_bytes().to_vec(),
        ],
    )
}

pub(super) fn fork_receipt_integrity(receipt: &ForkReceipt) -> Result<Vec<u8>, SessionStoreError> {
    let operation = receipt.operation();
    receipt_integrity(
        b"fork-v4",
        &[
            operation.workflow_id().as_bytes().to_vec(),
            operation.semantic_request().identity_bytes(),
            operation
                .expected_session_revision()
                .get()
                .to_be_bytes()
                .to_vec(),
            branch_head_bytes(operation.source()),
            session_ref_bytes(operation.target()),
            transition_tag(operation.transition())
                .to_be_bytes()
                .to_vec(),
            checkpoint_ref_bytes(operation.checkpoint()),
            operation.checkpoint_commit_identity().identity_bytes(),
            session_head_bytes(receipt.previous_session())?,
            session_head_bytes(receipt.session())?,
            branch_head_bytes(receipt.source()),
            branch_head_bytes(receipt.target()),
            lease_bytes(receipt.lease()),
            receipt.feed_seq().get().to_be_bytes().to_vec(),
        ],
    )
}

pub(super) fn archive_receipt_integrity(
    receipt: ArchiveReceipt,
) -> Result<Vec<u8>, SessionStoreError> {
    receipt_integrity(
        b"archive",
        &[
            text_bytes(receipt.request().session_id()),
            receipt
                .request()
                .expected_revision()
                .get()
                .to_be_bytes()
                .to_vec(),
            session_head_bytes(receipt.previous())?,
            session_head_bytes(receipt.session())?,
        ],
    )
}

pub(super) fn delete_receipt_integrity(
    receipt: DeleteReceipt,
) -> Result<Vec<u8>, SessionStoreError> {
    receipt_integrity(
        b"delete-v1",
        &[
            text_bytes(receipt.request().session_id()),
            receipt
                .request()
                .expected_revision()
                .get()
                .to_be_bytes()
                .to_vec(),
            session_head_bytes(receipt.archived())?,
            receipt.reclaimed_checkpoints().to_be_bytes().to_vec(),
        ],
    )
}

pub(super) fn receipt_integrity(
    label: &[u8],
    fields: &[Vec<u8>],
) -> Result<Vec<u8>, SessionStoreError> {
    let encoded = framed(label, fields);
    Digest::sha256(DigestDomain::Blob, 1, &[&encoded])
        .map(|digest| digest.bytes().to_vec())
        .map_err(|_| SessionStoreError::corrupt())
}

fn framed(label: &[u8], fields: &[Vec<u8>]) -> Vec<u8> {
    let mut encoded = Vec::new();
    encode_field(&mut encoded, label);
    for field in fields {
        encode_field(&mut encoded, field);
    }
    encoded
}

fn encode_field(encoded: &mut Vec<u8>, value: &[u8]) {
    encoded.extend_from_slice(&u64n(value.len()).to_be_bytes());
    encoded.extend_from_slice(value);
}

pub(super) fn session_ref_bytes(value: SessionRef) -> Vec<u8> {
    framed(
        b"session-ref",
        &[
            text_bytes(value.session_id()),
            text_bytes(value.branch_id()),
        ],
    )
}

pub(super) fn checkpoint_ref_bytes(value: CheckpointObjectRef) -> Vec<u8> {
    framed(
        b"checkpoint-ref",
        &[
            text_bytes(value.checkpoint_id()),
            address_bytes(value.root()),
            value.integrity().identity_bytes(),
        ],
    )
}

fn branch_head_bytes(value: BranchHead) -> Vec<u8> {
    framed(
        b"branch-head",
        &[
            session_ref_bytes(value.session()),
            value.revision().get().to_be_bytes().to_vec(),
            checkpoint_ref_bytes(value.checkpoint()),
        ],
    )
}

fn session_head_bytes(value: SessionHead) -> Result<Vec<u8>, SessionStoreError> {
    Ok(framed(
        b"session-head",
        &[
            text_bytes(value.session_id()),
            text_bytes(value.initial_branch()),
            value.revision().get().to_be_bytes().to_vec(),
            timestamp_bytes(value.created_at()),
            vec![durability_tag(value.durability())?],
            vec![u8::from(value.is_archived())],
        ],
    ))
}

fn lease_bytes(value: &BranchLease) -> Vec<u8> {
    framed(
        b"branch-lease",
        &[
            session_ref_bytes(value.session()),
            text_bytes(value.owner()),
            value.fence().get().to_be_bytes().to_vec(),
            lease_instant_bytes(value.expires_at()),
            value.lease_seconds().get().to_be_bytes().to_vec(),
        ],
    )
}

fn address_bytes(value: RuntimeObjectAddress) -> Vec<u8> {
    framed(
        b"object-address",
        &[
            vec![kind_tag(value.kind())],
            value.codec_revision().to_be_bytes().to_vec(),
            value.digest().identity_bytes(),
        ],
    )
}

fn timestamp_bytes(value: Timestamp) -> Vec<u8> {
    framed(
        b"timestamp",
        &[
            value.seconds().to_be_bytes().to_vec(),
            value.nanoseconds().to_be_bytes().to_vec(),
        ],
    )
}

pub(super) fn text_bytes(value: impl fmt::Display) -> Vec<u8> {
    value.to_string().into_bytes()
}
