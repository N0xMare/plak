//! Chunk, choice, delta and usage wire checks and the cost and finish parsers.

use plakous_model::{FinishReason, ServedBy, Usage};

use super::{
    ChatFinishReason, ChatStreamError, ChatStreamErrorKind, ChatStreamExtensions, Delta, UsageWire,
    err,
};

impl Delta {
    pub(super) fn semantic(&self) -> bool {
        self.content.as_ref().is_some_and(|value| !value.is_empty())
            || self
                .reasoning
                .as_ref()
                .is_some_and(|value| !value.is_empty())
            || self
                .reasoning_content
                .as_ref()
                .is_some_and(|value| !value.is_empty())
            || self
                .reasoning_details
                .as_ref()
                .is_some_and(|details| !details.is_empty())
            || !self.tool_calls.is_empty()
    }
}

impl UsageWire {
    pub(super) fn canonical(
        self,
        extensions: ChatStreamExtensions,
    ) -> Result<Usage, ChatStreamError> {
        if self.prompt_tokens.checked_add(self.completion_tokens) != Some(self.total_tokens)
            || (extensions == ChatStreamExtensions::None
                && (self.prompt_tokens_details.cached_tokens > self.prompt_tokens
                    || self.completion_tokens_details.reasoning_tokens > self.completion_tokens))
        {
            return Err(err(
                ChatStreamErrorKind::InvalidResponse,
                "usage counters are inconsistent",
            ));
        }
        let cost_micros = parse_cost_micros(self.cost.as_ref(), extensions)?;
        Ok(Usage::new(
            self.prompt_tokens,
            self.completion_tokens,
            self.completion_tokens_details.reasoning_tokens,
            self.prompt_tokens_details.cached_tokens,
            self.prompt_tokens_details.cache_write_tokens,
        )
        .with_cost_micros(cost_micros))
    }
}

pub(super) fn capture_served_provider(
    provider: Option<&serde_json::Value>,
    extensions: ChatStreamExtensions,
    served_by: &mut Option<ServedBy>,
) -> Result<(), ChatStreamError> {
    if extensions != ChatStreamExtensions::OpenRouterV1 {
        return Ok(());
    }
    let Some(value) = provider else {
        return Ok(());
    };
    let text = value.as_str().ok_or_else(|| {
        err(
            ChatStreamErrorKind::InvalidResponse,
            "provider is not a string",
        )
    })?;
    let parsed = ServedBy::new(text)
        .map_err(|_| err(ChatStreamErrorKind::InvalidResponse, "provider is invalid"))?;
    if let Some(existing) = served_by {
        if *existing != parsed {
            return Err(err(
                ChatStreamErrorKind::InvalidResponse,
                "provider changed mid-stream",
            ));
        }
        return Ok(());
    }
    *served_by = Some(parsed);
    Ok(())
}

fn parse_cost_micros(
    cost: Option<&serde_json::Value>,
    extensions: ChatStreamExtensions,
) -> Result<Option<u64>, ChatStreamError> {
    if extensions != ChatStreamExtensions::OpenRouterV1 {
        return Ok(None);
    }
    let Some(value) = cost else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let number = value.as_number().ok_or_else(|| {
        err(
            ChatStreamErrorKind::InvalidResponse,
            "usage cost is not a number",
        )
    })?;
    Ok(Some(usd_number_to_cost_micros(number)?))
}

/// Largest USD value whose micro-dollar rounding fits in `u64`.
const MAX_USD_FOR_COST_MICROS: f64 = 18_446_744_073_709.55;

fn usd_number_to_cost_micros(number: &serde_json::Number) -> Result<u64, ChatStreamError> {
    let invalid = || {
        err(
            ChatStreamErrorKind::InvalidResponse,
            "usage cost is invalid",
        )
    };
    let out_of_range = || {
        err(
            ChatStreamErrorKind::InvalidResponse,
            "usage cost is out of range",
        )
    };
    let usd = number.as_f64().ok_or_else(invalid)?;
    if !usd.is_finite() || usd < 0.0 {
        return Err(invalid());
    }
    if usd > MAX_USD_FOR_COST_MICROS {
        return Err(out_of_range());
    }
    let micros_f64 = (usd * 1_000_000.0).round();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "usd is bounded below MAX_USD so the rounded micros fit u64"
    )]
    Ok(micros_f64 as u64)
}
pub(super) fn parse_finish(value: &str) -> Result<ChatFinishReason, ChatStreamError> {
    let canonical = match value {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::Policy,
        _ => {
            return Err(err(
                ChatStreamErrorKind::UnsupportedSemantic,
                "finish reason is outside Chat V1",
            ));
        }
    };
    Ok(ChatFinishReason {
        wire: value.to_owned(),
        canonical,
    })
}
