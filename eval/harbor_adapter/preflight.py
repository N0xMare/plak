"""Bounded, credential-free executable probes in the actual Harbor cell."""

from __future__ import annotations

import asyncio
import shlex
from typing import Any


class PreflightError(RuntimeError):
    pass


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
