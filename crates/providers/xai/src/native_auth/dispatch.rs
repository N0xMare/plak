//! Provider-specific request preflight and response-head guard ownership.

#[cfg(test)]
use super::{Arc, SubscriptionRequestGate};

use super::{
    AUTHENTICATE_RESPONSE_VALUE, Client, CredentialGenerationFingerprint, Future, HEADER_ACCEPT,
    HEADER_AUTHENTICATE_RESPONSE, HEADER_AUTHORIZATION, HEADER_CONTENT_TYPE,
    HEADER_GROK_CLIENT_IDENTIFIER, HEADER_GROK_CLIENT_MODE, HEADER_GROK_CLIENT_VERSION,
    HEADER_GROK_CONV_ID, HEADER_USER_AGENT, HEADER_XAI_TOKEN_AUTH, HeaderMap, HeaderValue, Instant,
    Method, Origin, PLAKOUS_XAI_CLIENT_IDENTIFIER, PLAKOUS_XAI_CLIENT_MODE, Request,
    ResponsesAuthSlot, ResponsesCredentialAuthority, ResponsesDispatchRejection,
    ResponsesHeaderBinding, ResponsesPreflightFuture, ResponsesPreflightRequest,
    ResponsesPreparedPhysicalRequest, ResponsesRevalidationRequest,
    SUBSCRIPTION_INPUT_TOKEN_OVERHEAD, SUPERGROK_CLIENT_COMPATIBILITY_VERSION, SensitiveBytes,
    SubscriptionRequestPermit, USER_AGENT, XAI_TOKEN_AUTH_VALUE, XaiDialect, XaiResolvedModel,
    bearer_header, fmt, insert_subscription_inference_headers,
};

/// Immutable native SuperGrok construction data used by shared preflight.
pub(crate) struct SuperGrokResponsesExecution {
    resolved: XaiResolvedModel,
    client: Client,
    #[cfg(test)]
    request_gate: Option<Arc<dyn SubscriptionRequestGate>>,
}

impl SuperGrokResponsesExecution {
    pub(crate) fn new(resolved: XaiResolvedModel, client: Client) -> Option<Self> {
        let route = resolved.route();
        (route.is_routable()
            && route.responses_path() == Some(crate::SUPERGROK_RESPONSES_PATH)
            && supergrok_origin_allowed(route.origin()?.as_str()))
        .then_some(Self {
            resolved,
            client,
            #[cfg(test)]
            request_gate: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_request_gate(&mut self, gate: Arc<dyn SubscriptionRequestGate>) {
        self.request_gate = Some(gate);
    }
}

/// OAuth grant, inference permit, and exact route retained through response head.
pub(crate) struct SuperGrokDispatchGuard {
    grant: crate::XaiSubscriptionCredentialGrant,
    permit: Option<Box<dyn SubscriptionRequestPermit>>,
}

impl SuperGrokDispatchGuard {
    pub(crate) fn transport_failed(&mut self, failure: plakous_http::Failure) {
        if failure.transmission() == plakous_http::TransmissionFact::NotStarted
            && let Some(permit) = &mut self.permit
        {
            let _ = permit.refund_not_started();
        }
    }
}

impl plakous_responses_dialect::ResponsesRetrievalGuard for SuperGrokDispatchGuard {
    fn transport_failed(&mut self, failure: plakous_http::Failure) {
        Self::transport_failed(self, failure);
    }
}

impl fmt::Debug for SuperGrokDispatchGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SuperGrokDispatchGuard(<redacted lease-bound authority>)")
    }
}

pub(crate) fn supergrok_responses_preflight<'a>(
    dialect: &'a SuperGrokResponsesExecution,
    request: ResponsesPreflightRequest<'a>,
) -> ResponsesPreflightFuture<'a, SuperGrokDispatchGuard> {
    Box::pin(async move {
        if request.cancellation().is_cancelled() || Instant::now() >= request.deadline() {
            return Err(ResponsesDispatchRejection::Unavailable);
        }
        let route = dialect.resolved.route();
        let identity = route
            .identity()
            .ok_or(ResponsesDispatchRejection::Binding)?;
        if !supergrok_origin_allowed(identity.origin().as_str())
            || route.responses_path() != Some(crate::SUPERGROK_RESPONSES_PATH)
            || dialect.resolved.route().dialect() != XaiDialect::GrokSubscriptionResponses2026_07
        {
            return Err(ResponsesDispatchRejection::Binding);
        }
        let body: serde_json::Value = serde_json::from_slice(&request.body())
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        if body.get("store") != Some(&serde_json::Value::Bool(false))
            || body.get("include") != Some(&serde_json::json!(["reasoning.encrypted_content"]))
        {
            return Err(ResponsesDispatchRejection::Binding);
        }
        let authority = route
            .credential_authority()
            .ok_or(ResponsesDispatchRejection::Authentication)?;
        let input_liability = u64::try_from(request.body().len())
            .ok()
            .and_then(|bytes| bytes.checked_add(SUBSCRIPTION_INPUT_TOKEN_OVERHEAD))
            .ok_or(ResponsesDispatchRejection::Admission)?;
        // Refresh, then reserve inference, then materialize the grant. A failed
        // refresh must not consume the coding-atom reservation.
        authority
            .refresh_if_needed()
            .await
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        authority
            .preflight()
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        #[cfg(test)]
        let permit = match &dialect.request_gate {
            Some(gate) => gate
                .acquire_inference(input_liability)
                .map_err(|_| ResponsesDispatchRejection::Admission)?,
            None => authority
                .acquire_inference_permit(input_liability)
                .map_err(|_| ResponsesDispatchRejection::Admission)?
                .ok_or(ResponsesDispatchRejection::Admission)?,
        };
        #[cfg(not(test))]
        let permit = authority
            .acquire_inference_permit(input_liability)
            .map_err(|_| ResponsesDispatchRejection::Admission)?
            .ok_or(ResponsesDispatchRejection::Admission)?;
        let grant = authority
            .acquire()
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        let generation = supergrok_generation_fingerprint(
            authority.credential_generation(),
            authority.selection_epoch(),
        );
        let credential_authority = ResponsesCredentialAuthority::primary(generation);
        let origin = Origin::parse(identity.origin().as_str())
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(
            HEADER_CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            HEADER_AUTHORIZATION,
            bearer_header(grant.expose())
                .map_err(|_| ResponsesDispatchRejection::Authentication)?,
        );
        headers.insert(HEADER_USER_AGENT, HeaderValue::from_static(USER_AGENT));
        insert_subscription_inference_headers(&mut headers);
        let conv_id = request
            .authority()
            .cache_affinity()
            .map(|affinity| affinity.to_string());
        if let Some(ref conv_id) = conv_id {
            headers.insert(
                HEADER_GROK_CONV_ID,
                HeaderValue::from_str(conv_id).map_err(|_| ResponsesDispatchRejection::Binding)?,
            );
        }
        let wire = Request::new(
            origin,
            Method::POST,
            crate::SUPERGROK_RESPONSES_PATH,
            headers,
            SensitiveBytes::new(request.body().to_vec()).into_bytes(),
            dialect.client.limits(),
        )
        .map_err(|_| ResponsesDispatchRejection::Binding)?;
        let mut header_bindings = vec![
            ResponsesHeaderBinding::PublicExact {
                name: HEADER_ACCEPT,
                value: b"text/event-stream".to_vec(),
            },
            ResponsesHeaderBinding::PublicExact {
                name: HEADER_CONTENT_TYPE,
                value: b"application/json".to_vec(),
            },
            ResponsesHeaderBinding::CredentialSecret {
                name: HEADER_AUTHORIZATION,
                slot: ResponsesAuthSlot::Primary,
            },
            ResponsesHeaderBinding::CredentialMarkerExact {
                name: HEADER_XAI_TOKEN_AUTH,
                value: XAI_TOKEN_AUTH_VALUE.as_bytes().to_vec(),
                slot: ResponsesAuthSlot::Primary,
            },
            ResponsesHeaderBinding::PublicExact {
                name: HEADER_AUTHENTICATE_RESPONSE,
                value: AUTHENTICATE_RESPONSE_VALUE.as_bytes().to_vec(),
            },
            ResponsesHeaderBinding::PublicExact {
                name: HEADER_GROK_CLIENT_VERSION,
                value: SUPERGROK_CLIENT_COMPATIBILITY_VERSION.as_bytes().to_vec(),
            },
            ResponsesHeaderBinding::PublicExact {
                name: HEADER_GROK_CLIENT_IDENTIFIER,
                value: PLAKOUS_XAI_CLIENT_IDENTIFIER.as_bytes().to_vec(),
            },
            ResponsesHeaderBinding::PublicExact {
                name: HEADER_GROK_CLIENT_MODE,
                value: PLAKOUS_XAI_CLIENT_MODE.as_bytes().to_vec(),
            },
            ResponsesHeaderBinding::PublicExact {
                name: HEADER_USER_AGENT,
                value: USER_AGENT.as_bytes().to_vec(),
            },
        ];
        if let Some(ref conv_id) = conv_id {
            header_bindings.push(ResponsesHeaderBinding::PublicExact {
                name: HEADER_GROK_CONV_ID,
                value: conv_id.as_bytes().to_vec(),
            });
        }
        Ok(ResponsesPreparedPhysicalRequest::new(
            request.physical().clone(),
            wire,
            credential_authority,
            header_bindings,
            SuperGrokDispatchGuard {
                grant,
                permit: Some(permit),
            },
        ))
    })
}

/// Authorizes one physical hosted-retrieval side request carrying `body` with
/// the model round's sequence: refresh, credential preflight, inference
/// permit, grant, dispatch authorization, then grant revalidation. The guard
/// retains the grant and permit through the response head.
pub(crate) async fn supergrok_side_request(
    execution: &SuperGrokResponsesExecution,
    path: &'static str,
    accept: &'static str,
    body: &[u8],
) -> Result<(Request, SuperGrokDispatchGuard), &'static str> {
    let route = execution.resolved.route();
    let identity = route.identity().ok_or("SuperGrok route is not routable")?;
    if !supergrok_origin_allowed(identity.origin().as_str())
        || route.responses_path() != Some(crate::SUPERGROK_RESPONSES_PATH)
        || route.dialect() != XaiDialect::GrokSubscriptionResponses2026_07
    {
        return Err("SuperGrok route binding changed");
    }
    let authority = route
        .credential_authority()
        .ok_or("SuperGrok credential authority is unavailable")?;
    let input_liability = u64::try_from(body.len())
        .ok()
        .and_then(|bytes| bytes.checked_add(SUBSCRIPTION_INPUT_TOKEN_OVERHEAD))
        .ok_or("SuperGrok side request exceeds the inference bound")?;
    authority
        .refresh_if_needed()
        .await
        .map_err(|_| "SuperGrok login could not be refreshed")?;
    authority
        .preflight()
        .map_err(|_| "SuperGrok login is not ready")?;
    #[cfg(test)]
    let permit = match &execution.request_gate {
        Some(gate) => gate.acquire_inference(input_liability).ok(),
        None => authority
            .acquire_inference_permit(input_liability)
            .ok()
            .flatten(),
    };
    #[cfg(not(test))]
    let permit = authority
        .acquire_inference_permit(input_liability)
        .ok()
        .flatten();
    let mut permit = permit.ok_or("SuperGrok inference permit was refused")?;
    let grant = authority
        .acquire()
        .map_err(|_| "SuperGrok grant is unavailable")?;
    let origin =
        Origin::parse(identity.origin().as_str()).map_err(|_| "SuperGrok origin is invalid")?;
    let mut headers = HeaderMap::new();
    headers.insert(HEADER_ACCEPT, HeaderValue::from_static(accept));
    headers.insert(
        HEADER_CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HEADER_AUTHORIZATION,
        bearer_header(grant.expose()).map_err(|_| "SuperGrok grant is not a header value")?,
    );
    headers.insert(HEADER_USER_AGENT, HeaderValue::from_static(USER_AGENT));
    insert_subscription_inference_headers(&mut headers);
    let wire = Request::new(
        origin,
        Method::POST,
        path,
        headers,
        SensitiveBytes::new(body.to_vec()).into_bytes(),
        execution.client.limits(),
    )
    .map_err(|_| "SuperGrok side request could not be built")?;
    permit
        .authorize_dispatch()
        .map_err(|_| "SuperGrok inference permit was refused")?;
    authority
        .revalidate(&grant)
        .map_err(|_| "SuperGrok grant is no longer current")?;
    Ok((
        wire,
        SuperGrokDispatchGuard {
            grant,
            permit: Some(permit),
        },
    ))
}

/// Credential-rejection hook for the shared Responses attempt host: after an
/// HTTP 401 head, force one claimed refresh so the single re-dispatch carries
/// the renewed grant (native `grok` parity: refresh-and-retry on 401).
pub(crate) fn supergrok_credential_rejected(
    dialect: &SuperGrokResponsesExecution,
) -> std::pin::Pin<Box<dyn Future<Output = Result<(), ResponsesDispatchRejection>> + Send + '_>> {
    Box::pin(async move {
        let authority = dialect
            .resolved
            .route()
            .credential_authority()
            .ok_or(ResponsesDispatchRejection::Authentication)?;
        authority
            .refresh_after_rejection()
            .await
            .map_err(|_| ResponsesDispatchRejection::Authentication)
    })
}

pub(crate) fn revalidate_supergrok_responses(
    dialect: &SuperGrokResponsesExecution,
    mut request: ResponsesRevalidationRequest<'_, SuperGrokDispatchGuard>,
) -> Result<(), ResponsesDispatchRejection> {
    if request.cancellation().is_cancelled() || Instant::now() >= request.deadline() {
        return Err(ResponsesDispatchRejection::Unavailable);
    }
    let route = dialect.resolved.route();
    let identity = route
        .identity()
        .ok_or(ResponsesDispatchRejection::Binding)?;
    let authority = route
        .credential_authority()
        .ok_or(ResponsesDispatchRejection::Authentication)?;
    let expected = supergrok_generation_fingerprint(
        authority.credential_generation(),
        authority.selection_epoch(),
    );
    if request
        .binding()
        .credential_authority()
        .generation(ResponsesAuthSlot::Primary)
        != Some(expected)
        || request.binding().route().version() == 0
        || !supergrok_origin_allowed(identity.origin().as_str())
    {
        return Err(ResponsesDispatchRejection::Binding);
    }
    let guard = request.response_head_guard_mut();
    if let Some(permit) = &mut guard.permit {
        permit
            .authorize_dispatch()
            .map_err(|_| ResponsesDispatchRejection::Admission)?;
    }
    authority
        .revalidate(&guard.grant)
        .map_err(|_| ResponsesDispatchRejection::Authentication)?;
    Ok(())
}

pub(super) fn supergrok_generation_fingerprint(
    generation: u64,
    selection_epoch: &str,
) -> CredentialGenerationFingerprint {
    let mut binding = Vec::with_capacity(32 + selection_epoch.len());
    binding.extend_from_slice(b"plakous-xai-native-generation-v1");
    binding.extend_from_slice(&generation.to_be_bytes());
    binding.extend_from_slice(&(selection_epoch.len() as u64).to_be_bytes());
    binding.extend_from_slice(selection_epoch.as_bytes());
    match CredentialGenerationFingerprint::mint_exact(
        plakous_responses_dialect::CredentialProviderTag::XaiPlatform,
        &binding,
    ) {
        Ok(fingerprint) => fingerprint,
        Err(_) => unreachable!("native generation binding is nonempty and bounded"),
    }
}

fn supergrok_origin_allowed(origin: &str) -> bool {
    if origin == crate::SUPERGROK_RESPONSES_ORIGIN {
        return true;
    }
    #[cfg(test)]
    {
        origin.strip_prefix("https://127.0.0.1:").is_some()
            || origin.strip_prefix("https://localhost:").is_some()
    }
    #[cfg(not(test))]
    {
        false
    }
}
