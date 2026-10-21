"""Reduce native evidence to structural config; never retain headers or content."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any


def _digest(value: Any) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def _scalar(value: Any) -> Any:
    return value if value is None or type(value) in (str, int, float, bool) else None


def _object(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def request_config(request: dict[str, Any]) -> dict[str, Any]:
    """Only call on a complete request: null then means absent, not unobserved."""
    reasoning = _object(request.get("reasoning"))
    output = _object(request.get("output_config"))
    thinking = _object(request.get("thinking"))
    tools = []
    for tool in request.get("tools", []):
        if isinstance(tool, dict):
            function = _object(tool.get("function"))
            name = tool.get("name") or function.get("name") or tool.get("type")
            if isinstance(name, str):
                tools.append(name)
    instructions = request.get("instructions", request.get("system"))
    if instructions is None:
        instructions = [message for message in request.get("messages", [])
                        if isinstance(message, dict) and message.get("role") in ("system", "developer")]
    return {
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
        line = line.removeprefix("data: ")
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
    for path in sorted(directory.rglob("*.request")):
        records = json_records(path.read_text(encoding="utf-8"))
        if len(records) != 1 or not records[0].get("model"):
            continue
        request = request_config(records[0])
        body = path.with_suffix(".body")
        responses = json_records(body.read_text(encoding="utf-8")) if body.is_file() else []
        served = {}
        for response in responses:
            served.update({k: v for k, v in response_config(response).items() if v is not None})
        if served.get("served_model") or served.get("system_fingerprint"):
            for key in ("served_model", "system_fingerprint", "served_service_tier", "upstream_provider"):
                served.setdefault(key, None)
        requests.append({"request": request, "response": served,
                         "request_sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
    return {"schemaVersion": 1, "source": "exchange", "wire_verified": bool(requests),
            "first_request": requests[0]["request"] if requests else None, "requests": requests}


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
