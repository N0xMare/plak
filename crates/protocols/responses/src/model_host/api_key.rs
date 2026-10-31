//! Shared preparation and dispatch for Responses API-key routes.
//!
//! The skeleton never reads a bearer and never selects an origin. Each route
//! passes its own label, revisions, replay kinds, authorization header and extras.

use std::sync::Arc;
use std::time::{Duration, Instant};

use plakous_auth::api_key::CredentialGenerationFingerprint;
use plakous_http::{
    HeaderMap, HeaderName, HeaderValue, Limits, Method, Origin, Request, SensitiveBytes,
};
use plakous_model::{
    CapabilityRequirement, Clock, CompiledProviderConfiguration, Digest, ModelCacheAffinity,
    ModelPreparationContext, ModelPreparationError, ModelPreparationErrorKind, ModelRequest,
    PreparedCacheUse, PreparedCallError, PreparedCallRecord, PreparedModelCall,
    PreparedModelProjection, PreparedOutputLimit, PreparedTokenProjection, ProtocolId,
    ProviderBehaviorConfiguration, ReasoningEffortDisposition, ReasoningPolicy,
    ResolvedModelDescriptor, ValidatedRequestPlan,
};

use crate::{
    CanonicalIdSources, PreparedResponsesAttempt, ResponsesArtifactReplayBridge,
    ResponsesAttemptDialectData, ResponsesAttemptService, ResponsesAuthSlot,
    ResponsesCredentialAuthority, ResponsesDispatchRejection, ResponsesHeaderBinding,
    ResponsesPhysicalRequestBinding, ResponsesPreparedPhysicalRequest, ResponsesReplayError,
    ResponsesReplayKind, ResponsesRequestAuthority, ResponsesSemanticRevisions,
    ResponsesStoragePolicy, prepare_responses_attempt, start_prepared_responses_model_call,
};

const RESPONSES_FAMILY: &str = "responses-family";
const HEADER_ACCEPT: HeaderName = HeaderName::from_static("accept");
const HEADER_CONTENT_TYPE: HeaderName = HeaderName::from_static("content-type");
const HEADER_AUTHORIZATION: HeaderName = HeaderName::from_static("authorization");

/// Facts for one API-key Responses preparation. Revisions are the route's own constants.
pub struct Preparation<'a, D, G> {
    /// Route name prefixed onto this skeleton's preparation errors.
    pub label: &'static str,
    /// Admitted model.
    pub descriptor: &'a ResolvedModelDescriptor,
    /// Validated plan. Moved into the prepared execution.
    pub plan: ValidatedRequestPlan,
    /// Preparation context, including authority and any active artifact.
    pub context: &'a ModelPreparationContext,
    /// Dialect attempt data, including the execution identity.
    pub attempt: &'a ResponsesAttemptDialectData,
    /// Route canonical constructor output.
    pub canonical: crate::ResponsesCanonicalDialectData,
    /// Authority from [`request_authority`].
    pub authority: ResponsesRequestAuthority,
    /// Output-item replay kind for an active artifact.
    pub output_item_replay: ResponsesReplayKind<'static>,
    /// Previous-response replay kind, when this route has one.
    pub previous_response_replay: Option<ResponsesReplayKind<'static>>,
    /// Adapter revision constant.
    pub adapter_revision: &'static str,
    /// Projection revision constant.
    pub projection_revision: &'static str,
    /// Accounting revision constant.
    pub accounting_revision: &'static str,
    /// Cache use the route already classified.
    pub cache_use: PreparedCacheUse,
    /// Observation channel capacity.
    pub observation_capacity: usize,
    /// Clock for canonical item ids.
    pub clock: Arc<dyn Clock>,
    /// Attempt service bound to this route's execution and guard.
    pub service: Arc<ResponsesAttemptService<D, G>>,
    /// Operation timeout. The start closure uses `now + timeout`.
    pub operation_timeout: Duration,
}

/// Exact POST facts for one API-key Responses dispatch. The caller owns the origin.
pub struct Wire<G> {
    /// Dial origin. Credential authority stays on the route's own origin.
    pub origin: Origin,
    /// Exact path on that origin.
    pub path: &'static str,
    /// Primary bearer. The route resolves it only after its generation fence.
    pub authorization: HeaderValue,
    /// Public headers after accept, content-type and authorization.
    pub extra_headers: Vec<(HeaderName, Vec<u8>)>,
    /// Credential-free JSON body.
    pub body: Vec<u8>,
    /// Transport limits already bound to the route client.
    pub limits: Limits,
    /// Fingerprint of the generation compared before the bearer was read.
    pub fingerprint: CredentialGenerationFingerprint,
    /// Predicted physical generation.
    pub physical: ResponsesPhysicalRequestBinding,
    /// Opaque guard retained through the response head.
    pub guard: G,
}

/// Lowers one API-key Responses call and returns its one-shot start closure.
///
/// The closure's deadline is `Instant::now()` plus [`Preparation::operation_timeout`].
///
/// # Errors
///
/// Returns [`ModelPreparationError`]. Messages from this skeleton start with
/// the route [`Preparation::label`].
pub fn prepare<D, G>(
    preparation: Preparation<'_, D, G>,
) -> Result<PreparedModelCall, ModelPreparationError>
where
    D: Send + Sync + 'static,
    G: Send + 'static,
{
    let Preparation {
        label,
        descriptor,
        plan,
        context,
        attempt,
        canonical,
        authority,
        output_item_replay,
        previous_response_replay,
        adapter_revision,
        projection_revision,
        accounting_revision,
        cache_use,
        observation_capacity,
        clock,
        service,
        operation_timeout,
    } = preparation;
    let request = plan.request();
    let replay = active_replay(
        label,
        context,
        request,
        &authority,
        output_item_replay,
        previous_response_replay,
    )?;
    let prepared_attempt = prepare_responses_attempt(
        authority,
        request,
        replay.as_ref().map(ResponsesArtifactReplayBridge::context),
        attempt,
    )
    .map_err(|error| {
        ModelPreparationError::rejected(
            error.preparation_error_kind(),
            format!("{label} Responses request lowering failed: {error}"),
        )
    })?;
    let record = seal_prepared_record(
        label,
        descriptor,
        &plan,
        context,
        &prepared_attempt,
        (adapter_revision, projection_revision, accounting_revision),
        cache_use,
    )?;
    let artifact_binding = prepared_attempt.authority().artifact_binding().clone();
    let execution = crate::PreparedResponsesModelExecution::new(
        prepared_attempt,
        canonical,
        CanonicalIdSources::with_clock(Arc::clone(&clock)),
        CanonicalIdSources::with_clock(Arc::clone(&clock)),
        replay
            .as_ref()
            .map(|bridge| bridge.hosted_programs().clone())
            .unwrap_or_default(),
        plan,
        artifact_binding,
        Arc::clone(&clock),
        descriptor.clone(),
        observation_capacity,
        None,
    )
    .map_err(|_| {
        labeled(
            label,
            ModelPreparationErrorKind::InvalidConfiguration,
            "prepared execution is invalid",
        )
    })?;
    Ok(PreparedModelCall::new(record, move |call_context| {
        let now = Instant::now();
        let deadline = now.checked_add(operation_timeout).unwrap_or(now);
        start_prepared_responses_model_call(execution, service, call_context, deadline)
    }))
}

/// Builds the physical request.
///
/// Header order is accept, content-type, authorization, then `extra_headers`.
/// Physical identity sorts those frames, so the digest does not depend on order.
///
/// # Errors
///
/// Returns [`ResponsesDispatchRejection::Binding`] when a public header byte
/// string is not a header value or the request cannot be framed.
pub fn seal<G>(
    wire: Wire<G>,
) -> Result<ResponsesPreparedPhysicalRequest<G>, ResponsesDispatchRejection> {
    let mut headers = HeaderMap::new();
    let mut bindings = Vec::with_capacity(3 + wire.extra_headers.len());
    insert_public(
        &mut headers,
        &mut bindings,
        HEADER_ACCEPT,
        b"text/event-stream",
    )?;
    insert_public(
        &mut headers,
        &mut bindings,
        HEADER_CONTENT_TYPE,
        b"application/json",
    )?;
    headers.insert(HEADER_AUTHORIZATION, wire.authorization);
    bindings.push(ResponsesHeaderBinding::CredentialSecret {
        name: HEADER_AUTHORIZATION,
        slot: ResponsesAuthSlot::Primary,
    });
    for (name, value) in wire.extra_headers {
        insert_public(&mut headers, &mut bindings, name, &value)?;
    }
    let request = Request::new(
        wire.origin,
        Method::POST,
        wire.path,
        headers,
        SensitiveBytes::new(wire.body).into_bytes(),
        &wire.limits,
    )
    .map_err(|_| ResponsesDispatchRejection::Binding)?;
    Ok(ResponsesPreparedPhysicalRequest::new(
        wire.physical,
        request,
        ResponsesCredentialAuthority::primary(wire.fingerprint),
        bindings,
        wire.guard,
    ))
}

/// The shared cancel-or-deadline refusal used before a bearer read and on revalidation.
///
/// # Errors
///
/// Returns [`ResponsesDispatchRejection::Unavailable`] when the call is cancelled
/// or the deadline has passed.
pub fn dispatch_available(
    cancelled: bool,
    deadline: Instant,
) -> Result<(), ResponsesDispatchRejection> {
    if cancelled || Instant::now() >= deadline {
        Err(ResponsesDispatchRejection::Unavailable)
    } else {
        Ok(())
    }
}

/// Parses a stateless event-stream body (`stream: true`, `store: false`).
///
/// When `model` is `Some`, the body must also name that exact model.
///
/// # Errors
///
/// Returns [`ResponsesDispatchRejection::Binding`] when the body is not that JSON.
pub fn stateless_stream_body(
    bytes: &[u8],
    model: Option<&str>,
) -> Result<serde_json::Value, ResponsesDispatchRejection> {
    let body: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| ResponsesDispatchRejection::Binding)?;
    let stateless = body.get("stream") == Some(&serde_json::Value::Bool(true))
        && body.get("store") == Some(&serde_json::Value::Bool(false));
    let model_matches = model.is_none_or(|expected| {
        body.get("model").and_then(serde_json::Value::as_str) == Some(expected)
    });
    if stateless && model_matches {
        Ok(body)
    } else {
        Err(ResponsesDispatchRejection::Binding)
    }
}

/// Refuses a revalidation whose primary fingerprint is not the admitted generation.
///
/// # Errors
///
/// Returns [`ResponsesDispatchRejection::Binding`] on a mismatch.
pub fn fingerprint_matches(
    authority: &ResponsesCredentialAuthority,
    fingerprint: CredentialGenerationFingerprint,
) -> Result<(), ResponsesDispatchRejection> {
    if authority.generation(ResponsesAuthSlot::Primary) == Some(fingerprint) {
        Ok(())
    } else {
        Err(ResponsesDispatchRejection::Binding)
    }
}

/// Selected and effective configuration identities on the plan match the model.
#[must_use]
pub fn configuration_matches(
    plan: &ValidatedRequestPlan,
    configuration: &CompiledProviderConfiguration,
) -> bool {
    plan.request().selected_provider_configuration_id() == configuration.selected_identity()
        && plan.request().effective_provider_configuration_id()
            == configuration.effective_identity()
}

/// OpenAI Platform and xAI Platform: cache use is the active artifact, or none.
///
/// Runtime affinity alone is not cache use on these two routes.
#[must_use]
pub fn artifact_cache(context: &ModelPreparationContext) -> PreparedCacheUse {
    context
        .active_family_artifact()
        .map_or(PreparedCacheUse::None, |artifact| {
            PreparedCacheUse::ActiveFamilyArtifact {
                artifact: artifact.integrity(),
            }
        })
}

/// Request authority for a stateless API-key Responses operation.
///
/// `revisions` is the route's semantic revision triple, passed through unchanged.
///
/// # Errors
///
/// Returns the authority error unchanged so each route keeps its own message.
pub fn request_authority(
    descriptor: &ResolvedModelDescriptor,
    plan: &ValidatedRequestPlan,
    context: &ModelPreparationContext,
    revisions: ResponsesSemanticRevisions,
) -> Result<ResponsesRequestAuthority, ResponsesReplayError> {
    ResponsesRequestAuthority::from_admitted_operation(
        descriptor,
        plan,
        plan.request(),
        context.authority().account_binding(),
        revisions,
        context.cache_affinity().map(ModelCacheAffinity::identity),
        ResponsesStoragePolicy::Stateless,
    )
}

/// OpenAI Platform and xAI Platform behavior match. OpenRouter keeps its compact rules.
#[must_use]
pub fn request_matches_configuration(
    request: &ModelRequest,
    behavior: &ProviderBehaviorConfiguration,
) -> bool {
    let reasoning = match (request.reasoning(), behavior.reasoning_effort) {
        (ReasoningPolicy::Disabled, ReasoningEffortDisposition::Disabled) => true,
        (
            ReasoningPolicy::Enabled {
                effort: Some(requested),
                requirement: CapabilityRequirement::Required,
            },
            ReasoningEffortDisposition::Enabled(configured),
        ) => requested == configured,
        _ => false,
    };
    reasoning
        && request.output() == &behavior.output
        && request.cache() == behavior.cache
        && behavior.matches_request_execution_controls(request)
        && request.context_admission_requirement() == behavior.context_evidence_requirement
        && request.limits().stop_sequences().is_empty()
        && request.native().entries().is_empty()
        && request.tools().len() == behavior.tools.len()
        && request.hosted_tools() == behavior.hosted_tools
        && request
            .tools()
            .iter()
            .zip(&behavior.tools)
            .all(|(request, configured)| request == configured)
}

fn active_replay(
    label: &str,
    context: &ModelPreparationContext,
    request: &ModelRequest,
    authority: &ResponsesRequestAuthority,
    output_item: ResponsesReplayKind<'_>,
    previous_response: Option<ResponsesReplayKind<'_>>,
) -> Result<Option<ResponsesArtifactReplayBridge>, ModelPreparationError> {
    // An artifact for a different canonical request prepares fresh.
    context
        .active_family_artifact()
        .map(|active| {
            ResponsesArtifactReplayBridge::from_active_artifact(
                active,
                request,
                authority.clone(),
                output_item,
                None,
                previous_response,
                context.prepared_at(),
            )
        })
        .transpose()
        .or_else(|error| {
            error
                .artifact_is_not_for_this_request()
                .then_some(None)
                .ok_or(error)
        })
        .map_err(|_| {
            labeled(
                label,
                ModelPreparationErrorKind::InvalidRequest,
                "active Responses artifact is not reusable",
            )
        })
}

fn seal_prepared_record(
    label: &str,
    descriptor: &ResolvedModelDescriptor,
    plan: &ValidatedRequestPlan,
    context: &ModelPreparationContext,
    attempt: &PreparedResponsesAttempt,
    revisions: (&str, &str, &str),
    cache_use: PreparedCacheUse,
) -> Result<PreparedCallRecord, ModelPreparationError> {
    let (adapter_revision, projection_revision, accounting_revision) = revisions;
    let wire_digest = attempt.wire_digest();
    let wire_size = u64::try_from(attempt.wire_size()).map_err(|_| {
        labeled(
            label,
            ModelPreparationErrorKind::ResourceLimit,
            "prepared request exceeds addressable size",
        )
    })?;
    let count_observation = descriptor
        .context_contract()
        .and_then(|contract| contract.measurements().first())
        .map(|certificate| {
            plakous_model::CountObservation::new(
                certificate.certificate_id(),
                certificate.revision(),
                wire_size,
            )
            .map_err(|_| {
                labeled(
                    label,
                    ModelPreparationErrorKind::InvalidConfiguration,
                    "wire measurement is invalid",
                )
            })
        })
        .transpose()?;
    let body = attempt.body();
    let token_projection = PreparedTokenProjection::new(wire_digest, wire_size, count_observation)
        .map_err(ModelPreparationError::from)?
        .with_input_parts(plakous_model::ContextInputParts::measure_wire(
            body.as_ref(),
        ))
        .with_wire_input(
            body.as_ref(),
            Some(plakous_model::CONSERVATIVE_IMAGE_MAX_TOKENS),
            context,
        )?;
    let output_limit = enforced_output_limit(
        attempt.enforced_output_tokens(),
        wire_digest,
        adapter_revision,
    )
    .map_err(ModelPreparationError::from)?;
    let family = ProtocolId::new(RESPONSES_FAMILY).map_err(|_| {
        labeled(
            label,
            ModelPreparationErrorKind::InvalidConfiguration,
            "Responses family identity is invalid",
        )
    })?;
    let projection = PreparedModelProjection::new(
        descriptor,
        plan,
        context,
        family,
        adapter_revision,
        projection_revision,
        accounting_revision,
        wire_digest,
        wire_size,
        token_projection,
        cache_use,
        output_limit,
    )
    .map_err(ModelPreparationError::from)?;
    PreparedCallRecord::for_prepared(descriptor, plan, projection, context)
}

fn enforced_output_limit(
    tokens: Option<std::num::NonZeroU32>,
    wire_digest: Digest,
    lowering_revision: &str,
) -> Result<Option<PreparedOutputLimit>, PreparedCallError> {
    tokens
        .map(|tokens| {
            PreparedOutputLimit::enforced_wire_maximum(
                tokens,
                "max_output_tokens",
                wire_digest,
                lowering_revision,
            )
        })
        .transpose()
}

fn insert_public(
    headers: &mut HeaderMap,
    bindings: &mut Vec<ResponsesHeaderBinding>,
    name: HeaderName,
    value: &[u8],
) -> Result<(), ResponsesDispatchRejection> {
    let parsed = HeaderValue::from_bytes(value).map_err(|_| ResponsesDispatchRejection::Binding)?;
    headers.insert(name.clone(), parsed);
    bindings.push(ResponsesHeaderBinding::PublicExact {
        name,
        value: value.to_vec(),
    });
    Ok(())
}

fn labeled(label: &str, kind: ModelPreparationErrorKind, message: &str) -> ModelPreparationError {
    ModelPreparationError::rejected(kind, format!("{label} {message}"))
}

#[cfg(test)]
mod tests {
    use plakous_model::{Digest, DigestDomain, PreparedOutputLimitSource};
    use std::num::NonZeroU32;

    #[test]
    fn stateless_stream_requires_both_fields_and_the_exact_model() {
        let body = serde_json::json!({"stream": true, "store": false, "model": "m"});
        let bytes = body.to_string();
        assert!(super::stateless_stream_body(bytes.as_bytes(), Some("m")).is_ok());
        assert!(super::stateless_stream_body(bytes.as_bytes(), Some("other")).is_err());
        assert!(super::stateless_stream_body(bytes.as_bytes(), None).is_ok());
        assert!(super::stateless_stream_body(br#"{"stream":true}"#, None).is_err());
        assert!(
            super::stateless_stream_body(br#"{"stream":true,"store":true}"#, Some("m")).is_err()
        );
    }

    #[test]
    fn emitted_output_bound_retains_exact_wire_projection_proof()
    -> Result<(), Box<dyn std::error::Error>> {
        let wire_digest =
            Digest::sha256(DigestDomain::Blob, 1, &[br#"{"max_output_tokens":128}"#])?;
        let limit =
            super::enforced_output_limit(NonZeroU32::new(128), wire_digest, "route-adapter")?
                .ok_or("emitted output maximum was absent")?;
        assert_eq!(limit.tokens().get(), 128);
        assert!(matches!(
            limit.source(),
            PreparedOutputLimitSource::EnforcedWireMaximum {
                wire_field,
                wire_digest: proof_wire_digest,
                lowering_revision,
            } if wire_field == "max_output_tokens"
                && *proof_wire_digest == wire_digest
                && lowering_revision == "route-adapter"
        ));
        assert_eq!(
            super::enforced_output_limit(None, wire_digest, "route-adapter")?,
            None
        );
        Ok(())
    }
}
