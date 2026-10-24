//! The binary artifact envelope: encoding, decoding and its integrity witness.

use std::fmt;
use std::sync::Arc;

use plakous_model::{
    Digest, DigestDomain, ModelAdapterProfileId, ModelFamilyId, ModelId, ModelOperationRoot,
    ProtocolId, ReplayKindId, RouteId, SessionWorkflowRootId, Timestamp,
};

use super::{
    ENVELOPE_OVERHEAD, EncodedBody, INTEGRITY_DOMAIN, MAGIC, MAX_BODY_BYTES, MAX_ID_BYTES,
    MAX_OPAQUE_ID_BYTES, MAX_RESPONSES_ARTIFACT_BYTES, RESPONSES_ARTIFACT_CODEC_REVISION,
    RESPONSES_ARTIFACT_SCHEMA_REVISION, ResponsesArtifact, ResponsesArtifactBinding,
    ResponsesArtifactError, ResponsesArtifactProduction, ResponsesArtifactReplayKind,
    ResponsesCacheFact, ResponsesContinuation, ResponsesReplayContract, ResponsesSemanticRevisions,
    ResponsesStorageMode, UUID_TEXT_BYTES, validate_digest,
};

/// Closed family-local codec for canonical Responses artifact bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResponsesArtifactCodec;

impl ResponsesArtifactCodec {
    /// Encodes one artifact without reserializing or rehashing its immutable body.
    pub fn encode(artifact: &ResponsesArtifact) -> Result<Vec<u8>, ResponsesArtifactError> {
        if artifact.encoded_len != envelope_len(artifact.canonical_body.len())? {
            return Err(ResponsesArtifactError::IntegrityMismatch);
        }
        let body_len = u32::try_from(artifact.canonical_body.len()).map_err(|_| {
            ResponsesArtifactError::ArtifactTooLarge {
                maximum: MAX_RESPONSES_ARTIFACT_BYTES,
            }
        })?;
        let mut bytes = Vec::with_capacity(artifact.encoded_len);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&RESPONSES_ARTIFACT_CODEC_REVISION.to_be_bytes());
        bytes.extend_from_slice(&body_len.to_be_bytes());
        bytes.extend_from_slice(&artifact.canonical_body);
        bytes.extend_from_slice(&artifact.integrity);
        Ok(bytes)
    }

    /// Decodes structurally and exactly revalidates the original producer binding.
    pub fn decode(
        bytes: &[u8],
        expected: &ResponsesArtifactBinding,
        expected_production: &ResponsesArtifactProduction,
        now: Timestamp,
    ) -> Result<ResponsesArtifact, ResponsesArtifactError> {
        let artifact = Self::decode_inert(bytes)?;
        artifact.validate_for(expected, expected_production, now)?;
        Ok(artifact)
    }

    /// Decodes and integrity-checks inert reconstructive artifact bytes.
    ///
    /// Expiry is deliberately not consulted. The returned historical value
    /// grants no replay right until [`ResponsesArtifact::validate_for_reuse`].
    pub fn decode_inert(bytes: &[u8]) -> Result<ResponsesArtifact, ResponsesArtifactError> {
        if bytes.len() > MAX_RESPONSES_ARTIFACT_BYTES {
            return Err(ResponsesArtifactError::ArtifactTooLarge {
                maximum: MAX_RESPONSES_ARTIFACT_BYTES,
            });
        }
        let mut reader = Reader::new(bytes);
        if reader.take(MAGIC.len())? != MAGIC {
            return Err(ResponsesArtifactError::InvalidMagic);
        }
        let codec_revision = reader.u32()?;
        if codec_revision != RESPONSES_ARTIFACT_CODEC_REVISION {
            return Err(ResponsesArtifactError::UnsupportedCodecRevision {
                found: codec_revision,
            });
        }
        let body_len = reader.length(MAX_BODY_BYTES)?;
        let body = reader.take(body_len)?;
        let recorded_integrity = reader.array_32()?;
        reader.finish()?;
        if integrity(body)? != recorded_integrity {
            return Err(ResponsesArtifactError::IntegrityMismatch);
        }
        decode_body(body, recorded_integrity)
    }
}

fn decode_body(
    body: &[u8],
    recorded_integrity: [u8; 32],
) -> Result<ResponsesArtifact, ResponsesArtifactError> {
    let mut reader = Reader::new(body);
    let schema_revision = reader.u32()?;
    if schema_revision != RESPONSES_ARTIFACT_SCHEMA_REVISION {
        return Err(ResponsesArtifactError::UnsupportedSchemaRevision {
            found: schema_revision,
        });
    }
    let family = reader.u8()?;
    if family != 1 {
        return Err(ResponsesArtifactError::UnknownTag {
            field: "family",
            tag: family,
        });
    }
    let binding = decode_binding(&mut reader)?;
    let production = decode_production(&mut reader)?;
    let replay_contract = decode_replay_contract(&mut reader)?;
    let continuation = decode_continuation(&mut reader)?;
    let cache = decode_cache(&mut reader)?;
    let storage = decode_storage(&mut reader)?;
    let expires_at = decode_expiry(&mut reader)?;
    reader.finish()?;

    let artifact = ResponsesArtifact {
        binding,
        production,
        replay_contract,
        continuation,
        cache,
        storage,
        expires_at,
        integrity: recorded_integrity,
        encoded_len: envelope_len(body.len())?,
        replay_bytes: 0,
        canonical_body: Arc::from(body),
    };
    artifact.validate_structure()?;
    Ok(artifact)
}

fn decode_binding(
    reader: &mut Reader<'_>,
) -> Result<ResponsesArtifactBinding, ResponsesArtifactError> {
    let dialect = ProtocolId::new(reader.string(MAX_ID_BYTES)?)
        .map_err(|_| ResponsesArtifactError::InvalidIdentity { field: "dialect" })?;
    let route = RouteId::new(reader.string(MAX_ID_BYTES)?)
        .map_err(|_| ResponsesArtifactError::InvalidIdentity { field: "route" })?;
    let account = reader.digest(DigestDomain::ModelBinding)?;
    let model = ModelId::new(reader.string(MAX_ID_BYTES)?)
        .map_err(|_| ResponsesArtifactError::InvalidIdentity { field: "model" })?;
    let model_family = reader
        .optional_string(MAX_ID_BYTES, "model family")?
        .map(ModelFamilyId::new)
        .transpose()
        .map_err(|_| ResponsesArtifactError::InvalidIdentity {
            field: "model family",
        })?;
    let adapter = ModelAdapterProfileId::new(reader.string(MAX_ID_BYTES)?)
        .map_err(|_| ResponsesArtifactError::InvalidIdentity { field: "adapter" })?;
    let semantic_model = reader.digest(DigestDomain::ModelBinding)?;
    let revisions = ResponsesSemanticRevisions::new(reader.u32()?, reader.u32()?, reader.u32()?)?;
    let cache_affinity = reader
        .optional("cache affinity")?
        .then(|| reader.digest(DigestDomain::CacheAffinity))
        .transpose()?;
    let stable_prefix = reader.digest(DigestDomain::StableRequestPrefix)?;
    let projection = reader.digest(DigestDomain::HistoryProjection)?;
    let item_prefix = reader.digest(DigestDomain::CanonicalItemPrefix)?;
    let tool_plan = reader.digest(DigestDomain::ResolvedHarnessPlan)?;
    let request = reader.digest(DigestDomain::RequestPlan)?;
    let model_input = reader.digest(DigestDomain::RequestPlan)?;
    let validated_plan = reader.digest(DigestDomain::RequestPlan)?;
    let run = reader.uuid("run")?;
    let root = decode_operation_root(reader)?;
    let attempt = reader.uuid("attempt")?;
    ResponsesArtifactBinding::from_parts(
        dialect,
        route,
        account,
        model,
        model_family,
        adapter,
        semantic_model,
        revisions,
        cache_affinity,
        stable_prefix,
        projection,
        item_prefix,
        tool_plan,
        request,
        model_input,
        validated_plan,
        run,
        root,
        attempt,
    )
}

fn encode_operation_root(
    writer: &mut Writer,
    root: ModelOperationRoot,
) -> Result<(), ResponsesArtifactError> {
    match root {
        ModelOperationRoot::Turn(turn_id) => {
            writer.u8(1)?;
            writer.string(&turn_id.to_string(), UUID_TEXT_BYTES)
        }
        ModelOperationRoot::SessionWorkflow(workflow_id) => {
            writer.u8(2)?;
            writer.extend(workflow_id.as_bytes())
        }
    }
}

fn decode_operation_root(
    reader: &mut Reader<'_>,
) -> Result<ModelOperationRoot, ResponsesArtifactError> {
    match reader.u8()? {
        1 => reader.uuid("turn root").map(ModelOperationRoot::Turn),
        2 => SessionWorkflowRootId::new(reader.array_32()?)
            .map(ModelOperationRoot::SessionWorkflow)
            .map_err(|_| ResponsesArtifactError::InvalidIdentity {
                field: "session workflow root",
            }),
        tag => Err(ResponsesArtifactError::UnknownTag {
            field: "operation root",
            tag,
        }),
    }
}

fn decode_production(
    reader: &mut Reader<'_>,
) -> Result<ResponsesArtifactProduction, ResponsesArtifactError> {
    ResponsesArtifactProduction::from_parts(
        reader.digest(DigestDomain::RequestPlan)?,
        reader.digest(DigestDomain::CanonicalItemPrefix)?,
        reader.digest(DigestDomain::RequestPlan)?,
        reader.digest(DigestDomain::ProviderOperation)?,
        reader.digest(DigestDomain::ProviderOperation)?,
        reader.u32()?,
        reader.u32()?,
        reader.u32()?,
        reader.digest(DigestDomain::CanonicalItemPrefix)?,
    )
}

fn decode_replay_kind(
    reader: &mut Reader<'_>,
) -> Result<ResponsesArtifactReplayKind, ResponsesArtifactError> {
    let kind = ReplayKindId::new(reader.string(MAX_ID_BYTES)?).map_err(|_| {
        ResponsesArtifactError::InvalidIdentity {
            field: "replay kind",
        }
    })?;
    ResponsesArtifactReplayKind::new(kind, reader.u32()?)
}

fn decode_replay_contract(
    reader: &mut Reader<'_>,
) -> Result<ResponsesReplayContract, ResponsesArtifactError> {
    let mut contract = ResponsesReplayContract::new(decode_replay_kind(reader)?);
    if reader.optional("opaque output replay kind")? {
        contract = contract.with_opaque_output(decode_replay_kind(reader)?)?;
    }
    if reader.optional("encrypted reasoning replay kind")? {
        contract = contract.with_encrypted_reasoning(decode_replay_kind(reader)?)?;
    }
    Ok(contract)
}

fn decode_continuation(
    reader: &mut Reader<'_>,
) -> Result<Option<ResponsesContinuation>, ResponsesArtifactError> {
    if reader.optional("continuation")? {
        ResponsesContinuation::new(
            reader.string(MAX_OPAQUE_ID_BYTES)?,
            reader.u32()?,
            reader.digest(DigestDomain::CanonicalItemPrefix)?,
            reader.digest(DigestDomain::ProviderOperation)?,
        )
        .map(Some)
    } else {
        Ok(None)
    }
}

fn decode_cache(
    reader: &mut Reader<'_>,
) -> Result<Option<ResponsesCacheFact>, ResponsesArtifactError> {
    if reader.optional("cache")? {
        ResponsesCacheFact::new(
            reader.string(MAX_OPAQUE_ID_BYTES)?,
            reader.digest(DigestDomain::StableRequestPrefix)?,
            reader.u32()?,
            reader.digest(DigestDomain::CanonicalItemPrefix)?,
        )
        .map(Some)
    } else {
        Ok(None)
    }
}

fn decode_storage(reader: &mut Reader<'_>) -> Result<ResponsesStorageMode, ResponsesArtifactError> {
    match reader.u8()? {
        1 => Ok(ResponsesStorageMode::Stateless),
        2 => Ok(ResponsesStorageMode::ProviderStored),
        tag => Err(ResponsesArtifactError::UnknownTag {
            field: "storage mode",
            tag,
        }),
    }
}

fn decode_expiry(reader: &mut Reader<'_>) -> Result<Option<Timestamp>, ResponsesArtifactError> {
    if reader.optional("expiry")? {
        Timestamp::new(reader.u64()?, reader.u32()?)
            .map(Some)
            .map_err(|_| ResponsesArtifactError::InvalidTimestamp)
    } else {
        Ok(None)
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the canonical codec intentionally writes one closed schema in field order"
)]
pub(super) fn encode_body(
    artifact: &ResponsesArtifact,
) -> Result<EncodedBody, ResponsesArtifactError> {
    let mut out = Writer::new();
    out.u32(RESPONSES_ARTIFACT_SCHEMA_REVISION)?;
    out.u8(1)?;
    out.string(artifact.binding.dialect.as_str(), MAX_ID_BYTES)?;
    out.string(artifact.binding.route.as_str(), MAX_ID_BYTES)?;
    out.digest(
        artifact.binding.account,
        DigestDomain::ModelBinding,
        "account",
    )?;
    out.string(artifact.binding.model.as_str(), MAX_ID_BYTES)?;
    out.optional_string(
        artifact
            .binding
            .model_family
            .as_ref()
            .map(ModelFamilyId::as_str),
        MAX_ID_BYTES,
    )?;
    out.string(artifact.binding.adapter.as_str(), MAX_ID_BYTES)?;
    out.digest(
        artifact.binding.semantic_model,
        DigestDomain::ModelBinding,
        "resolved model",
    )?;
    for revision in [
        artifact.binding.revisions.family_kernel,
        artifact.binding.revisions.dialect,
        artifact.binding.revisions.adapter,
    ] {
        out.u32(revision)?;
    }
    out.optional(artifact.binding.cache_affinity.is_some())?;
    if let Some(cache_affinity) = artifact.binding.cache_affinity {
        out.digest(
            cache_affinity,
            DigestDomain::CacheAffinity,
            "cache affinity",
        )?;
    }
    out.digest(
        artifact.binding.stable_prefix,
        DigestDomain::StableRequestPrefix,
        "stable prefix",
    )?;
    out.digest(
        artifact.binding.projection,
        DigestDomain::HistoryProjection,
        "projection",
    )?;
    out.digest(
        artifact.binding.item_prefix,
        DigestDomain::CanonicalItemPrefix,
        "canonical item prefix",
    )?;
    out.digest(
        artifact.binding.tool_plan,
        DigestDomain::ResolvedHarnessPlan,
        "tool plan",
    )?;
    out.digest(
        artifact.binding.request,
        DigestDomain::RequestPlan,
        "request",
    )?;
    out.digest(
        artifact.binding.model_input,
        DigestDomain::RequestPlan,
        "model input",
    )?;
    out.digest(
        artifact.binding.validated_plan,
        DigestDomain::RequestPlan,
        "validated plan",
    )?;
    out.string(&artifact.binding.run.to_string(), UUID_TEXT_BYTES)?;
    encode_operation_root(&mut out, artifact.binding.root)?;
    out.string(&artifact.binding.attempt.to_string(), UUID_TEXT_BYTES)?;

    out.digest(
        artifact.production.producer_request,
        DigestDomain::RequestPlan,
        "producer request",
    )?;
    out.digest(
        artifact.production.producing_projection,
        DigestDomain::CanonicalItemPrefix,
        "producing item prefix",
    )?;
    out.digest(
        artifact.production.validated_plan,
        DigestDomain::RequestPlan,
        "validated plan",
    )?;
    out.digest(
        artifact.production.physical_request,
        DigestDomain::ProviderOperation,
        "physical request",
    )?;
    out.digest(
        artifact.production.terminal_response,
        DigestDomain::ProviderOperation,
        "terminal response",
    )?;
    out.u32(artifact.production.producing_projection_items)?;
    out.u32(artifact.production.output_items)?;
    out.u32(artifact.production.covered_projection_items)?;
    out.digest(
        artifact.production.covered_projection,
        DigestDomain::CanonicalItemPrefix,
        "covered item prefix",
    )?;

    encode_replay_kind(&mut out, &artifact.replay_contract.output)?;
    out.optional(artifact.replay_contract.opaque_output.is_some())?;
    if let Some(kind) = &artifact.replay_contract.opaque_output {
        encode_replay_kind(&mut out, kind)?;
    }
    out.optional(artifact.replay_contract.encrypted_reasoning.is_some())?;
    if let Some(kind) = &artifact.replay_contract.encrypted_reasoning {
        encode_replay_kind(&mut out, kind)?;
    }

    out.optional(artifact.continuation.is_some())?;
    if let Some(continuation) = &artifact.continuation {
        out.string(&continuation.response_id, MAX_OPAQUE_ID_BYTES)?;
        out.u32(continuation.covered_projection_items)?;
        out.digest(
            continuation.covered_projection,
            DigestDomain::CanonicalItemPrefix,
            "continuation covered item prefix",
        )?;
        out.digest(
            continuation.terminal_response,
            DigestDomain::ProviderOperation,
            "continuation terminal response",
        )?;
    }
    out.optional(artifact.cache.is_some())?;
    if let Some(cache) = &artifact.cache {
        out.string(&cache.key, MAX_OPAQUE_ID_BYTES)?;
        out.digest(
            cache.stable_prefix,
            DigestDomain::StableRequestPrefix,
            "cache stable prefix",
        )?;
        out.u32(cache.covered_projection_items)?;
        out.digest(
            cache.covered_projection,
            DigestDomain::CanonicalItemPrefix,
            "cache covered item prefix",
        )?;
    }
    out.u8(match artifact.storage {
        ResponsesStorageMode::Stateless => 1,
        ResponsesStorageMode::ProviderStored => 2,
    })?;
    out.optional(artifact.expires_at.is_some())?;
    if let Some(expiry) = artifact.expires_at {
        out.u64(expiry.seconds())?;
        out.u32(expiry.nanoseconds())?;
    }
    Ok(EncodedBody {
        body: out.finish(),
        replay_bytes: 0,
    })
}

fn encode_replay_kind(
    out: &mut Writer,
    kind: &ResponsesArtifactReplayKind,
) -> Result<(), ResponsesArtifactError> {
    out.string(kind.kind.as_str(), MAX_ID_BYTES)?;
    out.u32(kind.version)
}

pub(super) fn integrity(body: &[u8]) -> Result<[u8; 32], ResponsesArtifactError> {
    let codec = RESPONSES_ARTIFACT_CODEC_REVISION.to_be_bytes();
    let schema = RESPONSES_ARTIFACT_SCHEMA_REVISION.to_be_bytes();
    let body_len = u64::try_from(body.len())
        .map_err(|_| ResponsesArtifactError::IntegrityConstruction)?
        .to_be_bytes();
    Digest::sha256(
        DigestDomain::Blob,
        RESPONSES_ARTIFACT_CODEC_REVISION,
        &[INTEGRITY_DOMAIN, &codec, &schema, &body_len, body],
    )
    .map(|digest| *digest.bytes())
    .map_err(|_| ResponsesArtifactError::IntegrityConstruction)
}

pub(super) fn envelope_len(body_len: usize) -> Result<usize, ResponsesArtifactError> {
    let length = ENVELOPE_OVERHEAD.checked_add(body_len).ok_or(
        ResponsesArtifactError::ArtifactTooLarge {
            maximum: MAX_RESPONSES_ARTIFACT_BYTES,
        },
    )?;
    if length > MAX_RESPONSES_ARTIFACT_BYTES {
        return Err(ResponsesArtifactError::ArtifactTooLarge {
            maximum: MAX_RESPONSES_ARTIFACT_BYTES,
        });
    }
    Ok(length)
}

struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(2_048),
        }
    }

    fn reserve(&self, additional: usize) -> Result<(), ResponsesArtifactError> {
        if self
            .bytes
            .len()
            .checked_add(additional)
            .is_none_or(|length| length > MAX_BODY_BYTES)
        {
            return Err(ResponsesArtifactError::ArtifactTooLarge {
                maximum: MAX_RESPONSES_ARTIFACT_BYTES,
            });
        }
        Ok(())
    }

    fn extend(&mut self, bytes: &[u8]) -> Result<(), ResponsesArtifactError> {
        self.reserve(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn u8(&mut self, value: u8) -> Result<(), ResponsesArtifactError> {
        self.reserve(1)?;
        self.bytes.push(value);
        Ok(())
    }

    fn u32(&mut self, value: u32) -> Result<(), ResponsesArtifactError> {
        self.extend(&value.to_be_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<(), ResponsesArtifactError> {
        self.extend(&value.to_be_bytes())
    }

    fn length(&mut self, length: usize) -> Result<(), ResponsesArtifactError> {
        let length = u32::try_from(length).map_err(|_| ResponsesArtifactError::Malformed)?;
        self.u32(length)
    }

    fn bytes(&mut self, value: &[u8], maximum: usize) -> Result<(), ResponsesArtifactError> {
        if value.len() > maximum {
            return Err(ResponsesArtifactError::Malformed);
        }
        self.length(value.len())?;
        self.extend(value)
    }

    fn string(&mut self, value: &str, maximum: usize) -> Result<(), ResponsesArtifactError> {
        self.bytes(value.as_bytes(), maximum)
    }

    fn optional(&mut self, present: bool) -> Result<(), ResponsesArtifactError> {
        self.u8(u8::from(present))
    }

    fn optional_string(
        &mut self,
        value: Option<&str>,
        maximum: usize,
    ) -> Result<(), ResponsesArtifactError> {
        self.optional(value.is_some())?;
        if let Some(value) = value {
            self.string(value, maximum)?;
        }
        Ok(())
    }

    fn digest(
        &mut self,
        digest: Digest,
        domain: DigestDomain,
        field: &'static str,
    ) -> Result<(), ResponsesArtifactError> {
        validate_digest(digest, domain, field)?;
        self.u32(digest.version())?;
        self.extend(digest.bytes())
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ResponsesArtifactError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(ResponsesArtifactError::Malformed)?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or(ResponsesArtifactError::Malformed)?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, ResponsesArtifactError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(ResponsesArtifactError::Malformed)
    }

    fn u32(&mut self) -> Result<u32, ResponsesArtifactError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| ResponsesArtifactError::Malformed)?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, ResponsesArtifactError> {
        let bytes: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| ResponsesArtifactError::Malformed)?;
        Ok(u64::from_be_bytes(bytes))
    }

    fn array_32(&mut self) -> Result<[u8; 32], ResponsesArtifactError> {
        self.take(32)?
            .try_into()
            .map_err(|_| ResponsesArtifactError::Malformed)
    }

    fn length(&mut self, maximum: usize) -> Result<usize, ResponsesArtifactError> {
        let length = usize::try_from(self.u32()?).map_err(|_| ResponsesArtifactError::Malformed)?;
        if length > maximum {
            return Err(ResponsesArtifactError::Malformed);
        }
        Ok(length)
    }

    fn bytes(&mut self, maximum: usize) -> Result<&'a [u8], ResponsesArtifactError> {
        let length = self.length(maximum)?;
        self.take(length)
    }

    fn string(&mut self, maximum: usize) -> Result<String, ResponsesArtifactError> {
        let bytes = self.bytes(maximum)?;
        let value = std::str::from_utf8(bytes).map_err(|_| ResponsesArtifactError::Malformed)?;
        Ok(value.to_owned())
    }

    fn optional(&mut self, field: &'static str) -> Result<bool, ResponsesArtifactError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            tag => Err(ResponsesArtifactError::UnknownTag { field, tag }),
        }
    }

    fn optional_string(
        &mut self,
        maximum: usize,
        field: &'static str,
    ) -> Result<Option<String>, ResponsesArtifactError> {
        if self.optional(field)? {
            self.string(maximum).map(Some)
        } else {
            Ok(None)
        }
    }

    fn digest(&mut self, domain: DigestDomain) -> Result<Digest, ResponsesArtifactError> {
        let version = self.u32()?;
        let bytes = self.array_32()?;
        Digest::from_sha256_bytes(domain, version, bytes)
            .map_err(|_| ResponsesArtifactError::InvalidDigest { field: "encoded" })
    }

    fn uuid<T>(&mut self, field: &'static str) -> Result<T, ResponsesArtifactError>
    where
        T: std::str::FromStr + fmt::Display,
    {
        let encoded = self.string(UUID_TEXT_BYTES)?;
        let value = encoded
            .parse::<T>()
            .map_err(|_| ResponsesArtifactError::InvalidIdentity { field })?;
        if value.to_string() != encoded {
            return Err(ResponsesArtifactError::InvalidIdentity { field });
        }
        Ok(value)
    }

    fn finish(self) -> Result<(), ResponsesArtifactError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(ResponsesArtifactError::TrailingBytes)
        }
    }
}
