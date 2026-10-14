"""Eval-plane implied USD. Not Plakous, not product telemetry.

TB 4.0 / Harbor store whatever an agent puts on `cost_usd`. Codex CLI does
not report billed dollars; Harbor then LiteLLM-estimates. Plak OAuth leaves
`cost_status=unknown`. This module applies one frozen rate card to the same
token buckets on every arm so A/B dollars are comparable.

Label the result implied list price. Never write `$0` for a missing card or
unresolved usage. Do not copy these rates into `plakous-model`.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

RATE_CARD_PATH = Path(__file__).with_name("rate_card.json")


class PricingError(ValueError):
    pass


@dataclass(frozen=True)
class CanonicalUsage:
    """Prompt total plus observational cache/output buckets.

    `input_tokens` is the provider prompt total. Cache read and write are
    disjoint subsets. Cost charges the remaining input at the input rate.
    `reasoning` is informational when it is already inside `output`.
    """

    input_tokens: int
    output_tokens: int | None
    cache_read: int | None = None
    cache_write: int | None = None
    reasoning_tokens: int | None = None


@dataclass(frozen=True)
class ImpliedCost:
    usd: float | None
    status: str
    card_id: str
    model_key: str | None
    basis: str = "frozen_standard_proxy"


def load_rate_card(path: Path | None = None) -> dict[str, Any]:
    payload = json.loads((path or RATE_CARD_PATH).read_text(encoding="utf-8"))
    if not isinstance(payload, dict) or not isinstance(payload.get("models"), dict):
        raise PricingError("rate card is not an object with models")
    return payload


def _index_models(card: dict[str, Any]) -> dict[str, str]:
    index: dict[str, str] = {}
    models = card["models"]
    for key, row in models.items():
        index[key] = key
        if not isinstance(row, dict):
            continue
        for alias in row.get("aliases") or []:
            index[str(alias)] = key
    return index


def resolve_model_key(model: str, card: dict[str, Any] | None = None) -> str | None:
    loaded = card or load_rate_card()
    index = _index_models(loaded)
    if model in index:
        return index[model]
    bare = model.split("/")[-1]
    return index.get(bare)


def implied_usd(
    model: str,
    usage: CanonicalUsage,
    *,
    card: dict[str, Any] | None = None,
) -> ImpliedCost:
    """Apply the frozen card. Missing rate or unpriced non-zero bucket → None."""
    loaded = card or load_rate_card()
    card_id = str(loaded.get("id") or "unknown")
    if any(
        value is not None and value < 0
        for value in (
            usage.input_tokens,
            usage.cache_read,
            usage.cache_write,
            usage.output_tokens,
            usage.reasoning_tokens,
        )
    ):
        return ImpliedCost(None, "invalid", card_id, None)
    key = resolve_model_key(model, loaded)
    if key is None:
        return ImpliedCost(None, "unpriced_model", card_id, None)
    row = loaded["models"][key]
    if any(value is None for value in (usage.cache_read, usage.cache_write, usage.output_tokens)):
        return ImpliedCost(None, "unresolved_usage", card_id, key)
    assert usage.cache_read is not None and usage.cache_write is not None and usage.output_tokens is not None
    if usage.cache_read + usage.cache_write > usage.input_tokens:
        return ImpliedCost(None, "cache_exceeds_input", card_id, key)
    # This is a frozen standard-rate proxy. Aggregate usage does not establish
    # which requests crossed a provider's per-request pricing threshold.
    uncached = usage.input_tokens - usage.cache_read - usage.cache_write
    buckets = (
        ("input", uncached, row.get("input")),
        ("cache_read", usage.cache_read, row.get("cache_read")),
        ("cache_write", usage.cache_write, row.get("cache_write")),
        ("output", usage.output_tokens, row.get("output")),
    )
    total = 0.0
    for name, tokens, rate in buckets:
        if tokens == 0:
            continue
        if rate is None:
            return ImpliedCost(None, f"unpriced_{name}", card_id, key)
        total += (tokens / 1_000_000) * float(rate)
    return ImpliedCost(total, "implied", card_id, key)


def from_harbor_context(
    model: str,
    *,
    n_input_tokens: int | None,
    n_cache_tokens: int | None,
    n_output_tokens: int | None,
    n_reasoning_tokens: int | None = None,
    n_cache_write_tokens: int | None = None,
) -> ImpliedCost:
    if n_input_tokens is None or n_output_tokens is None:
        card = load_rate_card()
        return ImpliedCost(None, "unresolved_usage", str(card.get("id") or "unknown"), None)
    return implied_usd(
        model,
        CanonicalUsage(
            input_tokens=n_input_tokens,
            cache_read=n_cache_tokens,
            cache_write=n_cache_write_tokens,
            output_tokens=n_output_tokens,
            reasoning_tokens=n_reasoning_tokens,
        ),
    )
