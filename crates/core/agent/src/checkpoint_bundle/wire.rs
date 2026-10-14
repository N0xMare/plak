//! Canonical checkpoint node bytes.

use super::{
    CheckpointBundleError, CheckpointId, Digest, DigestDomain, MAX_RUNTIME_CHECKPOINT_BYTES,
    MAX_RUNTIME_RECORDS, RUNTIME_FORMAT_EPOCH, RuntimeObjectAddress, RuntimeObjectKind,
    WorkingProjectionLimits, decode_bundle_digest, encode_bundle_digest,
};

#[derive(Default)]
pub(super) struct BundleWriter {
    bytes: Vec<u8>,
}

impl BundleWriter {
    pub(super) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(super) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(super) fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub(super) fn count(&mut self, value: usize) -> Result<(), CheckpointBundleError> {
        if value > MAX_RUNTIME_RECORDS {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        self.usize(value)
    }

    pub(super) fn usize(&mut self, value: usize) -> Result<(), CheckpointBundleError> {
        self.u64(u64::try_from(value).map_err(|_| CheckpointBundleError::BoundExceeded)?);
        Ok(())
    }

    pub(super) fn raw(&mut self, value: &[u8]) -> Result<(), CheckpointBundleError> {
        let length =
            u32::try_from(value.len()).map_err(|_| CheckpointBundleError::BoundExceeded)?;
        self.u32(length);
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    pub(super) fn uuid(&mut self, value: uuid::Uuid) {
        self.bytes.extend_from_slice(value.as_bytes());
    }

    pub(super) fn digest(&mut self, value: Digest) -> Result<(), CheckpointBundleError> {
        self.raw(&encode_bundle_digest(value)?)
    }

    pub(super) fn address(&mut self, value: RuntimeObjectAddress) {
        self.u8(value.kind as u8);
        self.u32(value.codec_revision);
        self.bytes.extend_from_slice(value.digest.bytes());
    }

    pub(super) fn optional_address(&mut self, value: Option<RuntimeObjectAddress>) {
        self.u8(u8::from(value.is_some()));
        if let Some(value) = value {
            self.address(value);
        }
    }

    pub(super) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

struct BundleReader<'a> {
    bytes: &'a [u8],
    offset: usize,
    codec_revision: u32,
}

impl<'a> BundleReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self::new_at(bytes, RUNTIME_FORMAT_EPOCH)
    }

    const fn new_at(bytes: &'a [u8], codec_revision: u32) -> Self {
        Self {
            bytes,
            offset: 0,
            codec_revision,
        }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], CheckpointBundleError> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or(CheckpointBundleError::Malformed)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(CheckpointBundleError::Malformed)?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, CheckpointBundleError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, CheckpointBundleError> {
        Ok(u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| CheckpointBundleError::Malformed)?,
        ))
    }

    fn u64(&mut self) -> Result<u64, CheckpointBundleError> {
        Ok(u64::from_be_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| CheckpointBundleError::Malformed)?,
        ))
    }

    fn usize(&mut self) -> Result<usize, CheckpointBundleError> {
        usize::try_from(self.u64()?).map_err(|_| CheckpointBundleError::BoundExceeded)
    }

    fn count(&mut self) -> Result<usize, CheckpointBundleError> {
        let value = self.usize()?;
        if value > MAX_RUNTIME_RECORDS {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        Ok(value)
    }

    fn raw(&mut self) -> Result<&'a [u8], CheckpointBundleError> {
        let length = usize::try_from(self.u32()?).map_err(|_| CheckpointBundleError::Malformed)?;
        if length > MAX_RUNTIME_CHECKPOINT_BYTES {
            return Err(CheckpointBundleError::BoundExceeded);
        }
        self.take(length)
    }

    fn uuid(&mut self) -> Result<uuid::Uuid, CheckpointBundleError> {
        Ok(uuid::Uuid::from_bytes(
            self.take(16)?
                .try_into()
                .map_err(|_| CheckpointBundleError::Malformed)?,
        ))
    }

    fn digest(&mut self) -> Result<Digest, CheckpointBundleError> {
        decode_bundle_digest(self.raw()?).map_err(Into::into)
    }

    fn address(&mut self) -> Result<RuntimeObjectAddress, CheckpointBundleError> {
        let kind = RuntimeObjectKind::from_store_tag(self.u8()?)?;
        let codec_revision = self.u32()?;
        let bytes = self
            .take(32)?
            .try_into()
            .map_err(|_| CheckpointBundleError::Malformed)?;
        if codec_revision != self.codec_revision {
            return Err(CheckpointBundleError::UnsupportedRevision);
        }
        let digest = Digest::from_sha256_bytes(DigestDomain::Blob, self.codec_revision, bytes)
            .map_err(|_| CheckpointBundleError::InvalidAddress)?;
        Ok(RuntimeObjectAddress {
            kind,
            codec_revision,
            digest,
        })
    }

    fn optional_address(&mut self) -> Result<Option<RuntimeObjectAddress>, CheckpointBundleError> {
        match self.u8()? {
            0 => Ok(None),
            1 => self.address().map(Some),
            _ => Err(CheckpointBundleError::NonCanonical),
        }
    }

    fn finish(self) -> Result<(), CheckpointBundleError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(CheckpointBundleError::NonCanonical)
        }
    }
}

pub(super) fn parse_checkpoint_root(
    bytes: &[u8],
) -> Result<(CheckpointId, Digest, RuntimeObjectAddress), CheckpointBundleError> {
    let mut reader = BundleReader::new(bytes);
    let checkpoint_id =
        CheckpointId::try_from(reader.uuid()?).map_err(|_| CheckpointBundleError::Malformed)?;
    let integrity = reader.digest()?;
    let manifest = reader.address()?;
    reader.finish()?;
    Ok((checkpoint_id, integrity, manifest))
}

pub(super) struct ManifestRootSpec {
    pub(super) metadata: RuntimeObjectAddress,
    pub(super) harness: RuntimeObjectAddress,
    pub(super) provider_configuration: RuntimeObjectAddress,
    pub(super) canonical: RuntimeObjectAddress,
    pub(super) binding: RuntimeObjectAddress,
    pub(super) tools: RuntimeObjectAddress,
    pub(super) ledger: RuntimeObjectAddress,
    pub(super) family: Option<RuntimeObjectAddress>,
}

pub(super) fn parse_manifest_root(bytes: &[u8]) -> Result<ManifestRootSpec, CheckpointBundleError> {
    let mut reader = BundleReader::new(bytes);
    let value = ManifestRootSpec {
        metadata: reader.address()?,
        harness: reader.address()?,
        provider_configuration: reader.address()?,
        canonical: reader.address()?,
        binding: reader.address()?,
        tools: reader.address()?,
        ledger: reader.address()?,
        family: reader.optional_address()?,
    };
    reader.finish()?;
    Ok(value)
}

pub(super) struct SequenceNodeSpec {
    pub(super) count: usize,
    pub(super) previous: Option<RuntimeObjectAddress>,
    pub(super) value: RuntimeObjectAddress,
}

pub(super) fn parse_sequence_node(bytes: &[u8]) -> Result<SequenceNodeSpec, CheckpointBundleError> {
    let mut reader = BundleReader::new(bytes);
    let value = SequenceNodeSpec {
        count: reader.count()?,
        previous: reader.optional_address()?,
        value: reader.address()?,
    };
    reader.finish()?;
    if value.count == 0 || (value.count == 1) != value.previous.is_none() {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    Ok(value)
}

#[derive(Clone, Copy)]
pub(super) struct OperationNodeSpec {
    pub(super) key: u64,
    pub(super) count: usize,
    pub(super) height: u8,
    pub(super) left: Option<RuntimeObjectAddress>,
    pub(super) record: RuntimeObjectAddress,
    pub(super) right: Option<RuntimeObjectAddress>,
}

pub(super) fn parse_operation_node(
    bytes: &[u8],
) -> Result<OperationNodeSpec, CheckpointBundleError> {
    let mut reader = BundleReader::new(bytes);
    let value = OperationNodeSpec {
        key: reader.u64()?,
        count: reader.count()?,
        height: reader.u8()?,
        left: reader.optional_address()?,
        record: reader.address()?,
        right: reader.optional_address()?,
    };
    reader.finish()?;
    if value.key == 0 || value.count == 0 || value.height == 0 {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    Ok(value)
}

pub(super) struct WorkingSpec {
    pub(super) limits: WorkingProjectionLimits,
    pub(super) segments: Vec<RuntimeObjectAddress>,
}

pub(super) fn parse_working(bytes: &[u8]) -> Result<WorkingSpec, CheckpointBundleError> {
    let mut reader = BundleReader::new(bytes);
    let limits = WorkingProjectionLimits::new(reader.usize()?, reader.usize()?, reader.usize()?)
        .map_err(|_| CheckpointBundleError::InvalidTopology)?;
    let count = reader.count()?;
    let mut segments = Vec::with_capacity(count);
    for _ in 0..count {
        segments.push(reader.address()?);
    }
    reader.finish()?;
    Ok(WorkingSpec { limits, segments })
}

pub(super) struct CanonicalSpec {
    pub(super) prefix: RuntimeObjectAddress,
    pub(super) history_count: usize,
    pub(super) history_identity: Digest,
    pub(super) history_head: Option<RuntimeObjectAddress>,
    pub(super) working: RuntimeObjectAddress,
    pub(super) context: RuntimeObjectAddress,
}

pub(super) fn parse_canonical(bytes: &[u8]) -> Result<CanonicalSpec, CheckpointBundleError> {
    let mut reader = BundleReader::new(bytes);
    let value = CanonicalSpec {
        prefix: reader.address()?,
        history_count: reader.count()?,
        history_identity: reader.digest()?,
        history_head: reader.optional_address()?,
        working: reader.address()?,
        context: reader.address()?,
    };
    reader.finish()?;
    if (value.history_count == 0) != value.history_head.is_none()
        || value.history_identity.domain() != DigestDomain::CanonicalHistory
    {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    Ok(value)
}

pub(super) struct LedgerSpec {
    pub(super) command_count: usize,
    pub(super) command_head: Option<RuntimeObjectAddress>,
    pub(super) operation_count: usize,
    pub(super) operation_head: Option<RuntimeObjectAddress>,
}

pub(super) fn parse_ledger(bytes: &[u8]) -> Result<LedgerSpec, CheckpointBundleError> {
    let mut reader = BundleReader::new(bytes);
    let value = LedgerSpec {
        command_count: reader.count()?,
        command_head: reader.optional_address()?,
        operation_count: reader.count()?,
        operation_head: reader.optional_address()?,
    };
    reader.finish()?;
    if (value.command_count == 0) != value.command_head.is_none()
        || (value.operation_count == 0) != value.operation_head.is_none()
    {
        return Err(CheckpointBundleError::InvalidTopology);
    }
    Ok(value)
}
