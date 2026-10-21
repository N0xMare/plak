#!/usr/bin/env python3
"""Refuse maintainer packaging within 21 days of any compiled review expiry.

Read the provider-owned Rust constants, not a second table of dates. This check
is deliberately absent from the daily gate; its unit tests always supply time.
"""

from __future__ import annotations

import ast
from datetime import datetime, timezone
from pathlib import Path
import re
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
DAY = 86_400
SURFACES = (
    ("Codex OAuth", "openai/src/oauth_pins.rs", "expires_at_unix"),
    ("OpenRouter Chat", "openrouter/src/chat.rs", "OPENROUTER_CHAT_REVIEW_EXPIRES_AT"),
    ("xAI Platform API", "xai/src/platform_binding.rs", "EXPIRES"),
    ("OpenAI Platform API", "openai/src/manifest.rs", "OPENAI_PLATFORM_MODEL_MANIFEST_EXPIRES_AT_UNIX_SECONDS"),
    ("Grok OAuth", "xai/src/oauth_pins.rs", "expires_at_unix"),
    ("Claude OAuth", "anthropic/src/oauth.rs", "CLAUDE_OAUTH_EXPIRES_AT_UNIX"),
    ("Anthropic API", "anthropic/src/config.rs", "ANTHROPIC_PLATFORM_ADMISSION_EXPIRES_AT_SECONDS"),
)


def unsigned_expression(expression: str, constants: dict[str, str], seen: frozenset[str] = frozenset()) -> int:
    """Only unsigned integer literals, named constants, addition, subtraction and multiplication."""
    def value(node: ast.AST) -> int:
        if isinstance(node, ast.Constant) and type(node.value) is int:
            result = node.value
        elif isinstance(node, ast.Name) and node.id not in seen and node.id in constants:
            result = unsigned_expression(constants[node.id], constants, seen | {node.id})
        elif isinstance(node, ast.BinOp) and isinstance(node.op, (ast.Add, ast.Sub, ast.Mult)):
            left, right = value(node.left), value(node.right)
            if isinstance(node.op, ast.Add):
                result = left + right
            elif isinstance(node.op, ast.Sub):
                result = left - right
            else:
                result = left * right
        else:
            raise ValueError(f"unsupported compiled review expression: {expression}")
        if not 0 <= result < 2**64:
            raise ValueError(f"compiled review value is not u64: {expression}")
        return result

    return value(ast.parse(expression, mode="eval").body)


def compiled_expiry(source: str, name: str) -> int:
    source = source.split("#[cfg(test)]", 1)[0]
    constants = dict(re.findall(r"\bconst\s+(\w+)\s*:\s*u64\s*=\s*([^;]+);", source))
    expression = constants.get(name)
    if expression is None:
        fields = re.findall(rf"\b{re.escape(name)}\s*:\s*([^,]+),", source)
        if len(fields) != 1:
            raise ValueError(f"expected exactly one compiled {name}")
        expression = fields[0]
    return unsigned_expression(expression.strip(), constants)


def compiled_reviews(root: Path) -> list[tuple[str, int]]:
    return [
        (surface, compiled_expiry((root / "crates/providers" / path).read_text(), name))
        for surface, path, name in SURFACES
    ]


def expiring(reviews: list[tuple[str, int]], now: int) -> list[tuple[str, int]]:
    return [(surface, expiry) for surface, expiry in reviews if expiry <= now + 21 * DAY]


def main(*, now: int | None = None, note: bool = False) -> int:
    try:
        checked_at = int(time.time()) if now is None else now
        reviews = compiled_reviews(ROOT)
        if note:
            for surface, expiry in reviews:
                print(f"package: {surface} review has {(expiry - checked_at) // DAY} days remaining (informational)")
            return 0
        refused = expiring(reviews, checked_at)
        for surface, expiry in refused:
            date = datetime.fromtimestamp(expiry, timezone.utc).isoformat().replace("+00:00", "Z")
            print(f"publish: {surface} review expires {date}; require more than 21 days remaining", file=sys.stderr)
        return int(bool(refused))
    except (OSError, ValueError, SyntaxError) as error:
        print(f"publish: cannot validate compiled review expiries: {error}", file=sys.stderr)
        return 0 if note else 1


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--note", action="store_true", help="non-failing qualification note")
    args = parser.parse_args()
    sys.exit(main(note=args.note))
