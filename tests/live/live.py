"""Shared pieces of the live smoke, soak, and job drivers.

Everything here drives the real `plak` binary: `plak run`, `plak agent` over
JSONL, and the TUI on a pseudo-terminal. Nothing talks to a provider directly.
A run gets a scratch `PLAK_HOME` under `target/live/`; OAuth logins are XDG
state outside that home, so they are shared with the real ones (a token refresh
during a run writes the real store). API-key routes copy the key installed by
`plak key set` into the scratch home without exposing it in output.
"""

from __future__ import annotations

import dataclasses
import collections
import fcntl
import hashlib
import json
import os
import pty
import queue
import re
import select
import signal
import stat
import struct
import subprocess
import tempfile
import termios
import threading
import time
from collections.abc import Collection
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def capture_root_mismatch(binary: Path, root: Path = ROOT) -> str | None:
    """Why `binary` cannot capture into this checkout's `tests/provider-authority`.

    A `test-hooks` build accepts capture directories only under the
    `tests/provider-authority` of the checkout that compiled it (its
    `bin/plak` manifest directory is baked in), so a driver must run from that
    checkout. Otherwise every launch fails before anything is sent.
    """
    manifest = root.resolve() / "bin" / "plak"
    try:
        data = binary.read_bytes()
    except OSError as error:
        return f"cannot read {binary}: {error}"
    if str(manifest).encode() in data:
        return None
    # A build through a symlinked path bakes in the unresolved spelling.
    for candidate in set(re.findall(rb"/[\x21-\x7e]{1,512}?/bin/plak(?=[^\x21-\x7e]|$)", data)):
        try:
            if Path(os.fsdecode(candidate)).resolve() == manifest:
                return None
        except (OSError, ValueError):
            continue
    return (f"{binary} was not built from this checkout ({root}); its capture root is "
            "another checkout's tests/provider-authority. Run the driver from the "
            "checkout that built the binary.")


def binary_sha256(binary: Path) -> str:
    """Identify the exact executable used by a live report, not its checkout."""
    return hashlib.sha256(binary.read_bytes()).hexdigest()


# Planned depth for a live route. The pre-release bar is spec 05 §3.4.
# `full` is the twelve smoke scenarios, long-session, jobs and both soaks on
# Apple Silicon, plus a Linux smoke. `light` is an Apple Silicon smoke, plus
# a Linux smoke on the four direct API rows. `informational` is one Apple
# Silicon smoke, recorded, and non-blocking except for a harness failure.
# A route's `informational_lanes` run at its class depth but score like an
# informational row.
FULL, LIGHT, INFORMATIONAL = "full", "light", "informational"
ROUTE_CLASSES = (FULL, LIGHT, INFORMATIONAL)

_CODEX = ("chatgpt.com", "/backend-api/codex/responses")
_GROK = ("cli-chat-proxy.grok.com", "/v1/responses")
_CLAUDE = ("api.anthropic.com", "/v1/messages?beta=true")
_OR_CHAT = ("openrouter.ai", "/api/v1/chat/completions")
_OR_MESSAGES = ("openrouter.ai", "/api/v1/messages")
_OPENAI_API = ("api.openai.com", "/v1/responses")
_ANTHROPIC_API = ("api.anthropic.com", "/v1/messages")
_XAI_API = ("api.x.ai", "/v1/responses")


@dataclasses.dataclass(frozen=True)
class Route:
    """One account/model tuple and where its inference requests go.

    `planned` is false for enrolled routes kept runnable but left out of the
    owner matrix (the dry-run estimator skips them). `informational_lanes`
    names lanes whose cell is informational on an otherwise blocking row.
    """

    name: str
    account: str
    model: str
    effort: str | None
    protocol: str | None
    host: str
    path: str
    route_class: str = LIGHT
    planned: bool = True
    informational_lanes: frozenset[str] = frozenset()

    def lane_class(self, lane: str) -> str:
        return INFORMATIONAL if lane in self.informational_lanes else self.route_class

    def flags(self) -> list[str]:
        flags = ["--account", self.account, "--model", self.model]
        if self.effort:
            flags += ["--effort", self.effort]
        if self.protocol:
            flags += ["--protocol", self.protocol]
        return flags


def _route(name: str, account: str, model: str, effort: str | None,
           protocol: str | None, endpoint: tuple[str, str], route_class: str,
           planned: bool = True, informational_lanes: frozenset[str] = frozenset()) -> Route:
    host, path = endpoint
    return Route(name, account, model, effort, protocol, host, path, route_class, planned,
                 informational_lanes)


# Exact ids are pinned spellings. OpenRouter ids are the 2026-10-08
# catalog probe: Messages is planned only for deepseek, the one id that probe
# marked Chat and Messages. Other OpenRouter rows stay on Chat, the default.
ROUTES = {
    "claude": _route("claude", "claude", "claude-sonnet-5", "low", None, _CLAUDE,
                     LIGHT, planned=False),
    "claude-sonnet-medium": _route(
        "claude-sonnet-medium", "claude", "claude-sonnet-5", "medium", None, _CLAUDE,
        LIGHT, planned=False),
    "claude-sonnet55-low": _route(
        "claude-sonnet55-low", "claude", "claude-sonnet-5-5", "low", None, _CLAUDE, FULL),
    "claude-sonnet55-medium": _route(
        "claude-sonnet55-medium", "claude", "claude-sonnet-5-5", "medium", None,
        _CLAUDE, LIGHT),
    "claude-haiku55-low": _route(
        "claude-haiku55-low", "claude", "claude-haiku-5-5", "low", None, _CLAUDE, LIGHT),
    "claude-haiku55-medium": _route(
        "claude-haiku55-medium", "claude", "claude-haiku-5-5", "medium", None, _CLAUDE, LIGHT),
    "claude-opus-low": _route(
        "claude-opus-low", "claude", "claude-opus-5-5", "low", None, _CLAUDE, LIGHT),
    "claude-opus-medium": _route(
        "claude-opus-medium", "claude", "claude-opus-5-5", "medium", None, _CLAUDE, LIGHT),
    "claude-fable": _route(
        "claude-fable", "claude", "claude-fable-5-1", "low", None, _CLAUDE, LIGHT),
    "grok": _route("grok", "grok", "grok-4.7", "low", None, _GROK, FULL),
    "grok-46": _route("grok-46", "grok", "grok-4.6", "low", None, _GROK, LIGHT),
    # Long-session is informational on both DeepSeek rows: given Plak's
    # complete summary request, the model's summary drops the earlier turn's
    # fact once large tool results follow it, on Chat and on Messages. On
    # Messages it also sends reads with only a path deep into long tool loops.
    # Smoke, jobs and soak stay blocking.
    "deepseek": _route(
        "deepseek", "openrouter", "deepseek/deepseek-v4.1-flash-1017", None, None,
        _OR_CHAT, FULL, informational_lanes=frozenset({"long-session"})),
    "deepseek-messages": _route(
        "deepseek-messages", "openrouter", "deepseek/deepseek-v4.1-flash-1017", None,
        "messages", _OR_MESSAGES, FULL, informational_lanes=frozenset({"long-session"})),
    "or-mimo": _route(
        "or-mimo", "openrouter", "xiaomi/mimo-v2.6-flash", None, None, _OR_CHAT, LIGHT),
    "or-glm": _route(
        "or-glm", "openrouter", "z-ai/glm-5.3-flash", None, None, _OR_CHAT, LIGHT),
    "or-qwen37": _route(
        "or-qwen37", "openrouter", "qwen/qwen3.7-flash", None, None, _OR_CHAT, LIGHT),
    "or-nemotron-free": _route(
        "or-nemotron-free", "openrouter", "nvidia/nemotron-3-ultra-550b-a55b:free",
        None, None, _OR_CHAT, INFORMATIONAL),
    "or-laguna-free": _route(
        "or-laguna-free", "openrouter", "poolside/laguna-s-2.1:free", None, None,
        _OR_CHAT, INFORMATIONAL),
    # Runnable, not planned: its free upstream streamed this 2.6B model at about
    # 7 tokens per second with in-band 504s on every 0.0.2 run, so its smoke
    # outlived scenario budgets without evidence that clears the product.
    "or-lfm-free": _route(
        "or-lfm-free", "openrouter", "liquid/lfm-2.5-2.6b:free", None, None,
        _OR_CHAT, INFORMATIONAL, planned=False),
    # Not offered as `:free` on the probe day. Paid ids, informational depth.
    "or-ling": _route(
        "or-ling", "openrouter", "inclusionai/ling-3.0-flash-vl", None, None,
        _OR_CHAT, INFORMATIONAL),
    "or-qwen38": _route(
        "or-qwen38", "openrouter", "qwen/qwen3.8-27b", None, None, _OR_CHAT, INFORMATIONAL),
    # gpt-6.1-sol is the full Codex row: gpt-6-luna at low and medium ended
    # long growth turns early and skipped a page after a child's compaction in
    # every long-session cell, while sol paged through compaction and passed.
    "codex": _route("codex", "codex", "gpt-6-luna", "low", None, _CODEX, LIGHT),
    "codex-luna-medium": _route(
        "codex-luna-medium", "codex", "gpt-6-luna", "medium", None, _CODEX, LIGHT),
    "codex-sol": _route("codex-sol", "codex", "gpt-6.1-sol", "low", None, _CODEX, FULL),
    "codex-astra": _route("codex-astra", "codex", "gpt-6-astra", "low", None, _CODEX, LIGHT),
    # Direct API rows for the pre-release bar (spec 05 §3.4). Light, not
    # best-effort. Linux musl smokes these four plus the full rows.
    "api-openai": _route(
        "api-openai", "openai", "gpt-6-luna", "low", None, _OPENAI_API, LIGHT),
    "api-openai-medium": _route(
        "api-openai-medium", "openai", "gpt-6-luna", "medium", None, _OPENAI_API, LIGHT),
    "api-anthropic": _route(
        "api-anthropic", "anthropic", "claude-haiku-5-5", "medium", None,
        _ANTHROPIC_API, LIGHT),
    "api-xai": _route("api-xai", "xai-api", "grok-4.7", "low", None, _XAI_API, LIGHT),
    # Feature-scenario roster rows (image input, tools), outside the bar matrix.
    "api-anthropic-low": _route(
        "api-anthropic-low", "anthropic", "claude-haiku-5-5", "low", None, _ANTHROPIC_API,
        LIGHT, planned=False),
    "or-gemma4-free": _route(
        "or-gemma4-free", "openrouter", "google/gemma-4-31b-it:free", None, None, _OR_CHAT,
        INFORMATIONAL, planned=False),
}


def fault_plan(route: Route, fault: str, **fields: object) -> str:
    """A `PLAK_TEST_FAULTS` rule aimed at this route's first inference request."""
    parts = [f"host={route.host}", f"path={route.path}", "nth=1", f"fault={fault}"]
    parts += [f"{key.replace('_', '-')}={value}" for key, value in fields.items()]
    return ",".join(parts)


# Outcome classes. A harness failure is Plak's; a model failure is a completed
# turn whose work did not meet the oracle; a provider failure is the real
# provider refusing (quota, outage) without an injected fault.
PASS, HARNESS, MODEL, PROVIDER, PENDING = "pass", "harness", "model", "provider", "pending"


@dataclasses.dataclass
class Result:
    scenario: str
    surface: str
    outcome: str
    detail: str = ""
    seconds: float = 0.0
    input_tokens: int = 0
    output_tokens: int = 0
    cached_input_tokens: int = 0
    reasoning_tokens: int = 0


class ScenarioFailure(Exception):
    """Raised inside a scenario with the class the failure belongs to."""

    def __init__(self, outcome: str, detail: str):
        super().__init__(detail)
        self.outcome = outcome


class EventTimeout(ScenarioFailure):
    """No expected `plak agent` event arrived within the wait's budget."""


# Typed provider refusals and outages. `QuotaExhausted` is the name some
# driver surfaces used before the product kind `QuotaExceeded`; both count.
# `Authentication` is the provider refusing the credential, not a product bug.
# `Policy` is the provider refusing the request content (spec 05 §3.2: a
# refusal rejected before acceptance, or a model refusal terminal).
PROVIDER_ERROR_KINDS = frozenset({
    "RateLimited", "QuotaExceeded", "QuotaExhausted", "Unavailable", "Authentication",
    "Policy",
})
# Statuses that refuse before acceptance, or are the ambiguous 5xx the spec
# already records as provider. 429 is the rate-limit case on every surface.
PROVIDER_HTTP_STATUSES = frozenset({429, 500, 502, 503, 504, 529})
# A Chat gateway reports an upstream failure inside the stream it already
# committed to; the product types it as a provider error and renders
# "<provider> returned an in-band error after accepting the request: HTTP 504
# (timeout) from <upstream>" with the status the gateway named leading the
# account. A 429 or 5xx there, with the class the product typed it as, is the
# provider's outage, not a protocol violation. A quota 429 carries no class and
# is provider through its typed kind.
_IN_BAND_STATUS = re.compile(r"returned an in-band error after accepting the request: HTTP (\d{3}) "
                             r"\((?:rate limited|unavailable|timeout)\)", re.IGNORECASE)
IN_BAND_ERROR_MARKER = "returned an in-band error after accepting the request:"
# Kinds the product types an in-band provider error as; the typed kind scores
# it even where the rendered text was cut before its status.
IN_BAND_PROVIDER_KINDS = frozenset({"RateLimited", "Unavailable", "Timeout"})


def in_band_provider_status(text: str) -> int | None:
    """The 429/5xx status a provider's in-band stream error named, or None."""
    match = _IN_BAND_STATUS.search(text or "")
    if not match:
        return None
    status = int(match.group(1))
    return status if status == 429 or 500 <= status <= 599 else None
# Kernel phrases. Provider only when the capture shows the protocol violation.
STREAM_VALIDATION_MARKERS = (
    "stream validation failed",
    "usage must be one post-finish terminal-metadata chunk",
)
TOOL_ARGUMENT_VALIDATION_MARKERS = {
    "chat stream validation failed: tool arguments are malformed or ambiguous": "chat",
    "messages stream validation failed: terminal messages tool input was not complete valid json": "messages",
}
# Injected faults the product is required to absorb. A turn that fails with
# that same status was not absorbed (harness). A different provider refusal
# during the scenario is still provider.
ABSORBED_FAULTS = frozenset({"429", "529"})


def _blob(*parts: object) -> str:
    return " ".join(str(part) for part in parts if part).lower()


# The product's bracketed attempt summary, `[N requests; last: ...]`. Its
# label describes a physical request, never the terminal's own cause, so no
# provider text match may read it.
_ATTEMPT_SUMMARY = re.compile(r"\[\d+ requests;[^\]]*\]")


def without_attempt_summary(text: str) -> str:
    """Operator text with every bracketed attempt summary removed."""
    return _ATTEMPT_SUMMARY.sub("", text)


def text_is_provider_refusal(text: str) -> bool:
    """True when operator text is a typed provider refusal or outage.

    A product refusal that merely mentions a missing checkpoint or a tool
    contract is not in this set. `unavailable` alone is not either: the
    navigator uses that word for a missing hub. The attempt summary is not
    read.
    """
    lowered = without_attempt_summary(text).lower()
    if "server_is_overloaded" in lowered or "server is overloaded" in lowered:
        return True
    if "rejected-before-acceptance" in lowered or "rejected before acceptance" in lowered:
        return True
    if "temporarily overloaded" in lowered or "temporarily unavailable" in lowered:
        return True
    if any(marker in lowered for marker in (
            "ratelimited", "rate_limited", "rate limit", "rate-limit",
            "http 429", "status=429", "status 429", " 429", "(429)",
            "quotaexceeded", "quota_exceeded", "quotaexhausted", "quota is exhausted",
            "too many requests", "authentication failed", "signed out")):
        return True
    return False


# Quota labels and failure kinds on a child `blocked` or `failure` object.
# Bare "unavailable" in prose is not a provider refusal: the navigator uses
# that word for a missing hub. It counts here only as a quota or failure kind.
_PROVIDER_CHILD_LABELS = frozenset({
    "rate_limited",
    "ratelimited",
    "quota_exceeded",
    "quotaexceeded",
    "quota_exhausted",
    "quotaexhausted",
    "unavailable",
    "provider_unavailable",
})


def provider_child_label(label: str) -> bool:
    """True for a blocked quota or failure kind that is a provider refusal.

    `rate_limited`, a quota exhaustion (`quota` in the label), or
    provider-unavailable. Any other quota name stays a product failure.
    """
    token = label.strip().lower().replace("-", "_")
    if not token:
        return False
    if token in _PROVIDER_CHILD_LABELS:
        return True
    if "rate_limit" in token or "ratelimit" in token:
        return True
    if "unavailable" in token and "provider" in token:
        return True
    if "quota" in token:
        return True
    return False


def _provider_child_fault(text: str, lookup_faults: str | None = None) -> bool:
    """Fault prose that is a provider refusal, including provider-unavailable."""
    lookup = lookup_failure(text, lookup_faults)
    if lookup:
        return lookup[1] == PROVIDER
    if text_is_provider_refusal(text):
        return True
    lowered = without_attempt_summary(text).lower()
    if ("provider unavailable" in lowered or "provider-unavailable" in lowered
            or "provider_unavailable" in lowered):
        return True
    if "quota exceeded" in lowered or "quota exhausted" in lowered:
        return True
    return False


def provider_child_record(record: dict, *, lookup_faults: str | None = None) -> bool:
    """True when a child row's quota, failure kind, or fault text is a provider refusal."""
    blocked = record.get("blocked")
    if isinstance(blocked, dict):
        quota = blocked.get("quota")
        if isinstance(quota, str) and provider_child_label(quota):
            return True
    failure = record.get("failure")
    if isinstance(failure, dict):
        kind = failure.get("kind")
        message = failure.get("message")
        if isinstance(kind, str) and provider_child_label(kind):
            return True
        if isinstance(message, str) and _provider_child_fault(message, lookup_faults):
            return True
    elif isinstance(failure, str) and _provider_child_fault(failure, lookup_faults):
        return True
    fault = record.get("fault")
    if isinstance(fault, str) and _provider_child_fault(fault, lookup_faults):
        return True
    return False


def _provider_child_json(value: dict, lookup_faults: str | None = None) -> bool:
    if provider_child_record(value, lookup_faults=lookup_faults):
        return True
    for key in ("agents", "nodes"):
        rows = value.get(key)
        if isinstance(rows, list) and any(
                isinstance(row, dict) and provider_child_record(row, lookup_faults=lookup_faults) for row in rows):
            return True
    return False


def provider_child_text(text: str, *, lookup_faults: str | None = None) -> bool:
    """True when child-failure text, or wait/tree JSON embedded in it, is a provider refusal.

    Reads `quota <label>` and `failure <kind>` from the flattened row, and
    `blocked.quota` / `failure` / `fault` from a wait receipt or tree node.
    """
    lookup = lookup_failure(text, lookup_faults)
    if lookup:
        return lookup[1] == PROVIDER
    if _provider_child_fault(text, lookup_faults):
        return True
    if in_band_provider_status(text) is not None:
        return True
    parts = text.lower().split()
    for index, part in enumerate(parts[:-1]):
        if part not in ("quota", "failure"):
            continue
        if provider_child_label(parts[index + 1].strip(".,:;")):
            return True
    if "{" not in text:
        return False
    decoder = json.JSONDecoder()
    index = 0
    length = len(text)
    while index < length:
        start = text.find("{", index)
        if start < 0:
            return False
        try:
            value, end = decoder.raw_decode(text, start)
        except json.JSONDecodeError:
            index = start + 1
            continue
        if isinstance(value, dict) and _provider_child_json(value, lookup_faults):
            return True
        index = end if end > start else start + 1
    return False


# The typed fields of a `model_refusal` detail. The explanation is provider prose.
_REFUSAL_FIELDS = ("stop_reason", "stop_details.type", "category")


def refusal_summary(event: dict) -> str:
    """A `model_refusal` event's typed fields, without the provider's explanation."""
    kept = []
    for part in str(event.get("detail") or "").split(";"):
        key, _, value = part.partition("=")
        if key.strip() in _REFUSAL_FIELDS and value.strip():
            kept.append(f"{key.strip()}={value.strip()}")
    return ";".join(kept) or "model refusal"


def durable_refusals(record: dict) -> list[str]:
    """A session-log terminal `refused` is the canonical result of a model refusal."""
    return ["canonical model refusal"] if record.get("phase") == "refused" else []


LOOKUP_ENDPOINTS = (
    ("chatgpt.com", "/backend-api/codex/models"),
    ("cli-chat-proxy.grok.com", "/v1/models"),
    ("openrouter.ai", "/api/v1/models"),
    ("api.openai.com", "/v1/models"),
    ("api.anthropic.com", "/v1/models"),
    ("api.x.ai", "/v1/models"),
)


def _lookup_fault_matches(host: str, path: str) -> bool:
    return any((not host or host == origin) and (not path or target.startswith(path)
               or origin == "openrouter.ai" and path.startswith(target + "/"))
               for origin, target in LOOKUP_ENDPOINTS)


def lookup_failure(text: str, faults: str | None = None) -> tuple[str, str] | None:
    """An exhausted lookup's cause and evidence-based live outcome."""
    text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", text)
    match = re.search(
        r"model lookup failed after ([0-3]) attempts: "
        r"(operation deadline\b|HTTP (?:429|500|502|503|504)\b|transport timeout\b|DNS failure\b|"
        r"connect failure\b|transport stream failure\b|HTTP transport failed in "
        r"(?:Resolution|Connect|RequestWrite|RequestWriteOrResponseHead|ResponseHead|ResponseBody) "
        r"\([^\n]*, (?:Io|Resolution|DeadlineExceeded|Protocol)\))", text)
    if not match:
        return None
    attempts, cause = int(match.group(1)), match.group(2)
    if cause == "operation deadline":
        return match.group(0), HARNESS
    timed_out = "timeout" in cause or "DeadlineExceeded" in cause
    if (timed_out and attempts < 2) or (not timed_out and attempts != 3 and cause != "HTTP 429"):
        return match.group(0), HARNESS
    injected = 0
    for rule in (faults or "").split(";"):
        fields = {}
        for item in rule.split(","):
            if "=" in item:
                key, value = item.split("=", 1)
                fields[key.strip()] = value.strip()
        path = fields.get("path", "")
        if not _lookup_fault_matches(fields.get("host", ""), path):
            continue
        if fields.get("fault") not in {"connect", "stall", "reset", "eof", "429", "500", "502", "503", "504"}:
            continue
        if "nth" not in fields:
            return match.group(0), HARNESS
        try:
            nth = int(fields["nth"])
        except ValueError:
            continue
        if 1 <= nth <= attempts:
            injected += 1
    return match.group(0), HARNESS if attempts <= injected else PROVIDER


def lookup_failure_reason(text: str, faults: str | None = None) -> str | None:
    """Only exhausted real-provider failures, not a single deadline, qualify."""
    failure = lookup_failure(text, faults)
    return failure[0] if failure and failure[1] == PROVIDER else None


def provider_reason_from(kind: str | None, http_status: int | None, text: str, *,
                         lookup_faults: str | None = None) -> str | None:
    """Why this observation is a typed provider refusal, or None.

    Covers JSONL turn failures, `plak run` stderr, TUI session-log terminals,
    manual `/compact` refusals, and child job output. A kernel
    stream-validation failure is not decided here.
    """
    lookup = lookup_failure(text, lookup_faults)
    if lookup:
        return lookup[0] if lookup[1] == PROVIDER else None
    if kind in PROVIDER_ERROR_KINDS or http_status in PROVIDER_HTTP_STATUSES:
        status = f"{kind or ''} {http_status or ''}".strip()
        return status or text[:160]
    if http_status is not None:
        # The product typed the status, and not as a provider refusal; its
        # prose cannot overrule that.
        return None
    in_band = in_band_provider_status(text)
    if in_band is not None:
        return f"provider in-band error {in_band}"
    if text_is_provider_refusal(text):
        return " ".join(text.split())[:200]
    return None


def _chunk_violates_usage_rule(obj: object) -> bool:
    """Chat rule: usage is one post-finish terminal-metadata chunk.

    A chunk that carries `usage` and a choice with no `finish_reason` is the
    provider's response violating that rule (`stream.rs`). An empty `choices`
    list is the legal terminal metadata chunk.
    """
    if not isinstance(obj, dict) or not obj.get("usage"):
        return False
    choices = obj.get("choices")
    if not isinstance(choices, list) or not choices:
        return False
    return any(isinstance(choice, dict) and not choice.get("finish_reason") for choice in choices)


def capture_shows_usage_on_finishless_choice(segments: list[Path] | None) -> bool:
    """The raw body contains the documented Chat protocol violation.

    Evidence rule: some `exchange-*.body` in the turn's capture parses as
    JSON, or as SSE `data:` JSON, and that object has `usage` plus a
    `choices` entry whose `finish_reason` is missing or null. A truncated
    capture (`incomplete=true` only, no such object) does not qualify.
    """
    if not segments:
        return False
    for segment in segments:
        if not segment.is_dir():
            continue
        for body in sorted(segment.glob("exchange-*.body")):
            try:
                text = body.read_text(errors="replace")
            except OSError:
                continue
            for line in text.splitlines():
                payload = line.strip()
                if payload.startswith("data:"):
                    payload = payload[5:].strip()
                if not payload.startswith("{"):
                    continue
                try:
                    obj = json.loads(payload)
                except json.JSONDecodeError:
                    continue
                if _chunk_violates_usage_rule(obj):
                    return True
    return False


def _invalid_tool_arguments(arguments: str, *, reject_duplicate_keys: bool) -> bool:
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON key")
            result[key] = value
        return result

    def invalid_constant(value):
        raise ValueError("non-JSON constant")

    try:
        value = json.loads(arguments, object_pairs_hook=unique_object if reject_duplicate_keys else None,
                           parse_constant=invalid_constant)
        return not isinstance(value, dict)
    except ValueError:
        return True


def _finished_invalid_arguments(text: str, protocol: str) -> bool:
    """Reassemble Chat and Messages tool inputs; unfinished inputs are not evidence."""
    events = []
    for line in text.splitlines():
        payload = line.strip()
        if payload.startswith("data:"):
            payload = payload[5:].strip()
            if payload == "[DONE]":
                continue
        elif not payload.startswith("{"):
            continue
        try:
            event = json.loads(payload)
        except ValueError:
            return False
        if not isinstance(event, dict):
            return False
        events.append(event)

    chat = {}
    finished_choices = set()
    messages = {}
    closed_blocks = set()
    messages_finished = False
    for event in events:
        for choice in event.get("choices", []) if isinstance(event.get("choices"), list) else []:
            if not isinstance(choice, dict) or type(choice.get("index")) is not int:
                continue
            index = choice["index"]
            delta = choice.get("delta") or choice.get("message") or {}
            calls = delta.get("tool_calls", []) if isinstance(delta, dict) else []
            for call in calls if isinstance(calls, list) else []:
                if not isinstance(call, dict) or type(call.get("index")) is not int:
                    continue
                function = call.get("function")
                arguments = function.get("arguments") if isinstance(function, dict) else None
                if isinstance(arguments, str):
                    key = (index, call["index"])
                    chat[key] = chat.get(key, "") + arguments
            if choice.get("finish_reason") == "tool_calls":
                finished_choices.add(index)
        index = event.get("index")
        block = event.get("content_block")
        delta = event.get("delta")
        if event.get("type") == "content_block_start" and type(index) is int \
                and isinstance(block, dict) and block.get("type") == "tool_use":
            messages[index] = ""
        if event.get("type") == "content_block_delta" and type(index) is int \
                and index in messages and isinstance(delta, dict) \
                and delta.get("type") == "input_json_delta" \
                and isinstance(delta.get("partial_json"), str):
            messages[index] += delta["partial_json"]
        if event.get("type") == "content_block_stop" and type(index) is int:
            closed_blocks.add(index)
        if event.get("type") == "message_stop":
            messages_finished = True
    if protocol == "chat":
        return any(index in finished_choices
                   and _invalid_tool_arguments(arguments, reject_duplicate_keys=True)
                   for (index, _), arguments in chat.items())
    return messages_finished and any(
        index in closed_blocks and arguments
        and _invalid_tool_arguments(arguments, reject_duplicate_keys=False)
        for index, arguments in messages.items())


def capture_shows_invalid_tool_arguments(segments: list[Path] | None, protocol: str) -> bool:
    """Only the failing process's highest-numbered, finished exchange is model evidence.

    A kernel that fails closed on finished arguments stops reading there, so
    `ClosedEarly` counts: the finish evidence in the body itself, not end of
    body, shows the arguments were complete.
    """
    if not segments or len(segments) != 1 or not capture_integrity(segments):
        return False
    body = max(segments[0].glob("exchange-*.body"),
               key=lambda path: int(path.stem.removeprefix("exchange-")), default=None)
    if body is None:
        return False
    try:
        metadata = body.with_suffix(".info").read_text().splitlines()
        return (any(line in ("terminal=Complete", "terminal=ProtocolComplete",
                             "terminal=ClosedEarly") for line in metadata)
                and _finished_invalid_arguments(body.read_text(), protocol))
    except (OSError, UnicodeError):
        return False


def stream_validation_outcome(text: str, segments: list[Path] | None) -> str | None:
    """Provider protocol violations and model-invalid tool input need captured evidence.

    None when the text is not that failure; harness without the required evidence.
    """
    lowered = text.lower()
    if not any(marker in lowered for marker in STREAM_VALIDATION_MARKERS):
        return None
    if in_band_provider_status(text) is not None:
        # The provider's own error, classified by provider_reason_from.
        return None
    if capture_shows_usage_on_finishless_choice(segments):
        return PROVIDER
    for marker, protocol in TOOL_ARGUMENT_VALIDATION_MARKERS.items():
        if marker in lowered and capture_shows_invalid_tool_arguments(segments, protocol):
            return MODEL
    return HARNESS


# Spec 01 Length classification. A tool-free Length answer with visible text
# is committed: `plak run` prints it, then exits 5 with this last stderr line.
TRUNCATED_ANSWER_LINE = "plak: turn_failed:output-limit"
# An empty Length that may not retry again is a failed turn of typed kind
# OutputLimit. JSONL lifecycle frames carry the kind; `plak run` stderr and the
# session log's `turn.failed` row carry only the rendered diagnostic, which
# quotes one of these product messages.
OUTPUT_LIMIT_KIND = "OutputLimit"
OUTPUT_LIMIT_DIAGNOSTICS = (
    "response reached its output bound before completion",
    "Responses provider stopped at the output bound before",
)
OUTPUT_LIMIT_MISS = "OutputLimit: the model spent its output bound without an answer"
_COMPATIBILITY_NOTICE = re.compile(
    r"the (?:answer to the )?([\w-]+) compatibility notice "
    r"(?:is not accepted|needs an answer|could not be read)")


def last_line(text: str) -> str:
    """The last non-empty line: `plak run`'s final status on stderr."""
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    return lines[-1] if lines else ""


def failure_lines(stderr: str) -> str:
    """The final stderr line, after the nearest earlier error or failure line.

    The final line is the run's status; the error line before it, when there
    is one, usually names the cause.
    """
    lines = [" ".join(line.split()) for line in stderr.splitlines() if line.strip()]
    if not lines:
        return ""
    cause = next((line for line in reversed(lines[:-1])
                  if "error" in line.lower() or "failed" in line.lower()), None)
    return (f"{cause} | {lines[-1]}" if cause else lines[-1])[:300]


def truncated_answer(code: int, stderr: str) -> bool:
    """`plak run` printed a committed answer cut at its output limit."""
    return code == 5 and last_line(stderr) == TRUNCATED_ANSWER_LINE


def output_limit_failure(kind: str | None, text: str) -> bool:
    """A failed turn of typed kind OutputLimit, from its kind or its diagnostic."""
    return kind == OUTPUT_LIMIT_KIND or any(needle in text for needle in OUTPUT_LIMIT_DIAGNOSTICS)


def compatibility_notice_refusal(text: str) -> str | None:
    """The remedy for a subscription compatibility notice Plak could not ask about."""
    match = _COMPATIBILITY_NOTICE.search(text)
    if match is None:
        return None
    account = match[1]
    return (f"the {account} compatibility notice is not accepted; accept it once with "
            f"`plak models {account}` from a terminal before a live run")


def with_failure_messages(records: list[dict]) -> list[dict]:
    """Session-log records whose failed terminals carry their turn's failure text.

    The durable `turn.lifecycle` row stores only `turn_id` and `phase`. The host
    logs the rendered failure as a separate `turn.failed` row (`msg`) inside the
    same admitted bracket; that text becomes the terminal's `message`.
    """
    messages: dict[str, list[str]] = {}
    current = None
    for record in records:
        if record.get("kind") == "turn.lifecycle" and record.get("phase") == "admitted":
            current = str(record.get("turn_id") or "")
        elif record.get("kind") == "turn.failed" and current:
            messages.setdefault(current, []).append(str(record.get("msg") or ""))
    out = []
    for record in records:
        turn_id = str(record.get("turn_id") or "")
        if (record.get("kind") == "turn.lifecycle" and record.get("phase") not in (None, "admitted")
                and not record.get("message") and turn_id in messages):
            record = {**record, "message": " ".join(messages[turn_id])}
        out.append(record)
    return out


def _absorbed_injection(kind: str | None, http_status: int | None, fault: str | None,
                        text: str | None = None) -> bool:
    """The failure is the injected fault the product was required to absorb."""
    if fault not in ABSORBED_FAULTS:
        return False
    if text is not None and IN_BAND_ERROR_MARKER in text.lower():
        # Injected status faults answer before acceptance; an in-band error is
        # always the real provider.
        return False
    if fault == "429":
        return http_status == 429 or kind in ("RateLimited", "QuotaExceeded", "QuotaExhausted")
    return http_status == 529 or kind == "Unavailable"


def _real_errors_followed_the_injection(turn: Turn) -> bool:
    """`fault_plan` injects exactly one response (`nth=1`), so a second HTTP
    retry on the turn was the real provider refusing again."""
    return sum(1 for retry in turn.retries if retry.get("cause_kind") == "http_status") > 1


def classify_incomplete(turn: Turn, what: str, *, injected_fault: str | None = None,
                        segments: list[Path] | None = None, lookup_faults: str | None = None) -> None:
    """Raise when `turn` did not complete, using the shared outcome rules.

    A typed provider refusal is `provider` on every surface, including during
    an injected transport fault, unless the failure is that injected fault
    itself (the product was required to absorb it). A kernel
    stream-validation failure needs capture evidence to score provider protocol
    violations or model-invalid arguments; otherwise it is `harness`.
    """
    if turn.completed:
        return
    text = " ".join(turn.errors)
    lookup = lookup_failure(text, lookup_faults or turn.lookup_faults)
    if lookup:
        raise ScenarioFailure(lookup[1], f"{what}: {lookup[0]}")
    if output_limit_failure(turn.model_error_kind, text):
        raise ScenarioFailure(MODEL, f"{what}: {OUTPUT_LIMIT_MISS}")
    status = f"{turn.phase} {turn.model_error_kind or ''} {turn.http_status or ''}".strip()
    if IN_BAND_ERROR_MARKER in text.lower() and turn.model_error_kind in IN_BAND_PROVIDER_KINDS:
        raise ScenarioFailure(PROVIDER, f"{what}: provider in-band error "
                                        f"({turn.model_error_kind})")
    validation = stream_validation_outcome(f"{text} {turn.model_error_kind or ''}", segments)
    if validation == MODEL:
        raise ScenarioFailure(MODEL, f"{what}: model emitted invalid tool arguments")
    if validation == PROVIDER and not _absorbed_injection(
            turn.model_error_kind, turn.http_status, injected_fault):
        raise ScenarioFailure(PROVIDER, f"{what}: provider stream violated the documented protocol")
    if validation == HARNESS:
        raise ScenarioFailure(HARNESS, f"{what}: kernel stream validation failed without "
                                       f"a captured protocol violation")
    reason = provider_reason_from(turn.model_error_kind, turn.http_status, text)
    if reason and _absorbed_injection(turn.model_error_kind, turn.http_status, injected_fault, text) \
            and _real_errors_followed_the_injection(turn):
        raise ScenarioFailure(PROVIDER, f"{what}: {reason} after the injected fault was retried")
    if reason and not _absorbed_injection(turn.model_error_kind, turn.http_status, injected_fault, text):
        raise ScenarioFailure(PROVIDER, f"{what}: {reason}")
    if turn.phase == "refused" and turn.refusals:
        raise ScenarioFailure(PROVIDER, f"{what}: provider refused the request "
                                        f"({turn.refusals[-1]})")
    errors = " | ".join(" ".join(error.split()) for error in turn.errors)[:200]
    raise ScenarioFailure(HARNESS, f"{what}: {status} {errors}".strip())


def classify_command_error(message: str, what: str, *,
                           segments: list[Path] | None = None, lookup_faults: str | None = None) -> None:
    """A manual `/compact` or other command error, classified like a turn."""
    text = message or ""
    lookup = lookup_failure(text, lookup_faults)
    if lookup:
        raise ScenarioFailure(lookup[1], f"{what}: {lookup[0]}")
    validation = stream_validation_outcome(text, segments)
    if validation == MODEL:
        raise ScenarioFailure(MODEL, f"{what}: model emitted invalid tool arguments")
    if validation == PROVIDER:
        raise ScenarioFailure(PROVIDER, f"{what}: provider stream violated the documented protocol")
    if validation == HARNESS:
        raise ScenarioFailure(HARNESS, f"{what}: kernel stream validation failed without "
                                       f"a captured protocol violation")
    if provider_reason_from(None, None, text):
        raise ScenarioFailure(PROVIDER, f"{what}: {' '.join(text.split())[:200]}")
    raise ScenarioFailure(HARNESS, f"{what}: {' '.join(text.split())[:200]}")


def classify_cli_failure(code: int, stderr: str, what: str, *,
                         segments: list[Path] | None = None, lookup_faults: str | None = None) -> None:
    """`plak run` exit. Exit 5 is the agent class; the text decides provider.

    An unaccepted compatibility notice (exit 3) is `harness` naming the
    remedy, and a typed OutputLimit diagnostic is `model` as on every surface
    when it is the final stderr line, the turn's own failure.
    """
    lookup = lookup_failure(stderr, lookup_faults)
    if lookup:
        raise ScenarioFailure(lookup[1], f"{what}: {lookup[0]}")
    notice = compatibility_notice_refusal(stderr)
    if notice:
        raise ScenarioFailure(HARNESS, f"{what} exited {code}: {notice}")
    detail = failure_lines(stderr)
    validation = stream_validation_outcome(stderr, segments)
    if validation == MODEL:
        raise ScenarioFailure(MODEL, f"{what}: model emitted invalid tool arguments")
    if validation == PROVIDER:
        raise ScenarioFailure(PROVIDER, f"{what}: provider stream violated the documented protocol")
    if validation == HARNESS:
        raise ScenarioFailure(HARNESS, f"{what}: kernel stream validation failed without "
                                       f"a captured protocol violation")
    if output_limit_failure(None, last_line(stderr)):
        raise ScenarioFailure(MODEL, f"{what}: {OUTPUT_LIMIT_MISS}")
    if provider_reason_from(None, None, stderr):
        raise ScenarioFailure(PROVIDER, f"{what}: {detail}")
    raise ScenarioFailure(HARNESS, f"{what} exited {code}: {detail}".strip())


API_KEY_ENV = {
    "openrouter": "OPENROUTER_API_KEY",
    "openai": "OPENAI_API_KEY",
    "anthropic": "ANTHROPIC_API_KEY",
    "xai-api": "XAI_API_KEY",
}
AUTHORITY_DIAGNOSTICS_FILTER = "plak.xai.authority=warn"


def api_key_filename(account: str) -> str:
    """Product key file for an API-key account.

    `xai-api` shares the `xai` key slot, so the file is `xai.key`.
    """
    if account == "xai-api":
        return "xai.key"
    return f"{account}.key"


# Spec 05 §3.2 and §3.4: a qualification tuple names the composition. The bar
# uses this one. A scenario that pins another composition passes it here.
DEFAULT_COMPOSITION = "plakous-default"


def route_report(route: Route, composition: str = DEFAULT_COMPOSITION,
                 bound: set[str] | None = None) -> dict[str, object]:
    """The evidence tuple a live report should name for this route.

    `composition_requested` is what the home pins. A pack may bind it to
    another composition (Codex binds `plakous-default` as `native-ptc`), so a
    run report also names `composition_bound`: every composition the
    `plak agent` hellos of that run reported, sorted, empty when none did.
    """
    report: dict[str, object] = {
        "route": route.name,
        "route_class": route.route_class,
        "account": route.account,
        "model": route.model,
        "effort": route.effort,
        "protocol": route.protocol,
        "composition_requested": composition,
    }
    if bound is not None:
        report["composition_bound"] = sorted(bound)
    return report


PROVIDER_PENDING = "provider did not respond within the scenario budget"


def pending_exchange(segment: Path) -> int | None:
    """The number of `segment`'s last exchange when it still awaits the provider.

    The product writes `status=None`, `terminal=pending`, `incomplete=true`
    before dispatch and rewrites that record only at the exchange's terminal,
    so this marker on the highest-numbered exchange of a timed-out process
    means its request was still waiting on the provider.
    """
    try:
        last = max(segment.glob("exchange-*.info"),
                   key=lambda path: int(path.stem.removeprefix("exchange-")), default=None)
        if last is None:
            return None
        lines = last.read_text().splitlines()
        fields = dict(line.split("=", 1) for line in lines)
        if (len(fields) != len(lines)
                or fields.keys() != {"process_id", "exchange", "status", "terminal",
                                     "recorded_bytes", "incomplete"}
                or (fields["status"], fields["terminal"], fields["incomplete"])
                != ("None", "pending", "true")
                or not re.fullmatch(r"[1-9][0-9]*", fields["process_id"])
                or not re.fullmatch(r"[1-9][0-9]*", fields["exchange"])
                or last.stem != f"exchange-{int(fields['exchange']):04}"
                or not all(last.with_suffix(suffix).is_file() for suffix in (".request", ".body"))):
            return None
        return int(fields["exchange"])
    except (OSError, UnicodeError, ValueError):
        return None


# Protocol terminal events: a body that carries one was answered in full, so
# the provider is not what the process was still waiting on.
_TERMINAL_EVENT_MARKERS = (b"data: [DONE]", b"response.completed", b"response.done",
                           b"response.failed", b"message_stop")
# A wait the provider mandated after a failure. The capture records no response
# headers, so Retry-After advice (chapter 01 §13.1 honors up to 60 s) is not
# visible; the credit is chapter 02's 30 s transient continuation, the longest
# wait the product takes without advice, plus slack for scheduling.
PROVIDER_WAIT_CAP_S = 30.0
PROVIDER_WAIT_SLACK_S = 5.0
# The shortest retry wait without advice: half of the 1 s equal-jitter base
# (chapter 01 §13.1), less a little clock slack.
MIN_RETRY_BACKOFF_S = 0.4
# Identical resends of one request after provider failures: four physical
# requests per logical attempt (chapter 01 §13.1) are the original and three
# resends. A transient continuation grows the conversation, so it is not one.
MAX_IDENTICAL_RESENDS = 3
# Budget a timed-out process may spend outside provider exchanges and credited
# waits: startup, tools and the work between requests.
LOCAL_ALLOWANCE_S = 15.0
# What a successful exchange's own work explains: a fixed first-token and
# prefill allowance, plus its reported output tokens (reasoning included) at
# this rate. Only the duration beyond both is the provider's, so a loop of
# healthy rounds is mostly explained and does not by itself reach the
# provider share a timeout needs.
EXPLAINED_PREFILL_S = 5.0
EXPLAINED_TOKENS_PER_S = 15.0
# Bodiless model lookups in one window: two at startup and the bounded retry.
MAX_WINDOW_LOOKUPS = 6
# Staleness of the exchange pending at the kill: a provider still answering
# wrote a byte (keep-alives included) within `PENDING_RECENT_BYTES_S`; an
# empty body past the route's dispatch deadline, or a lookup past its 60 s cap
# (chapter 01 §6.2), means the product, not the provider, kept it from ending.
PENDING_RECENT_BYTES_S = 60.0
LOOKUP_CAP_S = 60.0
STALE_SLACK_S = 5.0
# Route dispatch deadlines: 120 s for the OpenAI Platform, xAI API and
# Anthropic API library defaults, 300 s for subscription routes and OpenRouter.
API_LIBRARY_ACCOUNTS = frozenset({"openai", "anthropic", "xai-api"})


def dispatch_deadline_s(route: "Route | None") -> float:
    """The route's dispatch deadline; the stricter 120 s when the route is unknown."""
    if route is None or route.account in API_LIBRARY_ACCOUNTS:
        return 120.0
    return 300.0


def provider_wait_status(status: int) -> bool:
    """An HTTP status after which the provider may mandate a wait."""
    return status in (408, 429) or 500 <= status <= 599


def sse_events(body: bytes) -> list[bytes]:
    """The SSE events in `body`, counted as the `after-events` stream cut counts them.

    A blank line ends a block; a block of only `:` comment lines, such as a
    keep-alive, dispatches no event (spec 05 §2.8). `\r` is ignored, and a
    final block no blank line ended is not an event yet.
    """
    events, block, has_field = [], [], False
    for line in body.replace(b"\r", b"").split(b"\n")[:-1]:
        if line:
            block.append(line)
            has_field = has_field or not line.startswith(b":")
            continue
        if has_field:
            events.append(b"\n".join(block))
        block, has_field = [], False
    return events


def _event_data(event: bytes) -> object:
    data = [line[5:].removeprefix(b" ") for line in event.split(b"\n") if line.startswith(b"data:")]
    try:
        return json.loads(b"\n".join(data)) if data else None
    except ValueError:
        return None


# In-band provider errors by wire: Messages `error` event types and Responses
# error codes, with the HTTP status they stand for.
_MESSAGES_ERROR_TYPES = {"rate_limit_error": 429, "api_error": 500, "overloaded_error": 529}
_RESPONSES_ERROR_CODES = {"rate_limit_exceeded": 429, "server_error": 500,
                          "server_is_overloaded": 503}


def event_error_status(event: bytes) -> int | None:
    """The status a well-formed in-band provider error event stands for, or None.

    Chat (OpenRouter): an object whose `error` object has an integer `code`
    of 408, 429 or a 5xx. Messages: `{"type":"error","error":{"type":T}}`
    with T `rate_limit_error`, `api_error` or `overloaded_error`. Responses:
    `response.failed` whose `response.error.code`, or an `error` event whose
    `code` or `error.code`, is `rate_limit_exceeded`, `server_error` or
    `server_is_overloaded`.
    """
    value = _event_data(event)
    if not isinstance(value, dict):
        return None
    error = value.get("error")
    code = error.get("code") if isinstance(error, dict) else None
    if type(code) is int:
        return code if provider_wait_status(code) else None
    if value.get("type") == "error" and isinstance(error, dict):
        status = _MESSAGES_ERROR_TYPES.get(error.get("type"))
        if status is not None:
            return status
    if value.get("type") == "response.failed":
        response = value.get("response")
        failed = response.get("error") if isinstance(response, dict) else None
        code = failed.get("code") if isinstance(failed, dict) else None
    elif value.get("type") == "error":
        code = value.get("code", code)
    return _RESPONSES_ERROR_CODES.get(code) if isinstance(code, str) else None


def last_event(body: bytes) -> bytes | None:
    """The body's last counted SSE event other than Chat's `data: [DONE]`."""
    events = [event for event in sse_events(body) if event.strip() != b"data: [DONE]"]
    return events[-1] if events else None


_TRANSPORT_FAILURE = re.compile(r"Failed\(Failure \{ phase: (ResponseHead|ResponseBody), "
                                r"transmission: \w+, kind: (\w+)(?:\(\w+\))?, "
                                r".*http2_reason: (None|Some\([0-9]+\))")


def network_failure(terminal: str) -> str | None:
    """A network-shaped transport failure while the provider owed the response.

    `Io`, `Tls` or `DeadlineExceeded` in the response head or body, or
    `Protocol` with an HTTP/2 reset reason. `Internal`, `LimitExceeded`,
    `InvalidResponse`, a cancellation and any other `Protocol` failure are
    the product's or undetermined, not the provider's.
    """
    match = _TRANSPORT_FAILURE.match(terminal)
    if match is None:
        return None
    phase, kind, reset = match.groups()
    if kind in ("Io", "Tls", "DeadlineExceeded") or (kind == "Protocol" and reset != "None"):
        return f"a transport failure ({kind}) in {phase}"
    return None


def exchange_failure(fields: dict[str, str], body: bytes) -> str | None:
    """How a terminal exchange ended in a provider-side failure, or None.

    An HTTP status of 408, 429 or a 5xx; a well-formed in-band error as its
    last event (`event_error_status`); or a network-shaped transport failure
    while the provider owed the response (`network_failure`).
    """
    status = re.fullmatch(r"Some\(([0-9]{3})\)", fields["status"])
    if status and provider_wait_status(int(status[1])):
        return f"HTTP {status[1]}"
    event = last_event(body)
    in_band = event_error_status(event) if event is not None else None
    if in_band is not None:
        return f"HTTP {in_band}"
    return network_failure(fields["terminal"])


def output_tokens(body: bytes) -> int | None:
    """The response's reported output tokens, reasoning included, or None.

    Chat `usage.completion_tokens`, Messages `usage.output_tokens` (the
    `message_delta` total), Responses `response.usage.output_tokens`; the
    largest reported value wins. A body that is one JSON object (a
    non-streamed answer) is read the same way.
    """
    values = [_event_data(event) for event in sse_events(body)]
    if not values:
        try:
            values = [json.loads(body)]
        except ValueError:
            return None
    found = []
    for value in values:
        if not isinstance(value, dict):
            continue
        for holder in (value, value.get("response"), value.get("message")):
            usage = holder.get("usage") if isinstance(holder, dict) else None
            if not isinstance(usage, dict):
                continue
            for key in ("completion_tokens", "output_tokens"):
                if type(usage.get(key)) is int:
                    found.append(usage[key])
    return max(found) if found else None


@dataclasses.dataclass
class _Exchange:
    number: int
    dispatched: float
    # The terminal record's time; None while the exchange is still pending.
    ended: float | None
    # The body's last write: the last byte the product pulled.
    last_byte: float
    body_bytes: int
    request_bytes: int
    # The request's conversation items, canonical; None for a bodiless lookup
    # or a request that carries no conversation.
    items: tuple[str, ...] | None
    settled: bool
    failure: str | None
    finished: bool
    # Reported output tokens of a terminal response, or None without usage.
    tokens: int | None = None

    @property
    def lookup(self) -> bool:
        return self.request_bytes == 0

    @property
    def conversation(self) -> bool:
        return self.items is not None


def request_items(request: Path) -> tuple[str, ...] | None:
    """A request's conversation (`messages`, or Responses `input`), item by item."""
    raw = request.read_bytes()
    try:
        value = json.loads(raw) if raw else None
    except ValueError:
        return None
    items = value.get("messages", value.get("input")) if isinstance(value, dict) else None
    if not isinstance(items, list):
        return None
    return tuple(json.dumps(item, sort_keys=True) for item in items)


def _extends(longer: tuple[str, ...], prefix: tuple[str, ...]) -> bool:
    return len(prefix) <= len(longer) and longer[:len(prefix)] == prefix


def _segment_exchanges(segment: Path, pending: int | None) -> list[_Exchange] | None:
    """Every exchange in an integrity-checked segment, in exchange-number order."""
    exchanges = []
    for info in segment.glob("exchange-*.info"):
        number = int(info.stem.removeprefix("exchange-"))
        request, body_path = info.with_suffix(".request"), info.with_suffix(".body")
        body = body_path.read_bytes()
        finished = any(marker in body for marker in _TERMINAL_EVENT_MARKERS)
        common = dict(number=number, dispatched=request.stat().st_mtime,
                      last_byte=body_path.stat().st_mtime, body_bytes=len(body),
                      request_bytes=request.stat().st_size,
                      items=request_items(request), finished=finished)
        if number == pending:
            exchanges.append(_Exchange(ended=None, settled=False, failure=None, **common))
            continue
        fields = terminal_fields(info, body_path)
        if fields is None:
            return None
        failure = exchange_failure(fields, body)
        settled = (fields["terminal"] in ("Complete", "ProtocolComplete")
                   or (fields["terminal"] == "ClosedEarly" and finished) or failure is not None)
        exchanges.append(_Exchange(ended=info.stat().st_mtime, settled=settled, failure=failure,
                                   tokens=output_tokens(body), **common))
    return sorted(exchanges, key=lambda exchange: exchange.number)


def _resumes(later: _Exchange, failed: _Exchange) -> bool:
    """`later` is the next request of the work `failed` was doing."""
    if failed.lookup:
        return later.lookup
    if failed.conversation:
        return later.conversation and _extends(later.items, failed.items)
    return not later.lookup


def _measure(spans: list[tuple[float, float]], low: float, high: float) -> float:
    """The length of the union of `spans` inside [low, high]."""
    total, reach = 0.0, low
    for start, end in sorted(spans):
        start, end = max(start, reach), min(end, high)
        if end > start:
            total, reach = total + end - start, end
    return total


def _stale_pending(last: _Exchange, killed_at: float, deadline: float) -> str | None:
    """Why the exchange pending at the kill was the product's to end, or None."""
    if last.finished:
        return f"exchange {last.number} pending after the provider finished"
    if last.lookup:
        waited = killed_at - last.dispatched
        if waited > LOOKUP_CAP_S + STALE_SLACK_S:
            return (f"lookup exchange {last.number} pending {waited:.0f}s, past its "
                    f"{LOOKUP_CAP_S:.0f}s cap")
        return None
    if last.body_bytes:
        silent = killed_at - last.last_byte
        if silent > PENDING_RECENT_BYTES_S:
            return (f"exchange {last.number} received no byte in the {silent:.0f}s before "
                    f"the kill, more than {PENDING_RECENT_BYTES_S:.0f}s")
        return None
    waited = killed_at - last.dispatched
    if waited > deadline + STALE_SLACK_S:
        return (f"exchange {last.number} had no first byte for {waited:.0f}s, past the "
                f"{deadline:.0f}s dispatch deadline")
    return None


def _request_loop(exchanges: list[_Exchange], start: float) -> str | None:
    """A resend no provider failure explains, inside the window.

    A request whose conversation repeats, or rolls back to, an earlier
    request's conversation is a loop, unless it is an identical resend after
    that earlier exchange's provider failure, at least the minimal backoff
    later, and within `MAX_IDENTICAL_RESENDS`. A shorter conversation that is
    not such a prefix (a compaction) is not a loop. Interleaved agents keep
    distinct conversations.
    """
    for index, exchange in enumerate(exchanges):
        if not exchange.conversation or exchange.dispatched < start:
            continue
        identical = 0
        for before in exchanges[:index]:
            if not before.conversation or not _extends(before.items, exchange.items):
                continue
            if before.items != exchange.items or before.failure is None:
                return (f"exchange {exchange.number} resent exchange {before.number}'s "
                        f"conversation without a provider failure to retry")
            if before.ended is not None and exchange.dispatched - before.ended < MIN_RETRY_BACKOFF_S:
                return f"exchange {exchange.number} retried exchange {before.number} without backoff"
            identical += 1
        if identical > MAX_IDENTICAL_RESENDS:
            return f"exchange {exchange.number} resent one request {identical} times"
    return None


def budget_attribution(segment: Path, killed_at: float, timeout: float,
                       deadline: float = 120.0) -> tuple[bool, str]:
    """Whether a timed-out process's capture proves the provider used its budget.

    Over the window [killed_at - timeout, killed_at] every exchange spans its
    dispatch (`.request` mtime) to its terminal record (`.info` mtime), a
    pending one to the kill; spans of in-process children sharing the segment
    are united. Provider time is the pending exchange, exchanges that ended
    in a provider failure (`exchange_failure`), and for each successful
    exchange its duration less `EXPLAINED_PREFILL_S` and what its own
    reported output explains at `EXPLAINED_TOKENS_PER_S` (nothing without
    usage), counted from its dispatch. After a provider failure, the gap until that work's next
    request (or the kill) is a credited wait of at most `PROVIDER_WAIT_CAP_S`
    plus slack. The rest of the exchange time is token-explained, and the
    rest of the window is local.

    The provider used the budget when every exchange that ended in the window
    is settled; the kill lands on a pending exchange that is not stale
    (`_stale_pending`, `deadline` being the route's dispatch deadline) or
    inside a credited wait; local time is within `LOCAL_ALLOWANCE_S`;
    provider time and credited waits are at least half the window; at most
    `MAX_WINDOW_LOOKUPS` lookups ran; and there is no request loop
    (`_request_loop`). Returns that verdict and the account.
    """
    pending = pending_exchange(segment)
    if not capture_integrity([segment], [segment]):
        return False, "capture incomplete"
    exchanges = _segment_exchanges(segment, pending)
    if not exchanges:
        return False, "capture incomplete" if exchanges is None else "no captured exchange"
    start = killed_at - timeout
    cap = PROVIDER_WAIT_CAP_S + PROVIDER_WAIT_SLACK_S
    spans, caused, waits = [], [], []
    for exchange in exchanges:
        span = (exchange.dispatched, killed_at if exchange.ended is None else exchange.ended)
        spans.append(span)
        if exchange.ended is None or exchange.failure is not None:
            caused.append(span)
        elif exchange.tokens is not None:
            unexplained = (span[1] - span[0] - EXPLAINED_PREFILL_S
                           - exchange.tokens / EXPLAINED_TOKENS_PER_S)
            if unexplained > 0:
                caused.append((span[0], span[0] + unexplained))
        if exchange.failure is None:
            continue
        resumed = min((later.dispatched for later in exchanges if later.number > exchange.number
                       and _resumes(later, exchange)), default=killed_at)
        waits.append((exchange.ended, min(resumed, exchange.ended + cap)))
    provider = _measure(caused, start, killed_at)
    credited = _measure(caused + waits, start, killed_at) - provider
    covered = _measure(spans + waits, start, killed_at)
    explained = covered - provider - credited
    local = max(0.0, timeout - covered)
    account = (f"provider {provider:.1f}s, token-explained {explained:.1f}s, credited wait "
               f"{credited:.1f}s, local {local:.1f}s")
    last = exchanges[-1]
    if last.ended is None:
        stale = _stale_pending(last, killed_at, deadline)
        if stale:
            return False, f"{account}; {stale}"
        state = f"exchange {last.number} pending"
    elif last.failure is not None and killed_at - last.ended <= cap:
        state = (f"killed {killed_at - last.ended:.1f}s into the wait after "
                 f"{last.failure} on exchange {last.number}")
    else:
        return False, f"{account}; killed {killed_at - last.ended:.1f}s after exchange {last.number} ended"
    unsettled = next((exchange for exchange in exchanges if exchange.ended is not None
                      and exchange.ended >= start and not exchange.settled), None)
    if unsettled is not None:
        return False, (f"{account}; exchange {unsettled.number} ended without a protocol "
                       f"terminal or provider failure")
    if local > LOCAL_ALLOWANCE_S:
        return False, f"{account}; local time exceeds {LOCAL_ALLOWANCE_S:.0f}s; {state}"
    if provider + credited < timeout / 2:
        return False, (f"{account}; the exchanges' own output, not the provider, used the "
                       f"budget; {state}")
    lookups = sum(1 for exchange in exchanges if exchange.lookup and exchange.dispatched >= start)
    if lookups > MAX_WINDOW_LOOKUPS:
        return False, f"{account}; {lookups} model lookups in the window; {state}"
    loop = _request_loop(exchanges, start)
    if loop:
        return False, f"{account}; {loop}"
    return True, f"{account}; {state}"


def timeout_failure(env: Env | None, segment: Path | None, what: str,
                    faults: str | None = None, *, timeout: float,
                    killed_at: float | None = None) -> ScenarioFailure:
    """Score a process that outlived its `timeout` budget, killed at `killed_at` (default now).

    `provider` when `budget_attribution` shows the provider used the budget,
    judged against the dispatch deadline of `env.route`; the segment is then
    recorded on `env` so that the one exchange the kill left pending does not
    by itself make the capture incomplete. A product hang, a stalled consumer,
    a request loop, a missing capture, or a scenario with injected faults,
    which the product must absorb, stays `harness`.
    """
    if faults or segment is None:
        return ScenarioFailure(HARNESS, what)
    try:
        provider, account = budget_attribution(
            segment, time.time() if killed_at is None else killed_at, timeout,
            dispatch_deadline_s(getattr(env, "route", None)))
    except (OSError, UnicodeError, ValueError):
        provider, account = False, "capture unreadable"
    if not provider:
        return ScenarioFailure(HARNESS, f"{what} ({account})")
    if env is not None:
        env.provider_pending.add(segment)
    return ScenarioFailure(PROVIDER, f"{what}: {PROVIDER_PENDING} ({account})")


def provider_pending(env: Env, segments: list[Path], outcome: str) -> list[Path]:
    """The scenario's segments a `provider` timeout on a pending exchange explains."""
    return [segment for segment in segments
            if segment in env.provider_pending] if outcome == PROVIDER else []


def terminal_fields(info: Path, body: Path) -> dict[str, str] | None:
    """A terminal exchange record's fields, or None when it is not one.

    The product rewrites the pre-dispatch record at the exchange's terminal
    with exactly these fields; `recorded_bytes` must equal the body's size.
    """
    fields = {}
    for line in info.read_text().splitlines():
        key, value = line.split("=", 1)
        if key in fields:
            return None
        fields[key] = value
    if (fields.keys() != {"process_id", "exchange", "status", "connection", "terminal",
                          "recorded_bytes", "incomplete"}
            or not re.fullmatch(r"[1-9][0-9]*", fields["process_id"])
            or not re.fullmatch(r"[1-9][0-9]*", fields["exchange"])
            or info.stem != f"exchange-{int(fields['exchange']):04}"
            or not re.fullmatch(r"None|Some\([0-9]{3}\)", fields["status"])
            or not re.fullmatch(r"None|Some\((?:Fresh|Reused|ReplacedStale)\)",
                                fields["connection"])
            or not re.fullmatch(
                r"Complete|ProtocolComplete|ClosedEarly|Abandoned|Failed\(Failure \{ "
                r"phase: \w+, transmission: \w+, kind: \w+(?:\(\w+\))?, "
                r'cause: (?:None|Some\("[a-z_0-9]+"\)), http2_reason: (?:None|Some\([0-9]+\)), '
                r"os_error: (?:None|Some\(-?[0-9]+\)), "
                r'tls: (?:None|Some\("[a-z_0-9]+"\)), tls_alert: (?:None|Some\([0-9]+\)), '
                r'alpn: (?:None|Some\("(?:h2|http/1\.1)"\)), http2_io: (?:true|false), '
                r"connection: (?:None|Some\((?:Fresh|Reused|ReplacedStale)\)), "
                r"idle_age: (?:None|Some\([0-9.]+(?:ns|µs|ms|s)\)) \}\)",
                fields["terminal"])
            or fields["incomplete"] != "false"
            or not re.fullmatch(r"[0-9]+", fields["recorded_bytes"])
            or int(fields["recorded_bytes"]) != body.stat().st_size):
        return None
    return fields


# The `test-hooks` capture's response cap (`MAX_RESPONSE_BYTES`). The capture is
# required, so a response that outgrows it aborts the exchange in its body.
CAPTURE_RESPONSE_CAP_BYTES = 8 * 1024 * 1024
_RUNAWAY_TERMINAL = re.compile(r"Failed\(Failure \{ phase: ResponseBody, transmission: \w+, "
                               r"kind: Internal, ")


def _output_item(value: dict) -> tuple[str, str, bool] | None:
    """The (item, kind, is a delta) of one streamed model-output event, or None.

    Responses: an `output_item.added` names its item and kind, and a
    `*.delta` names its `item_id`. Messages: `content_block_start` and
    `content_block_delta` by block index. Chat: a choice's text or one tool
    call's argument delta.
    """
    kind = value.get("type")
    if kind == "response.output_item.added" and isinstance(value.get("item"), dict):
        item = value["item"]
        return (str(item.get("id")),
                " ".join(str(part) for part in (item.get("type"), item.get("name")) if part), False)
    if isinstance(kind, str) and kind.startswith("response.") and kind.endswith(".delta"):
        return str(value.get("item_id")), "", True
    if kind == "content_block_start" and isinstance(value.get("content_block"), dict):
        block = value["content_block"]
        return (f"block {value.get('index')}",
                " ".join(str(part) for part in (block.get("type"), block.get("name")) if part), False)
    if kind == "content_block_delta":
        return f"block {value.get('index')}", "", True
    choices = value.get("choices")
    if isinstance(choices, list) and len(choices) == 1 and isinstance(choices[0], dict):
        delta = choices[0].get("delta")
        if isinstance(delta, dict):
            calls = delta.get("tool_calls")
            if isinstance(calls, list) and len(calls) == 1 and isinstance(calls[0], dict):
                function = calls[0].get("function") or {}
                name = function.get("name")
                return f"tool call {calls[0].get('index')}", f"tool call {name}" if name else "", True
            if isinstance(delta.get("content"), str):
                return "text", "assistant text", True
    return None


# Stream lifecycle events that carry no output of their own.
_STREAM_PREAMBLE = frozenset({"response.created", "response.in_progress", "message_start", "ping",
                              "response.content_part.added"})


def runaway_output(info: Path, body: Path) -> str | None:
    """The output item a model ran past the capture's response cap with, or None.

    Exactly this shape: an HTTP 200 exchange whose capture stopped at
    `CAPTURE_RESPONSE_CAP_BYTES` (`recorded_bytes` equal to it and to the
    body, `incomplete=true`) and failed `Internal` in `ResponseBody`, whose
    every complete SSE event is JSON stream preamble or model output for one
    output item. The final event the cap cut short is not read.
    """
    fields = dict(line.split("=", 1) for line in info.read_text().splitlines() if "=" in line)
    if (fields.get("status") != "Some(200)" or fields.get("incomplete") != "true"
            or fields.get("recorded_bytes") != str(CAPTURE_RESPONSE_CAP_BYTES)
            or body.stat().st_size != CAPTURE_RESPONSE_CAP_BYTES
            or not _RUNAWAY_TERMINAL.match(fields.get("terminal", ""))):
        return None
    items: set[str] = set()
    kind = ""
    deltas = 0
    for event in sse_events(body.read_bytes()):
        value = _event_data(event)
        if not isinstance(value, dict):
            return None
        if value.get("type") in _STREAM_PREAMBLE:
            continue
        output = _output_item(value)
        if output is None:
            return None
        items.add(output[0])
        kind = kind or output[1]
        deltas += output[2]
    if len(items) != 1 or not deltas:
        return None
    return kind or "one output item"


def _runaways(segments: list[Path]) -> list[tuple[Path, Path, str]]:
    """(segment, info, item kind) for every capture a runaway output stopped at the cap."""
    found = []
    for segment in segments:
        for info in sorted(segment.glob("exchange-*.info")) if segment.is_dir() else ():
            try:
                kind = runaway_output(info, info.with_suffix(".body"))
            except (OSError, UnicodeError, ValueError):
                kind = None
            if kind is not None:
                found.append((segment, info, kind))
    return found


def _runaway_reason(segment: Path, info: Path, kind: str) -> str:
    return (f"model output ran past the {CAPTURE_RESPONSE_CAP_BYTES >> 20} MiB capture bound "
            f"(runaway output in {kind}) at {segment.name} {info.stem}")


def runaway_outputs(segments: list[Path]) -> list[str]:
    """`model` reasons for every capture a runaway output stopped at the cap."""
    return [_runaway_reason(*found) for found in _runaways(segments)]


# How a turn the capture cap aborted fails: the product reports the
# interrupted response stream as a transport failure.
_STREAM_ABORT = re.compile(r"\bTransport\b|stream interrupted|transport failed", re.IGNORECASE)


def _last_answered(segment: Path, info: Path) -> bool:
    """No later exchange in the segment received a response byte."""
    number = int(info.stem.removeprefix("exchange-"))
    return not any(int(body.stem.removeprefix("exchange-")) > number and body.stat().st_size
                   for body in segment.glob("exchange-*.body"))


def score_runaway(outcome: str, detail: str, segments: list[Path]) -> tuple[str, str]:
    """A failure a runaway model output explains is `model`; any other keeps its class.

    The runaway explains a `harness` or `model` failure when that failure is
    a turn that ended on the aborted stream (its text reports a transport
    failure of the response stream), the runaway exchange is in the
    scenario's last process segment (the one whose turn failed), and it is
    the last exchange there that received a response byte.
    """
    runaways = _runaways(segments)
    if not runaways or outcome not in (HARNESS, MODEL) or not _STREAM_ABORT.search(detail):
        return outcome, detail
    explained = [found for found in runaways
                 if found[0] == segments[-1] and _last_answered(found[0], found[1])]
    if not explained:
        return outcome, detail
    return MODEL, "; ".join(_runaway_reason(*found) for found in explained)


def capture_problems(segments: list[Path], pending: Collection[Path] = ()) -> list[str]:
    """Why these process captures are incomplete, unpaired or possibly exhausted.

    Each problem names its segment and, where there is one, its exchange. A
    segment in `pending` (a process scored `provider` because it timed out
    on a pending exchange) may end with that one pending exchange.
    """
    problems = []
    for segment in segments:
        try:
            if not segment.is_dir():
                problems.append(f"{segment.name} missing")
                continue
            markers = [marker for marker in ("incomplete.log", "retention.info")
                       if (segment / marker).exists()]
            problems += [f"{segment.name} has {marker}" for marker in markers]
            files = {kind: {path.stem: path for path in segment.glob(f"*.{kind}")}
                     for kind in ("request", "body", "info")}
            names = set().union(*(found.keys() for found in files.values()))
            exchange = pending_exchange(segment)
            open_name = None if exchange is None else f"exchange-{exchange:04}"
            for name in sorted(names):
                if not all(name in found and found[name].is_file() for found in files.values()):
                    problems.append(f"{segment.name} {name} unpaired")
                elif name == open_name:
                    if segment not in pending:
                        problems.append(f"{segment.name} {name} pending")
                elif terminal_fields(files["info"][name], files["body"][name]) is None \
                        and runaway_output(files["info"][name], files["body"][name]) is None:
                    problems.append(f"{segment.name} {name} not terminal")
        except (OSError, UnicodeError, ValueError):
            problems.append(f"{segment.name} unreadable")
    return problems


def capture_integrity(segments: list[Path], pending: Collection[Path] = ()) -> bool:
    """Reject incomplete, unpaired, or potentially exhausted process captures."""
    return not capture_problems(segments, pending)


def write_home(home: Path, route: Route, source_home: Path | None = None) -> None:
    """A private Plak home whose root template is `route`, with one worker it may spawn.

    An API-key route uses the key the real home stores (`plak key set`), copied
    into this home; `Env` then drops the environment variable, which would
    otherwise override the file.
    """
    (home / "agents").mkdir(parents=True, exist_ok=True)
    home.chmod(0o700)
    source_home = source_home or Path.home() / ".plak"
    stored = source_home / "secrets" / api_key_filename(route.account)
    if route.account in API_KEY_ENV and stored.is_file():
        secrets = home / "secrets"
        secrets.mkdir(mode=0o700, exist_ok=True)
        (secrets / stored.name).write_bytes(stored.read_bytes())
        (secrets / stored.name).chmod(0o600)
    (home / "config.toml").write_text(
        '[root]\nagent = "root-agent"\n\n[roster]\nworking_set = ["worker"]\n\n'
        "[orchestration]\nsubagents = true\nmax_depth = 1\nmax_children = 4\n\n"
        "[compaction]\nenabled = true\n"
    )
    effort = f'effort = "{route.effort}"\n' if route.effort else ""
    protocol = f'protocol = "{route.protocol}"\n' if route.protocol else ""
    # The root spawns [roster] working_set; both templates are leaves as cards.
    for name, description in (
        ("root-agent", "Drive the session and delegate only when asked."),
        ("worker", "Implement one bounded change and report what changed."),
    ):
        (home / "agents" / f"{name}.toml").write_text(
            f'account = "{route.account}"\nmodel = "{route.model}"\n{effort}{protocol}'
            f'composition = "{DEFAULT_COMPOSITION}"\ndescription = "{description}"\n'
            'access = "full"\ndelegation = "explicit"\n'
        )


def pin_max_model_rounds(home: Path, rounds: int) -> None:
    """Append `[turn] max_model_rounds` to a home `write_home` just created.

    The smoke home is shared, and a running turn keeps the cap it was admitted
    under, so a scenario that must hit the wrap-up round uses a private home.
    The key is the product `[turn]` field.
    """
    path = home / "config.toml"
    text = path.read_text()
    if "[turn]" in text or "max_model_rounds" in text:
        raise ScenarioFailure(HARNESS, "PLAK_HOME already sets a turn round cap")
    if not text.endswith("\n"):
        text += "\n"
    path.write_text(f"{text}\n[turn]\nmax_model_rounds = {rounds}\n")


def pin_composition(home: Path, composition: str) -> None:
    """Switch every template `write_home` just created to `composition`."""
    for path in sorted((home / "agents").glob("*.toml")):
        text = path.read_text()
        needle = f'composition = "{DEFAULT_COMPOSITION}"\n'
        if needle not in text:
            raise ScenarioFailure(HARNESS, f"{path.name} does not use the default composition")
        path.write_text(text.replace(needle, f'composition = "{composition}"\n'))


def git_repo(path: Path, files: dict[str, str]) -> Path:
    """A committed git checkout holding `files`."""
    path.mkdir(parents=True)
    for name, text in files.items():
        (path / name).write_text(text)
    for argv in (["init", "-q"], ["add", "."],
                 ["-c", "user.name=smoke", "-c", "user.email=smoke@example.invalid",
                  "commit", "-qm", "seed"]):
        subprocess.run(["git", *argv], cwd=path, check=True)
    return path


def unittest_passes(repo: Path) -> bool:
    result = subprocess.run(["python3", "-m", "unittest", "-q"], cwd=repo,
                            capture_output=True, text=True, timeout=120)
    return result.returncode == 0


def session_records(home: Path, session_id: str) -> list[dict]:
    """The session's event log. State lives under a per-format key directory."""
    records = []
    for log in home.glob(f"state/*/sessions/{session_id}.events.jsonl"):
        for line in log.read_text().splitlines():
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return records


def committed_compactions(records: list[dict]) -> int:
    return sum(1 for record in records if record.get("kind") == "compact.finished"
               and record.get("outcome") == "committed")


# Spec 01 §10.2: a paid successor must measure strictly smaller than its source.
# The summary text is at most 8_192 tokens inside a 16_384 output bound, so
# 10_000 tokens of older tool text (bytes/4) stays larger than that summary.
# The read must also fit one turn's observation allowance, which charges one
# token per wire byte (spec 01 §10.3). OpenRouter's admitted input depends on
# endpoint context/prompt limits and the output reservation, not a fixed 64K cap.
COMPACTION_FOLD_TOKENS = 10_000
_FOLD_LINE = b"padding line for compaction fold; not a fact.\n"


def compaction_padding(tokens: int = COMPACTION_FOLD_TOKENS) -> str:
    """ASCII tool text with no surviving fact, sized above a semantic summary."""
    if tokens < 1:
        raise ValueError("fold tokens must be positive")
    target = tokens * 4
    repeats = (target + len(_FOLD_LINE) - 1) // len(_FOLD_LINE)
    return (_FOLD_LINE * repeats)[:target].decode("ascii")


def needs_compaction_fold(prepared_peak: int | None) -> bool:
    """True when prepared input is still smaller than a summary can replace."""
    peak = prepared_peak if type(prepared_peak) is int and prepared_peak > 0 else 0
    return peak < COMPACTION_FOLD_TOKENS


def _u64(record: dict, key: str) -> int:
    """A missing or non-integer usage field counts as zero so a partial row cannot crash a report."""
    value = record.get(key, 0)
    if type(value) is not int or value < 0:
        return 0
    return value


@dataclasses.dataclass(frozen=True)
class TokenUsage:
    """Provider-reported tokens. Absent observations stay zero."""

    input_tokens: int = 0
    output_tokens: int = 0
    cached_input_tokens: int = 0
    reasoning_tokens: int = 0

    def add(self, other: TokenUsage) -> TokenUsage:
        return TokenUsage(
            self.input_tokens + other.input_tokens,
            self.output_tokens + other.output_tokens,
            self.cached_input_tokens + other.cached_input_tokens,
            self.reasoning_tokens + other.reasoning_tokens,
        )

    def as_dict(self) -> dict[str, int]:
        return {
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cached_input_tokens": self.cached_input_tokens,
            "reasoning_tokens": self.reasoning_tokens,
        }


def usage_delta(after: TokenUsage, before: TokenUsage) -> TokenUsage:
    return TokenUsage(
        after.input_tokens - before.input_tokens,
        after.output_tokens - before.output_tokens,
        after.cached_input_tokens - before.cached_input_tokens,
        after.reasoning_tokens - before.reasoning_tokens,
    )


def sum_usage(usages) -> dict[str, int]:
    total = TokenUsage()
    for usage in usages:
        total = total.add(usage)
    return total.as_dict()


def usage_from_record(record: object) -> TokenUsage:
    """One session-log `turn.usage` row. Any other value contributes nothing."""
    if not isinstance(record, dict) or record.get("kind") != "turn.usage":
        return TokenUsage()
    return TokenUsage(
        _u64(record, "input_tokens"),
        _u64(record, "output_tokens"),
        _u64(record, "cached_input_tokens"),
        _u64(record, "reasoning_tokens"),
    )


def transcript_usage(path: Path) -> TokenUsage:
    """Sum JSONL `turn` usage events in one agent transcript.

    The session log and this transcript are two projections of the same
    observation. A path that emits only the JSONL record still has to count.
    Callers take the per-field maximum with the session log so a path that
    emits both is not billed twice.
    """
    total = TokenUsage()
    if not path.is_file():
        return total
    try:
        text = path.read_text(errors="replace")
    except OSError:
        return total
    for line in text.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(record, dict) or record.get("kind") != "turn":
            continue
        if not any(key in record for key in ("input_tokens", "output_tokens")):
            continue
        total = total.add(TokenUsage(
            _u64(record, "input_tokens"),
            _u64(record, "output_tokens"),
            _u64(record, "cached_input_tokens"),
            _u64(record, "reasoning_tokens"),
        ))
    return total


def larger_usage(left: TokenUsage, right: TokenUsage) -> TokenUsage:
    """Per-field maximum of two projections of the same turns."""
    return TokenUsage(
        max(left.input_tokens, right.input_tokens),
        max(left.output_tokens, right.output_tokens),
        max(left.cached_input_tokens, right.cached_input_tokens),
        max(left.reasoning_tokens, right.reasoning_tokens),
    )


def session_usage(home: Path) -> TokenUsage:
    """Sum `turn.usage` rows in every session log under this home."""
    total = TokenUsage()
    if not home.is_dir():
        return total
    for log in home.glob("state/*/sessions/*.events.jsonl"):
        try:
            text = log.read_text()
        except OSError:
            continue
        for line in text.splitlines():
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                continue
            total = total.add(usage_from_record(record))
    return total


class Env:
    """Where one run's binary, home and artifacts are."""

    def __init__(self, binary: Path, home: Path, artifacts: Path):
        self.binary, self.home, self.artifacts = binary, home, artifacts
        # The route under test, when the driver names it; timeout scoring reads
        # its dispatch deadline.
        self.route: Route | None = None
        self.lookup_faults: str | None = None
        self.fixture_seed: int | None = None
        self._capture_root: Path | None = None
        self.capture_segments: list[Path] = []
        # Segments whose timed-out process was scored `provider` on a pending exchange.
        self.provider_pending: set[Path] = set()
        # Compositions the `plak agent` hellos of this run reported.
        self.bound_compositions: set[str] = set()

    @property
    def capture_root(self) -> Path:
        if self._capture_root is None:
            # Resolve ancestors (a lane checkout may sit behind a symlink); the
            # private-directory check below still refuses a symlinked leaf.
            root = ROOT.resolve() / "tests/provider-authority"
            root.mkdir(mode=0o700, exist_ok=True)
            self._private_directory(root)
            self._capture_root = Path(tempfile.mkdtemp(prefix=f"{self.artifacts.name}-", dir=root))
        return self._capture_root

    @staticmethod
    def _private_directory(path: Path) -> None:
        path = path.parent.resolve() / path.name
        metadata = path.lstat()
        if (stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode)
                or metadata.st_uid != os.getuid()
                or stat.S_IMODE(metadata.st_mode) != 0o700):
            raise ScenarioFailure(HARNESS, "capture directory must be owned, private and symlink-free")

    def environ(self, faults: str | None = None,
                source: dict[str, str] | None = None,
                capture_dir: Path | None = None) -> dict[str, str]:
        self.lookup_faults = faults
        dropped = {"PLAK_TEST_FAULTS", "PLAK_HOME", "PLAK_TEST_EXCHANGE_CAPTURE_DIR"}
        # A stale environment key must never shadow a key copied from the
        # product-supported file store. Drop all supported API-key variables;
        # this harness intentionally authenticates only through that store.
        dropped |= set(API_KEY_ENV.values())
        source = source if source is not None else os.environ
        env = {key: value for key, value in source.items() if key not in dropped}
        env["PLAK_HOME"] = str(self.home)
        capture_dir = capture_dir or self.capture_root / f"segment-{len(self.capture_segments) + 1:03d}"
        if not capture_dir.is_relative_to(self.capture_root):
            raise ScenarioFailure(HARNESS, "capture segment must belong to this run")
        if not capture_dir.exists():
            capture_dir.mkdir(mode=0o700)
        self._private_directory(capture_dir)
        if capture_dir in self.capture_segments or any(capture_dir.iterdir()):
            raise ScenarioFailure(HARNESS, "capture segment must be fresh for each process")
        self.capture_segments.append(capture_dir)
        env["PLAK_TEST_EXCHANGE_CAPTURE_DIR"] = str(capture_dir)
        env["PLAK_LOG"] = AUTHORITY_DIAGNOSTICS_FILTER
        if faults:
            env["PLAK_TEST_FAULTS"] = faults
        return env


def run_cli(env: Env, repo: Path, args: list[str], log: Path,
            timeout: float = 300, home: Path | None = None,
            faults: str | None = None) -> tuple[int, str]:
    """`plak run …`; returns the exit status and stdout, keeping both streams.

    `home` overrides `PLAK_HOME` for this process only. Capture still belongs
    to `env`. Stdin is closed, so a prompt Plak cannot ask (a compatibility
    notice) fails at once instead of waiting out the timeout. A committed
    answer cut at its output limit (`truncated_answer`) completed its turn and
    is returned as exit 0, so the scenario's oracle judges the printed answer;
    the log keeps the real status.
    """
    child = env.environ(faults=faults)
    segment = env.capture_segments[-1]
    if home is not None:
        child["PLAK_HOME"] = str(home)
    try:
        result = subprocess.run([str(env.binary), "run", *args], cwd=repo,
                                env=child, stdin=subprocess.DEVNULL, capture_output=True,
                                text=True, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        killed_at = time.time()
        log.write_text(f"timeout after {timeout}s\n{error.stdout or ''}{error.stderr or ''}")
        raise timeout_failure(env, segment, f"plak run did not finish in {timeout:.0f}s",
                              faults, timeout=timeout, killed_at=killed_at) from None
    code, note = result.returncode, ""
    if truncated_answer(code, result.stderr):
        code, note = 0, " (answer committed at its output limit; scored as completed)"
    log.write_text(f"exit {result.returncode}{note}\n--- stdout\n{result.stdout}"
                   f"--- stderr\n{result.stderr}")
    return code, result.stdout


@dataclasses.dataclass
class Turn:
    """One prompt's observed outcome on the JSONL wire."""

    phase: str | None = None
    model_error_kind: str | None = None
    http_status: int | None = None
    text: str = ""
    # Tool calls the model made: each starts `running` or, refused before it
    # ran (by the host or for its arguments), arrives as `rejected:<reason>`.
    tools: int = 0
    errors: list[str] = dataclasses.field(default_factory=list)
    # Typed `model_refusal` fields (`refusal_summary`), never the provider's prose.
    refusals: list[str] = dataclasses.field(default_factory=list)
    retries: list[dict] = dataclasses.field(default_factory=list)
    input_tokens: int = 0
    output_tokens: int = 0
    cached_input_tokens: int = 0
    reasoning_tokens: int = 0
    seconds: float = 0.0
    lookup_faults: str | None = None

    @property
    def completed(self) -> bool:
        return self.phase == "completed"

    def usage(self) -> TokenUsage:
        return TokenUsage(
            self.input_tokens, self.output_tokens,
            self.cached_input_tokens, self.reasoning_tokens,
        )


def apply_usage_event(turn: Turn, event: dict) -> None:
    """Add one JSONL `turn` usage observation. Other events are ignored."""
    if event.get("kind") != "turn":
        return
    turn.input_tokens += _u64(event, "input_tokens")
    turn.output_tokens += _u64(event, "output_tokens")
    turn.cached_input_tokens += _u64(event, "cached_input_tokens")
    turn.reasoning_tokens += _u64(event, "reasoning_tokens")


# Job lifecycle lines the turn waiter does not return. Large enough for one
# short turn once assistant and reasoning deltas are left out.
OBSERVED_LIMIT = 2048
_STREAM_KINDS = frozenset({"assistant", "reasoning"})


def _omitted_observation(event: dict) -> bool:
    """Text the bounded job log must not keep.

    Root `assistant` and `reasoning` deltas, and the same ops wrapped as
    `child_live`, would evict `job_started` from a chatty child. Any other
    `child_live` op stays. JSONL v1 omits `child_live`; this still drops it
    if a line arrives.
    """
    kind = event.get("kind")
    if kind in _STREAM_KINDS:
        return True
    if kind != "child_live":
        return False
    op = event.get("op")
    if not isinstance(op, dict):
        return True
    return op.get("kind") in _STREAM_KINDS


class Agent:
    """A `plak agent` process spoken to over JSONL."""

    def __init__(self, env: Env, repo: Path, name: str, *, args: list[str],
                 faults: str | None = None, capture_dir: Path | None = None,
                 home: Path | None = None, observe: bool = False):
        self.lookup_faults = faults
        child_env = env.environ(faults, capture_dir=capture_dir)
        # A scenario that must pin `[turn]` uses a private home. Capture stays
        # on `env`, matching `run_cli`'s home override.
        if home is not None:
            child_env["PLAK_HOME"] = str(home)
        self.home = home or env.home
        self.env = env
        self.capture_dir = env.capture_segments[-1]
        self.transcript = (env.artifacts / f"{name}.jsonl").open("a")
        self.stderr = (env.artifacts / f"{name}.stderr").open("a")
        self.proc = subprocess.Popen(
            [str(env.binary), "agent", *args], cwd=repo, env=child_env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
            text=True, start_new_session=True,
        )
        self.events: queue.Queue = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()
        self.session_id = None
        self.composition: str | None = None
        self.children: set[str] = set()
        self.recovered: list[dict] = []
        self.next_id = 0
        # Only the job driver keeps a bounded log. Smoke and the hour soak do
        # not: every assistant delta would otherwise stay for the whole run.
        # Constructors that skip __init__ simply do not have it.
        if observe:
            self.observed: collections.deque = collections.deque(maxlen=OBSERVED_LIMIT)
        # Leave room to observe the catalog outer deadline after setup/refresh.
        hello = self.wait_for(lambda event: event.get("kind") in ("ready", "bye"), 180)
        if hello.get("kind") != "ready" or not self.session_id:
            self.stderr.flush()
            detail = Path(self.stderr.name).read_text(errors="replace")
            classify_cli_failure(self.proc.poll() or 5, detail + json.dumps(hello),
                                 "plak agent startup", segments=[self.capture_dir], lookup_faults=faults)
        if self.composition:
            env.bound_compositions.add(self.composition)

    def _read(self) -> None:
        for line in self.proc.stdout:
            self.transcript.write(line)
            self.transcript.flush()
            try:
                self.events.put(json.loads(line))
            except json.JSONDecodeError:
                self.events.put({"kind": "unparsable", "line": line})
        self.events.put({"kind": "eof"})

    def send(self, kind: str, **fields: object) -> str:
        self.next_id += 1
        command_id = str(self.next_id)
        self.proc.stdin.write(json.dumps({"v": 1, "kind": kind, "id": command_id, **fields}) + "\n")
        self.proc.stdin.flush()
        return command_id

    def wait_for(self, predicate, timeout: float) -> dict:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise EventTimeout(HARNESS, f"no expected event within {timeout:.0f}s")
            try:
                event = self.events.get(timeout=remaining)
            except queue.Empty:
                continue
            self.note(event)
            if predicate(event) or event.get("kind") == "eof":
                return event

    def note(self, event: dict) -> None:
        """Remember one stdout event, including kinds the turn waiter does not return.

        Streamed text is not stored, including a child's `child_live` text or
        reasoning op. A deque's maxlen drops the oldest retained event, so a
        job turn cannot grow without bound either.
        """
        self._observe(event)
        observed = getattr(self, "observed", None)
        if observed is None or _omitted_observation(event):
            return
        observed.append((time.monotonic(), event))

    def _observe(self, event: dict) -> None:
        if event.get("kind") == "hello":
            self.session_id = event.get("session_id")
            composition = event.get("composition")
            if isinstance(composition, str) and composition:
                self.composition = composition
        if event.get("kind") == "recovered":
            # Attach can emit this before `ready`, which `__init__` consumes.
            # A partial Agent built by a unit test has no list yet.
            bucket = getattr(self, "recovered", None)
            if bucket is None:
                bucket = []
                self.recovered = bucket
            bucket.append(event)
        if event.get("kind") == "tree":
            for node in event.get("nodes", []):
                if node.get("depth", 0) > 0 and node.get("agent_id"):
                    self.children.add(node["agent_id"])

    def prompt(self, text: str, timeout: float = 300, on_event=None) -> Turn:
        """Send a root prompt and wait for its terminal lifecycle and `busy:false`."""
        command_id = self.send("prompt", text=text)
        turn, started, terminal = Turn(lookup_faults=getattr(self, "lookup_faults", None)), time.monotonic(), False
        while True:
            try:
                event = self.wait_for(lambda e: e.get("id") == command_id or
                                      e.get("kind") in ("error", "busy", "eof"), timeout)
            except EventTimeout as error:
                raise timeout_failure(getattr(self, "env", None), getattr(self, "capture_dir", None),
                                      f"turn: {error}", turn.lookup_faults, timeout=timeout) from None
            kind = event.get("kind")
            if on_event:
                on_event(event)
            if kind == "eof":
                raise ScenarioFailure(HARNESS, "plak agent exited during a turn")
            if kind == "assistant":
                turn.text += event.get("text", "")
            elif kind == "tool" and (event.get("state") == "running"
                                     or str(event.get("state", "")).startswith("rejected")):
                turn.tools += 1
            elif kind == "error":
                turn.errors.append(event.get("message", ""))
            elif kind == "model_refusal":
                turn.refusals.append(refusal_summary(event))
            elif kind == "retry":
                turn.retries.append(event)
            elif kind == "turn":
                apply_usage_event(turn, event)
            elif kind == "turn_lifecycle" and event.get("phase") != "admitted":
                turn.phase = event.get("phase")
                turn.model_error_kind = event.get("model_error_kind")
                turn.http_status = event.get("http_status")
                terminal = True
            elif kind == "busy" and event.get("busy") is False and terminal:
                turn.seconds = time.monotonic() - started
                return turn
            elif kind == "busy" and event.get("busy") is False and turn.errors:
                # Refused before admission: the error is the whole answer.
                turn.phase, turn.seconds = "refused", time.monotonic() - started
                return turn

    def command(self, kind: str, timeout: float = 300) -> dict:
        """An idle command such as `compact`: its error, or the `busy:false` that ends it.

        The host's busy events for a command carry no id (only turn-scoped events
        do), so the end is the first `busy:false` after a `busy:true`.
        """
        command_id = self.send(kind)
        busy = False
        failed_tool = None
        notes: list[str] = []

        def done(event: dict) -> bool:
            nonlocal busy, failed_tool
            if event.get("kind") == "error" and event.get("id") == command_id:
                return True
            if event.get("kind") == "tool" and event.get("activity_kind") == kind \
                    and event.get("state") == "failed":
                failed_tool = event.get("subject") or f"{kind} failed"
            # The failed workflow's cause (for example a provider 429) is a
            # note on the command, not part of the tool row's subject.
            if event.get("kind") == "status" and event.get("phase") == "note" \
                    and event.get("id") == command_id and event.get("detail"):
                notes.append(str(event["detail"]))
            if event.get("kind") == "busy":
                busy, was_busy = event.get("busy") is True, busy
                return was_busy and not busy
            return False

        answer = self.wait_for(done, timeout)
        if failed_tool:
            return {"kind": "error", "message": " ".join([failed_tool, *notes])}
        if answer.get("kind") == "error" and notes:
            message = str(answer.get("message") or "")
            return {**answer, "message": " ".join([message, *notes]).strip()}
        return answer

    def quit(self, timeout: float = 45) -> int:
        try:
            self.send("quit")
            self.proc.stdin.close()
        except (BrokenPipeError, OSError):
            # The process is gone (a scenario may have killed it). Close the
            # pipe now, discarding the unsent line, so garbage collection does
            # not retry the flush and print an ignored BrokenPipeError.
            try:
                self.proc.stdin.close()
            except OSError:
                pass
        try:
            return self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            os.killpg(self.proc.pid, signal.SIGKILL)
            self.proc.wait()
            raise ScenarioFailure(HARNESS, "plak agent did not exit after quit")
        finally:
            self.transcript.close()
            self.stderr.close()

    def signal(self, number: int) -> None:
        os.killpg(self.proc.pid, number)


class Tui:
    """The interactive TUI on a pseudo-terminal, driven by keystrokes."""

    def __init__(self, env: Env, repo: Path, log: Path, args: list[str] = (),
                 capture_dir: Path | None = None, faults: str | None = None):
        self.lookup_faults = faults
        child_env = {**env.environ(faults=faults, capture_dir=capture_dir), "TERM": "xterm-256color"}
        self.capture_dir = env.capture_segments[-1]
        self.log = log.open("wb")
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(repo)
            os.execve(str(env.binary), [str(env.binary), *args],
                      child_env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        self.screen = b""
        self._quit_output: bytearray | None = None

    def pump(self, seconds: float) -> None:
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.fd], [], [], 0.1)
            if ready:
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError:
                    return
                self.log.write(chunk)
                self.screen = (self.screen + chunk)[-200_000:]
                if self._quit_output is not None:
                    self._quit_output.extend(chunk)

    def type(self, text: str) -> None:
        for char in text:
            os.write(self.fd, char.encode())
            self.pump(0.01)

    def key(self, sequence: bytes) -> None:
        os.write(self.fd, sequence)
        self.pump(0.2)

    def wait_until(self, predicate, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump(0.5)
            if predicate():
                return True
            failure = lookup_failure(self.screen.decode(errors="replace"), getattr(self, "lookup_faults", None))
            if failure:
                self.quit()
                raise ScenarioFailure(failure[1], f"TUI startup: {failure[0]}")
        return False

    def quit(self, timeout: float = 30) -> int | None:
        # The screen tail can roll over; collect only bytes received after Ctrl+Q.
        self._quit_output = bytearray()
        try:
            self.key(b"\x11")  # Ctrl+Q
            confirmed = False
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                self.pump(0.2)
                # Quit asks while a turn, an agent or a background process still
                # runs (spec 02 §5.3). Focus starts on Quit, so Enter confirms;
                # Right would move onto Cancel.
                if not confirmed and b"Quit plak?" in self._quit_output:
                    confirmed = True
                    time.sleep(0.8)
                    try:
                        os.write(self.fd, b"\r")
                    except OSError:
                        pass
                pid, status = os.waitpid(self.pid, os.WNOHANG)
                if pid:
                    self.log.close()
                    return os.waitstatus_to_exitcode(status)
            os.kill(self.pid, signal.SIGKILL)
            os.waitpid(self.pid, 0)
            self.log.close()
            return None
        finally:
            self._quit_output = None


def table(results: list[Result]) -> str:
    """The pass/fail table printed at the end of a run."""
    rows = [("scenario", "surface", "outcome", "secs", "detail")]
    rows += [(r.scenario, r.surface, r.outcome, f"{r.seconds:.0f}", r.detail) for r in results]
    widths = [max(len(row[i]) for row in rows) for i in range(4)]
    lines = ["  ".join(cell.ljust(width) for cell, width in zip(row[:4], widths)) + "  " + row[4]
             for row in rows]
    return "\n".join(line.rstrip() for line in lines)
