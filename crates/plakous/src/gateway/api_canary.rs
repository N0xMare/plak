//! A single API-key inference exchange with no application retry or session authority.

use std::fmt;
use std::time::Instant;

use plakous_auth::custody::{CredentialCustodian, CustodySlot, CustodyStatus};
use plakous_http::{
    Client, ExchangeControl, HeaderMap, HeaderName, HeaderValue, Limits, Method, Origin, Request,
};
use serde_json::{Value, json};

use super::GatewayCustody;

const TOKENS: u64 = 16;
const REPLY_BYTES: usize = 64 * 1024;
const PROMPT: &str = "Reply with OK.";

/// A content-free failure from a single API-key canary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ApiCanaryFailure {
    /// The account kind, model or effort is not a canary input.
    Input,
    /// The enrolled gateway could not prove the selected credential.
    Credential,
    /// The exact request could not be prepared.
    Request,
    /// The exchange or bounded cleanup did not finish.
    Transport,
    /// The provider declined this physical exchange; it was not replayed.
    Upstream(u16),
    /// The provider did not return bounded, completed assistant output.
    Reply,
}

impl fmt::Display for ApiCanaryFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input => f.write_str("invalid API canary inputs"),
            Self::Credential => f.write_str("API canary credential is not Ready at the gateway"),
            Self::Request => f.write_str("API canary request could not be prepared"),
            Self::Transport => f.write_str("API canary exchange did not settle"),
            Self::Upstream(status) => write!(f, "API canary refused by upstream (HTTP {status})"),
            Self::Reply => f.write_str("API canary did not return bounded assistant output"),
        }
    }
}

impl std::error::Error for ApiCanaryFailure {}

#[derive(Clone, Copy)]
enum Wire {
    Responses,
    Messages,
    Chat,
}

impl Wire {
    fn body(self, model: &str, low_effort: bool) -> Value {
        let messages = json!([{"role":"user","content":PROMPT}]);
        match self {
            Self::Responses => {
                let mut body = json!({"model":model,"input":messages,"store":false,"max_output_tokens":TOKENS});
                body["reasoning"] = json!({"effort":if low_effort {"low"} else {"none"}});
                body
            }
            Self::Messages => {
                let mut body = json!({"model":model,"messages":messages,"max_tokens":TOKENS});
                if low_effort {
                    body["thinking"] = json!({"type":"adaptive"});
                    body["output_config"] = json!({"effort":"low"});
                } else {
                    body["thinking"] = json!({"type":"disabled"});
                }
                body
            }
            Self::Chat => {
                let mut body = json!({
                    "model":model,"messages":messages,"max_tokens":TOKENS,
                    "provider":{"require_parameters":true,"allow_fallbacks":false},
                    "reasoning": if low_effort {json!({"effort":"low"})} else {json!({"enabled":false})}
                });
                body["stream"] = json!(false);
                body
            }
        }
    }

    fn completed(self, v: &Value) -> bool {
        let text = |v: &Value| v.as_str().is_some_and(|s| !s.trim().is_empty());
        let usage = |k: &str| {
            v["usage"][k]
                .as_u64()
                .is_some_and(|n| (1..=TOKENS).contains(&n))
        };
        match self {
            Self::Responses => {
                v["status"] == "completed"
                    && usage("output_tokens")
                    && v["output"].as_array().is_some_and(|items| {
                        items.iter().any(|item| {
                            item["type"] == "message"
                                && item["role"] == "assistant"
                                && item["content"].as_array().is_some_and(|blocks| {
                                    blocks
                                        .iter()
                                        .any(|b| b["type"] == "output_text" && text(&b["text"]))
                                })
                        })
                    })
            }
            Self::Messages => {
                v["type"] == "message"
                    && v["role"] == "assistant"
                    && v["stop_reason"] == "end_turn"
                    && usage("output_tokens")
                    && v["content"].as_array().is_some_and(|blocks| {
                        blocks
                            .iter()
                            .any(|b| b["type"] == "text" && text(&b["text"]))
                    })
            }
            Self::Chat => {
                v["choices"][0]["finish_reason"] == "stop"
                    && usage("completion_tokens")
                    && v["choices"][0]["message"]["role"] == "assistant"
                    && text(&v["choices"][0]["message"]["content"])
            }
        }
    }
}

fn target(kind: &str) -> Option<(&'static str, &'static str, Wire)> {
    Some(match kind {
        "openai" => ("https://api.openai.com", "/v1/responses", Wire::Responses),
        "xai-api" => ("https://api.x.ai", "/v1/responses", Wire::Responses),
        "anthropic" => ("https://api.anthropic.com", "/v1/messages", Wire::Messages),
        "openrouter" => (
            "https://openrouter.ai",
            "/api/v1/chat/completions",
            Wire::Chat,
        ),
        _ => return None,
    })
}

/// One gateway-relayed, tool-free API inference; contains no local key or durable session.
pub struct PreparedApiCanary {
    client: Client,
    request: Request,
    wire: Wire,
}

impl GatewayCustody {
    /// Prepares a fixed one-shot canary for an exact API-key account and model.
    /// This checks gateway facts and sends no inference.
    ///
    /// # Errors
    /// Refuses unsupported accounts, malformed models and unavailable gateway credentials.
    pub async fn prepare_api_canary(
        &self,
        account: &str,
        model: &str,
        low_effort: bool,
    ) -> Result<PreparedApiCanary, ApiCanaryFailure> {
        use ApiCanaryFailure as Error;
        let slot = CustodySlot::new(account).map_err(|_| Error::Input)?;
        let (origin, path, wire) = target(slot.kind()).ok_or(Error::Input)?;
        if model.is_empty()
            || model.len() > 128
            || !model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b))
        {
            return Err(Error::Input);
        }
        let facts = self
            .custodian()
            .facts(&slot)
            .await
            .map_err(|_| Error::Credential)?;
        let fence = facts
            .expected_fence()
            .filter(|_| facts.status == CustodyStatus::Ready)
            .ok_or(Error::Credential)?;
        let mut fields = self
            .custodian()
            .fields(&slot, &fence)
            .await
            .map_err(|_| Error::Credential)?;
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        if matches!(wire, Wire::Messages) {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        let names: Vec<_> = fields.names().collect();
        for name in names {
            let field = fields.take(name).ok_or(Error::Credential)?;
            let value = plakous_http::sensitive_header_value(field.into_header_bytes())
                .map_err(|_| Error::Request)?;
            headers.insert(HeaderName::from_static(name), value);
        }
        let limits = Limits {
            max_response_body_bytes: REPLY_BYTES,
            max_request_body_bytes: 4096,
            ..Limits::default()
        };
        let request = Request::new(
            Origin::parse(origin).map_err(|_| Error::Request)?,
            Method::POST,
            path,
            headers,
            serde_json::to_vec(&wire.body(model, low_effort))
                .map_err(|_| Error::Request)?
                .into(),
            &limits,
        )
        .map_err(|_| Error::Request)?;
        let client = self
            .transport()
            .client(Client::new(limits).map_err(|_| Error::Request)?);
        Ok(PreparedApiCanary {
            client,
            request,
            wire,
        })
    }
}

impl PreparedApiCanary {
    /// Consumes the prepared exchange. Never retries a physical inference.
    ///
    /// # Errors
    /// Returns only content-free failure classes for the wire, transport, or completion.
    pub async fn run(self, deadline: Instant) -> Result<(), ApiCanaryFailure> {
        use ApiCanaryFailure as Error;
        let response = self
            .client
            .execute(self.request, ExchangeControl::new(deadline))
            .await
            .map_err(|_| Error::Transport)?;
        let status = response.status();
        let mut body = response.into_body();
        if !status.is_success() {
            let _ = body.close().await;
            return Err(Error::Upstream(status.as_u16()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            bytes.extend_from_slice(&chunk.map_err(|_| Error::Transport)?);
        }
        let value = serde_json::from_slice(&bytes).map_err(|_| Error::Reply)?;
        if self.wire.completed(&value) {
            Ok(())
        } else {
            Err(Error::Reply)
        }
    }
}

#[cfg(test)]
mod tests;
