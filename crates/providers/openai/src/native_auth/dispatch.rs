//! Provider-specific request preflight and response-head guard ownership.

use super::{
    AuthError, Client, EndpointOrigin, HeaderMap, HeaderName, HeaderValue, Instant, Method,
    NativeCodexAuthorization, NativeCodexPreflightRequest, OPENAI_CHATGPT_ORIGIN, OpenAiTarget,
    Origin, Request, ResponsesAccountSlot, ResponsesAuthSlot, ResponsesCredentialAuthority,
    ResponsesDispatchRejection, ResponsesHeaderBinding, ResponsesPreflightFuture,
    ResponsesPreflightRequest, ResponsesPreparedPhysicalRequest, ResponsesRequestAuthority,
    ResponsesRevalidationRequest, SUBSCRIPTION_INPUT_TOKEN_OVERHEAD, SensitiveBytes, StorageMode,
    SubscriptionRequestPermit, USER_AGENT,
};

/// Immutable native Codex facts supplied to the shared Responses attempt host.
pub(crate) struct CodexResponsesDialect {
    target: OpenAiTarget,
    client: Client,
}

impl CodexResponsesDialect {
    pub(crate) fn new(target: OpenAiTarget, client: Client) -> Self {
        Self { target, client }
    }
}

pub(crate) struct CodexDispatchGuard {
    authorization: NativeCodexAuthorization,
    permit: Box<dyn SubscriptionRequestPermit>,
}

impl CodexDispatchGuard {
    pub(crate) fn transport_failed(&mut self, failure: plakous_http::Failure) {
        if failure.transmission() == plakous_http::TransmissionFact::NotStarted {
            let _ = self.permit.refund_not_started();
        }
    }
}

impl plakous_responses_dialect::ResponsesRetrievalGuard for CodexDispatchGuard {
    fn transport_failed(&mut self, failure: plakous_http::Failure) {
        Self::transport_failed(self, failure);
    }
}

/// Credential-rejection hook for the shared Responses attempt host: after an
/// HTTP 401 head, force one claimed refresh of the rejected receipt's own login
/// selection so the single re-dispatch carries the renewed grant. A rejected grant fails closed here and the original 401
/// is returned unchanged.
pub(crate) fn codex_credential_rejected(
    dialect: &CodexResponsesDialect,
) -> std::pin::Pin<Box<dyn Future<Output = Result<(), ResponsesDispatchRejection>> + Send + '_>> {
    Box::pin(async move {
        let (rejected, selection_epoch) = dialect
            .target
            .native_codex_credential_selection()
            .ok_or(ResponsesDispatchRejection::Authentication)?;
        dialect
            .target
            .route()
            .native_codex_refresh_after_rejection(rejected, selection_epoch)
            .await
            .map_err(|_| ResponsesDispatchRejection::Authentication)
    })
}

pub(crate) fn codex_responses_preflight<'a>(
    dialect: &'a CodexResponsesDialect,
    request: ResponsesPreflightRequest<'a>,
) -> ResponsesPreflightFuture<'a, CodexDispatchGuard> {
    Box::pin(async move {
        if request.cancellation().is_cancelled() || Instant::now() >= request.deadline() {
            return Err(ResponsesDispatchRejection::Unavailable);
        }
        dialect
            .target
            .validate_current_route_evidence()
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        validate_codex_request_authority(&dialect.target, request.authority())?;
        if !codex_origin_allowed(dialect.target.route().origin())
            || dialect.target.route().responses_path() != crate::OPENAI_CODEX_RESPONSES_PATH
            || dialect.target.route().storage() != StorageMode::Stateless
        {
            return Err(ResponsesDispatchRejection::Binding);
        }
        let body: serde_json::Value = serde_json::from_slice(&request.body())
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        // `reasoning.summary` is either absent (no effort on the wire) or the
        // official client's default `"auto"`; any other summary mode is not
        // the reviewed Codex request and fails closed here.
        let summary_is_official = body.get("reasoning").is_none_or(|reasoning| {
            reasoning
                .get("summary")
                .is_none_or(|summary| summary == &serde_json::Value::from("auto"))
        });
        if body.get("store") != Some(&serde_json::Value::Bool(false))
            || body.get("include")
                != Some(&serde_json::json!([
                    crate::OPENAI_CODEX_ENCRYPTED_REASONING_INCLUDE
                ]))
            || !summary_is_official
        {
            return Err(ResponsesDispatchRejection::Binding);
        }
        let responses_lite = dialect.target.codex_responses_lite();
        let lite_prefix = body
            .get("input")
            .and_then(serde_json::Value::as_array)
            .and_then(|input| input.first())
            .and_then(|first| first.get("type"))
            .and_then(serde_json::Value::as_str)
            == Some("additional_tools");
        // Codex carries instructions only as an input developer message; a
        // tool-less Lite request has no `additional_tools` prefix.
        if body.get("instructions").is_some()
            || (!responses_lite && lite_prefix)
            || (responses_lite && body.get("tools").is_some())
        {
            return Err(ResponsesDispatchRejection::Binding);
        }
        let input_liability = u64::try_from(request.body().len())
            .ok()
            .and_then(crate::model::codex_complete_input_token_upper_bound)
            .and_then(|tokens| tokens.checked_add(SUBSCRIPTION_INPUT_TOKEN_OVERHEAD))
            .ok_or(ResponsesDispatchRejection::Admission)?;
        let credential_request = NativeCodexPreflightRequest::new(
            dialect.target.clone(),
            request.physical().clone(),
            request.deadline(),
            request.cancellation().clone(),
        );
        // Official Codex/nanocodex refresh the access JWT, then POST. Charge the
        // inference class only after credential preflight succeeds so a failed
        // refresh cannot consume the coding-atom reservation budget.
        let mut authorization = dialect
            .target
            .route()
            .native_codex_preflight(credential_request)
            .await
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        dialect
            .target
            .route()
            .bind_native_codex_authorization(&mut authorization, request.physical().clone())
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        let permit = dialect
            .target
            .route()
            .acquire_native_codex_inference(input_liability)
            .map_err(|error| match error {
                AuthError::InferenceInputLimit => ResponsesDispatchRejection::InferenceInput,
                AuthError::InferenceClassLimit {
                    reserved,
                    allowance,
                } => ResponsesDispatchRejection::InferenceClass {
                    reserved,
                    allowance,
                },
                // No classified gate was installed on the manager.
                AuthError::Unavailable => ResponsesDispatchRejection::Binding,
                _ => ResponsesDispatchRejection::Admission,
            })?;
        let credential_generation = authorization.credential_generation();
        let origin = Origin::parse(dialect.target.route().origin().as_str())
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        authorization
            .authorize_headers(&dialect.target, &mut headers)
            .map_err(|_| ResponsesDispatchRejection::Authentication)?;
        let compact_headers = codex_compact_compat_headers(&body)?;
        for (name, value) in &compact_headers {
            headers.insert(name.clone(), value.clone());
        }
        if let Some((name, value)) = remote_compaction_v2_header(&body) {
            headers.insert(name.clone(), value.clone());
        }
        if responses_lite {
            headers.insert(
                HeaderName::from_static("x-openai-internal-codex-responses-lite"),
                HeaderValue::from_static("true"),
            );
        }
        let physical = request.physical().clone();
        let wire = Request::new(
            origin,
            Method::POST,
            dialect.target.route().responses_path(),
            headers,
            SensitiveBytes::new(request.body().to_vec()).into_bytes(),
            dialect.client.limits(),
        )
        .map_err(|_| ResponsesDispatchRejection::Binding)?;
        let account_authority = dialect
            .target
            .route()
            .account_binding()
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        let credential_authority = ResponsesCredentialAuthority::secondary(credential_generation)
            .with_account(ResponsesAccountSlot::Secondary, account_authority)
            .map_err(|_| ResponsesDispatchRejection::Binding)?;
        let mut header_bindings = vec![
            ResponsesHeaderBinding::PublicExact {
                name: HeaderName::from_static("accept"),
                value: b"text/event-stream".to_vec(),
            },
            ResponsesHeaderBinding::PublicExact {
                name: HeaderName::from_static("content-type"),
                value: b"application/json".to_vec(),
            },
            ResponsesHeaderBinding::CredentialSecret {
                name: HeaderName::from_static("authorization"),
                slot: ResponsesAuthSlot::Secondary,
            },
            ResponsesHeaderBinding::AccountSecret {
                name: HeaderName::from_static("chatgpt-account-id"),
                slot: ResponsesAccountSlot::Secondary,
                authority: account_authority,
            },
            ResponsesHeaderBinding::PublicExact {
                name: HeaderName::from_static("originator"),
                value: b"plak".to_vec(),
            },
            ResponsesHeaderBinding::PublicExact {
                name: HeaderName::from_static("user-agent"),
                value: USER_AGENT.as_bytes().to_vec(),
            },
        ];
        for (name, value) in compact_headers {
            header_bindings.push(ResponsesHeaderBinding::PublicExact {
                name,
                value: value.as_bytes().to_vec(),
            });
        }
        if let Some((name, value)) = remote_compaction_v2_header(&body) {
            header_bindings.push(ResponsesHeaderBinding::PublicExact {
                name,
                value: value.as_bytes().to_vec(),
            });
        }
        if responses_lite {
            header_bindings.push(ResponsesHeaderBinding::PublicExact {
                name: HeaderName::from_static("x-openai-internal-codex-responses-lite"),
                value: b"true".to_vec(),
            });
        }
        Ok(ResponsesPreparedPhysicalRequest::new(
            physical,
            wire,
            credential_authority,
            header_bindings,
            CodexDispatchGuard {
                authorization,
                permit,
            },
        ))
    })
}

/// Authorizes one physical Codex search side request carrying `body` with
/// the model round's sequence: route evidence, credential preflight bound to
/// a fresh side-request physical binding, inference permit, Codex headers,
/// then authorization revalidation and dispatch authorization. The guard
/// retains the credential lease and permit through the response head.
pub(crate) async fn codex_side_request(
    target: &OpenAiTarget,
    client: &Client,
    body: &[u8],
    deadline: Instant,
    cancellation: &plakous_model::CancellationToken,
) -> Result<(Request, CodexDispatchGuard), &'static str> {
    target
        .validate_current_route_evidence()
        .map_err(|_| "Codex route evidence is not current")?;
    if !codex_origin_allowed(target.route().origin())
        || target.route().storage() != StorageMode::Stateless
    {
        return Err("Codex route binding changed");
    }
    let attempt = plakous_model::AttemptId::generate(
        &plakous_model::SystemClock,
        &plakous_model::SystemIdGenerator,
    )
    .map_err(|_| "Codex side request id could not be generated")?;
    let operation = plakous_responses_dialect::ResponsesOperationBinding::retrieval_side_request(
        body,
        plakous_attempt::LedgerPolicy::OPENAI_STRICT,
        plakous_responses_dialect::ResponsesAuthSlots::Secondary,
    )
    .map_err(|_| "Codex side request binding could not be built")?;
    let physical = plakous_attempt::PhysicalRequestBinding::new(
        plakous_attempt::AttemptBinding::new(operation, attempt),
        1,
    );
    let input_liability = u64::try_from(body.len())
        .ok()
        .and_then(crate::model::codex_complete_input_token_upper_bound)
        .and_then(|tokens| tokens.checked_add(SUBSCRIPTION_INPUT_TOKEN_OVERHEAD))
        .ok_or("Codex side request exceeds the inference bound")?;
    let mut authorization = target
        .route()
        .native_codex_preflight(NativeCodexPreflightRequest::new(
            target.clone(),
            physical.clone(),
            deadline,
            cancellation.clone(),
        ))
        .await
        .map_err(|_| "Codex login could not be authorized")?;
    target
        .route()
        .bind_native_codex_authorization(&mut authorization, physical.clone())
        .map_err(|_| "Codex login changed")?;
    let mut permit = target
        .route()
        .acquire_native_codex_inference(input_liability)
        .map_err(|_| "Codex inference permit was refused")?;
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("accept"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    authorization
        .authorize_headers(target, &mut headers)
        .map_err(|_| "Codex login could not be authorized")?;
    let origin =
        Origin::parse(target.route().origin().as_str()).map_err(|_| "Codex origin is invalid")?;
    let wire = Request::new(
        origin,
        Method::POST,
        crate::CODEX_SEARCH_PATH,
        headers,
        SensitiveBytes::new(body.to_vec()).into_bytes(),
        client.limits(),
    )
    .map_err(|_| "Codex side request could not be built")?;
    authorization
        .revalidate(target, &physical)
        .map_err(|_| "Codex login changed")?;
    permit
        .authorize_dispatch()
        .map_err(|_| "Codex inference permit was refused")?;
    Ok((
        wire,
        CodexDispatchGuard {
            authorization,
            permit,
        },
    ))
}

pub(super) fn remote_compaction_v2_header(
    body: &serde_json::Value,
) -> Option<(HeaderName, HeaderValue)> {
    let trigger = body
        .get("input")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("compaction_trigger")
            })
        });
    trigger.then(|| {
        (
            HeaderName::from_static("x-codex-beta-features"),
            HeaderValue::from_static("remote_compaction_v2"),
        )
    })
}

pub(super) fn codex_compact_compat_headers(
    body: &serde_json::Value,
) -> Result<Vec<(HeaderName, HeaderValue)>, ResponsesDispatchRejection> {
    let Some(metadata) = body.get("client_metadata") else {
        return Ok(Vec::new());
    };
    let Some(window_id) = metadata
        .get("x-codex-window-id")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(Vec::new());
    };
    let Some(turn_metadata) = metadata
        .get("x-codex-turn-metadata")
        .and_then(serde_json::Value::as_str)
    else {
        return Err(ResponsesDispatchRejection::Binding);
    };
    let window =
        HeaderValue::from_str(window_id).map_err(|_| ResponsesDispatchRejection::Binding)?;
    let turn =
        HeaderValue::from_str(turn_metadata).map_err(|_| ResponsesDispatchRejection::Binding)?;
    Ok(vec![
        (HeaderName::from_static("x-codex-window-id"), window),
        (HeaderName::from_static("x-codex-turn-metadata"), turn),
    ])
}

pub(crate) fn revalidate_codex_responses(
    dialect: &CodexResponsesDialect,
    mut request: ResponsesRevalidationRequest<'_, CodexDispatchGuard>,
) -> Result<(), ResponsesDispatchRejection> {
    if request.cancellation().is_cancelled() || Instant::now() >= request.deadline() {
        return Err(ResponsesDispatchRejection::Unavailable);
    }
    dialect
        .target
        .validate_current_route_evidence()
        .map_err(|_| ResponsesDispatchRejection::Binding)?;
    validate_codex_request_authority(&dialect.target, request.authority())?;
    let physical = request.binding().physical().clone();
    let credential_generation = request
        .binding()
        .credential_authority()
        .generation(ResponsesAuthSlot::Secondary)
        .ok_or(ResponsesDispatchRejection::Binding)?;
    let guard = request.response_head_guard_mut();
    if guard.authorization.credential_generation() != credential_generation {
        return Err(ResponsesDispatchRejection::Binding);
    }
    guard
        .authorization
        .revalidate(&dialect.target, &physical)
        .map_err(|_| ResponsesDispatchRejection::Authentication)?;
    guard
        .permit
        .authorize_dispatch()
        .map_err(|_| ResponsesDispatchRejection::Admission)?;
    Ok(())
}

pub(super) fn validate_codex_request_authority(
    target: &OpenAiTarget,
    authority: &ResponsesRequestAuthority,
) -> Result<(), ResponsesDispatchRejection> {
    let binding = authority.artifact_binding();
    let descriptor = target.descriptor();
    let account = target
        .route()
        .account_binding()
        .map_err(|_| ResponsesDispatchRejection::Binding)?;
    let semantic_model = descriptor
        .semantic_binding_digest()
        .map_err(|_| ResponsesDispatchRejection::Binding)?;
    if binding.dialect() != descriptor.protocol()
        || binding.route() != descriptor.route()
        || binding.account() != account
        || binding.model() != descriptor.requested_model()
        || binding.model_family() != descriptor.model_family()
        || binding.adapter() != descriptor.profile()
        || binding.semantic_model() != semantic_model
    {
        return Err(ResponsesDispatchRejection::Binding);
    }
    Ok(())
}

fn codex_origin_allowed(origin: &EndpointOrigin) -> bool {
    if origin.as_str() == OPENAI_CHATGPT_ORIGIN {
        return true;
    }
    #[cfg(test)]
    {
        origin.as_str().strip_prefix("https://127.0.0.1:").is_some()
            || origin.as_str().strip_prefix("https://localhost:").is_some()
    }
    #[cfg(not(test))]
    {
        false
    }
}
