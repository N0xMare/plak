"""Reduce native evidence to structural config; never retain headers or content."""

from __future__ import annotations

import hashlib
import json
import copy
import re
from pathlib import Path
from typing import Any


def _digest(value: Any) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def _scalar(value: Any) -> Any:
    return value if value is None or type(value) in (str, int, float, bool) else None


def _object(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def claude_instruction_projection(instructions: dict[str, Any]) -> str | None:
    """Project only billing nonces, never change the recorded wire digest.

    The first-party fields come from anthropic/src/cch.rs:41, :80 and :142.
    Unknown fields and all non-billing instruction content remain pinned.
    """
    projected = copy.deepcopy(instructions)
    system = projected.get("system")
    if isinstance(system, str):
        text = system
    elif (isinstance(system, list) and system and isinstance(system[0], dict)
          and system[0].get("type") == "text" and isinstance(system[0].get("text"), str)):
        text = system[0]["text"]
    else:
        return None
    lines = text.splitlines(keepends=True)
    if not lines or not lines[0].startswith("x-anthropic-billing-header: "):
        return None
    line = lines[0]
    boundary = r"(?<![A-Za-z0-9_])"
    end = r"(?=;|\s|$)"
    line = re.sub(boundary + r"cc_prompt_id=[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}" + end,
                  "cc_prompt_id=<request-uuid>", line)
    line = re.sub(boundary + r"cch=[0-9a-f]{5}" + end, "cch=<body-attestation>", line)
    line = re.sub(boundary + r"cc_version=([0-9]+\.[0-9]+\.[0-9]+)\.[0-9a-f]{3}" + end,
                  r"cc_version=\1.<task-suffix>", line)
    text = line + "".join(lines[1:])
    if isinstance(system, str):
        projected["system"] = text
    else:
        system[0]["text"] = text
    return _digest(projected)


def first_request_matches(actual: dict, expected: dict, account: str) -> bool:
    if account != "claude":
        return actual == expected
    key = "claude_instruction_projection_sha256"
    if not all(re.fullmatch(r"[0-9a-f]{64}", str(value.get(key, ""))) for value in (actual, expected)):
        return actual == expected
    return ({k: v for k, v in actual.items() if k != "instructions_sha256"} ==
            {k: v for k, v in expected.items() if k != "instructions_sha256"})


def request_config(request: dict[str, Any]) -> dict[str, Any]:
    """Only call on a complete request: null then means absent, not unobserved."""
    reasoning = _object(request.get("reasoning"))
    output = _object(request.get("output_config"))
    thinking = _object(request.get("thinking"))
    tools = []
    def collect_tools(entries, prefix=""):
        for tool in entries:
            if not isinstance(tool, dict):
                continue
            function = _object(tool.get("function"))
            name = tool.get("name") or function.get("name") or tool.get("type")
            if not isinstance(name, str):
                continue
            if isinstance(tool.get("tools"), list):
                collect_tools(tool["tools"], prefix + name + ".")
            else:
                tools.append(prefix + name)
    collect_tools(request.get("tools", []))
    instructions = {key: request[key] for key in ("instructions", "system") if key in request}
    instructions["messages"] = [message for key in ("messages", "input")
                                for message in (request.get(key) if isinstance(request.get(key), list) else [])
                                if isinstance(message, dict) and message.get("role") in ("system", "developer")]
    config = {
        "model": _scalar(request.get("model")),
        "effort": _scalar(reasoning.get("effort", output.get("effort", request.get("reasoning_effort")))),
        "thinking": _scalar(thinking.get("type", reasoning.get("enabled"))),
        "thinking_budget": _scalar(thinking.get("budget_tokens")),
        "max_output": _scalar(request.get("max_output_tokens", request.get("max_completion_tokens", request.get("max_tokens")))),
        "temperature": _scalar(request.get("temperature")),
        "requested_service_tier": _scalar(request.get("service_tier")),
        "tool_names": sorted(tools), "tool_schema_sha256": _digest(request.get("tools", [])),
        "instructions_sha256": _digest(instructions),
    }
    projection = claude_instruction_projection(instructions)
    if projection is not None:
        config["claude_instruction_projection_sha256"] = projection
    return config


def response_config(response: dict[str, Any]) -> dict[str, Any]:
    response = _object(response.get("response", response.get("message", response)))
    return {key: _scalar(response.get(source)) for key, source in (
        ("served_model", "model"), ("system_fingerprint", "system_fingerprint"),
        ("served_service_tier", "service_tier"), ("upstream_provider", "provider"),
    )}


def json_records(text: str) -> list[dict[str, Any]]:
    records = []
    try:
        value = json.loads(text)
        if isinstance(value, dict):
            return [value]
    except ValueError:
        pass
    for line in text.splitlines():
        if line.startswith("data:"):
            line = line[5:].lstrip()
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            records.append(value)
    return records


def extract_native(harness: str, records: list[dict[str, Any]]) -> dict[str, Any]:
    """Native declarations are useful, but are not wire calibration evidence."""
    values = {}
    for record in records:
        kind = record.get("type")
        payload = _object(record.get("payload"))
        if harness == "codex" and kind == "turn_context" and not values:
            values = {"model": _scalar(payload.get("model")), "effort": _scalar(payload.get("effort")),
                      "summary": _scalar(payload.get("summary"))}
        elif harness == "pi":
            if kind == "model_change" and "model" not in values:
                values["model"] = _scalar(record.get("modelId"))
                values["provider"] = _scalar(record.get("provider"))
            elif kind == "thinking_level_change" and "effort" not in values:
                values["effort"] = _scalar(record.get("thinkingLevel"))
        # Do not let a later configuration change stand in for the first request.
        if kind in ("message", "message_start") and harness == "pi":
            break
    return {"schemaVersion": 1, "source": "native_declaration", "wire_verified": False,
            "first_request": values or None, "requests": []}


def extract_exchanges(directory: Path) -> dict[str, Any]:
    requests = []
    incomplete = False
    for path in sorted(directory.rglob("*.request")):
        records = json_records(path.read_text(encoding="utf-8"))
        if len(records) != 1 or not records[0].get("model"):
            continue
        request = request_config(records[0])
        body = path.with_suffix(".body")
        responses = json_records(body.read_text(encoding="utf-8")) if body.is_file() else []
        served = {}
        info = path.with_suffix(".info")
        if info.is_file():
            metadata = info.read_text()
            status = re.search(r"^status=Some\((\d+)\)$", metadata, re.MULTILINE)
            served["status_code"] = int(status[1]) if status else None
            incomplete |= "incomplete=true" in metadata
            served["transport_error"] = bool(re.search(r"^terminal=Failed\(", metadata, re.MULTILINE))
            served["client_cancelled"] = bool(re.search(r"^terminal=(ClosedEarly|Abandoned)$", metadata, re.MULTILINE))
        for response in responses:
            served.update({k: v for k, v in response_config(response).items() if v is not None})
        if served.get("served_model") or served.get("system_fingerprint"):
            for key in ("served_model", "system_fingerprint", "served_service_tier", "upstream_provider"):
                served.setdefault(key, None)
        requests.append({"request": request, "response": served,
                         "request_sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
    incomplete |= any(directory.rglob("retention.info")) or any(directory.rglob("incomplete.log"))
    return {"schemaVersion": 1, "source": "exchange", "wire_verified": bool(requests) and not incomplete,
            "first_request": requests[0]["request"] if requests else None, "requests": requests}



def retry_telemetry(observed: dict[str, Any]) -> dict[str, Any]:
    """Symmetric wire lower bound; never mistake an unobserved retry for zero."""
    previous = {}
    causes = {}
    for record in observed.get("requests", []):
        digest = record.get("request_sha256")
        if not digest:
            continue
        before = previous.get(digest, {})
        status = before.get("status_code")
        cause = ("transport" if before.get("transport_error") else
                 "http_" + str(status) if isinstance(status, int) and status >= 400 else None)
        if cause:
            causes[cause] = causes.get(cause, 0) + 1
        previous[digest] = record.get("response", {})
    return {"observed_retries": sum(causes.values()), "causes": causes,
            "complete": False, "basis": "identical wire body after HTTP or transport failure; lower bound"}

def archived_config(agent: Path, harness: str) -> dict[str, Any]:
    reduced = agent / "observed-config.json"
    if reduced.is_file():
        try:
            value = json.loads(reduced.read_text())
            if (isinstance(value, dict) and value.get("schemaVersion") == 1
                    and isinstance(value.get("first_request"), (dict, type(None)))
                    and isinstance(value.get("requests"), list)
                    and all(isinstance(r, dict) for r in value["requests"])):
                return value
        except (OSError, ValueError):
            pass
    records = []
    for path in sorted((agent / "sessions").rglob("*.jsonl")):
        records.extend(json_records(path.read_text(encoding="utf-8", errors="replace")))
    return extract_native(harness, records)


def resolved_composition(stderr: str) -> str | None:
    values = set()
    for line in stderr.splitlines():
        if line.startswith("plak: composition Direct:"):
            values.add("direct")
        elif line.startswith("plak: composition Code Mode (native-ptc):"):
            values.add("native-ptc")
    return values.pop() if len(values) == 1 else None


if __name__ == "__main__":
    import sys
    # In-cell reduction. Raw exchange captures never go under /logs.
    Path(sys.argv[2]).write_text(json.dumps(extract_exchanges(Path(sys.argv[1])), indent=2) + "\n")
