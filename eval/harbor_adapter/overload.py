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


# A daemon can be lost for reasons that have nothing to do with Rosetta, so
# scoring through requires a daemon-loss marker AND Rosetta-shaped evidence.
_DAEMON_LOSS_MARKERS = (
    "daemonlost",
    "rmuxunavailable",
)

_ROSETTA_EVIDENCE_MARKERS = (
    "rosetta",
    "failed to create temporary file",
    "threadcontextfcntl",
    "create_tempfile",
)


def should_score_through_rosetta(
    stderr: str,
    *,
    has_transcripts: bool,
    cell_arch: str,
    host_system: str,
) -> bool:
    """Darwin Rosetta killed pmuxd; verifier should still score the tree.

    Zero deaths cannot be promised on Apple Silicon when the cell is an
    emulated linux/amd64 one, and Harbor must not record that crash as the
    trial result when JSONL exists. The exception is scoped to exactly that
    case: an amd64 cell on a Darwin host, with a daemon-loss marker AND the
    Rosetta tempfile signature in stderr. A native linux/arm64 cell runs no
    Rosetta, so a daemon loss there is an ordinary harness failure and is
    reported as one.
    """
    if not has_transcripts:
        return False
    if cell_arch != "amd64" or host_system != "Darwin":
        return False
    lower = stderr.lower()
    daemon_lost = any(marker in lower for marker in _DAEMON_LOSS_MARKERS)
    rosetta = any(marker in lower for marker in _ROSETTA_EVIDENCE_MARKERS)
    return daemon_lost and rosetta
