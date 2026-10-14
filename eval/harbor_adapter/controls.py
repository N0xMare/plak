"""Matching model/effort so bakeoff arms differ only by harness.

Plak `--effort` is the source of truth. Vendor Harbor `--ak` values are the
documented equivalent on that CLI. There is no silent default: a ladder
rung the vendor CLI cannot name fails closed.

Pi and OpenCode cells use host OAuth `auth.json` (same native-home rule as
Codex/Grok). They must not inherit host interactive config via Harbor
`model_api` / `opencode_config`.
"""

from __future__ import annotations

from harbor_adapter.vendor_pins import CLI_PINS

PLAQUOUS_EFFORTS = ("low", "medium", "high", "xhigh", "max", "ultra")
SMOKE_EFFORT = "low"

# Harbor CodexOptions / GrokBuildOptions Literal.
CODEX_GROK_REASONING = (
    "none",
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
)

# Harbor PiOptions plus the CLI's documented `max`.
PI_THINKING = ("off", "minimal", "low", "medium", "high", "xhigh", "max")

# OpenCode `--variant` on gpt-5.6-luna / grok-4.6 (models.dev reasoningEffort).
OPENCODE_VARIANT = ("none", "minimal", "low", "medium", "high", "xhigh")

AGENT_CLI = {
    "codex": "codex",
    "grok-build": "grok",
    "pi": "pi",
    "opencode": "opencode",
}

# Host overlays that would make a cell not stock Harbor behavior.
NON_STOCK_KWARGS = {
    "codex": ("config",),
    "grok": ("grok_config",),
    "pi": ("model_api",),
    "opencode": ("opencode_config",),
}


class ControlError(ValueError):
    pass


def vendor_effort_ak(agent: str, effort: str) -> tuple[str, ...]:
    """Harbor `--ak` strings that pin the same effort Plak named."""
    if effort == "none":
        return ()
    if effort not in PLAQUOUS_EFFORTS:
        raise ControlError(f"unknown effort {effort!r}")
    if agent == "codex":
        if effort not in CODEX_GROK_REASONING:
            raise ControlError(f"Codex CLI has no reasoning_effort={effort}")
        return (f"reasoning_effort={effort}",)
    if agent == "grok-build":
        if effort not in CODEX_GROK_REASONING:
            raise ControlError(f"Grok Build has no reasoning_effort={effort}")
        return (f"reasoning_effort={effort}",)
    if agent == "pi":
        if effort not in PI_THINKING:
            raise ControlError(f"Pi --thinking has no {effort}")
        return (f"thinking={effort}",)
    if agent == "opencode":
        if effort not in OPENCODE_VARIANT:
            raise ControlError(
                f"OpenCode --variant has no documented {effort} on bakeoff models"
            )
        return (f"variant={effort}",)
    raise ControlError(f"unknown vendor agent {agent!r}")


def harbor_ak_flags(agent: str, *, effort: str, version: str) -> tuple[str, ...]:
    """`--ak` values for a frozen vendor arm: CLI pin plus matching effort."""
    if agent not in AGENT_CLI:
        raise ControlError(f"unknown vendor agent {agent!r}")
    pinned = CLI_PINS[AGENT_CLI[agent]]
    if version != pinned:
        raise ControlError(f"{agent} version {version!r} is not pin {pinned}")
    return (f"version={version}",) + vendor_effort_ak(agent, effort)
