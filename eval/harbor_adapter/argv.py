"""Record the actual CLI invocation, excluding the task instruction and shell IO."""
import json
from pathlib import Path
import shlex


def native_argv(command: str, instruction: str, harness: str) -> list[str] | None:
    cli = {"codex": "codex", "grok-build": "grok", "pi": "pi", "opencode": "opencode"}.get(harness)
    if not cli:
        return None
    lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|()<>")
    lexer.whitespace_split = True
    try:
        tokens = list(lexer)
    except ValueError:
        return None
    for index, token in enumerate(tokens):
        if token != cli or index + 1 == len(tokens):
            continue
        expected = {"codex": "exec", "grok": "--no-auto-update", "pi": "--print", "opencode": "run"}[cli]
        if tokens[index + 1] != expected and not (cli == "opencode" and tokens[index + 1].startswith("--model=")):
            continue
        if cli == "pi" and instruction not in tokens[index:]:
            return None
        prompt_index = max((i for i, value in enumerate(tokens) if value == instruction), default=-1)
        result = []
        skip_prompt = False
        for offset in range(index, len(tokens)):
            value = tokens[offset]
            if skip_prompt:
                skip_prompt = False
                continue
            if cli == "pi" and offset == prompt_index:
                continue
            if value in (";", "&&", "||", "|", "&", ">", ">>", "<", "2>"):
                break
            if value.isdigit() and offset + 1 < len(tokens) and ">" in tokens[offset + 1]:
                break
            if value == "--":
                result.append(value)
                break
            if cli == "grok" and value == "--single":
                result.append(value)
                skip_prompt = True
                continue
            result.append(value)
        return result
    return None


def write_argv(directory: Path, argv: list[str]) -> None:
    (directory / "argv.json").write_text(json.dumps({"argv_without_prompt": argv}, indent=2) + "\n")
