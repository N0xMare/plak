"""Pure argv/env construction for `plak run` inside a Harbor task."""

from __future__ import annotations

from pathlib import Path

from harbor_adapter.tuples import (
    ACCOUNTS,
    DEFAULT_COMPOSITION,
    PROMPT_VARIANT_ENV,
    PROMPT_VARIANTS,
    QUALIFIED,
    REMOTE_BINARY,
    REMOTE_HOME,
    REMOTE_JS_HOST,
    REMOTE_WORKSPACE,
    REMOTE_XDG,
    STRIP_ENV,
)


class ArgumentError(ValueError):
    pass


def require_tuple(account: str, model: str, effort: str) -> None:
    if account not in ACCOUNTS:
        raise ArgumentError(f"unsupported eval account {account!r}")
    if (account, model, effort) not in QUALIFIED:
        raise ArgumentError(
            f"tuple {account}/{model}/{effort} is not a §5.1 bakeoff cell"
        )


def require_js_host(path: Path, composition: str) -> None:
    if composition in {"native-ptc", "js-ptc"} and not path.is_file():
        raise ArgumentError(
            f"missing JavaScript host {path}; run `just build-harbor-agent`"
        )


def sanitize_extra_env(extra_env: dict[str, str] | None) -> dict[str, str]:
    cleaned: dict[str, str] = {}
    for key, value in (extra_env or {}).items():
        if key in STRIP_ENV or key.upper() in STRIP_ENV:
            continue
        cleaned[key] = value
    return cleaned


def require_prompt_variant(variant: str | None) -> str | None:
    if variant is None:
        return None
    if variant not in PROMPT_VARIANTS:
        raise ArgumentError(
            f"unknown prompt variant {variant!r}; expected one of {PROMPT_VARIANTS}"
        )
    return variant


def run_env(
    *,
    extra_env: dict[str, str] | None = None,
    prompt_variant: str | None = None,
) -> dict[str, str]:
    env = sanitize_extra_env(extra_env)
    env["PLAK_HOME"] = REMOTE_HOME
    env["XDG_STATE_HOME"] = REMOTE_XDG
    env["PLAK_JS_HOST"] = REMOTE_JS_HOST
    env["PLAK_LOG"] = "1"
    # The ablation switch reaches the plak binary only. Vendor arms run their
    # own prompts and never see it.
    variant = require_prompt_variant(prompt_variant)
    if variant is not None:
        env[PROMPT_VARIANT_ENV] = variant
    return env


def run_arguments(
    *,
    account: str,
    model: str,
    effort: str,
    prompt: str,
    composition: str | None = None,
) -> list[str]:
    require_tuple(account, model, effort)
    if not prompt.strip():
        raise ArgumentError("prompt is empty")
    resolved = composition or DEFAULT_COMPOSITION[account]
    argv = [
        REMOTE_BINARY,
        "run",
        "--account",
        account,
        "--model",
        model,
    ]
    # OpenRouter Chat models that attest no effort enumeration refuse --effort.
    if effort != "none":
        argv.extend(["--effort", effort])
    argv.extend(
        [
            "--composition",
            resolved,
            "--print-session-id",
            "--",
            prompt,
        ]
    )
    return argv


def host_binary_paths(repo_root: Path) -> tuple[Path, Path]:
    installed = repo_root / ".plak" / "installed"
    return installed / "plak", installed / "plakous-js-host"


def workspace_cwd() -> str:
    return REMOTE_WORKSPACE
