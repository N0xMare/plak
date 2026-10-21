"""Symmetric attribution; Harbor's transcript regex is not provider evidence."""

from __future__ import annotations


def failure_class(termination: str, exception: str | None = None) -> str:
    if termination.startswith(("agent_init_failed", "result_unparseable", "infra:")):
        return "infra"
    if termination == "turn_failed:credential":
        return "infra"
    if exception in {
        "CredentialError", "CancelledError", "RuntimeError", "PreflightError", "VendorArtifactError",
        "AgentSetupTimeoutError", "EnvironmentBuildError", "EnvironmentStartError",
        "VerifierTimeoutError", "VerifierOutputParseError", "BudgetWatchStop",
    }:
        return "infra"
    if termination in {"completed", "agent_exit_nonzero"} or termination.startswith("turn_failed:"):
        return "agent"
    if exception in {"AgentTimeoutError", "NonZeroAgentExitCodeError"}:
        return "agent"
    if "DaemonLost" in termination:
        return "infra"
    return "unknown"
