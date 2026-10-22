"""Eval setup retry and platform-specific scoring predicates."""

from __future__ import annotations

_COMPOSE_RETRY_MARKERS = (
    "context deadline exceeded",
    "deadline exceeded",
    "tls handshake timeout",
    "i/o timeout",
    "connection reset by peer",
    "toomanyrequests",
    "429 too many requests",
)


def should_retry_compose(text: str) -> bool:
    """Retry Harbor compose start once on a live Docker Hub handshake."""
    lower = text.lower()
    return any(marker in lower for marker in _COMPOSE_RETRY_MARKERS)
