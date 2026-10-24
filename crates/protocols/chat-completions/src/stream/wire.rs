//! Chunk, choice, delta and usage wire checks and the cost and finish parsers.

use std::collections::BTreeMap;

use plakous_model::{FinishReason, ServedBy, Usage};

use super::{
    ChatFinishReason, ChatStreamError, ChatStreamErrorKind, ChatStreamExtensions, Choice, Chunk,
    Delta, ToolDelta, UsageWire, err,
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

impl Chunk {
    pub(super) fn extensions_allowed(&self, extensions: ChatStreamExtensions) -> bool {
        match extensions {
            ChatStreamExtensions::None => {
                self.extra.is_empty()
                    && self.choices.iter().all(Choice::has_no_extensions)
                    && self.usage.as_ref().is_none_or(UsageWire::has_no_extensions)
            }
            ChatStreamExtensions::OpenRouterV1 => {
                keys_allowed(
                    &self.extra,
                    &[
                        "provider",
                        "service_tier",
                        "openrouter_metadata",
                        "system_fingerprint",
                    ],
                ) && self
                    .choices
                    .iter()
                    .all(Choice::has_only_openrouter_v1_extensions)
                    && self
                        .usage
                        .as_ref()
                        .is_none_or(UsageWire::has_only_openrouter_v1_extensions)
            }
        }
    }

    pub(super) fn first_unreviewed_extension(
        &self,
        extensions: ChatStreamExtensions,
    ) -> Option<String> {
        let allowed_chunk = match extensions {
            ChatStreamExtensions::None => &[][..],
            ChatStreamExtensions::OpenRouterV1 => &[
                "provider",
                "service_tier",
                "openrouter_metadata",
                "system_fingerprint",
            ][..],
        };
        first_unallowed_key(&self.extra, allowed_chunk)
            .map(|key| format!("chunk.{key}"))
            .or_else(|| {
                self.choices.iter().find_map(|choice| {
                    let allowed_choice = match extensions {
                        ChatStreamExtensions::None => &[][..],
                        ChatStreamExtensions::OpenRouterV1 => &["native_finish_reason"][..],
                    };
                    first_unallowed_key(&choice.extra, allowed_choice)
                        .map(|key| format!("choice.{key}"))
                        .or_else(|| {
                            first_unallowed_key(&choice.delta.extra, &[])
                                .map(|key| format!("choice.delta.{key}"))
                        })
                        .or_else(|| {
                            choice.delta.tool_calls.iter().find_map(|tool| {
                                first_unallowed_key(&tool.extra, &[])
                                    .map(|key| format!("choice.delta.tool.{key}"))
                                    .or_else(|| {
                                        tool.function.as_ref().and_then(|function| {
                                            first_unallowed_key(&function.extra, &[]).map(|key| {
                                                format!("choice.delta.tool.function.{key}")
                                            })
                                        })
                                    })
                            })
                        })
                })
            })
            .or_else(|| {
                let usage = self.usage.as_ref()?;
                let (usage_allowed, prompt_allowed, completion_allowed) = match extensions {
                    ChatStreamExtensions::None => (&[][..], &[][..], &[][..]),
                    ChatStreamExtensions::OpenRouterV1 => (
                        &["cost", "is_byok", "cost_details"][..],
                        &["cache_write_tokens", "audio_tokens", "video_tokens"][..],
                        &["image_tokens", "audio_tokens"][..],
                    ),
                };
                first_unallowed_key(&usage.extra, usage_allowed)
                    .map(|key| format!("usage.{key}"))
                    .or_else(|| {
                        first_unallowed_key(&usage.prompt_tokens_details.extra, prompt_allowed)
                            .map(|key| format!("usage.prompt_tokens_details.{key}"))
                    })
                    .or_else(|| {
                        first_unallowed_key(
                            &usage.completion_tokens_details.extra,
                            completion_allowed,
                        )
                        .map(|key| format!("usage.completion_tokens_details.{key}"))
                    })
            })
    }
}
impl Choice {
    fn has_no_extensions(&self) -> bool {
        self.extra.is_empty() && self.delta.has_no_extensions()
    }

    fn has_only_openrouter_v1_extensions(&self) -> bool {
        keys_allowed(&self.extra, &["native_finish_reason"]) && self.delta.has_no_extensions()
    }
}
impl Delta {
    fn has_no_extensions(&self) -> bool {
        self.extra.is_empty() && self.tool_calls.iter().all(ToolDelta::has_no_extensions)
    }
}
impl ToolDelta {
    fn has_no_extensions(&self) -> bool {
        self.extra.is_empty()
            && self
                .function
                .as_ref()
                .is_none_or(|function| function.extra.is_empty())
    }
}
impl UsageWire {
    fn has_no_extensions(&self) -> bool {
        self.extra.is_empty()
            && self.prompt_tokens_details.extra.is_empty()
            && self.completion_tokens_details.extra.is_empty()
    }

    fn has_only_openrouter_v1_extensions(&self) -> bool {
        keys_allowed(&self.extra, &["cost", "is_byok", "cost_details"])
            && keys_allowed(
                &self.prompt_tokens_details.extra,
                &["cache_write_tokens", "audio_tokens", "video_tokens"],
            )
            && keys_allowed(
                &self.completion_tokens_details.extra,
                &["image_tokens", "audio_tokens"],
            )
    }
}

fn keys_allowed(extra: &BTreeMap<String, serde_json::Value>, allowed: &[&str]) -> bool {
    extra.keys().all(|key| allowed.contains(&key.as_str()))
}

fn first_unallowed_key<'a>(
    extra: &'a BTreeMap<String, serde_json::Value>,
    allowed: &[&str],
) -> Option<&'a str> {
    extra
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
        .map(String::as_str)
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
        let cost_micros = parse_cost_micros(&self.extra, extensions)?;
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
    extra: &BTreeMap<String, serde_json::Value>,
    extensions: ChatStreamExtensions,
    served_by: &mut Option<ServedBy>,
) -> Result<(), ChatStreamError> {
    if extensions != ChatStreamExtensions::OpenRouterV1 {
        return Ok(());
    }
    let Some(value) = extra.get("provider") else {
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
    extra: &BTreeMap<String, serde_json::Value>,
    extensions: ChatStreamExtensions,
) -> Result<Option<u64>, ChatStreamError> {
    if extensions != ChatStreamExtensions::OpenRouterV1 {
        return Ok(None);
    }
    let Some(value) = extra.get("cost") else {
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
