//! Replay context, prefix bindings and exact output-item replay for request lowering.

use super::{
    ActiveFamilyArtifact, BTreeSet, ContentBlock, ConversationItem, Digest, DigestDomain, HashSet,
    MAX_REPLAY_KIND_BYTES, MAX_RESPONSE_ID_BYTES, ModelFamilyId, ModelRequest,
    RESPONSES_ARTIFACT_CODEC_REVISION, RESPONSES_ARTIFACT_SCHEMA_REVISION, RawValue,
    ReplayEnvelope, ReplayKindId, ResolvedModelDescriptor, ResponsesArtifact,
    ResponsesArtifactBinding, ResponsesArtifactCodec, ResponsesHostDecodeError,
    ResponsesReplayError, ResponsesReplayMode, ResponsesReplayPolicy, ResponsesReplayPurpose,
    ResponsesRequestConfig, ResponsesRequestError, ResponsesRequestPolicy,
    ResponsesSemanticRevisions, ResponsesStorageMode, ResponsesStoragePolicy, Timestamp,
    ValidatedRequestPlan, Value, fmt, parse_output_item, validate_output_semantic_pair,
};

/// One versioned replay-envelope kind admitted by the selected dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesReplayKind<'a> {
    name: &'a str,
    version: u32,
}

impl<'a> ResponsesReplayKind<'a> {
    /// Creates a borrowed replay-kind pin. Validation occurs at lowering.
    #[must_use]
    pub const fn new(name: &'a str, version: u32) -> Self {
        Self { name, version }
    }

    /// Returns the exact replay kind.
    #[must_use]
    pub const fn name(self) -> &'a str {
        self.name
    }

    /// Returns the exact replay payload revision.
    #[must_use]
    pub const fn version(self) -> u32 {
        self.version
    }
}

/// Exact current-operation authority used to derive replay bindings.
///
/// Construction proves that the admitted target binding names this request's
/// stable prefix and complete history projection. The identity additionally
/// binds route, account, model/profile, tool plan, semantic revisions, and
/// storage policy. It contains no credential material.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResponsesRequestAuthority {
    binding: ResponsesArtifactBinding,
    storage: ResponsesStoragePolicy,
    request: Digest,
    plan: Digest,
    identity: Digest,
}

impl ResponsesRequestAuthority {
    /// Constructs authority including the runtime-owned branch cache affinity.
    ///
    /// # Errors
    ///
    /// Rejects incomplete admitted facts, a foreign cache-affinity digest, or
    /// an authority whose exact identity cannot be constructed.
    pub fn from_admitted_operation(
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        request: &ModelRequest,
        account: Digest,
        revisions: ResponsesSemanticRevisions,
        cache_affinity: Option<Digest>,
        storage: ResponsesStoragePolicy,
    ) -> Result<Self, ResponsesReplayError> {
        let binding = ResponsesArtifactBinding::from_admitted_operation(
            descriptor,
            plan,
            request,
            account,
            revisions,
            cache_affinity,
        )
        .map_err(|error| match error {
            plakous_responses::ResponsesArtifactError::MissingHarnessPlanIdentity => {
                ResponsesReplayError::IncompleteAuthority
            }
            plakous_responses::ResponsesArtifactError::AdmissionMismatch => {
                ResponsesReplayError::AuthorityMismatch
            }
            _ => ResponsesReplayError::AuthorityConstruction,
        })?;
        let request_identity = request.identity();
        let plan_identity = plan.digest();
        let identity =
            operation_authority_digest(&binding, storage, request_identity, plan_identity)?;
        Ok(Self {
            binding,
            storage,
            request: request_identity,
            plan: plan_identity,
            identity,
        })
    }

    /// Returns the exact producing binding for an artifact created by this operation.
    #[must_use]
    pub const fn artifact_binding(&self) -> &ResponsesArtifactBinding {
        &self.binding
    }

    /// Returns the runtime-owned branch cache-affinity identity, when present.
    #[must_use]
    pub const fn cache_affinity(&self) -> Option<Digest> {
        self.binding.cache_affinity()
    }

    /// Returns the derived current-operation authority identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// One recomputable canonical-prefix integrity binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesPrefixBinding {
    prefix_items: usize,
    prefix: Digest,
    binding: Digest,
}

impl ResponsesPrefixBinding {
    /// Derives one binding from the immutable request and current authority.
    ///
    /// # Errors
    ///
    /// Returns a bounded request error when authority is stale, the prefix is
    /// outside the request projection, or canonical serialization fails.
    pub fn for_request(
        request: &ModelRequest,
        prefix_items: usize,
        authority: &ResponsesRequestAuthority,
    ) -> Result<Self, ResponsesRequestError> {
        validate_authority(request, authority)?;
        let prefix = request
            .input()
            .canonical_item_prefix_identity(prefix_items)
            .map_err(|_| ResponsesReplayError::MissingPrefixBinding)?
            .identity();
        let binding = prefix_binding_digest(authority.identity, prefix)?;
        Ok(Self {
            prefix_items,
            prefix,
            binding,
        })
    }

    /// Returns the canonical item count covered by the binding.
    #[must_use]
    pub const fn prefix_items(self) -> usize {
        self.prefix_items
    }

    /// Returns the exact prefix binding.
    #[must_use]
    pub const fn binding(self) -> Digest {
        self.binding
    }
}

/// Exact coordinates for one raw official output-item replay envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResponsesOutputReplayCoordinate {
    envelope: Digest,
    binding: Digest,
    group: Digest,
    group_start: usize,
    output_index: u32,
    output_count: u32,
    canonical_position: usize,
}

/// Owned, bounded bridge from one validated family artifact to a next request.
///
/// Raw provider payloads remain byte-exact in the artifact. This bridge carries
/// the separate authority and coordinate proof needed to consume them.
#[derive(Debug)]
pub struct ResponsesArtifactReplayBridge {
    authority: ResponsesRequestAuthority,
    output_item: ReplayKindId,
    output_item_version: u32,
    opaque_output: Option<(ReplayKindId, u32)>,
    previous_response: Option<(ReplayKindId, u32)>,
    prefix_bindings: Vec<ResponsesPrefixBinding>,
    output_coordinates: Vec<ResponsesOutputReplayCoordinate>,
    continuation: Option<ResponsesProviderContinuation>,
    hosted_programs: crate::ResponsesHostedProgramContext,
    expires_at: Option<Timestamp>,
    now: Timestamp,
}

#[derive(Debug)]
pub(super) struct ResponsesProviderContinuation {
    response_id: String,
    prefix_items: usize,
}

/// Borrowed replay-validation data for one request lowering.
///
/// Prefix bindings must be strictly increasing by `prefix_items`. Keeping the
/// bindings outside this host lets the reconstructive family artifact own
/// prefix identity without granting it request-lowering authority.
#[derive(Clone, Copy)]
pub struct ResponsesReplayContext<'a> {
    authority: &'a ResponsesRequestAuthority,
    output_item: ResponsesReplayKind<'a>,
    opaque_output: Option<ResponsesReplayKind<'a>>,
    previous_response: Option<ResponsesReplayKind<'a>>,
    prefix_bindings: &'a [ResponsesPrefixBinding],
    output_coordinates: &'a [ResponsesOutputReplayCoordinate],
    continuation: Option<&'a ResponsesProviderContinuation>,
    expires_at: Option<Timestamp>,
    now: Timestamp,
}

impl<'a> ResponsesReplayContext<'a> {
    /// Creates borrowed replay data. Structural validation occurs at lowering.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "the context borrows independent closed authority tables without allocating"
    )]
    const fn new(
        authority: &'a ResponsesRequestAuthority,
        output_item: ResponsesReplayKind<'a>,
        previous_response: Option<ResponsesReplayKind<'a>>,
        prefix_bindings: &'a [ResponsesPrefixBinding],
        output_coordinates: &'a [ResponsesOutputReplayCoordinate],
        continuation: Option<&'a ResponsesProviderContinuation>,
        expires_at: Option<Timestamp>,
        now: Timestamp,
    ) -> Self {
        Self {
            authority,
            output_item,
            opaque_output: None,
            previous_response,
            prefix_bindings,
            output_coordinates,
            continuation,
            expires_at,
            now,
        }
    }

    const fn with_optional_opaque_output(mut self, kind: Option<ResponsesReplayKind<'a>>) -> Self {
        self.opaque_output = kind;
        self
    }

    fn output_for(
        self,
        canonical_position: usize,
        envelope: Digest,
    ) -> Option<ResponsesOutputReplayCoordinate> {
        self.output_coordinates
            .binary_search_by_key(&canonical_position, |coordinate| {
                coordinate.canonical_position
            })
            .ok()
            .map(|index| self.output_coordinates[index])
            .filter(|coordinate| coordinate.envelope == envelope)
    }
}

impl fmt::Debug for ResponsesReplayContext<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResponsesReplayContext")
            .field("authority", &self.authority.identity)
            .field("output_item", &self.output_item)
            .field("previous_response", &self.previous_response)
            .field("prefix_bindings", &"<redacted exact bindings>")
            .field("output_coordinates", &self.output_coordinates.len())
            .field("expires_at", &self.expires_at)
            .field("now", &self.now)
            .finish()
    }
}

impl ResponsesArtifactReplayBridge {
    /// Admits the sole active neutral Responses artifact for this exact request.
    ///
    /// The neutral wrapper must name the exact release-owned Responses
    /// artifact schema and codec plus the decoded producer binding. Its bytes
    /// remain inert until family-local target, storage, expiry, and
    /// canonical-prefix validation succeeds.
    ///
    /// # Errors
    ///
    /// Returns a bounded request error for an incompatible neutral wrapper,
    /// invalid artifact bytes, target/storage drift, expiry, canonical-prefix
    /// mismatch, or replay-kind drift. Only this validated active artifact can
    /// authorize continuation.
    #[allow(
        clippy::too_many_arguments,
        reason = "all arguments are independent closed replay authorities"
    )]
    pub fn from_active_artifact(
        active: &ActiveFamilyArtifact,
        request: &ModelRequest,
        authority: ResponsesRequestAuthority,
        output_item: ResponsesReplayKind<'_>,
        opaque_output: Option<ResponsesReplayKind<'_>>,
        previous_response: Option<ResponsesReplayKind<'_>>,
        now: Timestamp,
    ) -> Result<Self, ResponsesRequestError> {
        validate_authority(request, &authority)?;
        if active.family() != "responses"
            || active.schema_revision() != RESPONSES_ARTIFACT_SCHEMA_REVISION
            || active.codec_revision() != RESPONSES_ARTIFACT_CODEC_REVISION
        {
            return Err(ResponsesReplayError::InvalidArtifact.into());
        }
        let artifact = ResponsesArtifactCodec::decode_inert(active.bytes())
            .map_err(|_| ResponsesReplayError::InvalidArtifact)?;
        if active.binding() != artifact.binding().identity() {
            return Err(ResponsesReplayError::AuthorityMismatch.into());
        }
        Self::from_latest_artifact(
            &artifact,
            request,
            authority,
            output_item,
            opaque_output,
            previous_response,
            now,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "all arguments are independent closed replay authorities"
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "one linear host step whose ordering is the invariant"
    )]
    fn from_latest_artifact(
        artifact: &ResponsesArtifact,
        request: &ModelRequest,
        authority: ResponsesRequestAuthority,
        output_item: ResponsesReplayKind<'_>,
        opaque_output: Option<ResponsesReplayKind<'_>>,
        previous_response: Option<ResponsesReplayKind<'_>>,
        now: Timestamp,
    ) -> Result<Self, ResponsesRequestError> {
        if artifact.is_expired(now) {
            return Err(ResponsesReplayError::ArtifactExpired.into());
        }
        artifact
            .validate_active(now)
            .map_err(|_| ResponsesReplayError::InvalidArtifact)?;
        if let Some(axis) = artifact_target_mismatch(
            authority.artifact_binding(),
            artifact.binding(),
            authority.storage,
            artifact.storage(),
        ) {
            return Err(ResponsesReplayError::ArtifactTargetMismatch { axis }.into());
        }
        if authority.artifact_binding().stable_prefix() != artifact.binding().stable_prefix() {
            return Err(ResponsesReplayError::ArtifactStablePrefixMismatch.into());
        }
        validate_replay_kind(output_item)?;
        if let Some(kind) = opaque_output {
            validate_replay_kind(kind)?;
        }
        if let Some(kind) = previous_response {
            validate_replay_kind(kind)?;
        }
        validate_artifact_replay_kind(
            artifact,
            ResponsesReplayPurpose::OutputItem,
            Some(output_item),
        )?;
        validate_artifact_replay_kind(
            artifact,
            ResponsesReplayPurpose::OpaqueOutputItem,
            opaque_output,
        )?;
        let reuse = artifact
            .validate_for_reuse(authority.artifact_binding(), request, now)
            .map_err(|error| match error {
                plakous_responses::ResponsesArtifactError::BindingMismatch => {
                    ResponsesReplayError::ArtifactBindingMismatch
                }
                plakous_responses::ResponsesArtifactError::CanonicalCoordinateMismatch
                | plakous_responses::ResponsesArtifactError::UnauthorizedReplayTail { .. } => {
                    ResponsesReplayError::ArtifactCanonicalMismatch
                }
                _ => ResponsesReplayError::InvalidArtifact,
            })?;

        let producing_prefix = usize::try_from(artifact.production().producing_projection_items())
            .map_err(|_| ResponsesReplayError::InvalidOutputCoordinates)?;
        let latest_output_count = usize::try_from(artifact.production().output_items())
            .map_err(|_| ResponsesReplayError::InvalidOutputCoordinates)?;
        let covered_prefix = producing_prefix
            .checked_add(latest_output_count)
            .ok_or(ResponsesReplayError::InvalidOutputCoordinates)?;
        if usize::try_from(artifact.production().covered_projection_items()) != Ok(covered_prefix)
            || covered_prefix > request.input().items().len()
        {
            return Err(ResponsesReplayError::InvalidOutputCoordinates.into());
        }
        let hosted_programs =
            validated_hosted_programs(request, covered_prefix, output_item, &authority)?;

        let prefix = artifact.production().covered_projection();
        let prefix_bindings = vec![ResponsesPrefixBinding {
            prefix_items: covered_prefix,
            prefix,
            binding: prefix_binding_digest(authority.identity, prefix)?,
        }];
        let output_binding = prefix_bindings[0].binding;
        let replay_items = reuse
            .replay()
            .iter()
            .copied()
            .filter(|item| is_output_replay_purpose(item.purpose()))
            .collect::<Vec<_>>();
        let mut segment_starts = BTreeSet::new();
        let mut segment_position = 0_usize;
        for segment in request
            .input()
            .history()
            .segments()
            .iter()
            .chain(request.input().working().segments())
        {
            segment_starts.insert(segment_position);
            segment_position = segment_position
                .checked_add(segment.items().len())
                .ok_or(ResponsesReplayError::InvalidOutputCoordinates)?;
        }
        for pair in replay_items.windows(2) {
            let previous = pair[0].canonical_position();
            let current = pair[1].canonical_position();
            if current != previous.saturating_add(1) && !segment_starts.contains(&current) {
                return Err(ResponsesReplayError::InvalidOutputCoordinates.into());
            }
        }
        if producing_prefix > 0
            && replay_items
                .iter()
                .any(|item| item.canonical_position() == producing_prefix.saturating_sub(1))
            && !segment_starts.contains(&producing_prefix)
        {
            return Err(ResponsesReplayError::InvalidOutputCoordinates.into());
        }
        let mut output_coordinates = Vec::with_capacity(replay_items.len());
        // Authorized positions ascend, so one cursor avoids repeated segment walks.
        let mut canonical_items = request.input().items().iter().enumerate();
        let mut cursor = 0_usize;
        while cursor < replay_items.len() {
            let group_start = replay_items[cursor].canonical_position();
            let mut group_end = cursor.saturating_add(1);
            while group_end < replay_items.len()
                && replay_items[group_end].canonical_position()
                    == replay_items[group_end - 1]
                        .canonical_position()
                        .saturating_add(1)
                && !segment_starts.contains(&replay_items[group_end].canonical_position())
            {
                group_end = group_end.saturating_add(1);
            }
            let output_count = u32::try_from(group_end.saturating_sub(cursor))
                .map_err(|_| ResponsesReplayError::InvalidOutputCoordinates)?;
            let group = canonical_replay_group(artifact, group_start, output_count)?;
            for (relative, replay_item) in replay_items[cursor..group_end].iter().enumerate() {
                let envelope = replay_item.envelope();
                let replay_kind =
                    ResponsesReplayKind::new(envelope.kind().as_str(), envelope.version());
                let expected_kind = match replay_item.purpose() {
                    ResponsesReplayPurpose::OutputItem => Some(output_item),
                    ResponsesReplayPurpose::OpaqueOutputItem => opaque_output,
                    ResponsesReplayPurpose::EncryptedReasoning => None,
                };
                if expected_kind != Some(replay_kind)
                    || !envelope.is_compatible(
                        authority.binding.dialect(),
                        authority.binding.model_family(),
                        authority.binding.model(),
                    )
                {
                    return Err(ResponsesReplayError::IncompatibleEnvelope.into());
                }
                let output_index = u32::try_from(relative)
                    .map_err(|_| ResponsesReplayError::InvalidOutputCoordinates)?;
                let canonical_position = replay_item.canonical_position();
                let (_, canonical_item) = canonical_items
                    .by_ref()
                    .find(|(position, _)| *position == canonical_position)
                    .ok_or(ResponsesReplayError::MissingOutputCoordinate)?;
                let canonical_envelope = exact_replay_for_kind(Some(canonical_item), replay_kind)?
                    .ok_or(ResponsesReplayError::MissingOutputCoordinate)?;
                if canonical_envelope.digest() != envelope.digest() {
                    return Err(ResponsesReplayError::MissingOutputCoordinate.into());
                }
                // Compatible historical envelopes in the bound prefix may still
                // be replayed by a latest stateless artifact, but they were
                // produced under the tool-plan that sealed them. After a
                // composition rebind the current catalog must not semantic-pair
                // those names (Direct `read_file` vs js-ptc `exec_js`/`wait_js`).
                // Pair only this response's output window.
                if canonical_position >= producing_prefix {
                    let native: Value = serde_json::from_str(envelope.payload().get())
                        .map_err(|_| ResponsesReplayError::MalformedEnvelope)?;
                    let raw = parse_output_item::<ResponsesHostDecodeError>(&native)
                        .map_err(|_| ResponsesReplayError::MalformedEnvelope)?;
                    validate_output_semantic_pair(&raw, envelope, canonical_item, request)
                        .map_err(|_| ResponsesReplayError::SemanticPairMismatch)?;
                }
                output_coordinates.push(ResponsesOutputReplayCoordinate {
                    envelope: envelope.digest(),
                    binding: output_binding,
                    group,
                    group_start,
                    output_index,
                    output_count,
                    canonical_position,
                });
            }
            cursor = group_end;
        }

        let continuation = match (reuse.continuation(), previous_response) {
            (Some(continuation), Some(_)) => Some(ResponsesProviderContinuation {
                response_id: continuation.response_id().to_owned(),
                prefix_items: covered_prefix,
            }),
            (Some(_), None) => return Err(ResponsesReplayError::IncompatibleEnvelope.into()),
            (None, _) => None,
        };
        Ok(Self {
            authority,
            output_item: ReplayKindId::new(output_item.name)
                .map_err(|_| ResponsesReplayError::InvalidKind)?,
            output_item_version: output_item.version,
            opaque_output: opaque_output
                .map(|kind| {
                    ReplayKindId::new(kind.name)
                        .map(|name| (name, kind.version))
                        .map_err(|_| ResponsesReplayError::InvalidKind)
                })
                .transpose()?,
            previous_response: previous_response
                .map(|kind| {
                    ReplayKindId::new(kind.name)
                        .map(|name| (name, kind.version))
                        .map_err(|_| ResponsesReplayError::InvalidKind)
                })
                .transpose()?,
            prefix_bindings,
            output_coordinates,
            continuation,
            hosted_programs,
            expires_at: artifact.expires_at(),
            now,
        })
    }

    /// Returns the exact current-operation authority.
    #[must_use]
    pub const fn authority(&self) -> &ResponsesRequestAuthority {
        &self.authority
    }

    /// Returns the exact live hosted-program state certified by this artifact.
    #[must_use]
    pub const fn hosted_programs(&self) -> &crate::ResponsesHostedProgramContext {
        &self.hosted_programs
    }

    /// Borrows this bridge as lowering context without copying replay payloads.
    #[must_use]
    pub fn context(&self) -> ResponsesReplayContext<'_> {
        ResponsesReplayContext::new(
            &self.authority,
            ResponsesReplayKind::new(self.output_item.as_str(), self.output_item_version),
            self.previous_response
                .as_ref()
                .map(|(name, version)| ResponsesReplayKind::new(name.as_str(), *version)),
            &self.prefix_bindings,
            &self.output_coordinates,
            self.continuation.as_ref(),
            self.expires_at,
            self.now,
        )
        .with_optional_opaque_output(
            self.opaque_output
                .as_ref()
                .map(|(name, version)| ResponsesReplayKind::new(name.as_str(), *version)),
        )
    }
}

pub(super) fn validate_replay_context(
    request: &ModelRequest,
    config: ResponsesRequestConfig<'_>,
) -> Result<(), ResponsesRequestError> {
    let Some(replay) = config.replay else {
        return Ok(());
    };
    if replay.expires_at.is_some_and(|expiry| expiry <= replay.now) {
        return Err(ResponsesReplayError::ArtifactExpired.into());
    }
    validate_authority_for_config(request, config, replay.authority)?;
    for kind in std::iter::once(replay.output_item)
        .chain(replay.opaque_output)
        .chain(replay.previous_response)
    {
        validate_replay_kind(kind)?;
    }
    if replay
        .prefix_bindings
        .windows(2)
        .any(|pair| pair[0].prefix_items >= pair[1].prefix_items)
    {
        return Err(ResponsesReplayError::PrefixBindingsNotStrictlyOrdered.into());
    }
    for entry in replay.prefix_bindings {
        if entry.prefix_items > request.input().items().len()
            || entry.binding != prefix_binding_digest(replay.authority.identity, entry.prefix)?
        {
            return Err(ResponsesReplayError::AuthorityMismatch.into());
        }
    }
    if replay.output_coordinates.len() > request.input().items().len() {
        return Err(ResponsesReplayError::InvalidOutputCoordinates.into());
    }
    let mut prior_position = None;
    for coordinate in replay.output_coordinates {
        let expected_position = usize::try_from(coordinate.output_index)
            .ok()
            .and_then(|index| coordinate.group_start.checked_add(index));
        if prior_position.is_some_and(|prior| prior >= coordinate.canonical_position)
            || coordinate.canonical_position >= request.input().items().len()
            || coordinate.output_count == 0
            || coordinate.output_index >= coordinate.output_count
            || expected_position != Some(coordinate.canonical_position)
            || !replay
                .prefix_bindings
                .iter()
                .any(|entry| entry.binding == coordinate.binding)
        {
            return Err(ResponsesReplayError::InvalidOutputCoordinates.into());
        }
        prior_position = Some(coordinate.canonical_position);
    }
    Ok(())
}

pub(super) const fn is_output_replay_purpose(purpose: ResponsesReplayPurpose) -> bool {
    matches!(
        purpose,
        ResponsesReplayPurpose::OutputItem | ResponsesReplayPurpose::OpaqueOutputItem
    )
}

pub(super) fn validate_replay_kind(
    kind: ResponsesReplayKind<'_>,
) -> Result<(), ResponsesReplayError> {
    if kind.name.is_empty() || kind.name.len() > MAX_REPLAY_KIND_BYTES {
        return Err(ResponsesReplayError::InvalidKind);
    }
    if kind.version == 0 {
        return Err(ResponsesReplayError::InvalidVersion);
    }
    Ok(())
}

pub(super) fn validate_artifact_replay_kind(
    artifact: &ResponsesArtifact,
    purpose: ResponsesReplayPurpose,
    expected: Option<ResponsesReplayKind<'_>>,
) -> Result<(), ResponsesReplayError> {
    match (artifact.replay_contract().for_purpose(purpose), expected) {
        (Some(actual), Some(expected))
            if actual.kind().as_str() == expected.name && actual.version() == expected.version =>
        {
            Ok(())
        }
        (None, None) => Ok(()),
        _ => Err(ResponsesReplayError::IncompatibleEnvelope),
    }
}

pub(crate) fn validate_authority_for_config(
    request: &ModelRequest,
    config: ResponsesRequestConfig<'_>,
    authority: &ResponsesRequestAuthority,
) -> Result<(), ResponsesReplayError> {
    validate_authority(request, authority)?;
    if authority.binding.dialect() != config.protocol
        || authority.binding.model_family() != config.model_family
        || authority.binding.model() != config.model
        || config
            .account
            .is_some_and(|account| *account != authority.binding.account())
        || authority.storage != config.policy.storage
    {
        return Err(ResponsesReplayError::AuthorityMismatch);
    }
    Ok(())
}

pub(super) fn validate_authority(
    request: &ModelRequest,
    authority: &ResponsesRequestAuthority,
) -> Result<(), ResponsesReplayError> {
    let item_prefix = request
        .input()
        .canonical_item_prefix_identity(request.input().items().len())
        .map_err(|_| ResponsesReplayError::AuthorityMismatch)?;
    if authority.request != request.identity()
        || authority.binding.stable_prefix() != request.stable_prefix().reuse_identity()
        || authority.binding.projection() != request.input().identity()
        || authority.binding.item_prefix() != item_prefix.identity()
        || request.resolved_harness_plan_identity() != Some(authority.binding.tool_plan())
        || operation_authority_digest(
            &authority.binding,
            authority.storage,
            authority.request,
            authority.plan,
        )? != authority.identity
    {
        return Err(ResponsesReplayError::AuthorityMismatch);
    }
    Ok(())
}

pub(super) fn operation_authority_digest(
    binding: &ResponsesArtifactBinding,
    storage: ResponsesStoragePolicy,
    request: Digest,
    plan: Digest,
) -> Result<Digest, ResponsesReplayError> {
    let revisions = binding.revisions();
    let family_kernel = revisions.family_kernel().to_be_bytes();
    let dialect_revision = revisions.dialect().to_be_bytes();
    let adapter_revision = revisions.adapter().to_be_bytes();
    let cache_affinity = binding
        .cache_affinity()
        .map(|cache_affinity| cache_affinity.identity_bytes());
    let cache_affinity_present = [u8::from(cache_affinity.is_some())];
    let family = binding.model_family().map_or("", ModelFamilyId::as_str);
    let storage = match storage {
        ResponsesStoragePolicy::ProviderStored => b"provider-stored".as_slice(),
        ResponsesStoragePolicy::Stateless => b"stateless".as_slice(),
    };
    let stable_prefix = binding.stable_prefix().identity_bytes();
    let projection = binding.projection().identity_bytes();
    let item_prefix = binding.item_prefix().identity_bytes();
    let tool_plan = binding.tool_plan().identity_bytes();
    let account = binding.account().identity_bytes();
    let semantic_model = binding.semantic_model().identity_bytes();
    let request = request.identity_bytes();
    let plan = plan.identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        6,
        &[
            b"plak-responses-current-operation-authority-v4",
            binding.dialect().as_str().as_bytes(),
            binding.route().as_str().as_bytes(),
            account.as_slice(),
            binding.model().as_str().as_bytes(),
            family.as_bytes(),
            binding.adapter().as_str().as_bytes(),
            semantic_model.as_slice(),
            &family_kernel,
            &dialect_revision,
            &adapter_revision,
            &cache_affinity_present,
            cache_affinity.as_deref().unwrap_or_default(),
            stable_prefix.as_slice(),
            projection.as_slice(),
            item_prefix.as_slice(),
            tool_plan.as_slice(),
            storage,
            request.as_slice(),
            plan.as_slice(),
        ],
    )
    .map_err(|_| ResponsesReplayError::AuthorityConstruction)
}

pub(super) fn prefix_binding_digest(
    authority: Digest,
    prefix: Digest,
) -> Result<Digest, ResponsesRequestError> {
    let authority = authority.identity_bytes();
    let prefix = prefix.identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        7,
        &[
            b"plak-responses-authorized-item-prefix-v3",
            authority.as_slice(),
            prefix.as_slice(),
        ],
    )
    .map_err(|_| ResponsesReplayError::AuthorityConstruction.into())
}

pub(super) fn storage_mode(policy: ResponsesStoragePolicy) -> ResponsesStorageMode {
    match policy {
        ResponsesStoragePolicy::ProviderStored => ResponsesStorageMode::ProviderStored,
        ResponsesStoragePolicy::Stateless => ResponsesStorageMode::Stateless,
    }
}

pub(super) fn canonical_replay_group(
    artifact: &ResponsesArtifact,
    group_start: usize,
    output_count: u32,
) -> Result<Digest, ResponsesReplayError> {
    let group_start = u64::try_from(group_start)
        .map_err(|_| ResponsesReplayError::InvalidOutputCoordinates)?
        .to_be_bytes();
    let output_count = output_count.to_be_bytes();
    let covered = artifact.production().covered_projection().identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        6,
        &[
            b"plak-responses-canonical-replay-group-v2",
            artifact.integrity(),
            covered.as_slice(),
            &group_start,
            &output_count,
        ],
    )
    .map_err(|_| ResponsesReplayError::AuthorityConstruction)
}

pub(super) fn artifact_target_mismatch(
    current: &ResponsesArtifactBinding,
    artifact: &ResponsesArtifactBinding,
    current_storage: ResponsesStoragePolicy,
    artifact_storage: ResponsesStorageMode,
) -> Option<&'static str> {
    [
        (current.dialect() != artifact.dialect(), "dialect"),
        (current.route() != artifact.route(), "route"),
        (current.account() != artifact.account(), "account"),
        (current.model() != artifact.model(), "model"),
        (
            current.model_family() != artifact.model_family(),
            "model-family",
        ),
        (current.adapter() != artifact.adapter(), "adapter"),
        (
            current.semantic_model() != artifact.semantic_model(),
            "semantic-model",
        ),
        (current.revisions() != artifact.revisions(), "revisions"),
        (current.tool_plan() != artifact.tool_plan(), "tool-plan"),
        (
            storage_mode(current_storage) != artifact_storage,
            "storage-mode",
        ),
    ]
    .into_iter()
    .find_map(|(mismatch, axis)| mismatch.then_some(axis))
}

pub(super) fn exact_replay_for_kind<'a>(
    item: Option<&'a ConversationItem>,
    kind: ResponsesReplayKind<'_>,
) -> Result<Option<&'a ReplayEnvelope>, ResponsesReplayError> {
    let Some(item) = item else {
        return Ok(None);
    };
    let mut exact = None;
    for block in item.blocks() {
        let replay = match block {
            ContentBlock::Opaque(opaque) => Some(opaque.replay()),
            ContentBlock::Reasoning(reasoning) => reasoning.replay(),
            _ => None,
        };
        if let Some(replay) = replay
            && replay.kind().as_str() == kind.name
        {
            if replay.version() != kind.version {
                return Err(ResponsesReplayError::IncompatibleEnvelope);
            }
            if exact.replace(replay).is_some() {
                return Err(ResponsesReplayError::MultipleOutputReplay);
            }
        }
    }
    Ok(exact)
}

pub(super) fn validated_hosted_programs(
    request: &ModelRequest,
    covered_prefix: usize,
    output_item: ResponsesReplayKind<'_>,
    authority: &ResponsesRequestAuthority,
) -> Result<crate::ResponsesHostedProgramContext, ResponsesRequestError> {
    let mut programs = crate::ResponsesHostedProgramContext::default();
    for item in request.input().items().iter().take(covered_prefix) {
        let Some(replay) = exact_replay_for_kind(Some(item), output_item)? else {
            continue;
        };
        if !replay.is_compatible(
            authority.binding.dialect(),
            authority.binding.model_family(),
            authority.binding.model(),
        ) {
            // A canonical prefix may retain replay from another model before
            // the active artifact's producing projection. Such history cannot
            // authorize a hosted program for this target and is ignored here;
            // every active-artifact replay coordinate remains validated above.
            continue;
        }
        let native = crate::parse_unique_json_value(replay.payload().get())
            .map_err(|_| ResponsesReplayError::MalformedEnvelope)?;
        let output = parse_output_item::<ResponsesHostDecodeError>(&native)
            .map_err(|_| ResponsesReplayError::MalformedEnvelope)?;
        // Each replay was paired under its producing route's canonical policy.
        // This pass only reconstructs hosted-program lineage; the exact Lite
        // functions namespace must remain admissible for that replay.
        programs
            .admit_replayed_output(
                &output,
                crate::ResponsesCanonicalPolicy::openai_platform().with_codex_lite_namespace(),
            )
            .map_err(|_| ResponsesReplayError::SemanticPairMismatch)?;
    }
    Ok(programs)
}

pub(super) fn select_replay(
    request: &ModelRequest,
    config: ResponsesRequestConfig<'_>,
) -> Result<ResponsesReplayMode, ResponsesRequestError> {
    if config.policy.storage != ResponsesStoragePolicy::ProviderStored {
        return Ok(ResponsesReplayMode::Full);
    }
    let Some(context) = config.replay else {
        return Ok(ResponsesReplayMode::Full);
    };
    let Some(previous_kind) = context.previous_response else {
        return Ok(ResponsesReplayMode::Full);
    };
    let Some(continuation) = context.continuation else {
        return Ok(ResponsesReplayMode::Full);
    };
    if continuation.response_id.is_empty()
        || continuation.response_id.len() > MAX_RESPONSE_ID_BYTES
        || continuation.prefix_items > request.input().items().len()
    {
        return Err(ResponsesReplayError::InvalidResponseId.into());
    }
    let _ = previous_kind;
    Ok(ResponsesReplayMode::PreviousResponse {
        response_id: continuation.response_id.clone(),
        prefix_items: continuation.prefix_items,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReplayAuthority {
    Active,
    Unbound,
}

pub(super) fn replay_authority(
    config: ResponsesRequestConfig<'_>,
    canonical_position: usize,
    replay: &ReplayEnvelope,
) -> Result<ReplayAuthority, ResponsesRequestError> {
    if !replay.is_compatible(config.protocol, config.model_family, config.model) {
        return Ok(ReplayAuthority::Unbound);
    }
    let Some(context) = config.replay else {
        return Ok(ReplayAuthority::Unbound);
    };
    let coordinate = context
        .output_coordinates
        .binary_search_by_key(&canonical_position, |coordinate| {
            coordinate.canonical_position
        })
        .ok()
        .map(|index| context.output_coordinates[index]);
    match coordinate {
        Some(coordinate) if coordinate.envelope == replay.digest() => Ok(ReplayAuthority::Active),
        Some(_) => Err(ResponsesReplayError::MissingOutputCoordinate.into()),
        None => Ok(ReplayAuthority::Unbound),
    }
}

pub(super) fn exact_output_replay<'a>(
    item_index: usize,
    item: &'a ConversationItem,
    config: ResponsesRequestConfig<'_>,
) -> Result<Option<(usize, &'a ReplayEnvelope)>, ResponsesRequestError> {
    let Some(context) = config.replay else {
        return Ok(None);
    };
    let mut exact = None;
    for (block_index, block) in item.blocks().iter().enumerate() {
        let replay = match block {
            ContentBlock::Opaque(opaque) => Some(opaque.replay()),
            ContentBlock::Reasoning(reasoning) => reasoning.replay(),
            _ => None,
        };
        if let Some(replay) = replay
            && replay_authority(config, item_index, replay)? == ReplayAuthority::Active
            && (replay.kind().as_str() == context.output_item.name
                || context
                    .opaque_output
                    .is_some_and(|kind| replay.kind().as_str() == kind.name))
            && exact.replace((block_index, replay)).is_some()
        {
            return Err(ResponsesReplayError::MultipleOutputReplay.into());
        }
    }
    Ok(exact)
}

pub(super) fn replay_output_item(
    config: ResponsesRequestConfig<'_>,
    item_index: usize,
    replay: &ReplayEnvelope,
) -> Result<Value, ResponsesRequestError> {
    let bound = bound_output_item(config, item_index, replay)?;
    serde_json::from_str(bound.item.get())
        .map_err(|_| ResponsesReplayError::MalformedEnvelope.into())
}

/// Grok Build: `status` is output-only and rejected on input; function_call
/// `id`/`status` are likewise omitted. Reasoning `content` parts need the
/// `reasoning_text` discriminator when present.
pub(super) fn omit_output_only_input_fields(policy: ResponsesRequestPolicy, item: &mut Value) {
    if !policy.omits_output_only_input_fields() {
        return;
    }
    let Some(kind) = item.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return;
    };
    let Some(object) = item.as_object_mut() else {
        return;
    };
    match kind.as_str() {
        "reasoning" => {
            object.remove("status");
            if let Some(content) = object.get_mut("content").and_then(Value::as_array_mut) {
                for part in content {
                    if let Some(part) = part.as_object_mut() {
                        part.entry("type")
                            .or_insert_with(|| Value::String("reasoning_text".to_owned()));
                    }
                }
            }
        }
        "function_call" | "function_call_output" => {
            object.remove("id");
            object.remove("status");
        }
        _ => {}
    }
}

pub(super) fn replay_output_for_policy(
    config: ResponsesRequestConfig<'_>,
    item_index: usize,
    replay: &ReplayEnvelope,
) -> Result<Option<Value>, ResponsesRequestError> {
    let replayed = replay_output_item(config, item_index, replay)?;
    if replayed.get("type").and_then(Value::as_str) == Some("reasoning") {
        // Codex's reference-client reasoning item has no `status` field,
        // while SuperGrok's exact proxy item does. A present status must be
        // terminal; absence is valid only because the dialect-specific
        // projection decides whether status is required before promotion.
        let terminal = replayed
            .get("status")
            .and_then(Value::as_str)
            .is_none_or(|status| status == "completed");
        let replayable = terminal
            && replayed
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty());
        return match config.policy.replay {
            ResponsesReplayPolicy::Strict if !replayable => {
                Err(ResponsesReplayError::StrictEncryptedReasoning.into())
            }
            ResponsesReplayPolicy::DropUnreplayableReasoning => Ok(replayable.then_some(replayed)),
            ResponsesReplayPolicy::Strict => Ok(Some(replayed)),
        };
    }
    Ok(Some(replayed))
}

pub(super) struct ValidatedOutputItem<'a> {
    group: Digest,
    output_index: u32,
    output_count: u32,
    item: &'a RawValue,
}

pub(super) fn bound_output_item<'a>(
    config: ResponsesRequestConfig<'_>,
    canonical_position: usize,
    replay: &'a ReplayEnvelope,
) -> Result<ValidatedOutputItem<'a>, ResponsesRequestError> {
    let context = config
        .replay
        .ok_or(ResponsesReplayError::IncompatibleEnvelope)?;
    if context
        .expires_at
        .is_some_and(|expiry| expiry <= context.now)
    {
        return Err(ResponsesReplayError::ArtifactExpired.into());
    }
    let kind_matches = (replay.version() == context.output_item.version
        && replay.kind().as_str() == context.output_item.name)
        || context.opaque_output.is_some_and(|kind| {
            replay.version() == kind.version && replay.kind().as_str() == kind.name
        });
    if !kind_matches || !replay.is_compatible(config.protocol, config.model_family, config.model) {
        return Err(ResponsesReplayError::IncompatibleEnvelope.into());
    }
    if let Some(coordinate) = context.output_for(canonical_position, replay.digest()) {
        if !context
            .prefix_bindings
            .iter()
            .any(|entry| entry.binding == coordinate.binding)
            || coordinate.group_start > coordinate.canonical_position
        {
            return Err(ResponsesReplayError::InvalidOutputCoordinates.into());
        }
        return Ok(ValidatedOutputItem {
            group: coordinate.group,
            output_index: coordinate.output_index,
            output_count: coordinate.output_count,
            item: replay.payload(),
        });
    }
    Err(ResponsesReplayError::MissingOutputCoordinate.into())
}

pub(super) fn validate_output_replay_sequence(
    request: &ModelRequest,
    config: ResponsesRequestConfig<'_>,
    first_position: usize,
) -> Result<(), ResponsesRequestError> {
    let mut completed_responses = HashSet::with_capacity(
        config
            .replay
            .map_or(0, |context| context.output_coordinates.len()),
    );
    let mut active: Option<(Digest, u32, u32)> = None;
    for (canonical_position, item) in request
        .input()
        .items()
        .iter()
        .enumerate()
        .skip(first_position)
    {
        let Some((_, replay)) = exact_output_replay(canonical_position, item, config)? else {
            close_replay_group(&mut active, &mut completed_responses)?;
            continue;
        };
        let bound = bound_output_item(config, canonical_position, replay)?;
        match &mut active {
            Some((group, next_index, output_count)) if *group == bound.group => {
                if *next_index != bound.output_index || *output_count != bound.output_count {
                    return Err(ResponsesReplayError::IncompleteOutputGroup.into());
                }
                *next_index = next_index
                    .checked_add(1)
                    .ok_or(ResponsesReplayError::InvalidOutputCoordinates)?;
            }
            Some(_) => {
                close_replay_group(&mut active, &mut completed_responses)?;
                start_replay_group(&mut active, &completed_responses, &bound)?;
            }
            None => start_replay_group(&mut active, &completed_responses, &bound)?,
        }
    }
    close_replay_group(&mut active, &mut completed_responses)
}

pub(super) fn start_replay_group(
    active: &mut Option<(Digest, u32, u32)>,
    completed_responses: &HashSet<Digest>,
    bound: &ValidatedOutputItem<'_>,
) -> Result<(), ResponsesRequestError> {
    if completed_responses.contains(&bound.group) {
        return Err(ResponsesReplayError::RepeatedOutputGroup.into());
    }
    if bound.output_index != 0 {
        return Err(ResponsesReplayError::IncompleteOutputGroup.into());
    }
    *active = Some((bound.group, 1, bound.output_count));
    Ok(())
}

pub(super) fn close_replay_group(
    active: &mut Option<(Digest, u32, u32)>,
    completed_responses: &mut HashSet<Digest>,
) -> Result<(), ResponsesRequestError> {
    let Some((group, next_index, output_count)) = active.take() else {
        return Ok(());
    };
    if next_index != output_count {
        return Err(ResponsesReplayError::IncompleteOutputGroup.into());
    }
    if !completed_responses.insert(group) {
        return Err(ResponsesReplayError::RepeatedOutputGroup.into());
    }
    Ok(())
}
