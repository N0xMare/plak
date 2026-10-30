//! Request, wire, target and host-behavior identity digests for attempt bindings.

use super::{
    ATTEMPT_DIALECT_IDENTITY_REVISION, ATTEMPT_TARGET_IDENTITY_REVISION, Client, Digest,
    DigestDomain, HOST_BEHAVIOR_IDENTITY_REVISION, HeaderName, LedgerPolicy,
    LoweredResponsesRequest, NonZeroU32, PREPARED_WIRE_DIGEST_REVISION, REQUEST_DIGEST_REVISION,
    ReplaySafety, Request, ResponsesAttemptBuildError, ResponsesAttemptTarget, ResponsesAuthSlots,
    ResponsesAuthorizationAdapter, ResponsesCredentialAuthority, ResponsesHeaderBinding,
    ResponsesPhysicalRequestBinding, ResponsesProviderErrorPolicy, ResponsesReplayProjection,
    ResponsesRequestAuthority, ResponsesRequestPolicy, ResponsesResponseLimits,
    ResponsesSemanticRevisions, ResponsesStorageMode, ResponsesStreamDialectData,
    ResponsesStreamLimits, ResponsesWireOccurrenceCounts, header_name_requires_secret,
    inspect_wire_occurrences, parse_unique_json_value, validate_header_authority,
};

pub(super) fn request_body_digest(bytes: &[u8]) -> Result<Digest, ResponsesAttemptBuildError> {
    Digest::sha256(
        DigestDomain::ProviderOperation,
        REQUEST_DIGEST_REVISION,
        &[b"responses-request-body-v2", bytes],
    )
    .map_err(|_| ResponsesAttemptBuildError::Binding)
}

pub(super) fn prepared_wire_digest(bytes: &[u8]) -> Result<Digest, ResponsesAttemptBuildError> {
    Digest::sha256(
        DigestDomain::Blob,
        PREPARED_WIRE_DIGEST_REVISION,
        &[b"responses-prepared-wire-body-v1", bytes],
    )
    .map_err(|_| ResponsesAttemptBuildError::Binding)
}

pub(super) struct PreparedWireInspection {
    pub(super) enforced_output_tokens: Option<NonZeroU32>,
    pub(super) wire_occurrence_counts: Option<ResponsesWireOccurrenceCounts>,
}

pub(super) fn inspect_serialized_body(
    bytes: &[u8],
    require_wire_occurrences: bool,
) -> Result<PreparedWireInspection, ResponsesAttemptBuildError> {
    let body = std::str::from_utf8(bytes)
        .ok()
        .and_then(|body| parse_unique_json_value(body).ok())
        .ok_or(ResponsesAttemptBuildError::Binding)?;
    let wire_occurrence_counts = if require_wire_occurrences {
        Some(inspect_wire_occurrences(&body).map_err(|_| ResponsesAttemptBuildError::Binding)?)
    } else {
        None
    };
    Ok(PreparedWireInspection {
        enforced_output_tokens: exact_serialized_output_limit(&body)?,
        wire_occurrence_counts,
    })
}

pub(super) fn exact_serialized_output_limit(
    body: &serde_json::Value,
) -> Result<Option<NonZeroU32>, ResponsesAttemptBuildError> {
    if !body.is_object() {
        return Err(ResponsesAttemptBuildError::Binding);
    }
    let Some(value) = body.get("max_output_tokens") else {
        return Ok(None);
    };
    let tokens = value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .and_then(NonZeroU32::new)
        .ok_or(ResponsesAttemptBuildError::Binding)?;
    Ok(Some(tokens))
}

pub(super) fn authority_target(
    authority: &ResponsesRequestAuthority,
) -> Result<ResponsesAttemptTarget, ResponsesAttemptBuildError> {
    let binding = authority.artifact_binding();
    target_identity(
        binding.dialect().as_str(),
        binding.route().as_str(),
        binding.account(),
        binding.model().as_str(),
        binding
            .model_family()
            .map_or("", plakous_model::ModelFamilyId::as_str),
        binding.adapter().as_str(),
        binding.semantic_model(),
        binding.revisions(),
    )
    .map(ResponsesAttemptTarget)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the target identity is the complete closed admitted service tuple"
)]
pub(super) fn target_identity(
    protocol: &str,
    route: &str,
    account: Digest,
    model: &str,
    family: &str,
    adapter: &str,
    semantic_model: Digest,
    revisions: ResponsesSemanticRevisions,
) -> Result<Digest, ResponsesAttemptBuildError> {
    if account.domain() != DigestDomain::ModelBinding
        || semantic_model.domain() != DigestDomain::ModelBinding
    {
        return Err(ResponsesAttemptBuildError::Binding);
    }
    let family_kernel = revisions.family_kernel().to_be_bytes();
    let dialect = revisions.dialect().to_be_bytes();
    let adapter_revision = revisions.adapter().to_be_bytes();
    let account = account.identity_bytes();
    let semantic_model = semantic_model.identity_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        ATTEMPT_TARGET_IDENTITY_REVISION,
        &[
            b"responses-attempt-target-v2",
            protocol.as_bytes(),
            route.as_bytes(),
            account.as_slice(),
            model.as_bytes(),
            family.as_bytes(),
            adapter.as_bytes(),
            semantic_model.as_slice(),
            &family_kernel,
            &dialect,
            &adapter_revision,
        ],
    )
    .map_err(|_| ResponsesAttemptBuildError::Binding)
}

#[allow(
    clippy::too_many_arguments,
    reason = "canonical identity covers the complete closed dialect behavior table"
)]
pub(super) fn dialect_execution_identity(
    dialect_revision: u32,
    target: ResponsesAttemptTarget,
    host_behavior: Digest,
    ledger_policy: LedgerPolicy,
    auth_slots: ResponsesAuthSlots,
    request_policy: ResponsesRequestPolicy,
    stream: &ResponsesStreamDialectData,
    stream_limits: ResponsesStreamLimits,
    replay: &ResponsesReplayProjection,
    storage: ResponsesStorageMode,
    provider_error_policy: ResponsesProviderErrorPolicy,
    error_limits: ResponsesResponseLimits,
    provider_request_id_header: Option<&HeaderName>,
    allow_missing_sse_content_type: bool,
) -> Result<Digest, ResponsesAttemptBuildError> {
    let stream = stream
        .execution_identity()
        .ok_or(ResponsesAttemptBuildError::DialectIdentity)?;
    let replay = replay.execution_identity()?;
    let revision = dialect_revision.to_be_bytes();
    let target = target.identity().identity_bytes();
    let host_behavior = host_behavior.identity_bytes();
    let ledger = [
        u8::from(ledger_policy.require_service_admitted_for_dispatch),
        u8::from(ledger_policy.require_active_request_always),
        u8::from(ledger_policy.dual_auth_slots),
        u8::from(ledger_policy.clear_service_on_terminal_success),
    ];
    let request_policy = request_policy
        .execution_identity()
        .map_err(|_| ResponsesAttemptBuildError::DialectIdentity)?
        .identity_bytes();
    let frame_bytes = stream_limits.max_frame_bytes().to_be_bytes();
    let total_bytes = stream_limits.max_total_bytes().to_be_bytes();
    let events = stream_limits.max_events().to_be_bytes();
    let storage = match storage {
        ResponsesStorageMode::Stateless => b"stateless".as_slice(),
        ResponsesStorageMode::ProviderStored => b"provider-stored".as_slice(),
    };
    let provider_error_policy = match provider_error_policy {
        ResponsesProviderErrorPolicy::OpenAiCompatible => b"openai-compatible".as_slice(),
        ResponsesProviderErrorPolicy::OpenRouter => b"openrouter".as_slice(),
        ResponsesProviderErrorPolicy::Grok => b"grok".as_slice(),
    };
    let response_bytes = error_limits.max_response_bytes().to_be_bytes();
    let output_items = error_limits.max_output_items().to_be_bytes();
    let unknown_bytes = error_limits.max_unknown_bytes().to_be_bytes();
    let request_id_header = provider_request_id_header.map_or("", HeaderName::as_str);
    let missing_content_type = [u8::from(allow_missing_sse_content_type)];
    Digest::sha256(
        DigestDomain::ModelBinding,
        ATTEMPT_DIALECT_IDENTITY_REVISION,
        &[
            b"responses-attempt-dialect-v3",
            revision.as_slice(),
            target.as_slice(),
            host_behavior.as_slice(),
            ledger.as_slice(),
            auth_slots.identity_label(),
            request_policy.as_slice(),
            &stream.identity_bytes(),
            frame_bytes.as_slice(),
            total_bytes.as_slice(),
            events.as_slice(),
            &replay.identity_bytes(),
            storage,
            provider_error_policy,
            response_bytes.as_slice(),
            output_items.as_slice(),
            unknown_bytes.as_slice(),
            request_id_header.as_bytes(),
            missing_content_type.as_slice(),
        ],
    )
    .map_err(|_| ResponsesAttemptBuildError::DialectIdentity)
}

pub(super) fn host_behavior_identity<D, G>(
    client: &Client,
    authorization: &ResponsesAuthorizationAdapter<D, G>,
) -> Result<Digest, ResponsesAttemptBuildError> {
    let transport = client
        .behavior_identity()
        .ok_or(ResponsesAttemptBuildError::DialectIdentity)?
        .as_bytes();
    let preflight = authorization.preflight_revision.to_be_bytes();
    let final_authorization = authorization.final_authorization_revision.to_be_bytes();
    Digest::sha256(
        DigestDomain::ModelBinding,
        HOST_BEHAVIOR_IDENTITY_REVISION,
        &[
            b"responses-attempt-host-behavior-v11",
            transport.as_slice(),
            preflight.as_slice(),
            final_authorization.as_slice(),
        ],
    )
    .map_err(|_| ResponsesAttemptBuildError::DialectIdentity)
}

/// Top-level Responses fields whose replay can only cost duplicate inference.
/// `store` and `previous_response_id` only persist or read the response
/// itself; `conversation`, `background` and every unlisted field may have an
/// effect beyond inference.
pub(super) const COST_ONLY_FIELDS: &[&str] = &[
    "include",
    "input",
    "instructions",
    "max_output_tokens",
    "metadata",
    "model",
    "parallel_tool_calls",
    "plugins",
    "previous_response_id",
    "prompt_cache_key",
    "provider",
    "reasoning",
    "service_tier",
    "session_id",
    "store",
    "stream",
    "temperature",
    "text",
    "tool_choice",
    "tools",
    "top_p",
];

/// Classifies the exact lowered body by allowlist: only `function` and
/// `custom` tools, disabled gateway plugins, and the fields above are
/// cost-only. A hosted tool or any unclassified field is a server effect.
pub(super) fn lowered_replay_safety(
    lowered: &LoweredResponsesRequest,
) -> Result<ReplaySafety, ResponsesAttemptBuildError> {
    let serde_json::Value::Object(fields) = lowered.body() else {
        return Err(ResponsesAttemptBuildError::Binding);
    };
    let mut safety = ReplaySafety::CostOnly;
    for (name, value) in fields {
        let cost_only = match name.as_str() {
            "tools" => {
                let tools = value
                    .as_array()
                    .ok_or(ResponsesAttemptBuildError::Binding)?;
                let mut client_only = true;
                for tool in tools {
                    let kind = tool
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .ok_or(ResponsesAttemptBuildError::Binding)?;
                    client_only &= matches!(kind, "function" | "custom");
                }
                client_only
            }
            "plugins" => value.as_array().is_some_and(|plugins| {
                plugins
                    .iter()
                    .all(|plugin| plugin.get("enabled") == Some(&serde_json::Value::Bool(false)))
            }),
            name => COST_ONLY_FIELDS.contains(&name),
        };
        if !cost_only {
            safety = ReplaySafety::ServerEffect;
        }
    }
    Ok(safety)
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear host step whose ordering is the invariant"
)]
pub(super) fn physical_request_digest(
    physical: &ResponsesPhysicalRequestBinding,
    credential_authority: &ResponsesCredentialAuthority,
    request: &Request,
    header_bindings: &[ResponsesHeaderBinding],
) -> Result<(Digest, Digest), ResponsesAttemptBuildError> {
    if physical.attempt().operation().auth_slots() != credential_authority.auth_slots() {
        return Err(ResponsesAttemptBuildError::AuthSlots);
    }
    validate_header_authority(header_bindings, credential_authority)?;
    if request.headers().len() != header_bindings.len() {
        return Err(ResponsesAttemptBuildError::Binding);
    }
    let mut headers = Vec::with_capacity(header_bindings.len());
    let mut seen = std::collections::BTreeSet::new();
    for (name, value) in request.headers().iter() {
        if !seen.insert(name.as_str()) {
            return Err(ResponsesAttemptBuildError::Binding);
        }
        let mut matching = header_bindings
            .iter()
            .filter(|binding| binding.name() == name);
        let binding = matching.next().ok_or(ResponsesAttemptBuildError::Binding)?;
        if matching.next().is_some() {
            return Err(ResponsesAttemptBuildError::Binding);
        }
        let mut field = Vec::new();
        append_framed(&mut field, name.as_str().as_bytes())?;
        match binding {
            ResponsesHeaderBinding::PublicExact {
                value: expected, ..
            } => {
                if header_name_requires_secret(name) || value.as_bytes() != expected.as_slice() {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                append_framed(&mut field, b"public-exact")?;
                append_framed(&mut field, value.as_bytes())?;
            }
            ResponsesHeaderBinding::CredentialMarkerExact {
                value: expected,
                slot,
                ..
            } => {
                if !header_name_requires_secret(name)
                    || value.as_bytes() != expected.as_slice()
                    || credential_authority.generation(*slot).is_none()
                {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                append_framed(&mut field, b"credential-marker-exact")?;
                append_framed(&mut field, slot.identity_label())?;
                append_framed(&mut field, value.as_bytes())?;
            }
            ResponsesHeaderBinding::CredentialSecret { slot, .. } => {
                if !header_name_requires_secret(name) {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                let generation = credential_authority
                    .generation(*slot)
                    .ok_or(ResponsesAttemptBuildError::AuthSlots)?;
                append_framed(&mut field, b"credential-secret")?;
                append_framed(&mut field, slot.identity_label())?;
                append_framed(&mut field, generation.as_bytes())?;
            }
            ResponsesHeaderBinding::AccountSecret {
                slot, authority, ..
            } => {
                if !header_name_requires_secret(name) {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                if credential_authority.account(*slot) != Some(*authority) {
                    return Err(ResponsesAttemptBuildError::Binding);
                }
                append_framed(&mut field, b"account-secret")?;
                append_framed(&mut field, slot.identity_label())?;
                append_framed(&mut field, &authority.identity_bytes())?;
            }
        }
        headers.push(field);
    }
    if seen.len() != header_bindings.len() {
        return Err(ResponsesAttemptBuildError::Binding);
    }
    headers.sort();
    let operation = physical.attempt().operation();
    let authority = operation.request_authority.identity_bytes();
    let execution = operation.execution.identity_bytes();
    let attempt_id = physical.attempt().attempt_id_ref().to_string();
    let generation = physical.generation().to_be_bytes();
    let route = Digest::sha256(
        DigestDomain::ModelBinding,
        REQUEST_DIGEST_REVISION,
        &[
            b"responses-physical-route-v2",
            execution.as_slice(),
            request.origin().binding_authority().as_bytes(),
            request.path_and_query().as_str().as_bytes(),
        ],
    )
    .map_err(|_| ResponsesAttemptBuildError::Binding)?;
    let route_bytes = route.identity_bytes();
    let hosted_effect = [u8::from(
        operation.replay_safety() == ReplaySafety::ServerEffect,
    )];
    let mut parts = vec![
        b"responses-physical-request-v2".as_slice(),
        authority.as_slice(),
        execution.as_slice(),
        route_bytes.as_slice(),
        attempt_id.as_bytes(),
        generation.as_slice(),
        credential_authority.auth_slots().identity_label(),
        hosted_effect.as_slice(),
        request.method().as_str().as_bytes(),
        request.path_and_query().as_str().as_bytes(),
        request.body().as_ref(),
    ];
    parts.extend(headers.iter().map(Vec::as_slice));
    let request = Digest::sha256(
        DigestDomain::ProviderOperation,
        REQUEST_DIGEST_REVISION,
        &parts,
    )
    .map_err(|_| ResponsesAttemptBuildError::Binding)?;
    Ok((route, request))
}

pub(super) fn append_framed(
    output: &mut Vec<u8>,
    value: &[u8],
) -> Result<(), ResponsesAttemptBuildError> {
    let length = u32::try_from(value.len()).map_err(|_| ResponsesAttemptBuildError::Binding)?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value);
    Ok(())
}
