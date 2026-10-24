#!/usr/bin/env python3
"""Refuse maintainer packaging unless every compiled review keeps more than a
third of its window.

Subscription profiles carry a hard expiry; documented API facts carry a review
due date that only warns. Both end a window that starts at the review, and
publication requires more than a third of it remaining on every surface.

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
# Shared window constants every surface may name.
WINDOW_CONSTANTS = "crates/common/auth/src/oauth_pins.rs"
HARD_EXPIRY = "hard expiry"
REVIEW_DUE = "review due"
SURFACES = (
    ("Codex OAuth", "openai/src/oauth_pins.rs", "reviewed_at_unix", "expires_at_unix", HARD_EXPIRY),
    ("OpenRouter Chat", "openrouter/src/chat.rs", "OPENROUTER_CHAT_REVIEWED_AT", "OPENROUTER_CHAT_REVIEW_DUE_AT", REVIEW_DUE),
    ("xAI Platform API", "xai/src/platform_binding.rs", "REVIEWED", "REVIEW_DUE", REVIEW_DUE),
    ("OpenAI Platform API", "openai/src/manifest.rs", "OPENAI_PLATFORM_MODEL_MANIFEST_REVIEWED_AT_UNIX_SECONDS",
     "OPENAI_PLATFORM_MODEL_MANIFEST_REVIEW_DUE_AT_UNIX_SECONDS", REVIEW_DUE),
    ("Grok OAuth", "xai/src/oauth_pins.rs", "reviewed_at_unix", "expires_at_unix", HARD_EXPIRY),
    ("Claude OAuth", "anthropic/src/oauth.rs", "CLAUDE_OAUTH_REVIEWED_AT_UNIX", "CLAUDE_OAUTH_EXPIRES_AT_UNIX", HARD_EXPIRY),
    ("Anthropic API", "anthropic/src/config.rs", "ANTHROPIC_PLATFORM_ADMISSION_REVIEWED_AT_SECONDS",
     "ANTHROPIC_PLATFORM_ADMISSION_REVIEW_DUE_AT_SECONDS", REVIEW_DUE),
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


def production_constants(source: str) -> dict[str, str]:
    source = source.split("#[cfg(test)]", 1)[0]
    return dict(re.findall(r"\bconst\s+(\w+)\s*:\s*u64\s*=\s*([^;]+);", source))


def compiled_value(source: str, name: str, shared: dict[str, str] | None = None) -> int:
    """Evaluate a named u64 constant or a single struct field of the source."""
    own = production_constants(source)
    constants = {**(shared or {}), **own}
    expression = own.get(name)
    if expression is None:
        fields = re.findall(rf"\b{re.escape(name)}\s*:\s*([^,]+),", source.split("#[cfg(test)]", 1)[0])
        if len(fields) != 1:
            raise ValueError(f"expected exactly one compiled {name}")
        expression = fields[0]
    # A Rust path names the constant its last segment names.
    return unsigned_expression(re.sub(r"\b(?:\w+::)+(\w+)\b", r"\1", expression.strip()), constants)


def compiled_reviews(root: Path) -> list[tuple[str, str, int, int]]:
    """(surface, class, reviewed_at, deadline) for every compiled surface."""
    shared = production_constants((root / WINDOW_CONSTANTS).read_text())
    reviews = []
    for surface, path, reviewed_name, deadline_name, review_class in SURFACES:
        source = (root / "crates/providers" / path).read_text()
        reviewed = compiled_value(source, reviewed_name, shared)
        deadline = compiled_value(source, deadline_name, shared)
        if deadline <= reviewed:
            raise ValueError(f"{surface} deadline does not follow its review")
        reviews.append((surface, review_class, reviewed, deadline))
    return reviews


def floor_seconds(reviewed: int, deadline: int) -> int:
    return (deadline - reviewed) // 3


def refused(reviews: list[tuple[str, str, int, int]], now: int) -> list[tuple[str, str, int, int]]:
    """Surfaces with a third of their window or less remaining."""
    return [review for review in reviews if review[3] - now <= floor_seconds(review[2], review[3])]


def refusal(review: tuple[str, str, int, int]) -> str:
    surface, review_class, reviewed, deadline = review
    date = datetime.fromtimestamp(deadline, timezone.utc).isoformat().replace("+00:00", "Z")
    event = "review expires" if review_class == HARD_EXPIRY else "review is due"
    floor = floor_seconds(reviewed, deadline) // DAY
    window = (deadline - reviewed) // DAY
    return (f"publish: {surface} {event} {date}; require more than {floor} days "
            f"(a third of the {window}-day window) remaining")


def main(*, now: int | None = None, note: bool = False) -> int:
    try:
        checked_at = int(time.time()) if now is None else now
        reviews = compiled_reviews(ROOT)
        if note:
            for surface, _, reviewed, deadline in reviews:
                print(f"package: {surface} review has {(deadline - checked_at) // DAY} days remaining; "
                      f"publication requires more than {floor_seconds(reviewed, deadline) // DAY} (informational)")
            return 0
        failed = refused(reviews, checked_at)
        for review in failed:
            print(refusal(review), file=sys.stderr)
        return int(bool(failed))
    except (OSError, ValueError, SyntaxError) as error:
        print(f"publish: cannot validate compiled review windows: {error}", file=sys.stderr)
        return 0 if note else 1


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--note", action="store_true", help="non-failing qualification note")
    args = parser.parse_args()
    sys.exit(main(note=args.note))
