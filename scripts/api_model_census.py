#!/usr/bin/env python3
"""Regenerate the reviewed API model tables from provider lists and docs.

Maintainer census, opt-in and networked. Not a gate tier. Offline runs pass
``--fixture`` and never open a socket. The script reports what changed and,
with ``--write``, refreshes the checked-in tables. It does not mark a model
usable: a new id stays needs-review, and an id the family rules would admit
is not added unless ``--promote`` names it.

Keys are read from the environment or ``<PLAK_HOME>/secrets/<account>.key``
and are sent only as HTTP headers. They are never printed, never put on an
argv, and never written into the report. xAI error bodies can carry a team
id; those bodies are reduced to a status and a redacted message.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from datetime import date
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]

EFFORT_LADDER = ("none", "low", "medium", "high", "xhigh", "max")
# ``minimal`` and ``ultra`` show up in docs. Neither is an API effort this
# table records: ultra is Codex-client-only, and minimal is not on the ladder.
# The probe still sends ``minimal``: it is schema-valid, and a model that
# rejects it names that model's ladder. A value outside the schema (``ultra``,
# anything invented) only returns the global enum.
PROBE_ORDER = ("minimal", "none", "max", "xhigh")
PROBE_KNOWN = ("none", "minimal", "low", "medium", "high", "xhigh", "max")
REVIEWED_OUTPUT_BOUND = 128_000
OPENAI_PROFILES = (
    "gpt-6-luna",
    "gpt-6-sol",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
)
XAI_PROFILES = ("grok-4.3", "grok-4.5", "grok-4.6", "grok-4.7")
NOT_CODING_MARKERS = (
    "tts",
    "whisper",
    "dall-e",
    "dalle",
    "embedding",
    "moderation",
    "realtime",
    "sora",
    "audio",
    "transcri",
    "image",
    "video",
)
OPENAI_HOSTED = ("web_search", "code_execution", "tool_search")
ANTHROPIC_HOSTED = (
    "compaction",
    "hosted_ptc",
    "tool_search",
    "web_search",
    "web_fetch",
    "code_execution",
)
ANTHROPIC_LINES = ("opus", "sonnet", "haiku", "fable")
OPENAI_FIELDS = (
    "max_input_tokens",
    "max_tokens",
    "efforts",
    "price_threshold_input_tokens",
    "codex_context_window",
    "responses",
    "hosted_features",
)
ANTHROPIC_FIELDS = (
    "max_input_tokens",
    "max_tokens",
    "efforts",
    "adaptive_thinking",
    "price_threshold_input_tokens",
    "lifecycle",
    "hosted_features",
)
XAI_FIELDS = ("context_length", "long_context_threshold", "efforts")
KEY_ENV = {
    "openai": "OPENAI_API_KEY",
    "xai": "XAI_API_KEY",
    "anthropic": "ANTHROPIC_API_KEY",
}
KEY_FILE = {"openai": "openai.key", "xai": "xai.key", "anthropic": "anthropic.key"}
MAX_BODY = 4 * 1024 * 1024
CENSUS_USER_AGENT = "plak-api-model-census/1"
DOC_BODY = 2 * 1024 * 1024

SECRET_RE = re.compile(
    r"(?i)\b(?:sk-[A-Za-z0-9_\-]{6,}|bearer\s+[A-Za-z0-9._\-]{8,}|x-api-key\s*[:=]\s*\S+)"
)
TEAM_RE = re.compile(
    r"(?i)(?:\b(?:team[_ -]?id|team|org[_ -]?id)\b\s*[:=]\s*['\"]?[A-Za-z0-9_\-]+|team_[A-Za-z0-9]{4,})"
)
EMAIL_RE = re.compile(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}")


class CensusError(Exception):
    """A census failure whose message is safe to print."""


def sanitize(text: str) -> str:
    """Drop credentials, team ids and emails from text that may be stored."""
    cleaned = SECRET_RE.sub("[redacted]", text)
    cleaned = TEAM_RE.sub("[redacted]", cleaned)
    cleaned = EMAIL_RE.sub("[redacted]", cleaned)
    return cleaned


def public_error(status: int, body: bytes) -> str:
    """A status plus a redacted message. The raw body is not kept."""
    text = body[:2000].decode("utf-8", "replace")
    message = ""
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        payload = None
    if isinstance(payload, dict):
        err = payload.get("error")
        if isinstance(err, dict):
            message = str(err.get("message") or err.get("type") or "")
        elif isinstance(err, str):
            message = err
        elif "message" in payload:
            message = str(payload.get("message") or "")
    else:
        message = text
    return f"HTTP {status}: {sanitize(message)[:300]}"


def table_paths(root: Path) -> dict[str, Path]:
    return {
        "openai": root / "crates/providers/openai/src/api_models.json",
        "anthropic": root / "crates/providers/anthropic/src/api_models.json",
        "xai": root / "crates/providers/xai/src/api_models.json",
    }


def load_table(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text())


def _collapse_short_arrays(text: str) -> str:
    pattern = re.compile(
        r"\[\n(?:[ \t]+(?:\"(?:[^\"\\]|\\.)*\"|-?\d+(?:\.\d+)?|true|false|null),?\n)+[ \t]*\]",
        re.M,
    )

    def repl(match: re.Match[str]) -> str:
        items = []
        for line in match.group(0).splitlines()[1:-1]:
            items.append(line.strip().rstrip(","))
        if any(item.startswith('"') and len(item) > 20 for item in items):
            return match.group(0)
        joined = "[" + ", ".join(items) + "]"
        if len(joined) > 88:
            return match.group(0)
        return joined

    return pattern.sub(repl, text)


def dump_table(provider: str, table: dict[str, Any]) -> str:
    """The bytes the crate loader accepts, in the checked-in shape."""
    if provider == "openai":
        return json.dumps(table, indent=2) + "\n"
    if provider == "anthropic":
        lines = ["{"]
        lines.append(f'  "revision": {json.dumps(table["revision"])},')
        lines.append(f'  "reviewed_at": {json.dumps(table["reviewed_at"])},')
        lines.append('  "sources": [')
        sources = table["sources"]
        for index, source in enumerate(sources):
            comma = "," if index < len(sources) - 1 else ""
            lines.append(f"    {json.dumps(source)}{comma}")
        lines.append("  ],")
        lines.append('  "models": [')
        models = table["models"]
        for index, row in enumerate(models):
            comma = "," if index < len(models) - 1 else ""
            lines.append(f"    {json.dumps(row, separators=(', ', ': '))}{comma}")
        lines.append("  ]")
        lines.append("}")
        return "\n".join(lines) + "\n"
    if provider == "xai":
        return _collapse_short_arrays(json.dumps(table, indent=2) + "\n")
    raise CensusError(f"unknown provider {provider}")


def _ints(text: str) -> int:
    return int(text.replace(",", "").replace("_", ""))


def not_coding(provider: str, model_id: str) -> bool:
    lowered = model_id.lower()
    if provider == "xai":
        markers = ("image", "video", "speech", "embedding", "tts", "whisper")
        return any(marker in lowered for marker in markers)
    return any(marker in lowered for marker in NOT_CODING_MARKERS)


def extract_efforts(text: str) -> list[str] | None:
    """Efforts a doc page lists, or None when the page names no ladder."""
    lines = text.splitlines()
    chunks: list[str] = []
    for index, line in enumerate(lines):
        if re.search(r"reasoning[._ ]efforts?", line, re.I) and re.search(
            r"support", line, re.I
        ):
            blob = line
            if index + 1 < len(lines) and re.match(r"\s*`[a-z]+`", lines[index + 1]):
                blob = f"{blob} {lines[index + 1]}"
            chunks.append(blob)
        elif re.search(r"not supported", line, re.I) and re.search(
            r"effort", line, re.I
        ):
            chunks.append(line)
    if not chunks:
        return None
    blob = " ".join(chunks)
    unsupported: set[str] = set()
    for match in re.finditer(r"([^.]+?)\s+are not supported", blob, re.I):
        clause = match.group(1)
        if not re.search(r"effort", clause, re.I):
            continue
        unsupported.update(re.findall(r"`([a-z]+)`", clause.lower()))
    found: list[str] = []
    for match in re.finditer(
        r"`([a-z]+)`|(?<![A-Za-z])(none|minimal|low|medium|high|xhigh|max|ultra)(?![A-Za-z])",
        blob,
        re.I,
    ):
        name = (match.group(1) or match.group(2)).lower()
        if name not in found:
            found.append(name)
    found = [name for name in found if name not in unsupported]
    return [name for name in EFFORT_LADDER if name in found]


def parse_openai_model_page(text: str) -> dict[str, Any]:
    """Facts from a developers.openai.com model page (``.md``)."""
    id_match = re.search(r"Model ID:\s*`([^`]+)`", text)
    model_id = id_match.group(1).strip() if id_match else ""
    context_match = re.search(r"([0-9][0-9,]*)\s+context window", text)
    input_match = re.search(r"Maximum input tokens:\s*([0-9][0-9,]*)", text)
    output_match = re.search(r"([0-9][0-9,]*)\s+max output tokens", text)
    context = _ints(context_match.group(1)) if context_match else None
    max_input = _ints(input_match.group(1)) if input_match else context
    max_output = _ints(output_match.group(1)) if output_match else None
    efforts = extract_efforts(text)
    price = None
    if re.search(
        r"prompts with\s+(?:more than\s+|>\s*)272\s*K\s+input tokens are priced at\s+2x\s+input",
        text,
        re.I,
    ):
        price = 272_000
    responses_match = re.search(
        r"\|\s*Responses\s*\|\s*[^|\n]*\|\s*(Supported|Not supported)\s*\|",
        text,
    )
    responses = bool(responses_match and responses_match.group(1) == "Supported")
    tools = _markdown_list(_section(text, "Supported tools"))
    if not tools:
        tools = _markdown_list(_section(text, "Supported features"))
    features: list[str] = []
    if "web_search" in tools:
        features.append("web_search")
    if "code_interpreter" in tools or "code_execution" in tools:
        features.append("code_execution")
    if "tool_search" in tools:
        features.append("tool_search")
    return {
        "id": model_id,
        "max_input_tokens": max_input if max_input is not None else 0,
        "max_tokens": max_output if max_output is not None else 0,
        "efforts": [] if efforts is None else efforts,
        "price_threshold_input_tokens": price,
        "responses": responses,
        "hosted_features": [name for name in OPENAI_HOSTED if name in features],
        "page": True,
    }


def _section(text: str, heading: str) -> str:
    match = re.search(
        rf"^## {re.escape(heading)}\n(.*?)(?=^## |\Z)",
        text,
        re.M | re.S,
    )
    return match.group(1) if match else ""


def _markdown_list(section: str) -> list[str]:
    return re.findall(r"^-\s+([A-Za-z0-9_]+)\s*$", section, re.M)


def parse_model_ids(payload: Any) -> list[str]:
    """Ids from an OpenAI, xAI or models-list JSON document."""
    rows: list[Any]
    if isinstance(payload, list):
        rows = payload
    elif isinstance(payload, dict):
        rows = payload.get("data") or payload.get("models") or []
    else:
        raise CensusError("model list is not JSON")
    if not isinstance(rows, list):
        raise CensusError("model list is not a list")
    ids = []
    for row in rows:
        if isinstance(row, str):
            ids.append(row)
        elif isinstance(row, dict) and isinstance(row.get("id"), str):
            ids.append(row["id"])
    return ids


def parse_codex_windows(payload: Any) -> dict[str, int]:
    """slug -> context_window. Instructions and other catalog fields are ignored."""
    rows = payload.get("models") if isinstance(payload, dict) else payload
    if isinstance(payload, dict) and "models" not in payload and "context_window" not in payload:
        # A slim ``{slug: window}`` map.
        windows = {}
        for slug, window in payload.items():
            if isinstance(window, int):
                windows[slug] = window
            elif isinstance(window, dict) and isinstance(window.get("context_window"), int):
                windows[slug] = window["context_window"]
        return windows
    if not isinstance(rows, list):
        raise CensusError("codex catalog has no models")
    windows = {}
    for row in rows:
        if not isinstance(row, dict):
            continue
        slug = row.get("slug") or row.get("id")
        window = row.get("context_window")
        if isinstance(slug, str) and isinstance(window, int):
            windows[slug] = window
    return windows


def zero_openai_row(model_id: str) -> dict[str, Any]:
    return {
        "id": model_id,
        "max_input_tokens": 0,
        "max_tokens": 0,
        "efforts": [],
        "price_threshold_input_tokens": None,
        "codex_context_window": None,
        "responses": False,
        "hosted_features": [],
        "page": False,
    }


def parse_anthropic_model(row: dict[str, Any]) -> dict[str, Any]:
    """One ``/v1/models`` row: limits, efforts, adaptive thinking, lifecycle."""
    capabilities = row.get("capabilities") or {}
    effort = capabilities.get("effort") or {}
    names = []
    if isinstance(effort, dict):
        for name, value in effort.items():
            if name == "supported":
                continue
            if isinstance(value, dict) and value.get("supported") is True:
                names.append(name)
    thinking = capabilities.get("thinking") or {}
    types = thinking.get("types") if isinstance(thinking, dict) else None
    adaptive = False
    if isinstance(types, dict):
        adaptive_row = types.get("adaptive")
        adaptive = isinstance(adaptive_row, dict) and adaptive_row.get("supported") is True
    server = capabilities.get("server_tools") or {}
    server_features = []
    if isinstance(server, dict):
        for name in ("web_search", "web_fetch", "code_execution"):
            value = server.get(name)
            if isinstance(value, dict) and value.get("supported") is True:
                server_features.append(name)
    lifecycle = row.get("lifecycle") if isinstance(row.get("lifecycle"), str) else "active"
    return {
        "id": row.get("id") or "",
        "max_input_tokens": int(row.get("max_input_tokens") or 0),
        "max_tokens": int(row.get("max_tokens") or 0),
        "efforts": [name for name in EFFORT_LADDER if name in names],
        "adaptive_thinking": bool(adaptive),
        "lifecycle": lifecycle,
        "server_features": server_features,
    }


def parse_anthropic_page(payload: Any) -> tuple[list[dict[str, Any]], str | None]:
    """Rows plus the next ``after_id`` when ``has_more`` is set."""
    if not isinstance(payload, dict):
        raise CensusError("Anthropic model list is not an object")
    data = payload.get("data")
    if not isinstance(data, list):
        raise CensusError("Anthropic model list has no data")
    rows = [parse_anthropic_model(row) for row in data if isinstance(row, dict)]
    after = None
    if payload.get("has_more") is True:
        last = payload.get("last_id") or (rows[-1]["id"] if rows else None)
        if isinstance(last, str) and last:
            after = last
    return rows, after


def feature_supported_models(text: str) -> list[str] | None:
    """``featureMetadata.supportedModels``, or a compatibility table of ids."""
    match = re.search(r"^featureMetadata:\n(.*?)(?:^---|\Z)", text, re.M | re.S)
    if match:
        listed = re.search(
            r"^  supportedModels:\n((?:    - \S+\n)+)",
            match.group(1),
            re.M,
        )
        if listed:
            return re.findall(r"^    - (\S+)", listed.group(1), re.M)
    section = re.search(
        r"^## Model compatibility\n(.*?)(?=^## |\Z)",
        text,
        re.M | re.S,
    )
    if not section:
        return None
    seen: list[str] = []
    for model_id in re.findall(r"\((claude-[A-Za-z0-9.\-]+)\)", section.group(1)):
        if model_id not in seen:
            seen.append(model_id)
    return seen or None


def anthropic_doc_feature(name: str) -> str | None:
    lowered = name.lower()
    if "compaction" in lowered:
        return "compaction"
    if "programmatic" in lowered or "hosted-ptc" in lowered or "hosted_ptc" in lowered:
        return "hosted_ptc"
    if "tool-search" in lowered or "tool_search" in lowered:
        return "tool_search"
    if "web-fetch" in lowered or "web_fetch" in lowered:
        return "web_fetch"
    if "web-search" in lowered or "web_search" in lowered:
        return "web_search"
    if "code-execution" in lowered or "code_execution" in lowered:
        return "code_execution"
    return None


def haiku_price_thresholds(text: str) -> dict[str, int]:
    """The only published long-context price step in the reviewed pricing page."""
    if re.search(
        r"Claude Haiku 5\.5 is priced by prompt length:.*over 100,000 tokens",
        text,
        re.S,
    ):
        return {"claude-haiku-5-5": 100_000}
    return {}


def anthropic_family(model_id: str) -> str | None:
    """Mirrors the crate's adaptive family rule. Report-only."""
    if not model_id.startswith("claude-"):
        return None
    parts = model_id.removeprefix("claude-").split("-")
    if not parts or parts[0] not in ANTHROPIC_LINES:
        return None
    line = parts[0]
    major = int(parts[1]) if len(parts) > 1 and parts[1].isdigit() else None
    if major is None:
        return None
    if len(parts) == 2:
        minor = 0
    elif parts[2].isdigit() and len(parts[2]) <= 2:
        minor = int(parts[2])
    else:
        minor = 0
    if major >= 5:
        return f"claude-5-{line}"
    if major == 4 and minor >= 6 and line in ("opus", "sonnet"):
        return f"claude-4-{line}"
    return None


def parse_xai_detail(row: dict[str, Any]) -> dict[str, Any]:
    """One ``/v1/models/<id>`` or language-models row. Prices are not kept."""
    capabilities = row.get("capabilities") if isinstance(row.get("capabilities"), dict) else {}
    efforts = capabilities.get("reasoning_effort") if isinstance(capabilities, dict) else None
    if efforts is None:
        effort_list: list[str] | None = None
    elif isinstance(efforts, list):
        effort_list = [name for name in EFFORT_LADDER if name in efforts]
    else:
        effort_list = []
    context = row.get("context_length")
    threshold = row.get("long_context_threshold")
    return {
        "id": row.get("id") or "",
        "context_length": int(context) if isinstance(context, int) else None,
        "long_context_threshold": int(threshold) if isinstance(threshold, int) else None,
        "efforts": effort_list,
    }


def parse_xai_docs(text: str) -> dict[str, Any]:
    """Facts from a docs.x.ai model page."""
    id_match = re.search(r"\*\*Model name:\*\*\s*`([^`]+)`", text)
    context_match = re.search(
        r"\*\*Context window:\*\*\s*([0-9][0-9,]*)\s+tokens",
        text,
    )
    efforts = extract_efforts(text)
    threshold = None
    reach = re.search(r"reaches\s+([0-9]+)\s*k tokens", text, re.I)
    if reach:
        threshold = int(reach.group(1)) * 1000
    return {
        "id": id_match.group(1) if id_match else "",
        "context_length": _ints(context_match.group(1)) if context_match else None,
        "long_context_threshold": threshold,
        "efforts": efforts,
    }


def parse_models_dev(payload: Any) -> dict[str, dict[str, int | None]]:
    """Report-only windows. models.dev is never written into a table."""
    if not isinstance(payload, dict):
        raise CensusError("models.dev payload is not an object")
    found: dict[str, dict[str, int | None]] = {}
    for key, row in payload.items():
        if not isinstance(row, dict):
            continue
        limit = row.get("limit") if isinstance(row.get("limit"), dict) else {}
        model_id = row.get("id") if isinstance(row.get("id"), str) else key
        context = limit.get("context") if isinstance(limit, dict) else None
        output = limit.get("output") if isinstance(limit, dict) else None
        found[model_id] = {
            "context": context if isinstance(context, int) else None,
            "output": output if isinstance(output, int) else None,
        }
    return found


def _named_efforts(clause: str) -> list[str]:
    found = {name.lower() for name in re.findall(r"[A-Za-z]+", clause)}
    return [name for name in PROBE_KNOWN if name in found]


def parse_probe_message(text: str) -> dict[str, Any] | None:
    """Classify an effort-probe error.

    A message that names the model is that model's ladder. ``Supported values
    are:`` with no model id is the global schema enum and is not a ladder.
    """
    named = re.search(
        r"unsupported value:\s*'([^']+)'\s+is not supported with the '([^']+)' model\.\s*"
        r"supported values are:\s*([^.]+)",
        text,
        re.I,
    )
    if named:
        return {
            "scope": "model",
            "rejected": named.group(1).lower(),
            "model": named.group(2),
            "efforts": _named_efforts(named.group(3)),
        }
    generic = re.search(r"supported values are:\s*([^.]+)", text, re.I)
    if not generic:
        return None
    return {"scope": "global", "rejected": None, "model": None, "efforts": _named_efforts(generic.group(1))}


def effort_probe_body(model_id: str, effort: str) -> dict[str, Any]:
    """One schema-valid probe. ``effort`` is a member of ``PROBE_ORDER``.

    ``input`` is ``x`` and ``max_output_tokens`` is 16. A 400 that names the
    model costs nothing. A 200 means this value is supported and may bill a
    fraction of a cent; the caller stops once a 400 names the model's list.
    """
    if effort not in PROBE_ORDER:
        raise CensusError("probe effort must be a schema-valid probe value")
    return {
        "model": model_id,
        "input": "x",
        "store": False,
        "max_output_tokens": 16,
        "reasoning": {"effort": effort},
    }


def reduce_probe_attempts(model_id: str, attempts: list[tuple[int, str, str]]) -> dict[str, Any]:
    """Fold probe responses. Stop at the first 400 that names ``model_id``.

    Each attempt is ``(status, effort, message)``. A 200 records that effort
    as supported and does not end the probe. A global-enum 400 is not a ladder.
    """
    supported: list[str] = []
    used: list[str] = []
    for status, effort, message in attempts:
        if effort not in PROBE_ORDER:
            raise CensusError("probe effort must be a schema-valid probe value")
        used.append(effort)
        if status == 200:
            supported.append(effort)
            continue
        if status == 400:
            reading = parse_probe_message(message)
            if reading and reading["scope"] == "model" and reading["model"] == model_id:
                ladder = [name for name in EFFORT_LADDER if name in reading["efforts"]]
                return {
                    "ladder": ladder,
                    "supported_200": supported,
                    "stopped": True,
                    "used": used,
                }
            continue
        return {"ladder": None, "supported_200": supported, "stopped": True, "used": used}
    return {"ladder": None, "supported_200": supported, "stopped": False, "used": used}


def admitting(provider: str, row: dict[str, Any]) -> bool:
    """Whether the family rules would call this reviewed row usable.

    This mirrors the crates for the report. It is not admission.
    """
    model_id = row["id"]
    if provider == "openai":
        efforts = row.get("efforts") or []
        return (
            model_id in OPENAI_PROFILES
            and bool(row.get("responses"))
            and bool(efforts)
            and int(row.get("max_input_tokens") or 0) > 0
            and int(row.get("max_tokens") or 0) == REVIEWED_OUTPUT_BOUND
            and "ultra" not in efforts
        )
    if provider == "anthropic":
        if anthropic_family(model_id) is None:
            return False
        if row.get("lifecycle") == "retired":
            return False
        max_tokens = int(row.get("max_tokens") or 0)
        return (
            bool(row.get("adaptive_thinking"))
            and bool(row.get("efforts"))
            and 0 < max_tokens <= REVIEWED_OUTPUT_BOUND
            and int(row.get("max_input_tokens") or 0) > 0
        )
    if provider == "xai":
        if not_coding(provider, model_id) or not_single_agent(model_id):
            return False
        if model_id.startswith("grok-build"):
            return False
        return model_id in XAI_PROFILES and bool(row.get("efforts"))
    return False


def not_single_agent(model_id: str) -> bool:
    lowered = model_id.lower()
    return "non-reasoning" in lowered or "multi-agent" in lowered


def status_label(provider: str, row: dict[str, Any] | None, *, listed: bool) -> str:
    """Report status. A missing row is needs-review, never usable."""
    if row is None:
        return "needs-review"
    model_id = row["id"]
    if not_coding(provider, model_id):
        return "not-coding"
    if provider == "anthropic" and row.get("lifecycle") == "retired":
        return "retired"
    if provider == "xai" and (not_single_agent(model_id) or model_id.startswith("grok-build")):
        return "needs-review"
    if admitting(provider, row):
        return "usable"
    return "needs-review"


def _agree(values: list[Any]) -> tuple[Any, bool]:
    present = [value for value in values if value is not None]
    if not present:
        return None, False
    first = present[0]
    if any(value != first for value in present[1:]):
        return None, True
    return first, False


class Change:
    def __init__(self, kind: str, model_id: str, detail: str) -> None:
        self.kind = kind
        self.model_id = model_id
        self.detail = detail

    def line(self) -> str:
        return f"  {self.kind} {self.model_id} {self.detail}".rstrip()


def _field_changes(
    old: dict[str, Any], new: dict[str, Any], fields: tuple[str, ...]
) -> list[str]:
    changes = []
    for field in fields:
        if old.get(field) != new.get(field):
            changes.append(f"{field} {old.get(field)!r} -> {new.get(field)!r}")
    return changes


def _apply_fields(
    row: dict[str, Any], observed: dict[str, Any], fields: tuple[str, ...]
) -> dict[str, Any]:
    updated = dict(row)
    for field in fields:
        if field in observed and observed[field] is not _MISSING:
            updated[field] = observed[field]
    return updated


class _Missing:
    def __repr__(self) -> str:
        return "MISSING"


_MISSING = _Missing()


def regenerate(
    provider: str,
    table: dict[str, Any],
    observed: dict[str, dict[str, Any]],
    listed_ids: list[str] | None,
    *,
    include_new: bool,
    drop_removed: bool,
    promote: set[str],
    reviewed_on: str | None,
) -> tuple[dict[str, Any], list[Change]]:
    """Refresh factual fields. Never newly admits a model unless promoted."""
    fields = {"openai": OPENAI_FIELDS, "anthropic": ANTHROPIC_FIELDS, "xai": XAI_FIELDS}[
        provider
    ]
    changes: list[Change] = []
    models: list[dict[str, Any]] = []
    seen: set[str] = set()
    for row in table["models"]:
        model_id = row["id"]
        seen.add(model_id)
        if listed_ids is not None and model_id not in listed_ids:
            if drop_removed:
                changes.append(Change("removed", model_id, "dropped"))
                continue
            changes.append(Change("removed", model_id, "still in the table; pass --drop-removed"))
            models.append(row)
            continue
        facts = observed.get(model_id)
        if not facts:
            models.append(row)
            continue
        proposed = _apply_fields(row, facts, fields)
        if admitting(provider, proposed) and not admitting(provider, row):
            if model_id not in promote:
                changes.append(
                    Change(
                        "blocked",
                        model_id,
                        "new facts would admit it; pass --promote "
                        + model_id
                        + " to write them",
                    )
                )
                models.append(row)
                continue
            changes.append(Change("promoted", model_id, "maintainer passed --promote"))
        field_changes = _field_changes(row, proposed, fields)
        if field_changes:
            for detail in field_changes:
                changes.append(Change("changed", model_id, detail))
            models.append(proposed)
        else:
            models.append(row)
    if listed_ids is not None:
        for model_id in listed_ids:
            if model_id in seen:
                continue
            facts = observed.get(model_id) or {"id": model_id}
            proposed = _blank_row(provider, model_id)
            proposed = _apply_fields(proposed, facts, fields)
            label = status_label(provider, proposed, listed=True)
            if admitting(provider, proposed):
                if model_id not in promote:
                    changes.append(
                        Change(
                            "new",
                            model_id,
                            "needs-review (not written: the family rules would admit it; "
                            "pass --promote "
                            + model_id
                            + ")",
                        )
                    )
                    continue
                label = "promoted"
            if not include_new and model_id not in promote:
                changes.append(
                    Change(
                        "new",
                        model_id,
                        f"{label} (not written; pass --include-new to record facts)",
                    )
                )
                continue
            if label == "usable":
                # Promotion is the only path that may write an admitting row,
                # and the report names it. It is not a silent usable.
                label = "promoted"
            changes.append(Change("new", model_id, f"{label} included"))
            models.append(proposed)
    updated = dict(table)
    updated["models"] = models
    if models != table["models"] and reviewed_on:
        updated["reviewed_at"] = reviewed_on
    return updated, changes


def _blank_row(provider: str, model_id: str) -> dict[str, Any]:
    if provider == "openai":
        return zero_openai_row(model_id)
    if provider == "anthropic":
        return {
            "id": model_id,
            "max_input_tokens": 0,
            "max_tokens": 0,
            "efforts": [],
            "adaptive_thinking": False,
            "price_threshold_input_tokens": None,
            "lifecycle": "active",
            "hosted_features": [],
        }
    return {
        "id": model_id,
        "context_length": 0,
        "long_context_threshold": None,
        "efforts": [],
    }


def observe_openai(
    listed: list[str],
    pages: dict[str, str],
    codex: dict[str, int] | None,
    probes: dict[str, list[str]] | None = None,
) -> dict[str, dict[str, Any]]:
    observed: dict[str, dict[str, Any]] = {}
    for model_id in listed:
        page = pages.get(model_id)
        if page:
            facts = parse_openai_model_page(page)
            facts["id"] = model_id
            if codex is not None:
                facts["codex_context_window"] = codex.get(model_id)
            if probes and not facts["efforts"] and model_id in probes:
                facts["efforts"] = probes[model_id]
            observed[model_id] = facts
        elif not_coding("openai", model_id):
            row = zero_openai_row(model_id)
            if codex is not None:
                row["codex_context_window"] = codex.get(model_id)
            observed[model_id] = row
        else:
            observed[model_id] = {"id": model_id}
    return observed


def observe_anthropic(
    rows: list[dict[str, Any]],
    feature_docs: dict[str, str],
    pricing: str | None,
) -> dict[str, dict[str, Any]]:
    by_feature: dict[str, set[str]] = {name: set() for name in ANTHROPIC_HOSTED}
    for name, text in feature_docs.items():
        feature = anthropic_doc_feature(name)
        if feature is None:
            continue
        supported = feature_supported_models(text)
        if supported is None:
            continue
        by_feature[feature].update(supported)
    thresholds = haiku_price_thresholds(pricing or "")
    observed: dict[str, dict[str, Any]] = {}
    for row in rows:
        model_id = row["id"]
        features = []
        for name in ANTHROPIC_HOSTED:
            documented = model_id in by_feature[name]
            # Web search and web fetch pages point at the Models API when
            # they publish no supportedModels list. Code execution and the
            # others stay with the doc list: the API capability bit and the
            # doc list disagree for Haiku 4.5 code execution, and the table
            # follows the doc.
            if documented:
                features.append(name)
            elif name in ("web_search", "web_fetch") and not by_feature[name]:
                if name in row.get("server_features", []):
                    features.append(name)
        observed[model_id] = {
            "id": model_id,
            "max_input_tokens": row["max_input_tokens"],
            "max_tokens": row["max_tokens"],
            "efforts": row["efforts"],
            "adaptive_thinking": row["adaptive_thinking"],
            "price_threshold_input_tokens": thresholds.get(model_id),
            "lifecycle": row["lifecycle"],
            "hosted_features": features,
        }
    return observed


def observe_xai(
    details: list[dict[str, Any]],
    language: list[dict[str, Any]],
    docs: dict[str, dict[str, Any]],
) -> tuple[dict[str, dict[str, Any]], list[Change]]:
    """Merge per-id, language-models and docs. A disagreement keeps no fact."""
    by_id: dict[str, list[dict[str, Any]]] = {}
    order: list[str] = []
    for row in details + language:
        model_id = row["id"]
        if not model_id:
            continue
        if model_id not in by_id:
            order.append(model_id)
            by_id[model_id] = []
        by_id[model_id].append(row)
    for model_id, doc in docs.items():
        if model_id not in by_id:
            order.append(model_id)
            by_id[model_id] = []
        by_id[model_id].append(doc)
    observed: dict[str, dict[str, Any]] = {}
    conflicts: list[Change] = []
    for model_id in order:
        sources = by_id[model_id]
        context, context_conflict = _agree([row.get("context_length") for row in sources])
        threshold, threshold_conflict = _agree(
            [row.get("long_context_threshold") for row in sources]
        )
        effort_sources = [row.get("efforts") for row in sources if row.get("efforts") is not None]
        efforts, effort_conflict = _agree(effort_sources)
        facts: dict[str, Any] = {"id": model_id}
        if context_conflict:
            conflicts.append(Change("conflict", model_id, "context_length sources disagree; kept reviewed"))
        elif context is not None:
            facts["context_length"] = context
        if threshold_conflict:
            conflicts.append(
                Change("conflict", model_id, "long_context_threshold sources disagree; kept reviewed")
            )
        elif threshold is not None:
            facts["long_context_threshold"] = threshold
        if effort_conflict:
            conflicts.append(Change("conflict", model_id, "efforts sources disagree; kept reviewed"))
        elif efforts is not None:
            facts["efforts"] = efforts
        elif any("efforts" in row and row.get("efforts") is None for row in sources):
            # A detail row that publishes no ladder is an empty ladder, not a hole.
            if not effort_sources:
                facts["efforts"] = []
        observed[model_id] = facts
    return observed, conflicts


def models_dev_lines(
    provider: str, table: dict[str, Any], remote: dict[str, dict[str, int | None]]
) -> list[Change]:
    lines = []
    for row in table["models"]:
        remote_row = remote.get(row["id"])
        if not remote_row:
            continue
        if provider == "xai":
            local = row.get("context_length")
        else:
            local = row.get("max_input_tokens")
        remote_context = remote_row.get("context")
        if remote_context is not None and remote_context != local:
            lines.append(
                Change(
                    "models.dev",
                    row["id"],
                    f"context {remote_context} table {local} (report only)",
                )
            )
    return lines


def render_report(sections: list[tuple[str, str, list[Change]]]) -> str:
    lines = ["api-model-census"]
    for provider, revision, changes in sections:
        lines.append(f"{provider} revision={revision}")
        if not changes:
            lines.append("  unchanged")
            continue
        for change in changes:
            lines.append(sanitize(change.line()))
    return "\n".join(lines) + "\n"


def load_key(account: str, environ: dict[str, str] | None = None, home: Path | None = None) -> str:
    """Environment first, then the Plak key file. Empty env does not fall through."""
    env = environ if environ is not None else os.environ
    variable = KEY_ENV[account]
    if variable in env:
        value = env[variable]
        if value.strip() == "" or len(value) > 4096 or "\x00" in value:
            raise CensusError(f"{variable} is set but unusable")
        return value.strip()
    root = home if home is not None else _plak_home()
    path = root / "secrets" / KEY_FILE[account]
    if not path.is_file():
        raise CensusError(f"no {account} API key in the environment or the Plak key file")
    try:
        value = path.read_text()
    except OSError as exc:
        raise CensusError(f"could not read the {account} key file") from None
    if value.strip() == "" or len(value) > 4096 or "\n" in value.strip() or "\x00" in value:
        raise CensusError(f"the {account} key file is unusable")
    return value.strip()


def _plak_home() -> Path:
    raw = os.environ.get("PLAK_HOME")
    if raw:
        path = Path(raw)
        if not path.is_absolute():
            raise CensusError("PLAK_HOME must be absolute")
        return path
    return Path.home() / ".plak"


class _SameHost(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):  # noqa: ANN001
        old = urllib.parse.urlparse(req.full_url).netloc
        new = urllib.parse.urlparse(newurl).netloc
        if old != new:
            raise CensusError(f"refused redirect off {old}")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def fetch(
    url: str,
    headers: list[tuple[str, str]] | None = None,
    *,
    method: str = "GET",
    body: bytes | None = None,
    limit: int = MAX_BODY,
    timeout: float = 20,
) -> tuple[int, bytes]:
    """HTTP fetch. Header values are not copied into errors."""
    request = urllib.request.Request(url, data=body, method=method)
    # Some docs hosts refuse urllib's default agent (Cloudflare error 1010).
    request.add_header("user-agent", CENSUS_USER_AGENT)
    for name, value in headers or []:
        request.add_header(name, value)
    opener = urllib.request.build_opener(_SameHost)
    try:
        with opener.open(request, timeout=timeout) as response:
            status = getattr(response, "status", 200)
            payload = response.read(limit + 1)
    except urllib.error.HTTPError as exc:
        payload = exc.read(limit + 1)
        raise CensusError(public_error(exc.code, payload)) from None
    except CensusError:
        raise
    except Exception as exc:  # noqa: BLE001 - the message is sanitized before it leaves
        raise CensusError(sanitize(str(exc))[:300]) from None
    if len(payload) > limit:
        raise CensusError(f"response from {urllib.parse.urlparse(url).netloc} exceeds {limit} bytes")
    return status, payload


def fetch_doc(url: str) -> str:
    """Markdown docs. Try the ``.md`` URL first, then the HTML URL."""
    last = "doc unavailable"
    for candidate in (url if url.endswith(".md") else url + ".md", url):
        try:
            _, body = fetch(candidate, limit=DOC_BODY)
        except CensusError as exc:
            last = str(exc)
            continue
        return body.decode("utf-8", "replace")
    raise CensusError(sanitize(last)[:300])


def auth_headers(provider: str, key: str) -> list[tuple[str, str]]:
    if provider == "anthropic":
        return [("x-api-key", key), ("anthropic-version", "2023-06-01")]
    return [("Authorization", f"Bearer {key}")]


def _json_bytes(payload: bytes) -> Any:
    try:
        return json.loads(payload)
    except json.JSONDecodeError as exc:
        raise CensusError("response was not JSON") from None


def live_openai(key: str, table: dict[str, Any], *, doc_limit: int, probe: int) -> tuple[
    list[str], dict[str, dict[str, Any]], list[Change]
]:
    status, body = fetch(
        "https://api.openai.com/v1/models",
        auth_headers("openai", key),
    )
    del status
    listed = parse_model_ids(_json_bytes(body))
    wanted = _doc_ids("openai", listed, table, doc_limit)
    pages: dict[str, str] = {}
    notes: list[Change] = []
    for model_id in wanted:
        url = f"https://developers.openai.com/api/docs/models/{model_id}.md"
        try:
            _, page = fetch(url, limit=DOC_BODY)
        except CensusError as exc:
            notes.append(Change("doc", model_id, sanitize(str(exc))))
            continue
        pages[model_id] = page.decode("utf-8", "replace")
    probes: dict[str, list[str]] = {}
    if probe:
        for model_id in _probe_targets(pages, probe):
            outcome = _probe_openai(key, model_id)
            page_efforts = parse_openai_model_page(pages[model_id])["efforts"]
            notes.append(_probe_note(model_id, page_efforts, outcome))
            # The doc page is primary. The probe fills a page that lists no
            # ladder, and only when a 400 named that model. A 200 is recorded
            # on the note and is not a ladder by itself.
            if outcome["ladder"] is not None and not page_efforts:
                probes[model_id] = outcome["ladder"]
    notes.append(Change("note", "codex", "catalog not loaded; kept reviewed context windows"))
    return listed, observe_openai(listed, pages, None, probes), notes


def _probe_targets(pages: dict[str, str], limit: int) -> list[str]:
    """Empty doc ladders first, then a cross-check of pages that list one."""
    empty: list[str] = []
    listed: list[str] = []
    for model_id, page in pages.items():
        if not_coding("openai", model_id):
            continue
        if parse_openai_model_page(page)["efforts"]:
            listed.append(model_id)
        else:
            empty.append(model_id)
    return (empty + listed)[:limit]


def _probe_note(model_id: str, page_efforts: list[str], outcome: dict[str, Any]) -> Change:
    ladder = outcome["ladder"]
    paid = outcome["supported_200"]
    requests = len(outcome["used"])
    paid_note = (" 200 " + ",".join(paid)) if paid else ""
    if ladder is not None and not page_efforts:
        detail = "filled empty page " + ",".join(ladder)
    elif ladder is not None and ladder != page_efforts:
        detail = "cross-check page " + ",".join(page_efforts) + " probe " + ",".join(ladder)
    elif ladder is not None:
        detail = "cross-check agrees " + ",".join(ladder)
    elif paid:
        detail = "no model ladder"
    else:
        detail = "no model ladder; efforts unchanged"
    return Change("probe", model_id, f"{detail}{paid_note} requests {requests}")


def _probe_message(payload: bytes) -> str:
    text = payload[:4000].decode("utf-8", "replace")
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return text
    if isinstance(data, dict):
        err = data.get("error")
        if isinstance(err, dict) and isinstance(err.get("message"), str):
            return err["message"]
        if isinstance(err, str):
            return err
    return text


def _post_probe(key: str, body: dict[str, Any]) -> tuple[int, bytes]:
    request = urllib.request.Request(
        "https://api.openai.com/v1/responses",
        data=json.dumps(body).encode(),
        method="POST",
    )
    for name, value in auth_headers("openai", key):
        request.add_header(name, value)
    request.add_header("content-type", "application/json")
    opener = urllib.request.build_opener(_SameHost)
    try:
        with opener.open(request, timeout=20) as response:
            return getattr(response, "status", 200), response.read(8192)
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read(8192)
    except CensusError:
        raise
    except Exception as exc:  # noqa: BLE001
        raise CensusError(sanitize(str(exc))[:300]) from None


def _probe_openai(key: str, model_id: str) -> dict[str, Any]:
    attempts: list[tuple[int, str, str]] = []
    for effort in PROBE_ORDER:
        status, payload = _post_probe(key, effort_probe_body(model_id, effort))
        attempts.append((status, effort, _probe_message(payload)))
        outcome = reduce_probe_attempts(model_id, attempts)
        if outcome["stopped"]:
            return outcome
    return reduce_probe_attempts(model_id, attempts)


def _doc_ids(provider: str, listed: list[str], table: dict[str, Any], limit: int) -> list[str]:
    reviewed = [row["id"] for row in table["models"]]
    ordered = []
    for model_id in reviewed + listed:
        if model_id in ordered:
            continue
        if not_coding(provider, model_id):
            continue
        ordered.append(model_id)
    return ordered[:limit]


def live_anthropic(key: str) -> tuple[list[str], dict[str, dict[str, Any]]]:
    headers = auth_headers("anthropic", key)
    rows: list[dict[str, Any]] = []
    after: str | None = None
    for _page in range(8):
        url = "https://api.anthropic.com/v1/models?limit=1000"
        if after:
            url += "&after_id=" + urllib.parse.quote(after)
        _, body = fetch(url, headers)
        page_rows, after = parse_anthropic_page(_json_bytes(body))
        rows.extend(page_rows)
        if not after:
            break
    else:
        raise CensusError("Anthropic model list did not finish within 8 pages")
    docs = {
        "compaction-on-demand": "https://platform.claude.com/docs/en/build-with-claude/compaction-on-demand",
        "compaction-threshold": "https://platform.claude.com/docs/en/build-with-claude/compaction-threshold",
        "programmatic-tool-calling": "https://platform.claude.com/docs/en/agents-and-tools/tool-use/programmatic-tool-calling",
        "tool-search": "https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool",
        "web-search": "https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-search-tool",
        "web-fetch": "https://platform.claude.com/docs/en/agents-and-tools/tool-use/web-fetch-tool",
        "code-execution": "https://platform.claude.com/docs/en/agents-and-tools/tool-use/code-execution-tool",
    }
    feature_docs = {name: fetch_doc(url) for name, url in docs.items()}
    observed = observe_anthropic(
        rows, feature_docs, fetch_doc("https://platform.claude.com/docs/en/about-claude/pricing")
    )
    return [row["id"] for row in rows], observed


def live_xai(key: str, table: dict[str, Any], *, doc_limit: int) -> tuple[
    list[str], dict[str, dict[str, Any]], list[Change]
]:
    headers = auth_headers("xai", key)
    _, models_body = fetch("https://api.x.ai/v1/models", headers)
    models_payload = _json_bytes(models_body)
    listed = parse_model_ids(models_payload)
    _, language_body = fetch("https://api.x.ai/v1/language-models", headers)
    language_payload = _json_bytes(language_body)
    language_rows = _xai_rows(language_payload)
    details = []
    notes: list[Change] = []
    for model_id in _doc_ids("xai", listed, table, doc_limit):
        try:
            _, body = fetch(
                "https://api.x.ai/v1/models/" + urllib.parse.quote(model_id),
                headers,
            )
        except CensusError as exc:
            notes.append(Change("fetch", model_id, sanitize(str(exc))))
            continue
        row = _json_bytes(body)
        if isinstance(row, dict):
            details.append(parse_xai_detail(row))
    docs = {}
    for model_id in _doc_ids("xai", listed, table, doc_limit):
        try:
            text = fetch_doc(f"https://docs.x.ai/developers/models/{model_id}")
        except CensusError as exc:
            notes.append(Change("doc", model_id, sanitize(str(exc))))
            continue
        parsed = parse_xai_docs(text)
        parsed["id"] = model_id
        docs[model_id] = parsed
    observed, conflicts = observe_xai(details, language_rows, docs)
    return listed, observed, notes + conflicts


def _xai_rows(payload: Any) -> list[dict[str, Any]]:
    rows = payload.get("models") if isinstance(payload, dict) else payload
    if isinstance(payload, dict) and isinstance(payload.get("data"), list):
        rows = payload["data"]
    if not isinstance(rows, list):
        return []
    return [parse_xai_detail(row) for row in rows if isinstance(row, dict)]


def load_fixture(path: Path) -> dict[str, Any]:
    """Offline sources. The directory is the research-snapshot layout, trimmed."""
    bundle: dict[str, Any] = {}
    openai = path / "openai"
    if openai.is_dir():
        listed_payload = json.loads((openai / "models.json").read_text())
        pages = {}
        docs = openai / "docs"
        if docs.is_dir():
            for page in sorted(docs.glob("*.md")):
                text = page.read_text()
                parsed = parse_openai_model_page(text)
                model_id = parsed["id"] or page.stem
                pages[model_id] = text
                pages.setdefault(page.stem, text)
        codex = None
        catalog = openai / "codex.json"
        if catalog.is_file():
            codex = parse_codex_windows(json.loads(catalog.read_text()))
        listed = parse_model_ids(listed_payload)
        bundle["openai"] = {
            "listed": listed,
            "observed": observe_openai(listed, pages, codex),
            "notes": [],
        }
    anthropic = path / "anthropic"
    if anthropic.is_dir():
        payload = json.loads((anthropic / "models.json").read_text())
        rows, after = parse_anthropic_page(payload)
        if after:
            raise CensusError("Anthropic fixture page is truncated (has_more)")
        feature_docs = {}
        docs = anthropic / "docs"
        pricing = None
        if docs.is_dir():
            for page in sorted(docs.glob("*.md")):
                text = page.read_text()
                if "pricing" in page.stem:
                    pricing = text
                else:
                    feature_docs[page.stem] = text
        bundle["anthropic"] = {
            "listed": [row["id"] for row in rows],
            "observed": observe_anthropic(rows, feature_docs, pricing),
            "notes": [],
        }
    xai = path / "xai"
    if xai.is_dir():
        details = []
        jsonl = xai / "models-per-id.jsonl"
        if jsonl.is_file():
            for line in jsonl.read_text().splitlines():
                if line.strip():
                    details.append(parse_xai_detail(json.loads(line)))
        language = []
        language_path = xai / "language-models.json"
        if language_path.is_file():
            language = _xai_rows(json.loads(language_path.read_text()))
        docs = {}
        docs_dir = xai / "docs"
        if docs_dir.is_dir():
            for page in sorted(docs_dir.glob("*.md")):
                if page.stem.startswith("_"):
                    continue
                parsed = parse_xai_docs(page.read_text())
                model_id = parsed["id"] or page.stem
                parsed["id"] = model_id
                docs[model_id] = parsed
        observed, conflicts = observe_xai(details, language, docs)
        listed = [row["id"] for row in details] or list(observed)
        bundle["xai"] = {"listed": listed, "observed": observed, "notes": conflicts}
    models_dev = path / "models.dev.json"
    if models_dev.is_file():
        bundle["models_dev"] = parse_models_dev(json.loads(models_dev.read_text()))
    if not bundle:
        raise CensusError(f"no provider fixtures in {path}")
    return bundle


def run_census(
    *,
    root: Path,
    fixture: Path | None,
    providers: list[str],
    write: bool,
    include_new: bool,
    drop_removed: bool,
    promote: set[str],
    reviewed_on: str | None,
    tables: dict[str, Path] | None = None,
    doc_limit: int = 40,
    probe: int = 0,
    models_dev: bool = False,
    codex_catalog: Path | None = None,
) -> tuple[str, dict[str, bool]]:
    paths = tables or table_paths(root)
    if fixture is not None:
        bundle = load_fixture(fixture)
    else:
        bundle = {}
    if codex_catalog is not None and "openai" in (bundle or {"openai": None}):
        windows = parse_codex_windows(json.loads(codex_catalog.read_text()))
        # Applied below for the live path; fixtures already parsed theirs.
        bundle.setdefault("_codex", windows)
    remote = bundle.get("models_dev")
    if models_dev and fixture is None:
        _, body = fetch("https://models.dev/api.json", limit=8 * 1024 * 1024)
        remote = parse_models_dev(_json_bytes(body))
    prepared: list[tuple[str, Path, str, str, list[Change], bool]] = []
    for provider in providers:
        path = paths[provider]
        table = load_table(path)
        if fixture is not None:
            if provider not in bundle:
                raise CensusError(f"fixture has no {provider} sources")
            listed = bundle[provider]["listed"]
            observed = bundle[provider]["observed"]
            notes = list(bundle[provider]["notes"])
        else:
            key = load_key(provider)
            if provider == "openai":
                listed, observed, notes = live_openai(
                    key, table, doc_limit=doc_limit, probe=probe
                )
                extra = bundle.get("_codex")
                if isinstance(extra, dict):
                    for model_id, facts in observed.items():
                        if facts.get("page"):
                            facts["codex_context_window"] = extra.get(model_id)
                    notes = [note for note in notes if note.model_id != "codex"]
            elif provider == "anthropic":
                listed, observed = live_anthropic(key)
                notes = []
            else:
                listed, observed, notes = live_xai(key, table, doc_limit=doc_limit)
        updated, changes = regenerate(
            provider,
            table,
            observed,
            listed,
            include_new=include_new,
            drop_removed=drop_removed,
            promote=promote,
            reviewed_on=reviewed_on,
        )
        changes = notes + changes
        if isinstance(remote, dict):
            changes.extend(models_dev_lines(provider, table, remote))
        rendered = dump_table(provider, updated)
        revision = str(table.get("revision", ""))
        differs = rendered != path.read_text()
        prepared.append((provider, path, revision, rendered, changes, differs))
    wrote = {provider: False for provider in providers}
    if write:
        for provider, path, _revision, rendered, changes, differs in prepared:
            if not differs:
                continue
            path.write_text(rendered)
            wrote[provider] = True
            shown = path.relative_to(root) if path.is_relative_to(root) else path
            changes.append(Change("wrote", provider, str(shown)))
    sections = [
        (provider, revision, changes) for provider, _path, revision, _rendered, changes, _differs in prepared
    ]
    return render_report(sections), wrote


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Census the reviewed API model tables.")
    parser.add_argument(
        "--fixture",
        type=Path,
        help="offline snapshot directory; no network",
    )
    parser.add_argument(
        "--provider",
        action="append",
        choices=("openai", "anthropic", "xai"),
        help="limit to one provider (repeatable); default is all three",
    )
    parser.add_argument("--write", action="store_true", help="rewrite the checked-in tables")
    parser.add_argument(
        "--include-new",
        action="store_true",
        help="record new ids that the family rules would not admit",
    )
    parser.add_argument(
        "--drop-removed",
        action="store_true",
        help="drop table ids the provider list no longer returns",
    )
    parser.add_argument(
        "--promote",
        action="append",
        default=[],
        help="write an id the family rules would admit (repeatable)",
    )
    parser.add_argument(
        "--probe-efforts",
        action="store_true",
        help="OpenAI only: at most --probe-limit models. Schema-valid efforts, unsupported first. A 400 that names the model is its ladder; a 200 records that value. Docs stay primary.",
    )
    parser.add_argument("--probe-limit", type=int, default=5)
    parser.add_argument("--doc-limit", type=int, default=40)
    parser.add_argument(
        "--models-dev",
        action="store_true",
        help="cross-check models.dev and report only; never authoritative",
    )
    parser.add_argument("--codex-catalog", type=Path, help="Codex catalog JSON for context_window")
    parser.add_argument("--reviewed-on", help="reviewed_at when a write changes rows (YYYY-MM-DD)")
    parser.add_argument("--table", action="append", default=[], help="provider=path override")
    args = parser.parse_args(argv)
    if args.probe_efforts and args.fixture is not None:
        raise SystemExit("effort probes need the network; omit --fixture")
    reviewed_on = args.reviewed_on
    if args.write and not reviewed_on:
        reviewed_on = date.today().isoformat()
    if args.probe_limit < 0 or args.doc_limit < 0:
        raise SystemExit("limits must be non-negative")
    providers = args.provider or ["openai", "anthropic", "xai"]
    overrides: dict[str, Path] = {}
    for item in args.table:
        if "=" not in item:
            raise SystemExit("--table expects provider=path")
        name, raw = item.split("=", 1)
        if name not in KEY_ENV:
            raise SystemExit(f"unknown provider in --table: {name}")
        overrides[name] = Path(raw)
    tables = table_paths(ROOT)
    tables.update(overrides)
    try:
        report, _wrote = run_census(
            root=ROOT,
            fixture=args.fixture,
            providers=providers,
            write=args.write,
            include_new=args.include_new,
            drop_removed=args.drop_removed,
            promote=set(args.promote),
            reviewed_on=reviewed_on,
            tables=tables,
            doc_limit=args.doc_limit,
            probe=args.probe_limit if args.probe_efforts else 0,
            models_dev=args.models_dev,
            codex_catalog=args.codex_catalog,
        )
    except CensusError as exc:
        print(sanitize(str(exc)), file=sys.stderr)
        return 2
    sys.stdout.write(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
