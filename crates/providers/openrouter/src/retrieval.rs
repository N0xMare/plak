//! Hosted web search on the OpenRouter route.
//!
//! [`OpenRouterRetrieval`] runs one isolated Chat Completions side request per
//! call with the gateway `web` plugin, on the session's own route, key,
//! account, discovered model and client, whatever wire the session itself
//! speaks. Before every physical request it revalidates the discovery
//! evidence and the credential generation exactly as a model round does; it
//! never falls back to another route or account.

use std::fmt;
use std::sync::Arc;

use plakous_chat_completions::{
    ChatRetrievalAuthorization, ChatRetrievalError, ChatRetrievalErrorKind, ChatRetrievalLimits,
    ChatRetrievalOutcome, ChatRetrievalReasoning, ChatRetrievalRequest, ChatRetrievalRouting,
    ChatRetrievalTransport, run_chat_retrieval,
};
use plakous_http::{
    Client, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request, SensitiveBytes,
    sensitive_header_value,
};
use plakous_model::{CancellationToken, Clock, Digest, DigestDomain};

use crate::chat::{OPENROUTER_CHAT_COMPLETIONS_PATH, insert_attribution, reviewed_until};
use crate::{
    OpenRouterApiKeyGenerationGrant, OpenRouterChatBuildError, OpenRouterChatModelEvidence,
    OpenRouterChatRoute, OpenRouterReasoningFacts, OpenRouterRetentionPolicy,
};

/// The effort ladder, lowest first, in OpenRouter's spelling.
const EFFORT_LADDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// One session's hosted web search.
pub struct OpenRouterRetrieval {
    route: OpenRouterChatRoute,
    evidence: OpenRouterChatModelEvidence,
    generation: OpenRouterApiKeyGenerationGrant,
    client: Client,
    dial_origin: Origin,
    clock: Arc<dyn Clock>,
    limits: ChatRetrievalLimits,
}

impl OpenRouterRetrieval {
    /// Binds side requests to the session's route, discovered model and
    /// current credential generation.
    ///
    /// # Errors
    ///
    /// Refuses evidence that is stale or belongs to another route, and a
    /// credential source without a current generation.
    pub fn new(
        route: OpenRouterChatRoute,
        evidence: OpenRouterChatModelEvidence,
        client: Client,
    ) -> Result<Self, OpenRouterChatBuildError> {
        let now = route.clock.now();
        if !evidence.validates(route.identity(), now) {
            return Err(OpenRouterChatBuildError::StaleEvidence);
        }
        reviewed_until(evidence.expires_at(), now)?;
        let generation = route.current_credential_generation_grant()?;
        let dial_origin = Origin::parse(route.identity().origin().as_str())
            .map_err(|_| OpenRouterChatBuildError::Configuration)?;
        let clock = Arc::clone(&route.clock);
        Ok(Self {
            route,
            evidence,
            generation,
            client,
            dial_origin,
            clock,
            limits: ChatRetrievalLimits::default(),
        })
    }

    /// Replaces the exchange bounds and backoff sample.
    #[must_use]
    pub const fn with_limits(mut self, limits: ChatRetrievalLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sends side requests to `origin` instead of the route's origin; the
    /// credential and headers stay bound to the route.
    #[doc(hidden)]
    #[must_use]
    pub fn with_dial_origin(mut self, origin: Origin) -> Self {
        self.dial_origin = origin;
        self
    }

    /// The exact side request for `query` restricted to `include_domains`
    /// and away from `exclude_domains`.
    #[must_use]
    pub fn request(
        &self,
        query: &str,
        include_domains: Vec<String>,
        exclude_domains: Vec<String>,
    ) -> ChatRetrievalRequest {
        let retention = self.route.identity().retention_policy();
        ChatRetrievalRequest::new(self.evidence.model().as_str(), query)
            .with_domains(include_domains, exclude_domains)
            .with_reasoning(lowest_reasoning(self.evidence.reasoning()))
            .with_routing(ChatRetrievalRouting {
                data_collection: retention
                    .and_then(OpenRouterRetentionPolicy::data_collection)
                    .map(str::to_owned),
                zdr: retention.and_then(OpenRouterRetentionPolicy::zdr),
            })
    }

    /// Runs one side request.
    ///
    /// # Errors
    ///
    /// Returns the side request's failure after its bounded retries.
    pub async fn retrieve(
        &self,
        request: &ChatRetrievalRequest,
        cancellation: &CancellationToken,
    ) -> Result<ChatRetrievalOutcome, ChatRetrievalError> {
        run_chat_retrieval(&self.client, self, request, cancellation, self.limits).await
    }

    fn authorized(&self, body: &[u8]) -> Result<Request, ChatRetrievalError> {
        let now = self.clock.now();
        if !self.evidence.validates(self.route.identity(), now)
            || reviewed_until(self.evidence.expires_at(), now).is_err()
        {
            return Err(unauthorized(
                "side request authority is not current: the model's discovery evidence expired",
            ));
        }
        let current = self
            .route
            .current_credential_generation_grant()
            .map_err(|_| unauthorized("side request authority is not current: no credential"))?;
        if current != self.generation {
            return Err(unauthorized(
                "side request authority is not current: the credential changed",
            ));
        }
        let operation = Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[
                b"plakous-openrouter/retrieval-v1",
                self.route.identity().fingerprint().as_bytes(),
                OPENROUTER_CHAT_COMPLETIONS_PATH.as_bytes(),
                body,
            ],
        )
        .map_err(|_| invalid_request("side request identity failed"))?;
        let key = self
            .route
            .retrieval_credential(&operation, self.generation)
            .map_err(|_| unauthorized("side request authority is not current: credential"))?;
        let authorization = sensitive_header_value(key.bearer_bytes())
            .map_err(|_| unauthorized("OpenRouter authorization could not be represented"))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        headers.insert(HeaderName::from_static("authorization"), authorization);
        insert_attribution(self.route.identity(), &mut headers);
        Request::new(
            self.dial_origin.clone(),
            Method::POST,
            OPENROUTER_CHAT_COMPLETIONS_PATH,
            headers,
            SensitiveBytes::new(body.to_vec()).into_bytes(),
            self.client.limits(),
        )
        .map_err(|_| invalid_request("side request could not be bound"))
    }
}

/// The lowest reasoning the model admits: off where reasoning is optional,
/// the lowest attested effort where it is mandatory.
pub(crate) fn lowest_reasoning(facts: Option<&OpenRouterReasoningFacts>) -> ChatRetrievalReasoning {
    let Some(facts) = facts else {
        return ChatRetrievalReasoning::Omit;
    };
    if facts.mandatory() {
        return facts
            .supported_efforts()
            .and_then(|efforts| {
                EFFORT_LADDER
                    .into_iter()
                    .find(|rung| efforts.iter().any(|effort| effort == rung))
            })
            .map_or(ChatRetrievalReasoning::Omit, ChatRetrievalReasoning::Lowest);
    }
    if facts.default_enabled() {
        ChatRetrievalReasoning::Disabled
    } else {
        ChatRetrievalReasoning::Omit
    }
}

fn unauthorized(message: &str) -> ChatRetrievalError {
    ChatRetrievalError::new(ChatRetrievalErrorKind::Unauthorized, message)
}

fn invalid_request(message: &str) -> ChatRetrievalError {
    ChatRetrievalError::new(ChatRetrievalErrorKind::InvalidRequest, message)
}

impl ChatRetrievalTransport for OpenRouterRetrieval {
    fn authorize<'a>(&'a self, body: &'a [u8]) -> ChatRetrievalAuthorization<'a> {
        Box::pin(async move { self.authorized(body) })
    }
}

impl fmt::Debug for OpenRouterRetrieval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterRetrieval")
            .field("route", &self.route)
            .field("model", self.evidence.model())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "retrieval_tests.rs"]
mod tests;
