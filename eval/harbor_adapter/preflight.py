"""Bounded, credential-free executable probes in the actual Harbor cell."""

from __future__ import annotations

import asyncio
import shlex
from pathlib import Path
from typing import Any


class PreflightError(RuntimeError):
    pass


def task_budgets(environment: Any) -> tuple[float, float]:
    """Read the same pinned task timeout source Harbor uses (no CLI override)."""
    import tomllib
    try:
        task = tomllib.loads((Path(environment.environment_dir).parent / "task.toml").read_text())
        agent = float(task.get("agent", {}).get("timeout_sec", 600))
        verifier = float(task.get("verifier", {}).get("timeout_sec", 600))
    except (OSError, ValueError, TypeError, AttributeError):
        raise PreflightError("task timeout source unavailable") from None
    if not (10 < agent < 86400 and 0 < verifier < 86400):
        raise PreflightError("task timeout outside bounded eval policy")
    return agent, verifier


async def probe_executable(
    environment: Any, path: str, *, version_argument: str | None = "version"
) -> None:
    """Launch an uploaded executable in the task image before paid dispatch.

    The shell timeout kills a hung child. The outer timeout bounds a broken
    environment transport too. Output is intentionally omitted from errors:
    task images and vendor CLIs can print environment data on failure.
    """
    invocation = f"timeout -k 2s 15s {shlex.quote(path)}"
    if version_argument is None:
        invocation = f"{invocation} </dev/null"
    else:
        invocation = f"{invocation} {shlex.quote(version_argument)}"
    command = f"command -v timeout >/dev/null && {invocation}"
    try:
        result = await asyncio.wait_for(environment.exec(command=command), 20)
    except asyncio.CancelledError:
        raise
    except Exception:
        raise PreflightError(f"{path} cell startability probe failed") from None
    if result.return_code != 0:
        raise PreflightError(
            f"{path} cell startability probe returned {result.return_code}"
        )
    if version_argument is not None and not (result.stdout or "").startswith("plak "):
        raise PreflightError(f"{path} cell version probe returned no Plak version")
