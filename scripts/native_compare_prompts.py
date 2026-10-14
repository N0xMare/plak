#!/usr/bin/env python3
"""Capture native CLI HTTP requests into target/prompt-captures/native/ (first turn, or a scripted Claude multi-turn session).

Maintainer utility, not a workspace gate. Installed PATH binaries run with
isolated homes and synthetic credentials against local HTTP/TLS recorders.
Claude uses the interactive CLI, never print/SDK mode. The recorder rejects
unknown CONNECT destinations and never forwards requests. Captures establish
request construction, not provider acceptance or paid inference.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
import dataclasses
import fcntl
import gzip
import json
import os
import pty
import re
import select
import shutil
import socket
import socketserver
import struct
import subprocess
import sys
import termios
import tempfile
import threading
import time
import unicodedata
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
import urllib.request
from urllib.parse import urlparse


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import owned_processes

DEFAULT_OUT = ROOT / "target/prompt-captures/native"
DEFAULT_WORK = ROOT / "target/native-compare-workspace"
PROMPT = "hello"
TOKEN_METHOD = (
    "approximate chars/4 of the compact captured body; no tiktoken-compatible "
    "tokenizer in the workspace"
)

SECRET_HEADER = re.compile(
    r"(?i)(authorization|cookie|api-?key|token|secret|bearer|refresh|password|"
    r"session-id|device-id|account-id|prompt-id)"
)
SECRET_JSON_KEY = re.compile(
    r"(?i)^(authorization|access[_-]?token|refresh[_-]?token|id[_-]?token|"
    r"api[_-]?key|x-api-key|cookie|set-cookie|password|secret|client_secret|"
    r"oauth[_-]?token|bearer|user_id|session_id|device_id|account_uuid|prompt_id)$"
)
EMAIL = re.compile(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")
UUID_TEXT = re.compile(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
TOKENISH = re.compile(
    r"\b(?:sk-[A-Za-z0-9_-]{16,}|xai-[A-Za-z0-9_-]{16,}|"
    r"eyJ[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+)\b"
)

VENDORS = ("claude", "codex", "grok", "opencode")
CLAUDE_CLI_IDENTITY = "You are Claude Code, Anthropic's official CLI for Claude."
CLAUDE_SDK_IDENTITY = "You are a Claude agent, built on Anthropic's Claude Agent SDK."
CLAUDE_TUI_BLOCKER = (
    "claude: interactive Claude Code TUI only (never -p / --print). "
    "Print/SDK is cc_entrypoint=sdk-cli and a different identity/tool pack than "
    "Plak's Claude OAuth costume (cc_entrypoint=cli). "
    "Use just native-compare-prompts claude for local capture. "
    "--from accepts an existing CLI request body log; ordinary debug logs "
    "may not contain request bodies."
)


@dataclass(frozen=True)
class VendorSpec:
    name: str
    bin: str
    model: str
    effort: str | None
    notes: str


SPECS: dict[str, VendorSpec] = {
    "claude": VendorSpec(
        "claude",
        "claude",
        "claude-sonnet-5",
        "low",
        "interactive Claude Code TUI, original Anthropic origin through local TLS; claude-1 account reference, synthetic OAuth/config only; never -p/sdk-cli",
    ),
    "codex": VendorSpec(
        "codex",
        "codex",
        "gpt-5.6-luna",
        None,
        "Code Mode on luna; CONNECT MITM for chatgpt.com; compare to Plak native-ptc",
    ),
    "grok": VendorSpec(
        "grok",
        "grok",
        "grok-4.6",
        "low",
        "grok-build headless CLI; isolated synthetic auth and localhost route",
    ),
    "opencode": VendorSpec(
        "opencode",
        "opencode",
        "inclusionai/ling-3.0-flash-sante:free",
        "medium",
        "OpenRouter Chat Completions via project opencode.json baseURL",
    ),
}


@dataclass
class CapturedRequest:
    method: str
    path: str
    headers: list[tuple[str, str]]
    body: bytes
    # What a scripted responder answered (multi-turn captures only).
    reply: dict[str, Any] | None = None


@dataclass
class MockReply:
    status: int
    content_type: str
    body: bytes
    headers: list[tuple[str, str]]
    summary: dict[str, Any]


@dataclass
class CaptureState:
    requests: list[tuple[str, str]] = field(default_factory=list)
    inferences: list[CapturedRequest] = field(default_factory=list)
    lock: threading.Lock = field(default_factory=threading.Lock)
    # Optional scripted answer for inference requests; None falls back to the
    # fixed dummy replies.
    responder: "ClaudeScript | None" = None


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def which(name: str) -> Path | None:
    found = shutil.which(name)
    return Path(found) if found else None


def version_text(bin_path: Path, env: dict[str, str] | None = None) -> str:
    return asyncio.run(version_text_owned(bin_path, env))


async def version_text_owned(bin_path: Path, env: dict[str, str] | None = None) -> str:
    try:
        with tempfile.TemporaryDirectory(prefix="native-version-") as directory:
            root = Path(directory)
            result = await owned_processes.run_command(
                [str(bin_path), "--version"], cwd=root, env=env,
                stdout_path=root / "stdout", stderr_path=root / "stderr", timeout=20,
            )
            if result["timed_out"] or result["output_limit"] or not result["cleanup"]["joined"]:
                return "unreadable: bounded version capture failed"
            text = ((root / "stdout").read_text(errors="replace") or
                    (root / "stderr").read_text(errors="replace")).strip()
    except OSError:
        return "unreadable: version capture failed"
    return redact_text(text.splitlines()[0]) if text else "unknown"


def redact_text(text: str) -> str:
    text = EMAIL.sub("[redacted-email]", text)
    text = UUID_TEXT.sub("[redacted-id]", text)
    return TOKENISH.sub("[redacted]", text)


def redact_header_name(name: str) -> bool:
    return SECRET_HEADER.search(name) is not None


def redact_headers(headers: list[tuple[str, str]]) -> list[dict[str, str]]:
    out: list[dict[str, str]] = []
    for name, value in headers:
        if redact_header_name(name):
            out.append({"name": name, "value": "[redacted]"})
        else:
            out.append({"name": name, "value": redact_text(value)})
    return out


def redact_json(value: Any) -> Any:
    if isinstance(value, str):
        return redact_text(value)
    if isinstance(value, list):
        return [redact_json(item) for item in value]
    if isinstance(value, dict):
        redacted: dict[str, Any] = {}
        for key, item in value.items():
            if SECRET_JSON_KEY.match(str(key)):
                redacted[key] = "[redacted]"
            else:
                redacted[key] = redact_json(item)
        return redacted
    return value


ZSTD_MAGIC = b"\x28\xb5\x2f\xfd"


def decompress_zstd(body: bytes) -> bytes:
    binary = shutil.which("zstd")
    if binary is None:
        raise RuntimeError("zstd is required to decode Codex request bodies")
    result = subprocess.run(
        [binary, "-d", "-c"],
        input=body,
        capture_output=True,
        check=True,
    )
    return result.stdout


def decode_body(headers: list[tuple[str, str]], body: bytes) -> bytes:
    encoding = ""
    for name, value in headers:
        if name.lower() == "content-encoding":
            encoding = value.lower().strip()
            break
    if not body:
        return body
    if encoding in ("gzip", "x-gzip"):
        return gzip.decompress(body)
    if encoding in ("zstd", "zst") or body.startswith(ZSTD_MAGIC):
        return decompress_zstd(body)
    return body


def parse_json_body(body: bytes) -> Any | None:
    if not body:
        return None
    try:
        return json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return None


def wants_stream(headers: list[tuple[str, str]], payload: Any | None) -> bool:
    for name, value in headers:
        if name.lower() == "accept" and "text/event-stream" in value.lower():
            return True
    if isinstance(payload, dict):
        stream = payload.get("stream")
        if stream is True:
            return True
    return False


def is_inference_payload(path: str, payload: Any | None) -> bool:
    lowered = path.lower().split("?", 1)[0]
    if "analytics" in lowered or lowered.rstrip("/").endswith("/count_tokens"):
        return False
    path_hit = any(
        marker in lowered
        for marker in ("/messages", "/responses", "/chat/completions")
    )
    if lowered.rstrip("/").endswith("/completions"):
        path_hit = True
    if path_hit:
        return payload is None or isinstance(payload, dict)
    if not isinstance(payload, dict):
        return False
    if payload.get("tools"):
        return True
    if payload.get("model") and (
        payload.get("messages") is not None
        or payload.get("input") is not None
        or payload.get("system") is not None
        or payload.get("instructions") is not None
    ):
        return True
    return False


def inference_score(captured: CapturedRequest) -> tuple[int, int]:
    payload = parse_json_body(captured.body)
    ntools = 0
    if isinstance(payload, dict):
        tools = payload.get("tools")
        if isinstance(tools, list):
            ntools = len(tools)
    return (ntools, len(captured.body))


def select_inference(items: list[CapturedRequest]) -> CapturedRequest:
    if not items:
        raise RuntimeError("no inference requests")
    return max(items, key=inference_score)


def walk_dicts(value: Any) -> list[dict[str, Any]]:
    found: list[dict[str, Any]] = []
    if isinstance(value, dict):
        found.append(value)
        for item in value.values():
            found.extend(walk_dicts(item))
    elif isinstance(value, list):
        for item in value:
            found.extend(walk_dicts(item))
    return found


def iter_debug_objects(text: str) -> list[Any]:
    objects: list[Any] = []
    stripped = text.strip()
    if stripped:
        try:
            objects.append(json.loads(stripped))
            return objects
        except json.JSONDecodeError:
            pass
    decoder = json.JSONDecoder()
    for line in text.splitlines():
        line = line.strip()
        if not line or line[0] not in "{[":
            continue
        try:
            objects.append(decoder.decode(line))
        except json.JSONDecodeError:
            continue
    if objects:
        return objects
    idx = 0
    while True:
        start = text.find("{", idx)
        if start < 0:
            break
        try:
            obj, end = decoder.raw_decode(text, start)
        except json.JSONDecodeError:
            idx = start + 1
            continue
        objects.append(obj)
        idx = end
    return objects


def looks_like_messages(payload: Any) -> bool:
    return isinstance(payload, dict) and isinstance(payload.get("model"), str) and (
        "messages" in payload or "system" in payload
    )


def coerce_json(value: Any) -> Any:
    if isinstance(value, str):
        try:
            return json.loads(value)
        except json.JSONDecodeError:
            return value
    return value


def messages_payload_from(obj: dict[str, Any]) -> dict[str, Any] | None:
    if looks_like_messages(obj):
        return obj
    for key in ("body", "request", "payload", "data"):
        inner = coerce_json(obj.get(key))
        if looks_like_messages(inner):
            return inner
        if isinstance(inner, dict):
            nested = coerce_json(inner.get("body"))
            if looks_like_messages(nested):
                return nested
    return None


def normalize_logged_headers(raw: Any) -> list[tuple[str, str]]:
    if raw is None:
        return []
    if isinstance(raw, dict):
        rows: list[tuple[str, str]] = []
        for key, value in raw.items():
            if isinstance(value, (dict, list)):
                rows.append((str(key), json.dumps(value, ensure_ascii=False)))
            else:
                rows.append((str(key), str(value)))
        return rows
    if isinstance(raw, list):
        rows = []
        for item in raw:
            if isinstance(item, dict) and "name" in item:
                rows.append((str(item["name"]), str(item.get("value", ""))))
            elif isinstance(item, (list, tuple)) and len(item) >= 2:
                rows.append((str(item[0]), str(item[1])))
        return rows
    return []


def request_path_from(obj: dict[str, Any]) -> str:
    raw = obj.get("path") or obj.get("url") or obj.get("request_path")
    if not isinstance(raw, str) or not raw:
        return "/v1/messages"
    parsed = urlparse(raw)
    path = parsed.path or raw
    if parsed.query:
        return f"{path}?{parsed.query}"
    if path.startswith("/"):
        return path
    return "/v1/messages"


def claude_capture_kind(
    headers: list[tuple[str, str]], payload: dict[str, Any]
) -> str:
    blob = json.dumps(payload, ensure_ascii=False)
    for _, value in headers:
        blob += "\n" + value
    if (
        "sdk-cli" in blob
        or CLAUDE_SDK_IDENTITY in blob
        or "cc_entrypoint=sdk-cli" in blob
    ):
        return "sdk"
    if (
        CLAUDE_CLI_IDENTITY in blob
        or "cc_entrypoint=cli" in blob
        or "(external, cli)" in blob
    ):
        return "cli"
    return "unknown"


def ingest_claude_debug(path: Path) -> CapturedRequest:
    text = path.read_text(encoding="utf-8", errors="replace")
    candidates: list[CapturedRequest] = []
    sdk_seen = False
    for obj in iter_debug_objects(text):
        for node in walk_dicts(obj):
            payload = messages_payload_from(node)
            if payload is None:
                continue
            headers = normalize_logged_headers(
                node.get("headers") or node.get("request_headers")
            )
            kind = claude_capture_kind(headers, payload)
            if kind == "sdk":
                sdk_seen = True
                continue
            if kind != "cli":
                continue
            body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
            candidates.append(
                CapturedRequest("POST", request_path_from(node), headers, body)
            )
    if not candidates:
        if sdk_seen:
            raise RuntimeError(
                "claude debug log is sdk-cli / print path; recapture from the "
                "interactive TUI (never claude -p)"
            )
        raise RuntimeError(
            "claude debug log has no CLI messages request "
            f"(need {CLAUDE_CLI_IDENTITY!r} or cc_entrypoint=cli)"
        )
    return select_inference(candidates)


def pretty_jsonc(value: Any) -> str:
    out: list[str] = []
    _pretty_jsonc_value(value, 0, out)
    text = "".join(out)
    if not text.endswith("\n"):
        text += "\n"
    return text


def _pretty_jsonc_value(value: Any, indent: int, out: list[str]) -> None:
    if value is None:
        out.append("null")
    elif isinstance(value, bool):
        out.append("true" if value else "false")
    elif isinstance(value, int) and not isinstance(value, bool):
        out.append(str(value))
    elif isinstance(value, float):
        out.append(json.dumps(value))
    elif isinstance(value, str):
        _pretty_jsonc_string(value, out)
    elif isinstance(value, list):
        _pretty_jsonc_array(value, indent, out)
    elif isinstance(value, dict):
        _pretty_jsonc_object(value, indent, out)
    else:
        out.append(json.dumps(value))


def _pretty_jsonc_string(text: str, out: list[str]) -> None:
    out.append('"')
    for ch in text:
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\n")
        elif ch == "\r":
            out.append("\\r")
        elif ch == "\t":
            out.append("\\t")
        elif ord(ch) < 32:
            out.append(f"\\u{ord(ch):04x}")
        else:
            out.append(ch)
    out.append('"')


def _pretty_jsonc_array(items: list[Any], indent: int, out: list[str]) -> None:
    out.append("[")
    if not items:
        out.append("]")
        return
    out.append("\n")
    pad = "  " * (indent + 1)
    close = "  " * indent
    for index, item in enumerate(items):
        if index:
            out.append(",\n")
        out.append(pad)
        _pretty_jsonc_value(item, indent + 1, out)
    out.append("\n")
    out.append(close)
    out.append("]")


def _pretty_jsonc_object(mapping: dict[str, Any], indent: int, out: list[str]) -> None:
    out.append("{")
    if not mapping:
        out.append("}")
        return
    out.append("\n")
    pad = "  " * (indent + 1)
    close = "  " * indent
    for index, (key, value) in enumerate(mapping.items()):
        if index:
            out.append(",\n")
        out.append(pad)
        _pretty_jsonc_string(key, out)
        out.append(": ")
        _pretty_jsonc_value(value, indent + 1, out)
    out.append("\n")
    out.append(close)
    out.append("}")


def _sse(events: list[tuple[str, dict[str, Any]]], sequence: bool = False) -> bytes:
    chunks: list[str] = []
    for index, (name, payload) in enumerate(events):
        body = dict(payload)
        if sequence:
            body.setdefault("sequence_number", index)
        chunks.append(
            f"event: {name}\ndata: {json.dumps(body, separators=(',', ':'))}\n\n"
        )
    return "".join(chunks).encode("utf-8")


def echo_model(payload: Any | None, fallback: str) -> str:
    if isinstance(payload, dict):
        model = payload.get("model")
        if isinstance(model, str) and model:
            return model
    return fallback


def dummy_messages(model: str, stream: bool) -> tuple[str, bytes]:
    if not stream:
        body = {
            "id": "msg_native_compare",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "model": model,
            "stop_reason": "end_turn",
            "stop_sequence": None,
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }
        return "application/json", json.dumps(body).encode("utf-8")
    events = [
        (
            "message_start",
            {
                "type": "message_start",
                "message": {
                    "id": "msg_native_compare",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": model,
                    "stop_reason": None,
                    "stop_sequence": None,
                    "usage": {"input_tokens": 1, "output_tokens": 0},
                },
            },
        ),
        (
            "content_block_start",
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            },
        ),
        (
            "content_block_delta",
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "ok"},
            },
        ),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        (
            "message_delta",
            {
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                "usage": {"output_tokens": 1},
            },
        ),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "text/event-stream", _sse(events)


CLAUDE_TOOL_PROMPT = "nc-tools: run the two scripted tool rounds"
CLAUDE_RETRY_PROMPT = "nc-retry: answer after one overloaded reply"
CLAUDE_RESUME_PROMPT = "nc-resumed: continue after reopening"
CLAUDE_MULTITURN_PROMPTS = (PROMPT, CLAUDE_TOOL_PROMPT, CLAUDE_RETRY_PROMPT)
CLAUDE_TOOL_ROUNDS = (
    ("toolu_nc_1", "Bash", {"command": "echo nc-one", "description": "Print nc-one"}),
    ("toolu_nc_2", "Bash", {"command": "echo nc-two", "description": "Print nc-two"}),
)


def _message_blocks(message: Any) -> list[dict[str, Any]]:
    content = message.get("content") if isinstance(message, dict) else None
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    if isinstance(content, list):
        return [block for block in content if isinstance(block, dict)]
    return []


def last_user_blocks(payload: Any) -> list[dict[str, Any]]:
    messages = payload.get("messages") if isinstance(payload, dict) else None
    if not isinstance(messages, list):
        return []
    for message in reversed(messages):
        if isinstance(message, dict) and message.get("role") == "user":
            return _message_blocks(message)
    return []


def messages_reply(
    model: str,
    stream: bool,
    message_id: str,
    blocks: list[dict[str, Any]],
    stop_reason: str,
) -> tuple[str, bytes]:
    if not stream:
        body = {
            "id": message_id, "type": "message", "role": "assistant",
            "content": blocks, "model": model, "stop_reason": stop_reason,
            "stop_sequence": None, "usage": {"input_tokens": 1, "output_tokens": 1},
        }
        return "application/json", json.dumps(body).encode("utf-8")
    events: list[tuple[str, dict[str, Any]]] = [(
        "message_start",
        {"type": "message_start", "message": {
            "id": message_id, "type": "message", "role": "assistant", "content": [],
            "model": model, "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 1, "output_tokens": 0},
        }},
    )]
    for index, block in enumerate(blocks):
        if block["type"] == "tool_use":
            start = {**block, "input": {}}
            delta = {"type": "input_json_delta",
                     "partial_json": json.dumps(block["input"], separators=(",", ":"))}
        else:
            start = {"type": "text", "text": ""}
            delta = {"type": "text_delta", "text": block["text"]}
        events += [
            ("content_block_start",
             {"type": "content_block_start", "index": index, "content_block": start}),
            ("content_block_delta",
             {"type": "content_block_delta", "index": index, "delta": delta}),
            ("content_block_stop", {"type": "content_block_stop", "index": index}),
        ]
    events += [
        ("message_delta", {"type": "message_delta",
                           "delta": {"stop_reason": stop_reason, "stop_sequence": None},
                           "usage": {"output_tokens": 1}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "text/event-stream", _sse(events)


class ClaudeScript:
    """Scripted Anthropic Messages answers for a multi-turn Claude capture.

    Main-thread requests (those declaring tools) follow their last user
    message: the tool prompt answers with two sequential Bash `tool_use`
    rounds, the retry prompt fails once with a retryable 529, and everything
    else ends the turn with text. Side requests (titles, classifiers) get a
    plain text answer. Every reply carries a distinct message id and
    `request-id`, so continuation fields can be matched to the reply they
    name. Nothing is forwarded.
    """

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.served = 0
        self.overloaded = False

    def reply(self, path: str, payload: Any, stream: bool) -> MockReply | None:
        lowered = urlparse(path).path.lower()
        if not lowered.endswith("/messages") or not isinstance(payload, dict):
            return None
        main = bool(payload.get("tools"))
        blocks = last_user_blocks(payload)
        results = {b.get("tool_use_id") for b in blocks if b.get("type") == "tool_result"}
        text = "\n".join(str(b.get("text", "")) for b in blocks if b.get("type") == "text")
        with self.lock:
            self.served += 1
            serial = self.served
            action = "text"
            if main and CLAUDE_TOOL_ROUNDS[0][0] in results:
                action = "tool_use:2"
            elif main and CLAUDE_TOOL_ROUNDS[1][0] in results:
                action = "text"
            elif main and CLAUDE_TOOL_PROMPT in text:
                action = "tool_use:1"
            elif main and CLAUDE_RETRY_PROMPT in text and not self.overloaded:
                self.overloaded = True
                action = "overloaded"
        request_id = f"req_nc{serial:03d}"
        message_id = f"msg_nc{serial:03d}"
        headers = [("request-id", request_id)]
        summary = {"serial": serial, "main": main, "action": action,
                   "request_id": request_id, "message_id": None}
        if action == "overloaded":
            body = {"type": "error", "request_id": request_id,
                    "error": {"type": "overloaded_error", "message": "Overloaded"}}
            return MockReply(529, "application/json", json.dumps(body).encode("utf-8"),
                             headers + [("x-should-retry", "true")], summary)
        model = echo_model(payload, "native-compare-mock")
        if action.startswith("tool_use:"):
            tool_id, name, tool_input = CLAUDE_TOOL_ROUNDS[int(action[-1]) - 1]
            content = [{"type": "tool_use", "id": tool_id, "name": name, "input": tool_input}]
            stop = "tool_use"
        else:
            content = [{"type": "text", "text": "ok"}]
            stop = "end_turn"
        content_type, body_bytes = messages_reply(model, stream, message_id, content, stop)
        summary["message_id"] = message_id
        return MockReply(200, content_type, body_bytes, headers, summary)


def forced_function_name(payload: Any | None) -> str | None:
    if not isinstance(payload, dict):
        return None
    choice = payload.get("tool_choice")
    if isinstance(choice, dict) and choice.get("type") == "function":
        name = choice.get("name")
        return name if isinstance(name, str) else None
    return None


def dummy_responses(
    model: str, stream: bool, payload: Any | None = None
) -> tuple[str, bytes]:
    tool_name = forced_function_name(payload)
    if tool_name == "session_title":
        arguments = json.dumps({"session_title": "Hello"})
    elif tool_name:
        arguments = "{}"
    else:
        arguments = None
    if arguments is not None:
        item = {
            "id": "fc_native_compare",
            "type": "function_call",
            "status": "completed",
            "name": tool_name,
            "call_id": "call_native_compare",
            "arguments": arguments,
        }
    else:
        item = {
            "id": "msg_native_compare",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "ok", "annotations": []}],
        }
    completed = {
        "id": "resp_native_compare",
        "object": "response",
        "created_at": 1,
        "status": "completed",
        "model": model,
        "output": [item],
        "usage": {
            "input_tokens": 1,
            "output_tokens": 1,
            "total_tokens": 2,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0},
        },
    }
    if not stream:
        return "application/json", json.dumps(completed).encode("utf-8")
    if arguments is not None:
        events = [
            (
                "response.created",
                {
                    "type": "response.created",
                    "response": {
                        "id": "resp_native_compare",
                        "object": "response",
                        "created_at": 1,
                        "status": "in_progress",
                        "model": model,
                        "output": [],
                    },
                },
            ),
            (
                "response.output_item.added",
                {
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {
                        "id": "fc_native_compare",
                        "type": "function_call",
                        "status": "in_progress",
                        "name": tool_name,
                        "call_id": "call_native_compare",
                        "arguments": "",
                    },
                },
            ),
            (
                "response.function_call_arguments.delta",
                {
                    "type": "response.function_call_arguments.delta",
                    "item_id": "fc_native_compare",
                    "output_index": 0,
                    "delta": arguments,
                },
            ),
            (
                "response.function_call_arguments.done",
                {
                    "type": "response.function_call_arguments.done",
                    "item_id": "fc_native_compare",
                    "output_index": 0,
                    "arguments": arguments,
                },
            ),
            (
                "response.output_item.done",
                {
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": item,
                },
            ),
            ("response.completed", {"type": "response.completed", "response": completed}),
        ]
        return "text/event-stream", _sse(events, sequence=True)
    message = item
    events = [
        (
            "response.created",
            {
                "type": "response.created",
                "response": {
                    "id": "resp_native_compare",
                    "object": "response",
                    "created_at": 1,
                    "status": "in_progress",
                    "model": model,
                    "output": [],
                },
            },
        ),
        (
            "response.output_item.added",
            {
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "msg_native_compare",
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            },
        ),
        (
            "response.content_part.added",
            {
                "type": "response.content_part.added",
                "item_id": "msg_native_compare",
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []},
            },
        ),
        (
            "response.output_text.delta",
            {
                "type": "response.output_text.delta",
                "item_id": "msg_native_compare",
                "output_index": 0,
                "content_index": 0,
                "delta": "ok",
            },
        ),
        (
            "response.output_text.done",
            {
                "type": "response.output_text.done",
                "item_id": "msg_native_compare",
                "output_index": 0,
                "content_index": 0,
                "text": "ok",
            },
        ),
        (
            "response.content_part.done",
            {
                "type": "response.content_part.done",
                "item_id": "msg_native_compare",
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": "ok", "annotations": []},
            },
        ),
        (
            "response.output_item.done",
            {
                "type": "response.output_item.done",
                "output_index": 0,
                "item": message,
            },
        ),
        ("response.completed", {"type": "response.completed", "response": completed}),
    ]
    return "text/event-stream", _sse(events, sequence=True)


def dummy_chat(model: str, stream: bool) -> tuple[str, bytes]:
    if not stream:
        body = {
            "id": "chatcmpl_native_compare",
            "object": "chat.completion",
            "created": 1,
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop",
                }
            ],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        }
        return "application/json", json.dumps(body).encode("utf-8")
    chunks = [
        {
            "id": "chatcmpl_native_compare",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": model,
            "choices": [
                {
                    "index": 0,
                    "delta": {"role": "assistant", "content": "ok"},
                    "finish_reason": None,
                }
            ],
        },
        {
            "id": "chatcmpl_native_compare",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        },
    ]
    lines = "".join(
        f"data: {json.dumps(chunk, separators=(',', ':'))}\n\n" for chunk in chunks
    )
    lines += "data: [DONE]\n\n"
    return "text/event-stream", lines.encode("utf-8")


def dummy_for_path(
    path: str, payload: Any | None, stream: bool
) -> tuple[str, bytes]:
    model = echo_model(payload, "native-compare-mock")
    lowered = path.lower()
    if "/messages" in lowered:
        return dummy_messages(model, stream)
    if "/chat/completions" in lowered or lowered.rstrip("/").endswith("/completions"):
        return dummy_chat(model, stream)
    if "/responses" in lowered or "/codex/responses" in lowered:
        return dummy_responses(model, stream, payload)
    if isinstance(payload, dict) and payload.get("messages") is not None:
        if "/v1/messages" in lowered or "anthropic" in lowered:
            return dummy_messages(model, stream)
        return dummy_chat(model, stream)
    if isinstance(payload, dict) and payload.get("input") is not None:
        return dummy_responses(model, stream, payload)
    return dummy_chat(model, stream)


def models_catalog() -> bytes:
    ids = [
        "claude-sonnet-5",
        "claude-opus-4-6",
        "gpt-5.6-luna",
        "gpt-5.6-sol",
        "grok-4.6",
        "inclusionai/ling-3.0-flash-sante:free",
        "openrouter/inclusionai/ling-3.0-flash-sante:free",
    ]
    data = []
    for model_id in ids:
        data.append(
            {
                "id": model_id,
                "object": "model",
                "created": 0,
                "owned_by": "native-compare",
                "name": model_id,
                "context_length": 128000,
                "pricing": {"prompt": "0", "completion": "0"},
            }
        )
    return json.dumps({"object": "list", "data": data}).encode("utf-8")


def is_models_path(path: str) -> bool:
    lowered = path.lower().split("?", 1)[0]
    return lowered.rstrip("/").endswith("/models") or lowered.endswith("/models.json")


# Codex 0.160 keeps control-plane calls on chatgpt_base_url (default
# https://chatgpt.com/backend-api) and sends the responses turn to
# {openai_base_url}/responses. The provider base defaults to
# https://chatgpt.com/backend-api/codex when openai_base_url is unset, so a
# chatgpt_base_url override alone never reaches this mock.
CODEX_DEFAULT_INFERENCE_BASE = "https://chatgpt.com/backend-api/codex"
CODEX_LOCAL_HOSTS = frozenset({"127.0.0.1", "localhost", "::1"})


def codex_override_map(argv: list[str]) -> dict[str, str]:
    found: dict[str, str] = {}
    index = 0
    while index < len(argv):
        if argv[index] == "-c" and index + 1 < len(argv):
            key, separator, value = argv[index + 1].partition("=")
            if separator:
                found[key.strip()] = value.strip().strip('"')
            index += 2
            continue
        index += 1
    return found


def codex_inference_url(argv: list[str]) -> str:
    base = codex_override_map(argv).get("openai_base_url") or CODEX_DEFAULT_INFERENCE_BASE
    return base.rstrip("/") + "/responses"


def codex_control_url(argv: list[str], path: str) -> str | None:
    base = codex_override_map(argv).get("chatgpt_base_url")
    if not base:
        return None
    return base.rstrip("/") + "/" + path.lstrip("/")


def _codex_loopback(url: str) -> bool:
    return urlparse(url).hostname in CODEX_LOCAL_HOSTS


def simulate_codex_capture(argv: list[str]) -> int:
    """Fake Codex CLI: the capture command's URLs, against the local mock only.

    Refuses a non-loopback inference URL so a test cannot leave the machine.
    """
    inference = codex_inference_url(argv)
    if not _codex_loopback(inference):
        return 2
    controls: list[str] = []
    for path in ("wham/accounts/check", "codex/models"):
        url = codex_control_url(argv, path)
        if url is None or not _codex_loopback(url):
            return 2
        controls.append(url)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    for url in controls:
        with opener.open(url, timeout=5) as response:
            response.read()
    body = json.dumps(
        {"model": SPECS["codex"].model, "input": PROMPT, "stream": True}
    ).encode("utf-8")
    request = urllib.request.Request(
        inference,
        data=body,
        headers={"Content-Type": "application/json", "Accept": "text/event-stream"},
        method="POST",
    )
    with opener.open(request, timeout=5) as response:
        response.read()
    return 0


def codex_control_reply(method: str, path: str, payload: Any) -> tuple[int, bytes] | None:
    path = urlparse(path).path.rstrip("/")
    reply: Any
    status = 200
    if method.upper() == "GET":
        if path.endswith("/accounts/check"):
            reply = {"accounts": [{"id": "native-compare-account", "workspace_backend_origin": "https://chatgpt.com", "account_routing_override": "NO_CONSTRAINT"}]}
        elif path.endswith("/codex/models"):
            reply = {"models": [{
                "slug": SPECS["codex"].model,
                "display_name": "Native compare synthetic model",
                "description": None,
                "supported_reasoning_levels": [],
                "shell_type": "shell_command",
                "visibility": "list",
                "supported_in_api": True,
                "priority": 0,
                "availability_nux": None,
                "upgrade": None,
                "support_verbosity": False,
                "default_verbosity": None,
                "apply_patch_tool_type": None,
                "truncation_policy": {"mode": "tokens", "limit": 10000},
                "experimental_supported_tools": [],
            }]}
        elif "/plugins" in path:
            reply = ({"enabled": False, "plugins": []} if path.endswith("/suggested")
                     else {"plugins": [], "next_page_token": None})
        else:
            return None
    elif method.upper() == "POST" and path == "/backend-api/ps/mcp":
        payload = payload if isinstance(payload, dict) else {}
        if "id" not in payload:
            return 202, b"{}"
        rpc_method = payload.get("method")
        if rpc_method == "initialize":
            result = {"protocolVersion": "2024-11-05", "capabilities": {},
                      "serverInfo": {"name": "native-compare", "version": "1"}}
        elif rpc_method == "tools/list":
            result = {"tools": []}
        else:
            reply = {"jsonrpc": "2.0", "id": payload["id"],
                     "error": {"code": -32601, "message": "Method not found"}}
            return 200, json.dumps(reply).encode("utf-8")
        reply = {"jsonrpc": "2.0", "id": payload.get("id"), "result": result}
    else:
        return None
    return status, json.dumps(reply).encode("utf-8")


def make_handler(state: CaptureState) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, format: str, *args: object) -> None:
            sys.stderr.write(f"native-compare mock: {args[0] if args else format}\n")

        def _header_list(self) -> list[tuple[str, str]]:
            return [(str(name), str(value)) for name, value in self.headers.items()]

        def _read_body(self) -> bytes:
            length = int(self.headers.get("Content-Length") or 0)
            if length <= 0:
                return b""
            return self.rfile.read(length)

        def _send(self, status: int, content_type: str, body: bytes) -> None:
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.wfile.write(body)

        def do_CONNECT(self) -> None:  # noqa: N802
            self.send_response(403)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def do_OPTIONS(self) -> None:  # noqa: N802
            self.send_response(204)
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Access-Control-Allow-Headers", "*")
            self.send_header("Access-Control-Allow-Methods", "GET,POST,PUT,PATCH,OPTIONS")
            self.end_headers()

        def do_HEAD(self) -> None:  # noqa: N802
            parsed = urlparse(self.path)
            with state.lock:
                state.requests.append(("HEAD", parsed.path))
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", "2")
            self.end_headers()

        def do_GET(self) -> None:  # noqa: N802
            parsed = urlparse(self.path)
            with state.lock:
                state.requests.append(("GET", parsed.path))
            control = codex_control_reply("GET", parsed.path, None)
            if control is not None:
                self._send(control[0], "application/json", control[1])
                return
            if is_models_path(parsed.path):
                self._send(200, "application/json", models_catalog())
                return
            self._send(200, "application/json", b"{}")

        def do_POST(self) -> None:  # noqa: N802
            self._handle_write()

        def do_PUT(self) -> None:  # noqa: N802
            self._handle_write()

        def do_PATCH(self) -> None:  # noqa: N802
            self._handle_write()

        def _handle_write(self) -> None:
            parsed = urlparse(self.path)
            headers = self._header_list()
            raw = self._read_body()
            try:
                body = decode_body(headers, raw)
            except Exception as error:
                sys.stderr.write(f"native-compare decode: {error}\n")
                body = raw
            payload = parse_json_body(body)
            captured = None
            with state.lock:
                state.requests.append((self.command, parsed.path))
                if self.command != "GET" and is_inference_payload(parsed.path, payload):
                    captured = CapturedRequest(
                        method=self.command,
                        path=self.path,
                        headers=headers,
                        body=body,
                    )
                    state.inferences.append(captured)
            stream = wants_stream(headers, payload)
            control = codex_control_reply(self.command, parsed.path, payload)
            if control is not None:
                self._send(control[0], "application/json", control[1])
                return
            scripted = scripted_reply(state, captured, self.path, payload, stream)
            if scripted is not None:
                self.send_response(scripted.status)
                for name, value in scripted.headers:
                    self.send_header(name, value)
                self.send_header("Content-Type", scripted.content_type)
                self.send_header("Content-Length", str(len(scripted.body)))
                self.end_headers()
                self.wfile.write(scripted.body)
                return
            content_type, reply = dummy_for_path(parsed.path, payload, stream)
            self._send(200, content_type, reply)

    return Handler


def scripted_reply(
    state: CaptureState,
    captured: CapturedRequest | None,
    path: str,
    payload: Any,
    stream: bool,
) -> MockReply | None:
    if state.responder is None or captured is None:
        return None
    scripted = state.responder.reply(path, payload, stream)
    if scripted is not None:
        with state.lock:
            captured.reply = scripted.summary
    return scripted


def start_mock(
    state: CaptureState | None = None,
) -> tuple[ThreadingHTTPServer, CaptureState, str]:
    state = state or CaptureState()
    handler = make_handler(state)
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    httpd.daemon_threads = True
    origin = f"http://127.0.0.1:{httpd.server_address[1]}"
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    return httpd, state, origin


MITM_HOST_SUFFIXES = (
    "chatgpt.com",
    "chat.openai.com",
    "api.openai.com",
    "api.anthropic.com",
    "chatgpt-staging.com",
)


def mitm_host(host: str) -> bool:
    lowered = host.lower().split(":", 1)[0]
    return any(
        lowered == suffix or lowered.endswith("." + suffix)
        for suffix in MITM_HOST_SUFFIXES
    )


def generate_mitm_certs(directory: Path) -> tuple[Path, Path, Path]:
    directory.mkdir(parents=True, exist_ok=True)
    ca_key = directory / "ca.key"
    ca_pem = directory / "ca.pem"
    leaf_key = directory / "leaf.key"
    leaf_csr = directory / "leaf.csr"
    leaf_pem = directory / "leaf.pem"
    ext = directory / "leaf.ext"
    ext.write_text(
        "subjectAltName=DNS:chatgpt.com,DNS:*.chatgpt.com,DNS:chat.openai.com,"
        "DNS:api.openai.com,DNS:api.anthropic.com,DNS:chatgpt-staging.com\n"
        "extendedKeyUsage=serverAuth\n",
        encoding="utf-8",
    )
    subprocess.run(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-keyout",
            str(ca_key),
            "-out",
            str(ca_pem),
            "-days",
            "1",
            "-nodes",
            "-subj",
            "/CN=native-compare-ca",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
        ],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        [
            "openssl",
            "req",
            "-newkey",
            "rsa:2048",
            "-keyout",
            str(leaf_key),
            "-out",
            str(leaf_csr),
            "-nodes",
            "-subj",
            "/CN=chatgpt.com",
        ],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        [
            "openssl",
            "x509",
            "-req",
            "-in",
            str(leaf_csr),
            "-CA",
            str(ca_pem),
            "-CAkey",
            str(ca_key),
            "-CAcreateserial",
            "-out",
            str(leaf_pem),
            "-days",
            "1",
            "-extfile",
            str(ext),
        ],
        check=True,
        capture_output=True,
    )
    return ca_pem, leaf_pem, leaf_key


def _recv_until(sock: socket.socket, marker: bytes, limit: int = 1024 * 1024) -> bytes:
    buf = b""
    while marker not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            break
        buf += chunk
        if len(buf) > limit:
            raise RuntimeError("HTTP header too large")
    return buf


def _read_http_request(sock: socket.socket) -> tuple[str, str, list[tuple[str, str]], bytes] | None:
    buf = _recv_until(sock, b"\r\n\r\n")
    if not buf:
        return None
    header, rest = buf.split(b"\r\n\r\n", 1)
    lines = header.split(b"\r\n")
    request_line = lines[0].decode("latin1")
    method, path, _proto = request_line.split(" ", 2)
    headers: list[tuple[str, str]] = []
    length = 0
    chunked = False
    for line in lines[1:]:
        name, value = line.decode("latin1").split(":", 1)
        headers.append((name.strip(), value.strip()))
        lowered = name.lower()
        if lowered == "content-length":
            length = int(value.strip())
        if lowered == "transfer-encoding" and "chunked" in value.lower():
            chunked = True
    body = rest
    if chunked:
        body = _read_chunked(sock, rest)
    else:
        while len(body) < length:
            chunk = sock.recv(length - len(body))
            if not chunk:
                break
            body += chunk
        body = body[:length]
    return method, path, headers, body


def _read_chunked(sock: socket.socket, initial: bytes) -> bytes:
    buf = initial
    out = b""
    while True:
        while b"\r\n" not in buf:
            chunk = sock.recv(4096)
            if not chunk:
                return out
            buf += chunk
        line, buf = buf.split(b"\r\n", 1)
        size = int(line.split(b";", 1)[0], 16)
        if size == 0:
            return out
        while len(buf) < size + 2:
            chunk = sock.recv(size + 2 - len(buf))
            if not chunk:
                return out + buf[:size]
            buf += chunk
        out += buf[:size]
        buf = buf[size + 2 :]


HTTP_REASONS = {200: "OK", 202: "Accepted", 529: "Overloaded"}


def _send_http_response(
    sock: socket.socket,
    content_type: str,
    body: bytes,
    status: int = 200,
    extra_headers: list[tuple[str, str]] | None = None,
) -> None:
    extra = "".join(f"{name}: {value}\r\n" for name, value in extra_headers or [])
    head = (
        f"HTTP/1.1 {status} {HTTP_REASONS.get(status, 'Status')}\r\n"
        f"Content-Type: {content_type}\r\n"
        f"Content-Length: {len(body)}\r\n"
        f"{extra}"
        "Cache-Control: no-cache\r\n"
        "Connection: close\r\n"
        "\r\n"
    )
    sock.sendall(head.encode("ascii") + body)


@dataclass
class MitmProxy:
    server: socketserver.ThreadingTCPServer
    ca_pem: Path
    bundle_pem: Path
    proxy_url: str

    def shutdown(self) -> None:
        self.server.shutdown()
        self.server.server_close()


def start_mitm(state: CaptureState, cert_dir: Path) -> MitmProxy:
    import ssl

    ca_pem, leaf_pem, leaf_key = generate_mitm_certs(cert_dir)
    bundle_pem = cert_dir / "trust-bundle.pem"
    system_ca = Path("/opt/homebrew/etc/openssl@3/cert.pem")
    if not system_ca.is_file():
        system_ca = Path("/etc/ssl/cert.pem")
    bundle = ca_pem.read_text(encoding="utf-8") + "\n"
    if system_ca.is_file():
        bundle += system_ca.read_text(encoding="utf-8")
    bundle_pem.write_text(bundle, encoding="utf-8")
    tls_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls_ctx.load_cert_chain(leaf_pem, leaf_key)
    tls_ctx.set_alpn_protocols(["http/1.1"])

    class Handler(socketserver.BaseRequestHandler):
        def handle(self) -> None:
            sock: socket.socket = self.request
            sock.settimeout(10)
            try:
                prelude = _recv_until(sock, b"\r\n\r\n")
            except OSError:
                return
            if not prelude.startswith(b"CONNECT "):
                return
            request_line = prelude.split(b"\r\n", 1)[0].decode("latin1")
            target = request_line.split(" ", 2)[1]
            host, _, port_s = target.partition(":")
            if port_s not in ("", "443") or not mitm_host(host):
                sock.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                return
            sock.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            try:
                tls = tls_ctx.wrap_socket(sock, server_side=True)
            except ssl.SSLError as error:
                sys.stderr.write(f"native-compare mitm tls: {error}\n")
                return
            try:
                parsed = _read_http_request(tls)
                if parsed is None:
                    return
                method, path, headers, raw = parsed
                try:
                    body = decode_body(headers, raw)
                except Exception as error:
                    sys.stderr.write(f"native-compare decode: {error}\n")
                    body = raw
                payload = parse_json_body(body)
                captured = None
                with state.lock:
                    state.requests.append((method, urlparse(path).path))
                    if method.upper() != "GET" and is_inference_payload(path, payload):
                        captured = CapturedRequest(method, path, headers, body)
                        state.inferences.append(captured)
                sys.stderr.write(f"native-compare mitm: {method} {path}\n")
                stream = wants_stream(headers, payload)
                control = codex_control_reply(method, path, payload)
                if control is not None:
                    _send_http_response(tls, "application/json", control[1], control[0])
                    return
                if method.upper() == "GET":
                    if is_models_path(path):
                        _send_http_response(tls, "application/json", models_catalog())
                    else:
                        _send_http_response(tls, "application/json", b"{}")
                    return
                scripted = scripted_reply(state, captured, path, payload, stream)
                if scripted is not None:
                    _send_http_response(
                        tls, scripted.content_type, scripted.body,
                        scripted.status, scripted.headers,
                    )
                    return
                content_type, reply = dummy_for_path(path, payload, stream)
                _send_http_response(tls, content_type, reply)
            finally:
                try:
                    tls.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                tls.close()

    class Server(socketserver.ThreadingTCPServer):
        allow_reuse_address = True
        daemon_threads = True

    server = Server(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    proxy_url = f"http://127.0.0.1:{server.server_address[1]}"
    return MitmProxy(
        server=server, ca_pem=ca_pem, bundle_pem=bundle_pem, proxy_url=proxy_url
    )


def git(args: list[str], cwd: Path) -> None:
    subprocess.run(
        ["git", *args],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
    )


def prepare_fixture(work_root: Path, vendor: str, origin: str) -> Path:
    dest = work_root / vendor
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    (dest / "src").mkdir()
    (dest / "README.md").write_text(
        "Isolated fixture workspace for native prompt capture. First user turn: hello.\n",
        encoding="utf-8",
    )
    (dest / "src" / "hello.txt").write_text("hello\n", encoding="utf-8")
    if vendor == "claude":
        (dest / "mcp.json").write_text('{"mcpServers": {}}\n', encoding="utf-8")
    if vendor == "opencode":
        config = {
            "$schema": "https://opencode.ai/config.json",
            "enabled_providers": ["openrouter"],
            "provider": {
                "openrouter": {
                    "options": {
                        "baseURL": f"{origin}/api/v1",
                        "apiKey": "native-compare-synthetic-key",
                    },
                    "models": {SPECS["opencode"].model: {
                        "name": "Ling synthetic capture", "tool_call": True,
                        "limit": {"context": 128000, "output": 8192},
                    }},
                }
            },
        }
        (dest / "opencode.json").write_text(
            json.dumps(config, indent=2) + "\n", encoding="utf-8"
        )
    git(["init"], dest)
    git(["add", "-A"], dest)
    git(
        [
            "-c",
            "user.name=native-compare",
            "-c",
            "user.email=nobody@invalid.local",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "init",
        ],
        dest,
    )
    return dest


def capture_env(
    origin: str,
    *,
    proxy_url: str | None = None,
    ca_pem: Path | None = None,
    home: Path | None = None,
) -> dict[str, str]:
    # An allowlist prevents inherited credentials, plugins and route overrides.
    env = {key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL") if key in os.environ}
    env["HTTPS_PROXY"] = env["HTTP_PROXY"] = origin
    env["https_proxy"] = env["http_proxy"] = origin
    env["NO_PROXY"] = ",".join(
        part
        for part in [env.get("NO_PROXY"), "127.0.0.1", "localhost"]
        if part
    )
    env["no_proxy"] = env["NO_PROXY"]
    if home is not None:
        home.mkdir(parents=True, exist_ok=True)
        for name in ("config", "cache", "data", "state", "claude-1", "codex", "grok", "tmp"):
            (home / name).mkdir(exist_ok=True)
        env.update({
            "HOME": str(home), "XDG_CONFIG_HOME": str(home / "config"),
            "XDG_CACHE_HOME": str(home / "cache"), "XDG_DATA_HOME": str(home / "data"),
            "XDG_STATE_HOME": str(home / "state"), "TMPDIR": str(home / "tmp"),
            "CLAUDE_CONFIG_DIR": str(home / "claude-1"),
            "CODEX_HOME": str(home / "codex"), "GROK_HOME": str(home / "grok"),
        })
        claims = {"exp": int(time.time()) + 86400, "email": "capture@invalid.local",
                  "https://api.openai.com/auth": {"chatgpt_account_id": "native-compare-account",
                      "chatgpt_user_id": "native-compare-user", "chatgpt_plan_type": "pro"}}
        token = ".".join(base64.urlsafe_b64encode(json.dumps(part).encode()).decode().rstrip("=")
                         for part in ({"alg": "none"}, claims)) + ".synthetic"
        (home / "codex" / "auth.json").write_text(json.dumps({
            "auth_mode": "chatgpt", "tokens": {"id_token": token, "access_token": token,
                "refresh_token": "native-compare-synthetic-refresh", "account_id": "native-compare-account"},
            "last_refresh": utc_now(),
        }))
        # Native Grok indexes OIDC logins by the public issuer/client pair.
        issuer = "https://auth.x.ai"
        client_id = "b1a00492-073a-47ea-816f-4c329264a828"
        expires = (datetime.now(timezone.utc) + timedelta(days=1)).isoformat()
        (home / "grok" / "auth.json").write_text(json.dumps({f"{issuer}::{client_id}": {
            "key": "native-compare-synthetic-key", "auth_mode": "oidc",
            "create_time": utc_now(), "expires_at": expires,
            "user_id": "native-compare-user", "email": "capture@invalid.local",
            "first_name": "Synthetic", "profile_image_asset_id": "",
            "principal_type": "User", "principal_id": "native-compare-user",
            "team_id": "native-compare-team", "coding_data_retention_opt_out": True,
            "refresh_token": "native-compare-synthetic-refresh",
            "oidc_issuer": issuer, "oidc_client_id": client_id,
        }}))
        (home / "claude-1" / ".claude.json").write_text(json.dumps({
            "hasCompletedOnboarding": True, "theme": "dark",
            "bypassPermissionsModeAccepted": True,
        }))
    env["ANTHROPIC_API_KEY"] = "native-compare-synthetic-key"
    env["OPENROUTER_API_KEY"] = "native-compare-synthetic-key"
    env["GROK_XAI_API_BASE_URL"] = f"{origin}/v1"
    env["XAI_API_BASE_URL"] = f"{origin}/v1"
    env["GROK_DISABLE_AUTOUPDATER"] = "1"
    env["GROK_TELEMETRY_ENABLED"] = "false"
    env["OPENCODE_DISABLE_MODELS_FETCH"] = "true"
    env["DISABLE_AUTOUPDATER"] = "1"
    env["DO_NOT_TRACK"] = "1"
    env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"] = "1"
    env["ANTHROPIC_BASE_URL"] = origin
    env["GROK_CLI_CHAT_PROXY_BASE_URL"] = f"{origin}/v1"
    env["CLI_CHAT_PROXY_BASE_URL"] = f"{origin}/v1"
    if proxy_url:
        env["HTTPS_PROXY"] = proxy_url
        env["HTTP_PROXY"] = proxy_url
        env["https_proxy"] = proxy_url
        env["http_proxy"] = proxy_url
    if ca_pem is not None:
        pem = str(ca_pem)
        env["CODEX_CA_CERTIFICATES"] = pem
        env["SSL_CERT_FILE"] = pem
        env["REQUESTS_CA_BUNDLE"] = pem
        env["CURL_CA_BUNDLE"] = pem
        env["NODE_EXTRA_CA_CERTS"] = pem
    return env


def claude_trust_project_keys(project: Path) -> list[str]:
    """Project keys Claude 2.1.293 may use for persisted workspace trust.

    Trust is ``projects[<key>].hasTrustDialogAccepted`` where the key is
    ``path.normalize`` of the NFC git root (or the cwd when it is not a
    repo). ``process.cwd()`` is the physical path, so both the absolute
    path and its realpath are seeded. Callers pass only the capture fixture.
    """
    keys: list[str] = []
    for candidate in (os.path.abspath(project), os.path.realpath(project)):
        key = unicodedata.normalize("NFC", os.path.normpath(candidate))
        if key not in keys:
            keys.append(key)
    return keys


def _synthetic_claude_config_dir(config_dir: Path) -> Path:
    """Return config_dir only when it is not the operator's Claude config."""
    resolved = config_dir.expanduser().resolve()
    home = Path.home().resolve()
    destination = (resolved / ".claude.json").resolve()
    blocked_dirs = [home / ".claude"]
    inherited = os.environ.get("CLAUDE_CONFIG_DIR", "").strip()
    if inherited:
        blocked_dirs.append(Path(inherited).expanduser())
    blocked: list[Path] = []
    for raw in blocked_dirs:
        try:
            blocked.append(raw.resolve())
        except OSError:
            continue
    real_config = (home / ".claude.json").resolve()
    if resolved == home or destination == real_config or destination == home:
        raise RuntimeError(
            "refusing to seed Claude workspace trust outside the capture "
            "script's synthetic config"
        )
    for block in blocked:
        if (
            resolved == block
            or block in resolved.parents
            or destination == block
            or block in destination.parents
        ):
            raise RuntimeError(
                "refusing to seed Claude workspace trust outside the capture "
                "script's synthetic config"
            )
    return resolved


def _trusted_claude_project(existing: object) -> dict[str, Any]:
    entry: dict[str, Any] = {
        "allowedTools": [],
        "mcpContextUris": [],
        "mcpServers": {},
        "enabledMcpjsonServers": [],
        "disabledMcpjsonServers": [],
        "hasClaudeMdExternalIncludesApproved": False,
        "hasClaudeMdExternalIncludesWarningShown": False,
    }
    if isinstance(existing, dict):
        for key, value in existing.items():
            if key != "hasTrustDialogAccepted":
                entry[key] = value
    entry["hasTrustDialogAccepted"] = True
    return entry


def seed_claude_workspace_trust(config_dir: Path, project: Path) -> list[str]:
    """Pre-accept workspace trust for project in a synthetic Claude config.

    Writes ``<config_dir>/.claude.json`` only. ``config_dir`` is the capture
    home's ``claude-1`` directory (``CLAUDE_CONFIG_DIR``), never ``~/.claude``
    or the operator's real ``CLAUDE_CONFIG_DIR``.
    """
    resolved = _synthetic_claude_config_dir(config_dir)
    path = resolved / ".claude.json"
    if path.is_file():
        config = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(config, dict):
            raise RuntimeError("synthetic .claude.json is not an object")
    else:
        resolved.mkdir(parents=True, exist_ok=True)
        config = {
            "hasCompletedOnboarding": True,
            "theme": "dark",
            "bypassPermissionsModeAccepted": True,
        }
    projects = config.get("projects")
    if not isinstance(projects, dict):
        projects = {}
    keys = claude_trust_project_keys(project)
    for key in keys:
        projects[key] = _trusted_claude_project(projects.get(key))
    config["projects"] = projects
    path.write_text(json.dumps(config, indent=2) + "\n", encoding="utf-8")
    return keys


def claude_tui_command(spec: VendorSpec, bin_path: Path, fixture: Path, debug_file: Path) -> list[str]:
    return [
        str(bin_path),
        "--debug",
        "api",
        "--debug-file",
        str(debug_file),
        "--model",
        spec.model,
        "--effort",
        spec.effort or "low",
        "--dangerously-skip-permissions",
        "--permission-mode",
        "bypassPermissions",
        "--strict-mcp-config",
        "--mcp-config",
        str(fixture / "mcp.json"),
        "--disable-slash-commands",
        "--setting-sources",
        "project",
    ]


def vendor_command(
    spec: VendorSpec, bin_path: Path, fixture: Path, origin: str
) -> list[str]:
    if spec.name == "claude":
        raise RuntimeError(CLAUDE_TUI_BLOCKER)
    if spec.name == "codex":
        return [
            str(bin_path),
            "exec",
            PROMPT,
            "-m",
            spec.model,
            "-C",
            str(fixture),
            "--skip-git-repo-check",
            "--dangerously-bypass-approvals-and-sandbox",
            "--ephemeral",
            "--ignore-user-config",
            "-c",
            f'chatgpt_base_url="{origin}/backend-api"',
            "-c",
            f'openai_base_url="{origin}/backend-api/codex"',
        ]
    if spec.name == "grok":
        return [
            str(bin_path),
            "-p",
            PROMPT,
            "-m",
            spec.model,
            "--effort",
            "low",
            "--cwd",
            str(fixture),
            "--permission-mode",
            "bypassPermissions",
            "--max-turns",
            "1",
            "--output-format",
            "json",
        ]
    if spec.name == "opencode":
        return [
            str(bin_path),
            "run",
            "--pure",
            "--print-logs",
            PROMPT,
            "-m",
            f"openrouter/{spec.model}",
            "--variant",
            "medium",
            "--format",
            "json",
            "--dir",
            str(fixture),
            "--auto",
        ]
    raise ValueError(f"unknown vendor {spec.name}")


def compact_chars(value: Any) -> int:
    return len(json.dumps(value, ensure_ascii=False, separators=(",", ":")))


def write_capture(
    out_dir: Path,
    spec: VendorSpec,
    bin_path: Path,
    version: str,
    captured: CapturedRequest,
    origin: str,
    probes: list[tuple[str, str]],
    *,
    cli_exit_code: int | None = None,
    client_status: str = "imported",
    imported: bool = False,
) -> None:
    dest = out_dir / spec.name
    dest.mkdir(mode=0o700, parents=True, exist_ok=True)
    dest.chmod(0o700)
    payload = parse_json_body(captured.body)
    if payload is None:
        raise RuntimeError(f"{spec.name}: captured body is not JSON ({len(captured.body)} bytes)")
    redacted_body = redact_json(payload)
    redacted_headers = redact_headers(captured.headers)
    chars = compact_chars(redacted_body)
    pin = {
        "vendor": spec.name,
        "bin": str(bin_path),
        "version": version,
        "model": spec.model,
        "effort": spec.effort,
        "prompt": PROMPT,
        "captured_at": utc_now(),
        "notes": spec.notes,
        "credential_source": "external debug import; unknown" if imported else "synthetic isolated config; no existing login read",
        "capture_status": "captured",
        "client_status": client_status,
        "privacy": "private local request: contains full prompt/tool content; never commit or share",
        "cli_exit_code": cli_exit_code,
        "authentication": ("unknown external import" if imported else
                           {"claude": "synthetic OAuth bearer", "codex": "synthetic ChatGPT tokens",
                            "grok": "synthetic OIDC tokens", "opencode": "synthetic API key"}[spec.name]),
        "evidence_limits": "Native request construction only; no provider acceptance or paid inference",
        "account_reference": "claude-1" if spec.name == "claude" else None,
        "capture_entrypoint": "interactive-cli" if spec.name == "claude" else "headless-cli",
        "mock_origin": origin,
        "request_origin": "https://api.anthropic.com" if spec.name == "claude" and not imported else origin,
        "request_method": captured.method,
        "request_path": captured.path,
        "probe_paths": [f"{method} {path}" for method, path in probes],
    }
    headers_doc = {
        "body": {
            "approx_tokens": chars // 4,
            "chars": chars,
            "token_count_method": TOKEN_METHOD,
        },
        "headers": redacted_headers,
    }
    (dest / "pin.json").write_text(
        json.dumps(pin, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    (dest / "hello.headers.json").write_text(
        json.dumps(headers_doc, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    (dest / "hello.request.json").write_text(
        json.dumps(redacted_body, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    jsonc = pretty_jsonc(redacted_body)
    header_rows = "\n".join(
        f"| {row['name']} | {row['value']} |" for row in redacted_headers
    )
    markdown = (
        f"# `{spec.name}`\n"
        f"\n"
        f"Native capture from host `{spec.bin}` `{version}`. First user turn: "
        f"`{PROMPT}`.\n"
        f"Isolated fixture workspace (not this git tree). Model `{spec.model}`"
        f"{f', effort `{spec.effort}`' if spec.effort else ''}.\n"
        f"{spec.notes}.\n"
        f"Authentication: {pin['authentication']}. {pin['evidence_limits']}.\n"
        f"Client status: {client_status}; exit code: {cli_exit_code}.\n"
        f"\n"
        f"## Pin\n"
        f"\n"
        f"| field | value |\n"
        f"| --- | --- |\n"
        f"| bin | `{bin_path}` |\n"
        f"| version | {version} |\n"
        f"| model | `{spec.model}` |\n"
        f"| effort | {spec.effort or '—'} |\n"
        f"| captured_at | {pin['captured_at']} |\n"
        f"| request | `{captured.method} {captured.path}` |\n"
        f"\n"
        f"## Headers\n"
        f"\n"
        f"| name | value |\n"
        f"| --- | --- |\n"
        f"{header_rows}\n"
        f"\n"
        f"## Request\n"
        f"\n"
        f"```jsonc\n"
        f"{jsonc.rstrip()}\n"
        f"```\n"
    )
    (dest / "hello.md").write_text(markdown, encoding="utf-8")


def run_vendor(
    spec: VendorSpec,
    out_dir: Path,
    work_root: Path,
    timeout: int,
    *,
    claude_from: Path | None = None,
    claude_scenario: str = "single",
) -> str | None:
    for name in ("pin.json", "hello.headers.json", "hello.request.json", "hello.md",
                 "multiturn.json", "multiturn.facts.json"):
        (out_dir / spec.name / name).unlink(missing_ok=True)
    if spec.name == "claude":
        if claude_from is not None:
            return run_claude_ingest(spec, out_dir, claude_from)
        if claude_scenario == "multiturn":
            return asyncio.run(run_claude_multiturn_owned(spec, out_dir, work_root, timeout))
        return run_claude_tui(spec, out_dir, work_root, timeout)
    bin_path = which(spec.bin)
    if bin_path is None:
        return f"{spec.name}: `{spec.bin}` not on PATH"
    state = CaptureState()
    httpd, state, origin = start_mock(state)
    mitm: MitmProxy | None = None
    log_dir = ROOT / "target/native-compare-logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    log_path = log_dir / f"{spec.name}.log"
    home = Path(tempfile.mkdtemp(prefix=f"{spec.name}-home-", dir=work_root))
    try:
        if spec.name == "codex":
            mitm = start_mitm(state, ROOT / "target/native-compare-certs")
        fixture = prepare_fixture(work_root, spec.name, origin)
        command = vendor_command(spec, bin_path, fixture, origin)
        env = capture_env(
            origin,
            proxy_url=None if mitm is None else mitm.proxy_url,
            ca_pem=None if mitm is None else mitm.bundle_pem,
            home=home,
        )
        version = version_text(bin_path, env)
        print(f"capturing {spec.name}: {' '.join(command)}", flush=True)
        stdout_path, stderr_path = home / "stdout", home / "stderr"
        execution = asyncio.run(owned_processes.run_command(
            command, cwd=fixture, env=env, stdout_path=stdout_path,
            stderr_path=stderr_path, timeout=timeout,
        ))
        result = subprocess.CompletedProcess(
            command, execution["returncode"],
            stdout_path.read_text(encoding="utf-8", errors="replace"),
            stderr_path.read_text(encoding="utf-8", errors="replace"),
        )
        log_path.write_text(
            redact_text(
                f"exit={result.returncode}\n"
                f"argv={command!r}\n"
                f"--- stdout ---\n{result.stdout}\n"
                f"--- stderr ---\n{result.stderr}\n"
            ),
            encoding="utf-8",
        )
        if not execution["cleanup"]["joined"]:
            return f"{spec.name}: native capture cleanup incomplete (log {log_path})"
        if execution["output_limit"]:
            return f"{spec.name}: native output limit exceeded (log {log_path})"
        if not state.inferences:
            return (
                f"{spec.name}: no inference request captured "
                f"(exit {result.returncode}; probes {state.requests!r}; "
                f"log {log_path})"
            )
        captured = select_inference(state.inferences)
        write_capture(
            out_dir,
            spec,
            bin_path,
            version,
            captured,
            origin,
            state.requests,
            cli_exit_code=result.returncode,
            client_status="timed-out-after-capture" if execution["timed_out"] else "exited",
        )
        rel = out_dir / spec.name
        print(
            f"wrote {rel} from {captured_summary(captured)} "
            f"({len(state.inferences)} inference POSTs, cli exit {result.returncode})",
            flush=True,
        )
        return None
    finally:
        shutil.rmtree(home)
        if mitm is not None:
            mitm.shutdown()
        httpd.shutdown()
        httpd.server_close()


def set_pty_window(fd: int, rows: int = 40, cols: int = 120) -> None:
    winsz = struct.pack("HHHH", rows, cols, 0, 0)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, winsz)


async def drain_pty(fd: int, collected: bytearray, seconds: float) -> None:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        remaining = deadline - time.monotonic()
        ready, _, _ = select.select([fd], [], [], max(0.0, min(0.02, remaining)))
        await asyncio.sleep(0)
        if not ready:
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            return
        if not chunk:
            return
        available = max(0, owned_processes.OUTPUT_LIMIT - len(collected))
        collected.extend(chunk[:available])
        if len(chunk) > available:
            raise RuntimeError("native PTY output limit exceeded")


ANSI_CSI = re.compile(rb"\x1b(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~]|].*?(?:\x07|\x1b\\))")


def visible_pty_text(raw: bytes) -> str:
    return ANSI_CSI.sub(b"", raw).decode("utf-8", "replace")


def folded_pty_text(raw: bytes) -> str:
    return re.sub(r"\W+", "", visible_pty_text(raw).lower())


def pty_write(fd: int, data: bytes) -> bool:
    try:
        os.write(fd, data)
        return True
    except OSError:
        return False


def wait_claude_cli_inference(
    state: CaptureState, deadline: float
) -> CapturedRequest | None:
    while time.monotonic() < deadline:
        with state.lock:
            items = list(state.inferences)
        cli: list[CapturedRequest] = []
        for item in items:
            payload = parse_json_body(item.body)
            if isinstance(payload, dict) and claude_capture_kind(item.headers, payload) == "cli":
                cli.append(item)
        if cli:
            return select_inference(cli)
        time.sleep(0.4)
    return None


def require_claude_cli(captured: CapturedRequest) -> str | None:
    payload = parse_json_body(captured.body)
    if not isinstance(payload, dict):
        return "claude: captured body is not JSON"
    kind = claude_capture_kind(captured.headers, payload)
    if kind != "cli":
        return (
            f"claude: captured {kind} entrypoint, not interactive CLI "
            "(cc_entrypoint=cli / official CLI identity)"
        )
    return None


def run_claude_tui(
    spec: VendorSpec,
    out_dir: Path,
    work_root: Path,
    timeout: int,
) -> str | None:
    return asyncio.run(run_claude_tui_owned(spec, out_dir, work_root, timeout))


async def run_claude_tui_owned(
    spec: VendorSpec, out_dir: Path, work_root: Path, timeout: int,
) -> str | None:
    bin_path = which(spec.bin)
    if bin_path is None:
        return f"claude: `{spec.bin}` not on PATH"
    state = CaptureState()
    httpd, state, origin = start_mock(state)
    log_dir = ROOT / "target/native-compare-logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    log_path = log_dir / "claude.tui.log"
    debug_file = log_dir / "claude.debug.log"
    if debug_file.exists():
        debug_file.unlink()
    mitm = start_mitm(state, ROOT / "target/native-compare-certs/claude")
    pid = None
    owned = None
    fd = None
    collected = bytearray()
    home = Path(tempfile.mkdtemp(prefix="claude-home-", dir=work_root))
    try:
        fixture = prepare_fixture(work_root, spec.name, origin)
        command = claude_tui_command(spec, bin_path, fixture, debug_file)
        if "-p" in command or "--print" in command:
            return "claude: refused to spawn a print/SDK argv"
        env = capture_env(origin, home=home, proxy_url=mitm.proxy_url, ca_pem=mitm.bundle_pem)
        trust_keys = seed_claude_workspace_trust(home / "claude-1", fixture)
        env.pop("ANTHROPIC_API_KEY")
        env.pop("ANTHROPIC_BASE_URL")
        env["CLAUDE_CODE_OAUTH_TOKEN"] = "native-compare-synthetic-oauth-token"
        version = await version_text_owned(bin_path, env)
        env["TERM"] = "xterm-256color"
        env["COLUMNS"] = "120"
        env["LINES"] = "40"
        print(
            f"claude trust pre-accepted for {len(trust_keys)} synthetic project key(s)",
            flush=True,
        )
        print(f"capturing claude TUI: {' '.join(command)}", flush=True)
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(fixture)
            os.execvpe(command[0], command, env)
        process = owned_processes.PtyProcess(pid)
        owned = owned_processes.Owned(process)
        set_pty_window(fd)
        startup_deadline = time.monotonic() + timeout
        while time.monotonic() < startup_deadline:
            await drain_pty(fd, collected, 0.25)
            folded = folded_pty_text(bytes(collected))
            # Do not drive the dialog. A keypress can confirm "No, exit".
            # Trust was pre-accepted in the synthetic config; if the dialog
            # is still on screen the fixture key did not match.
            if "noexit" in folded and "itrustthisfolder" in folded:
                return (
                    "claude: workspace trust dialog is open; the synthetic "
                    f"config did not pre-accept {fixture} (log {log_path})"
                )
            if "bypasspermissionson" in folded:
                break
        if process.returncode is not None:
            return (
                "claude: TUI exited before the first user turn "
                f"(exit {process.returncode}; log {log_path})"
            )
        if not pty_write(fd, (PROMPT + "\r").encode("utf-8")):
            return f"claude: PTY closed before sending {PROMPT!r} (log {log_path})"
        # Drain the PTY while observing requests: a full terminal buffer can
        # otherwise stop the native process before its HTTP dispatch.
        captured = None
        deadline = time.monotonic() + timeout
        while captured is None and time.monotonic() < deadline:
            await drain_pty(fd, collected, 0.2)
            captured = wait_claude_cli_inference(state, time.monotonic() + 0.1)
        if captured is None and debug_file.is_file():
            try:
                captured = ingest_claude_debug(debug_file)
            except (OSError, RuntimeError, json.JSONDecodeError):
                captured = None
        if captured is None:
            return (
                "claude: no interactive CLI inference captured "
                f"(probes {state.requests!r}; tui log {log_path})"
            )
        error = require_claude_cli(captured)
        if error:
            return error
        # Persist the synthetic request before stopping the TUI. A client that
        # does not join promptly must not erase an already observed wire shape.
        write_capture(
            out_dir, spec, bin_path, version, captured, origin, state.requests,
            client_status="captured-before-cleanup",
        )
        # Closing the master hangs up the TUI; an undrained PTY could
        # otherwise block its shutdown writes.
        os.close(fd)
        fd = None
        cleanup = await owned_processes.shield_cleanup(owned.close())
        if not cleanup["joined"]:
            return "claude: native PTY cleanup incomplete"
        exit_code = process.returncode
        owned = None
        pid = None
        write_capture(
            out_dir, spec, bin_path, version, captured, origin, state.requests,
            cli_exit_code=exit_code, client_status="stopped-after-capture",
        )
        rel = out_dir / spec.name
        print(
            f"wrote {rel} from {captured_summary(captured)} "
            f"({len(state.inferences)} inference POSTs)",
            flush=True,
        )
        return None
    finally:
        try:
            if owned is not None:
                cleanup = await owned_processes.shield_cleanup(owned.close())
                if not cleanup["joined"]:
                    raise RuntimeError("native PTY cleanup incomplete")
        finally:
            log_path.write_text(redact_text(visible_pty_text(bytes(collected))), encoding="utf-8")
            if fd is not None:
                os.close(fd)
            debug_file.unlink(missing_ok=True)
            shutil.rmtree(home)
            mitm.shutdown()
            httpd.shutdown()
            httpd.server_close()


def main_text_replies(state: CaptureState) -> int:
    with state.lock:
        return sum(
            1 for item in state.inferences
            if item.reply and item.reply["main"] and item.reply["action"] == "text"
        )


async def wait_claude_ready(
    fd: int, collected: bytearray, process: Any, timeout: int,
) -> str | None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        await drain_pty(fd, collected, 0.25)
        folded = folded_pty_text(bytes(collected))
        if "noexit" in folded and "itrustthisfolder" in folded:
            return "claude: workspace trust dialog is open"
        if "bypasspermissionson" in folded:
            return None
        if process.returncode is not None:
            return f"claude: TUI exited before the first user turn (exit {process.returncode})"
    return "claude: TUI did not become ready"


async def claude_turn(
    fd: int, collected: bytearray, state: CaptureState, prompt: str, timeout: int,
) -> str | None:
    before = main_text_replies(state)
    if not pty_write(fd, (prompt + "\r").encode("utf-8")):
        return f"claude: PTY closed before sending {prompt!r}"
    deadline = time.monotonic() + timeout
    while main_text_replies(state) <= before:
        if time.monotonic() >= deadline:
            return f"claude: turn {prompt!r} did not end with a text reply"
        await drain_pty(fd, collected, 0.2)
    # Let side requests and transcript writes for this turn settle.
    await drain_pty(fd, collected, 2.0)
    return None


UUID_LABEL = re.compile(UUID_TEXT.pattern)


def multiturn_facts(items: list[CapturedRequest]) -> list[dict[str, Any]]:
    """Content-free continuation facts per inference request, in order.

    Prompt UUIDs are replaced by first-seen ordinals (`p1`, `p2`, ...) so the
    facts can be compared and shared without the native identifiers.
    """
    labels: dict[str, str] = {}

    def label(match: re.Match[str]) -> str:
        return labels.setdefault(match.group(0), f"p{len(labels) + 1}")

    facts: list[dict[str, Any]] = []
    for item in items:
        payload = parse_json_body(item.body)
        if not isinstance(payload, dict):
            continue
        system = payload.get("system")
        billing = None
        if isinstance(system, list) and system and isinstance(system[0], dict):
            text = str(system[0].get("text", ""))
            if text.startswith("x-anthropic-billing-header:"):
                billing = UUID_LABEL.sub(label, text)
        messages = payload.get("messages") or []
        facts.append({
            "reply": item.reply,
            "model": payload.get("model"),
            "tools": len(payload.get("tools") or []),
            "messages": len(messages),
            "last_user": [b.get("type") for b in last_user_blocks(payload)],
            "diagnostics": payload.get("diagnostics", "absent"),
            "billing": billing,
            "max_tokens": payload.get("max_tokens"),
            "thinking": payload.get("thinking"),
            "context_management": payload.get("context_management"),
            "betas": next((v for k, v in item.headers if k.lower() == "anthropic-beta"), None),
            "user_agent": next((v for k, v in item.headers if k.lower() == "user-agent"), None),
        })
    return facts


async def run_claude_multiturn_owned(
    spec: VendorSpec, out_dir: Path, work_root: Path, timeout: int,
) -> str | None:
    """Three human turns (one with two tool rounds, one retried after 529),
    then a `--continue` reopen and a fourth turn, in one isolated home."""
    bin_path = which(spec.bin)
    if bin_path is None:
        return f"claude: `{spec.bin}` not on PATH"
    state = CaptureState(responder=ClaudeScript())
    httpd, state, origin = start_mock(state)
    log_dir = ROOT / "target/native-compare-logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    debug_file = log_dir / "claude.debug.log"
    mitm = start_mitm(state, ROOT / "target/native-compare-certs/claude")
    home = Path(tempfile.mkdtemp(prefix="claude-home-", dir=work_root))
    try:
        fixture = prepare_fixture(work_root, spec.name, origin)
        env = capture_env(origin, home=home, proxy_url=mitm.proxy_url, ca_pem=mitm.bundle_pem)
        seed_claude_workspace_trust(home / "claude-1", fixture)
        env.pop("ANTHROPIC_API_KEY")
        env.pop("ANTHROPIC_BASE_URL")
        env["CLAUDE_CODE_OAUTH_TOKEN"] = "native-compare-synthetic-oauth-token"
        version = await version_text_owned(bin_path, env)
        env.update({"TERM": "xterm-256color", "COLUMNS": "120", "LINES": "40"})
        base = claude_tui_command(spec, bin_path, fixture, debug_file)
        runs = [(base, CLAUDE_MULTITURN_PROMPTS), (base + ["--continue"], (CLAUDE_RESUME_PROMPT,))]
        for index, (command, prompts) in enumerate(runs):
            if "-p" in command or "--print" in command:
                return "claude: refused to spawn a print/SDK argv"
            print(f"capturing claude TUI run {index + 1}: {' '.join(command)}", flush=True)
            collected = bytearray()
            pid, fd = pty.fork()
            if pid == 0:
                os.chdir(fixture)
                os.execvpe(command[0], command, env)
            process = owned_processes.PtyProcess(pid)
            owned = owned_processes.Owned(process)
            try:
                set_pty_window(fd)
                error = await wait_claude_ready(fd, collected, process, timeout)
                for prompt in prompts if error is None else ():
                    error = await claude_turn(fd, collected, state, prompt, timeout)
                    if error:
                        break
            finally:
                (log_dir / f"claude.multiturn.{index + 1}.log").write_text(
                    redact_text(visible_pty_text(bytes(collected))), encoding="utf-8",
                )
                # Closing the master hangs up the TUI; an undrained PTY could
                # otherwise block its shutdown writes.
                os.close(fd)
                cleanup = await owned_processes.shield_cleanup(owned.close())
            if not cleanup["joined"]:
                return f"claude: native PTY cleanup incomplete ({cleanup})"
            if error:
                return f"{error} (run {index + 1}; probes {len(state.requests)})"
        with state.lock:
            items = list(state.inferences)
        main = [item for item in items if item.reply and item.reply["main"]]
        if not main:
            return "claude: no main-thread inference captured"
        write_capture(
            out_dir, spec, bin_path, version, main[0], origin, state.requests,
            client_status="stopped-after-capture",
        )
        dest = out_dir / spec.name
        snapshots = [
            {"path": item.path, "reply": item.reply, "headers": redact_headers(item.headers),
             "body": redact_json(parse_json_body(item.body))}
            for item in items
        ]
        (dest / "multiturn.json").write_text(json.dumps(snapshots, indent=2) + "\n", encoding="utf-8")
        (dest / "multiturn.facts.json").write_text(
            json.dumps(multiturn_facts(items), indent=2) + "\n", encoding="utf-8",
        )
        print(f"wrote {dest} ({len(items)} inference POSTs, {len(main)} main-thread)", flush=True)
        return None
    finally:
        debug_file.unlink(missing_ok=True)
        shutil.rmtree(home)
        mitm.shutdown()
        httpd.shutdown()
        httpd.server_close()


def run_claude_ingest(
    spec: VendorSpec,
    out_dir: Path,
    from_path: Path | None,
) -> str | None:
    if from_path is None:
        return CLAUDE_TUI_BLOCKER
    source = from_path if from_path.is_absolute() else ROOT / from_path
    if not source.is_file():
        return f"claude: --from {source} is not an existing file"
    try:
        captured = ingest_claude_debug(source)
    except (OSError, RuntimeError, json.JSONDecodeError) as error:
        return f"claude: {error}"
    bin_path = which(spec.bin) or Path("claude")
    version = "external debug import; author version not verified"
    write_capture(
        out_dir,
        spec,
        bin_path,
        version,
        captured,
        f"ingested:{source}",
        [("INGEST", str(source))],
        imported=True,
    )
    rel = out_dir / spec.name
    print(f"wrote {rel} from {captured_summary(captured)} (TUI debug ingest)", flush=True)
    return None


def captured_summary(captured: CapturedRequest) -> str:
    return f"{captured.method} {captured.path}"


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    if argv is None:
        argv = sys.argv[1:]
    if argv[:1] == ["--"]:
        argv = argv[1:]
    parser = argparse.ArgumentParser(
        description="Capture first-turn native CLI requests into target/prompt-captures/native/"
    )
    parser.add_argument(
        "vendors",
        nargs="*",
        choices=VENDORS,
        help="subset to capture (default: all four; Claude uses an interactive TUI)",
    )
    parser.add_argument(
        "--from",
        dest="from_path",
        type=Path,
        default=None,
        help="Claude Code TUI --debug-file to ingest (never claude -p)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=DEFAULT_OUT,
        help="artifact directory (default: target/prompt-captures/native)",
    )
    parser.add_argument(
        "--workspace",
        type=Path,
        default=DEFAULT_WORK,
        help="isolated fixture parent (default: target/native-compare-workspace)",
    )
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument(
        "--claude-bin",
        default=None,
        help="Claude Code executable for this capture only (default: `claude` on PATH)",
    )
    parser.add_argument("--claude-model", default=None, help="Claude model override")
    parser.add_argument("--claude-effort", default=None, help="Claude effort override")
    parser.add_argument(
        "--claude-scenario",
        choices=("single", "multiturn"),
        default="single",
        help="multiturn: three scripted turns (two tool rounds, one 529 retry) and a --continue reopen",
    )
    return parser.parse_args(argv)


def claude_spec(args: argparse.Namespace) -> VendorSpec:
    spec = SPECS["claude"]
    return dataclasses.replace(
        spec,
        bin=args.claude_bin or spec.bin,
        model=args.claude_model or spec.model,
        effort=args.claude_effort or spec.effort,
    )


def resolve_vendors(args: argparse.Namespace) -> list[str]:
    from_path = args.from_path
    if from_path is None:
        env = os.environ.get("CLAUDE_NATIVE_DEBUG", "").strip()
        if env:
            args.from_path = Path(env)
            from_path = args.from_path
    names = list(args.vendors)
    if from_path is not None:
        if names and "claude" not in names:
            raise ValueError("--from / CLAUDE_NATIVE_DEBUG is Claude Code TUI ingest only")
        if "claude" not in names:
            names = ["claude"]
        return names
    return names or list(VENDORS)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        names = resolve_vendors(args)
    except ValueError as error:
        print(error, file=sys.stderr)
        return 2
    out_dir = args.output if args.output.is_absolute() else ROOT / args.output
    work_root = (
        args.workspace if args.workspace.is_absolute() else ROOT / args.workspace
    )
    out_dir.mkdir(parents=True, exist_ok=True)
    work_root.mkdir(parents=True, exist_ok=True)
    errors: list[str] = []
    for name in names:
        error = run_vendor(
            claude_spec(args) if name == "claude" else SPECS[name],
            out_dir,
            work_root,
            args.timeout,
            claude_from=args.from_path,
            claude_scenario=args.claude_scenario,
        )
        if error:
            errors.append(error)
            print(error, file=sys.stderr)
    if errors:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
