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
    owner matrix (the dry-run estimator skips them).
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

    def flags(self) -> list[str]:
        flags = ["--account", self.account, "--model", self.model]
        if self.effort:
            flags += ["--effort", self.effort]
        if self.protocol:
            flags += ["--protocol", self.protocol]
        return flags


def _route(name: str, account: str, model: str, effort: str | None,
           protocol: str | None, endpoint: tuple[str, str], route_class: str,
           planned: bool = True) -> Route:
    host, path = endpoint
    return Route(name, account, model, effort, protocol, host, path, route_class, planned)


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
    "deepseek": _route(
        "deepseek", "openrouter", "deepseek/deepseek-v4.1-flash", None, None,
        _OR_CHAT, FULL),
    "deepseek-messages": _route(
        "deepseek-messages", "openrouter", "deepseek/deepseek-v4.1-flash", None,
        "messages", _OR_MESSAGES, FULL),
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
    "or-lfm-free": _route(
        "or-lfm-free", "openrouter", "liquid/lfm-2.5-2.6b:free", None, None,
        _OR_CHAT, INFORMATIONAL),
    # Not offered as `:free` on the probe day. Paid ids, informational depth.
    "or-ling": _route(
        "or-ling", "openrouter", "inclusionai/ling-3.0-flash-vl", None, None,
        _OR_CHAT, INFORMATIONAL),
    "or-qwen38": _route(
        "or-qwen38", "openrouter", "qwen/qwen3.8-27b", None, None, _OR_CHAT, INFORMATIONAL),
    "codex": _route("codex", "codex", "gpt-6-luna", "low", None, _CODEX, FULL),
    "codex-luna-medium": _route(
        "codex-luna-medium", "codex", "gpt-6-luna", "medium", None, _CODEX, LIGHT),
    "codex-sol": _route("codex-sol", "codex", "gpt-6.1-sol", "low", None, _CODEX, LIGHT),
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


# Typed provider refusals and outages. `QuotaExhausted` is the name some
# driver surfaces used before the product kind `QuotaExceeded`; both count.
# `Authentication` is the provider refusing the credential, not a product bug.
PROVIDER_ERROR_KINDS = frozenset({
    "RateLimited", "QuotaExceeded", "QuotaExhausted", "Unavailable", "Authentication",
})
# Statuses that refuse before acceptance, or are the ambiguous 5xx the spec
# already records as provider. 429 is the rate-limit case on every surface.
PROVIDER_HTTP_STATUSES = frozenset({429, 500, 502, 503, 529})
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


def text_is_provider_refusal(text: str) -> bool:
    """True when operator text is a typed provider refusal or outage.

    A product refusal that merely mentions a missing checkpoint or a tool
    contract is not in this set. `unavailable` alone is not either: the
    navigator uses that word for a missing hub.
    """
    lowered = text.lower()
    if "server_is_overloaded" in lowered or "server is overloaded" in lowered:
        return True
    if "rejected-before-acceptance" in lowered or "rejected before acceptance" in lowered:
        return True
    if "temporarily overloaded" in lowered:
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


def _provider_child_fault(text: str) -> bool:
    """Fault prose that is a provider refusal, including provider-unavailable."""
    if text_is_provider_refusal(text):
        return True
    lowered = text.lower()
    if ("provider unavailable" in lowered or "provider-unavailable" in lowered
            or "provider_unavailable" in lowered):
        return True
    if "quota exceeded" in lowered or "quota exhausted" in lowered:
        return True
    return False


def provider_child_record(record: dict) -> bool:
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
        if isinstance(message, str) and _provider_child_fault(message):
            return True
    elif isinstance(failure, str) and _provider_child_fault(failure):
        return True
    fault = record.get("fault")
    if isinstance(fault, str) and _provider_child_fault(fault):
        return True
    return False


def _provider_child_json(value: dict) -> bool:
    if provider_child_record(value):
        return True
    for key in ("agents", "nodes"):
        rows = value.get(key)
        if isinstance(rows, list) and any(
                isinstance(row, dict) and provider_child_record(row) for row in rows):
            return True
    return False


def provider_child_text(text: str) -> bool:
    """True when child-failure text, or wait/tree JSON embedded in it, is a provider refusal.

    Reads `quota <label>` and `failure <kind>` from the flattened row, and
    `blocked.quota` / `failure` / `fault` from a wait receipt or tree node.
    """
    if _provider_child_fault(text):
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
        if isinstance(value, dict) and _provider_child_json(value):
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


def provider_reason_from(kind: str | None, http_status: int | None, text: str) -> str | None:
    """Why this observation is a typed provider refusal, or None.

    Covers JSONL turn failures, `plak run` stderr, TUI session-log terminals,
    manual `/compact` refusals, and child job output. A kernel
    stream-validation failure is not decided here.
    """
    if kind in PROVIDER_ERROR_KINDS or http_status in PROVIDER_HTTP_STATUSES:
        status = f"{kind or ''} {http_status or ''}".strip()
        return status or text[:160]
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
    """Only the failing process's highest-numbered, complete exchange is model evidence."""
    if not segments or len(segments) != 1 or not capture_integrity(segments):
        return False
    body = max(segments[0].glob("exchange-*.body"),
               key=lambda path: int(path.stem.removeprefix("exchange-")), default=None)
    if body is None:
        return False
    try:
        metadata = body.with_suffix(".info").read_text().splitlines()
        return (any(line in ("terminal=Complete", "terminal=ProtocolComplete") for line in metadata)
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
    if capture_shows_usage_on_finishless_choice(segments):
        return PROVIDER
    for marker, protocol in TOOL_ARGUMENT_VALIDATION_MARKERS.items():
        if marker in lowered and capture_shows_invalid_tool_arguments(segments, protocol):
            return MODEL
    return HARNESS


def _absorbed_injection(kind: str | None, http_status: int | None, fault: str | None) -> bool:
    """The failure is the injected fault the product was required to absorb."""
    if fault not in ABSORBED_FAULTS:
        return False
    if fault == "429":
        return http_status == 429 or kind in ("RateLimited", "QuotaExceeded", "QuotaExhausted")
    return http_status == 529 or kind == "Unavailable"


def _real_errors_followed_the_injection(turn: Turn) -> bool:
    """`fault_plan` injects exactly one response (`nth=1`), so a second HTTP
    retry on the turn was the real provider refusing again."""
    return sum(1 for retry in turn.retries if retry.get("cause_kind") == "http_status") > 1


def classify_incomplete(turn: Turn, what: str, *, injected_fault: str | None = None,
                        segments: list[Path] | None = None) -> None:
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
    status = f"{turn.phase} {turn.model_error_kind or ''} {turn.http_status or ''}".strip()
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
    if reason and _absorbed_injection(turn.model_error_kind, turn.http_status, injected_fault) \
            and _real_errors_followed_the_injection(turn):
        raise ScenarioFailure(PROVIDER, f"{what}: {reason} after the injected fault was retried")
    if reason and not _absorbed_injection(turn.model_error_kind, turn.http_status, injected_fault):
        raise ScenarioFailure(PROVIDER, f"{what}: {reason}")
    if turn.phase == "refused" and turn.refusals:
        raise ScenarioFailure(PROVIDER, f"{what}: provider refused the request "
                                        f"({turn.refusals[-1]})")
    errors = " | ".join(" ".join(error.split()) for error in turn.errors)[:200]
    raise ScenarioFailure(HARNESS, f"{what}: {status} {errors}".strip())


def classify_command_error(message: str, what: str, *,
                           segments: list[Path] | None = None) -> None:
    """A manual `/compact` or other command error, classified like a turn."""
    text = message or ""
    validation = stream_validation_outcome(text, segments)
    if validation == MODEL:
        raise ScenarioFailure(MODEL, f"{what}: model emitted invalid tool arguments")
    if validation == PROVIDER:
        raise ScenarioFailure(PROVIDER, f"{what}: provider stream violated the documented protocol")
    if validation == HARNESS:
        raise ScenarioFailure(HARNESS, f"{what}: kernel stream validation failed without "
                                       f"a captured protocol violation")
    if text_is_provider_refusal(text):
        raise ScenarioFailure(PROVIDER, f"{what}: {' '.join(text.split())[:200]}")
    raise ScenarioFailure(HARNESS, f"{what}: {' '.join(text.split())[:200]}")


def classify_cli_failure(code: int, stderr: str, what: str, *,
                         segments: list[Path] | None = None) -> None:
    """`plak run` exit. Exit 5 is the agent class; the text decides provider."""
    detail = " ".join(stderr.split())[:200]
    validation = stream_validation_outcome(stderr, segments)
    if validation == MODEL:
        raise ScenarioFailure(MODEL, f"{what}: model emitted invalid tool arguments")
    if validation == PROVIDER:
        raise ScenarioFailure(PROVIDER, f"{what}: provider stream violated the documented protocol")
    if validation == HARNESS:
        raise ScenarioFailure(HARNESS, f"{what}: kernel stream validation failed without "
                                       f"a captured protocol violation")
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


def route_report(route: Route, composition: str = DEFAULT_COMPOSITION) -> dict[str, str | None]:
    """The evidence tuple a live report should name for this route."""
    return {
        "route": route.name,
        "route_class": route.route_class,
        "account": route.account,
        "model": route.model,
        "effort": route.effort,
        "protocol": route.protocol,
        "composition": composition,
    }


def capture_integrity(segments: list[Path]) -> bool:
    """Reject incomplete, unpaired, or potentially exhausted process captures."""
    try:
        for segment in segments:
            if not segment.is_dir():
                return False
            if any((segment / marker).exists() for marker in ("incomplete.log", "retention.info")):
                return False
            files = {kind: {path.stem: path for path in segment.glob(f"*.{kind}")}
                     for kind in ("request", "body", "info")}
            if (files["request"].keys() != files["info"].keys()
                    or files["body"].keys() != files["info"].keys()):
                return False
            for name, path in files["info"].items():
                if not all(files[kind][name].is_file() for kind in files):
                    return False
                fields = {}
                for line in path.read_text().splitlines():
                    key, value = line.split("=", 1)
                    if key in fields:
                        return False
                    fields[key] = value
                if (fields.keys() != {"process_id", "exchange", "status", "connection", "terminal",
                                      "recorded_bytes", "incomplete"}
                        or not re.fullmatch(r"[1-9][0-9]*", fields["process_id"])
                        or not re.fullmatch(r"[1-9][0-9]*", fields["exchange"])
                        or name != f"exchange-{int(fields['exchange']):04}"
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
                        or int(fields["recorded_bytes"]) != files["body"][name].stat().st_size):
                    return False
    except (OSError, UnicodeError, ValueError):
        return False
    return True


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
# token per wire byte (spec 01 §10.3): an OpenRouter custom route caps projected
# input at 65_536, and a larger read is clipped and ends the turn ContextLimit.
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
        self.fixture_seed: int | None = None
        self._capture_root: Path | None = None
        self.capture_segments: list[Path] = []

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
    to `env`.
    """
    child = env.environ(faults=faults)
    if home is not None:
        child["PLAK_HOME"] = str(home)
    try:
        result = subprocess.run([str(env.binary), "run", *args], cwd=repo,
                                env=child, capture_output=True, text=True,
                                timeout=timeout)
    except subprocess.TimeoutExpired as error:
        log.write_text(f"timeout after {timeout}s\n{error.stdout or ''}{error.stderr or ''}")
        raise ScenarioFailure(HARNESS, f"plak run did not finish in {timeout:.0f}s")
    log.write_text(f"exit {result.returncode}\n--- stdout\n{result.stdout}"
                   f"--- stderr\n{result.stderr}")
    return result.returncode, result.stdout


@dataclasses.dataclass
class Turn:
    """One prompt's observed outcome on the JSONL wire."""

    phase: str | None = None
    model_error_kind: str | None = None
    http_status: int | None = None
    text: str = ""
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
        child_env = env.environ(faults, capture_dir=capture_dir)
        # A scenario that must pin `[turn]` uses a private home. Capture stays
        # on `env`, matching `run_cli`'s home override.
        if home is not None:
            child_env["PLAK_HOME"] = str(home)
        self.home = home or env.home
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
        self.children: set[str] = set()
        self.recovered: list[dict] = []
        self.next_id = 0
        # Only the job driver keeps a bounded log. Smoke and the hour soak do
        # not: every assistant delta would otherwise stay for the whole run.
        # Constructors that skip __init__ simply do not have it.
        if observe:
            self.observed: collections.deque = collections.deque(maxlen=OBSERVED_LIMIT)
        hello = self.wait_for(lambda event: event.get("kind") in ("ready", "bye"), 60)
        if hello.get("kind") != "ready" or not self.session_id:
            raise ScenarioFailure(HARNESS, f"plak agent did not become ready: {hello}")

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
                raise ScenarioFailure(HARNESS, f"no expected event within {timeout:.0f}s")
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
        turn, started, terminal = Turn(), time.monotonic(), False
        while True:
            event = self.wait_for(lambda e: e.get("id") == command_id or
                                  e.get("kind") in ("error", "busy", "eof"), timeout)
            kind = event.get("kind")
            if on_event:
                on_event(event)
            if kind == "eof":
                raise ScenarioFailure(HARNESS, "plak agent exited during a turn")
            if kind == "assistant":
                turn.text += event.get("text", "")
            elif kind == "tool" and event.get("state") == "running":
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
                 capture_dir: Path | None = None):
        child_env = {**env.environ(capture_dir=capture_dir), "TERM": "xterm-256color"}
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
