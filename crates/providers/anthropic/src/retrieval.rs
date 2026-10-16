//! Hosted retrieval on the Anthropic API and Claude OAuth routes.
//!
//! [`AnthropicRetrieval`] runs one isolated Messages side request per call on
//! the session's own executor: the same route, key or OAuth grant, account,
//! model and client. Before every physical request it revalidates admission
//! exactly as before a model round; it never falls back to another route or
//! account.

use std::fmt;
use std::sync::Arc;

use plakous_http::{Client, Origin};
use plakous_messages::{
    MessagesRetrievalAuthorization, MessagesRetrievalError, MessagesRetrievalErrorKind,
    MessagesRetrievalLimits, MessagesRetrievalOutcome, MessagesRetrievalQuery,
    MessagesRetrievalRequest, MessagesRetrievalTransport, run_messages_retrieval,
};
use plakous_model::{CancellationToken, ReasoningEffort};

use crate::model::AnthropicLanguageModel;
#[cfg(feature = "native-auth")]
use crate::oauth_model::ClaudeOAuthLanguageModel;
use crate::{AnthropicError, ThinkingSupport};

/// The lowest effort a model's ladder admits, by its Messages name.
pub(crate) fn lowest_effort(efforts: &[ReasoningEffort]) -> Option<&'static str> {
    [
        (ReasoningEffort::Low, "low"),
        (ReasoningEffort::Medium, "medium"),
        (ReasoningEffort::High, "high"),
        (ReasoningEffort::XHigh, "xhigh"),
        (ReasoningEffort::Max, "max"),
    ]
    .into_iter()
    .find(|(effort, _)| efforts.contains(effort))
    .map(|(_, name)| name)
}

enum Executor {
    Api(Arc<AnthropicLanguageModel>),
    #[cfg(feature = "native-auth")]
    ClaudeOAuth {
        model: Arc<ClaudeOAuthLanguageModel>,
        session_id: plakous_model::SessionId,
    },
}

/// One session's hosted web search and page fetch.
pub struct AnthropicRetrieval {
    executor: Executor,
    client: Client,
    dial_origin: Origin,
    effort: Option<&'static str>,
    limits: MessagesRetrievalLimits,
}

impl AnthropicRetrieval {
    pub(crate) fn api(
        model: Arc<AnthropicLanguageModel>,
        effort: Option<&'static str>,
    ) -> Result<Self, AnthropicError> {
        Ok(Self {
            client: model.client().clone(),
            dial_origin: model.dial_origin()?,
            effort: adaptive_effort(model.resolved().model().thinking(), effort),
            executor: Executor::Api(model),
            limits: MessagesRetrievalLimits::default(),
        })
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn claude_oauth(
        model: Arc<ClaudeOAuthLanguageModel>,
        session_id: plakous_model::SessionId,
    ) -> Result<Self, AnthropicError> {
        let dial_origin = Origin::parse(model.resolved().route().origin()).map_err(|_| {
            AnthropicError::new(
                crate::AnthropicErrorKind::InvalidConfiguration,
                crate::AnthropicErrorPhase::Prepare,
                "Claude OAuth origin is invalid",
            )
        })?;
        Ok(Self {
            client: model.client().clone(),
            dial_origin,
            // The reference client's ladder starts at low on every adaptive model.
            effort: adaptive_effort(model.resolved().model().thinking(), Some("low")),
            executor: Executor::ClaudeOAuth { model, session_id },
            limits: MessagesRetrievalLimits::default(),
        })
    }

    /// Replaces the exchange bounds and backoff sample.
    #[must_use]
    pub const fn with_limits(mut self, limits: MessagesRetrievalLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sends side requests to `origin` instead of the route's origin; the
    /// credential and headers stay bound to the official route.
    #[doc(hidden)]
    #[must_use]
    pub fn with_dial_origin(mut self, origin: Origin) -> Self {
        self.dial_origin = origin;
        self
    }

    /// The exact side request for `query`, route costume included, before
    /// any attestation the transport applies.
    #[must_use]
    pub fn request(&self, query: MessagesRetrievalQuery) -> MessagesRetrievalRequest {
        let model = match &self.executor {
            Executor::Api(model) => model.resolved().model().remote_id().as_str().to_owned(),
            #[cfg(feature = "native-auth")]
            Executor::ClaudeOAuth { model, .. } => {
                model.resolved().model().remote_id().as_str().to_owned()
            }
        };
        let request = MessagesRetrievalRequest::new(model, query);
        let request = match self.effort {
            Some(effort) => request.with_adaptive_thinking(effort),
            None => request,
        };
        match &self.executor {
            Executor::Api(_) => request,
            #[cfg(feature = "native-auth")]
            Executor::ClaudeOAuth { model, .. } => {
                let prompt = request.query().prompt();
                request.with_system(vec![
                    crate::cch::billing_header_prefix(model.resolved().session_lineage(), &prompt),
                    crate::CLAUDE_OAUTH_IDENTITY.to_owned(),
                ])
            }
        }
    }

    /// Runs one side request.
    ///
    /// # Errors
    ///
    /// Returns the side request's failure after its bounded retries.
    pub async fn retrieve(
        &self,
        query: MessagesRetrievalQuery,
        cancellation: &CancellationToken,
    ) -> Result<MessagesRetrievalOutcome, MessagesRetrievalError> {
        #[cfg_attr(not(feature = "native-auth"), allow(unused_mut))]
        let mut request = self.request(query);
        #[cfg(feature = "native-auth")]
        if let Executor::ClaudeOAuth { model, session_id } = &self.executor {
            let identity = model.identity();
            request = request.with_metadata(serde_json::json!({
                "user_id": serde_json::json!({
                    "device_id": identity.device_id().as_str(),
                    "account_uuid": identity.account_uuid().as_str(),
                    "session_id": session_id.to_string(),
                })
                .to_string(),
            }));
        }
        run_messages_retrieval(&self.client, self, &request, cancellation, self.limits).await
    }
}

/// Thinking on the side request only where the model requires it.
fn adaptive_effort(
    thinking: ThinkingSupport,
    effort: Option<&'static str>,
) -> Option<&'static str> {
    match thinking {
        ThinkingSupport::Adaptive => effort,
        _ => None,
    }
}

fn unauthorized(error: &AnthropicError) -> MessagesRetrievalError {
    MessagesRetrievalError::new(
        MessagesRetrievalErrorKind::Unauthorized,
        format!("side request authority is not current: {error}"),
    )
}

impl MessagesRetrievalTransport for AnthropicRetrieval {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> MessagesRetrievalAuthorization<'a> {
        Box::pin(async move {
            match &self.executor {
                Executor::Api(model) => {
                    let credential = model
                        .side_request_credential()
                        .await
                        .map_err(|error| unauthorized(&error))?;
                    crate::http::messages_request(
                        &self.client,
                        self.dial_origin.clone(),
                        credential,
                        None,
                        None,
                        body,
                    )
                    .map_err(|error| unauthorized(&error))
                }
                #[cfg(feature = "native-auth")]
                Executor::ClaudeOAuth { model, session_id } => {
                    let input_bound = u64::try_from(body.len()).unwrap_or(u64::MAX).max(1);
                    let (token, _) = model
                        .side_request_credential(input_bound)
                        .await
                        .map_err(|error| unauthorized(&error))?;
                    let credential = plakous_http::sensitive_header_value(
                        format!("Bearer {token}").into_bytes(),
                    )
                    .map_err(|_| {
                        MessagesRetrievalError::new(
                            MessagesRetrievalErrorKind::Unauthorized,
                            "Claude OAuth bearer token could not be represented as a header",
                        )
                    })?;
                    let beta = model.resolved().dialect().beta_header().ok_or_else(|| {
                        MessagesRetrievalError::new(
                            MessagesRetrievalErrorKind::InvalidRequest,
                            "Claude OAuth beta header was missing",
                        )
                    })?;
                    let mut attested = body.to_vec();
                    crate::attest_claude_oauth_cch(&mut attested).map_err(|message| {
                        MessagesRetrievalError::new(
                            MessagesRetrievalErrorKind::InvalidRequest,
                            message,
                        )
                    })?;
                    let request_id = request_id()?;
                    crate::http::messages_request(
                        &self.client,
                        self.dial_origin.clone(),
                        credential,
                        Some(&beta),
                        Some((&request_id, *session_id)),
                        &attested,
                    )
                    .map_err(|error| unauthorized(&error))
                }
            }
        })
    }
}

/// A fresh per-request id for the `x-client-request-id` header.
#[cfg(feature = "native-auth")]
fn request_id() -> Result<String, MessagesRetrievalError> {
    use plakous_model::{Clock as _, IdGenerator as _};
    plakous_model::SystemIdGenerator
        .next_uuid(plakous_model::SystemClock.now())
        .map(|uuid| uuid.to_string())
        .map_err(|_| {
            MessagesRetrievalError::new(
                MessagesRetrievalErrorKind::InvalidRequest,
                "side request id could not be generated",
            )
        })
}

impl fmt::Debug for AnthropicRetrieval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicRetrieval")
            .field(
                "route",
                &match self.executor {
                    Executor::Api(_) => "anthropic",
                    #[cfg(feature = "native-auth")]
                    Executor::ClaudeOAuth { .. } => "claude",
                },
            )
            .field("effort", &self.effort)
            .finish_non_exhaustive()
    }
}
