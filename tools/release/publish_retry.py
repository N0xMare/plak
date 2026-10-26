"""Classify Cargo's HTTP 429 diagnostic and choose a bounded retry delay."""

from datetime import datetime, timezone
from email.utils import parsedate_to_datetime
import math
from pathlib import Path
import re
import sys

MAX_RETRY_DELAY_SECONDS = 900


def reported_delay(value: str, now: datetime) -> int | None:
    if value.isascii() and value.isdecimal():
        return int(value)
    try:
        deadline = parsedate_to_datetime(value)
        if deadline.tzinfo is not None:
            return max(0, math.ceil((deadline - now).total_seconds()))
    except (ValueError, OverflowError, TypeError):
        pass
    return None


def retry_delay(diagnostic: str, retry: int, now: datetime | None = None) -> int | None:
    # Match HTTP status diagnostics, not a coincidental crate version or URL.
    statuses = re.findall(
        r"(?:HTTP/\S+[ \t]+|\bstatus(?:[ \t]+code)?(?:[ \t]*[:=][ \t]*|[ \t]+))([1-5]\d{2})\b",
        diagnostic, re.IGNORECASE)
    if not statuses or any(status != "429" for status in statuses):
        return None
    header = re.search(r"^\s*(?:[<>]\s*)?retry-after\s*:\s*(.+?)\s*$", diagnostic,
                       re.IGNORECASE | re.MULTILINE)
    # crates.io puts its new-crate rate-limit deadline in the error body;
    # Cargo prints that body but does not necessarily print Retry-After.
    body = re.search(r"Please try again after ([A-Za-z]{3},\s+\d{1,2}\s+[A-Za-z]{3}\s+\d{4}\s+\d{2}:\d{2}:\d{2}\s+GMT)",
                     diagnostic, re.IGNORECASE)
    for hint in (header, body):
        if hint:
            delay = reported_delay(hint[1], now or datetime.now(timezone.utc))
            if delay is not None:
                if delay > MAX_RETRY_DELAY_SECONDS:
                    raise ValueError(f"rate-limit delay {delay}s exceeds the {MAX_RETRY_DELAY_SECONDS}s maximum; retry publication later")
                return delay
    return min(5 * 2 ** (retry - 1), 30)


if __name__ == "__main__":
    try:
        delay = retry_delay(Path(sys.argv[1]).read_text(errors="replace"), int(sys.argv[2]))
    except ValueError as error:
        print(f"publish: {error}", file=sys.stderr)
        sys.exit(2)
    if delay is None:
        sys.exit(1)
    print(delay)
