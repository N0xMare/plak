"""Pure argv for Full-cell `pmux run` inside a Harbor task.

This is the Claude Code native-home wrap. `pmux ask` is not constructed here.
"""

from __future__ import annotations

from harbor_adapter.tuples import (
    QUALIFIED,
    REMOTE_PMUX,
    REMOTE_PMUX_RUNTIME,
    REMOTE_WORKSPACE,
)


class PmuxArgumentError(ValueError):
    pass


def require_claude_tuple(model: str, effort: str) -> None:
    if ("claude", model, effort) not in QUALIFIED:
        raise PmuxArgumentError(
            f"tuple claude/{model}/{effort} is not a §5.1 bakeoff cell"
        )


def pmux_run_arguments(
    *,
    model: str,
    effort: str,
    prompt: str | None = None,
    prompt_file: str | None = None,
    cwd: str = REMOTE_WORKSPACE,
) -> list[str]:
    require_claude_tuple(model, effort)
    file_path = (prompt_file or "").strip()
    prompt_text = prompt if prompt is not None else ""
    if file_path and prompt_text.strip():
        raise PmuxArgumentError("prompt and prompt_file are mutually exclusive")
    if file_path:
        if not file_path.startswith("/"):
            raise PmuxArgumentError(
                f"--prompt-file must be absolute; got {prompt_file!r}"
            )
    elif not prompt_text.strip():
        raise PmuxArgumentError("prompt is empty")
    if not cwd.startswith("/"):
        raise PmuxArgumentError(f"--cwd must be absolute; got {cwd!r}")
    pool = f"{REMOTE_PMUX_RUNTIME}/pool"
    cwd_norm = cwd.rstrip("/") or "/"
    if cwd_norm == pool or cwd_norm.startswith(f"{pool}/"):
        raise PmuxArgumentError(
            f"--cwd {cwd!r} is under the pmuxd pool parent {pool}"
        )
    argv = [
        REMOTE_PMUX,
        "--output",
        "json",
        "run",
        "--model",
        model,
        "--cwd",
        cwd,
        "--permission-mode",
        "dangerously-skip-permissions",
    ]
    if effort != "none":
        argv.extend(["--effort", effort])
    if file_path:
        argv.extend(["--prompt-file", file_path])
    else:
        argv.extend(["--", prompt_text])
    return argv
