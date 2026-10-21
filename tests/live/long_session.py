#!/usr/bin/env python3
"""Opt-in long-session scenarios: compaction, overflow, kill-reopen, child compaction.

Usage: long_session.py [route] (--only NAME[,NAME...] | --all) [--binary PATH]

Not part of the default smoke twelve or the hour soak. `just smoke <route>
--only <names>` and `just soak <route> --long-session --only <names>` select
the same scenarios. `--all` runs exactly the four scenarios and is mutually
exclusive with `--only`. A bare launch is refused. The pre-release bar's
full-cell command is `just soak <route> --long-session --all` (spec 05 §3.4).
Paid; never a gate.

Spec 07 has no context-window or compaction-trigger key. A private home pins
`[compaction] enabled = true` and `[turn] max_model_rounds`. Tool-result
growth is sized to the route's catalog window (spec 01 §6.7, user guide).
`PLAK_LIVE_GROWTH_PAGES` overrides that default.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import signal
import subprocess
import sys
import time
import traceback
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import live  # noqa: E402
import crash_triage  # noqa: E402
from live import HARNESS, MODEL, PASS, ScenarioFailure  # noqa: E402

# Product read_file page (`READ_RESPONSE_BYTES` in tool-files). One full page
# is 64k tokens at the bytes/4 observation measure.
READ_PAGE_BYTES = 262_144
PAGE_TOKENS = READ_PAGE_BYTES // 4
# Spec 01 §10.3: the generic rule subtracts max(ceil(window/10), this bound).
# 16384 is the semantic compaction output bound; 4096 is the summary headroom.
_SUMMARY_HEADROOM = 16_384 + 4_096
# Spec 01 observation policy bounds for the per-round text cap.
ROUND_CAP_MIN_BYTES = 32_768
ROUND_CAP_MAX_BYTES = 262_144
# An overflow round is one parallel batch of this many reads, filling this
# share of the round cap. Extra rounds past the trigger estimate absorb the
# estimate's error and put a batch on an almost full window.
OVERFLOW_BATCH_CALLS = 2
OVERFLOW_CAP_SHARE = 0.75
OVERFLOW_EXTRA_ROUNDS = 2
# Extra sequential child reads past the trigger estimate.
GROWTH_EXTRA_READS = 4
# Provider bytes per token for bulk lines: count_tokens for base64 and the
# Claude digit lines (see below); Codex prepared ~146k tokens for 617 KB of
# digit lines read live.
BASE64_BYTES_PER_TOKEN = 1.06
DIGIT_BYTES_PER_TOKEN = 2.0
CODEX_DIGIT_BYTES_PER_TOKEN = 4.0
# 28 pages are 7 MiB of tool text, under the 8 MiB capture cap on one request.
MAX_GROWTH_PAGES = 28
# Above the page count, below the spec 07 ceiling of 1024, so a growth turn
# is not cut off by the default 512 and still cannot run away.
ROUND_CAP = 128
LINE_BYTES = 1500
# One sequential read. 20 lines are 30_000 bytes, about 7_500 tokens. A
# 2_000-line read of this file is most of a 256k window and trips ContextLimit
# before compaction can run. Overflow sizes its calls from the round cap.
GROWTH_CALL_LINES = 20
# Anthropic's classifier refused a Claude growth turn reading base64 pages
# (`category=reasoning_extraction`), so the claude pack reads random decimal
# groups instead. count_tokens measured 1.06 bytes per token for the base64
# lines and 2.0 for the digit lines, so 40 digit lines cost about what 20
# base64 lines do per call (~29k tokens) and the page math is unchanged.
CLAUDE_GROWTH_CALL_LINES = 40
# `OPENROUTER_CUSTOM_MAX_PROJECTED_INPUT` in crates/hosting/hosted/src/openrouter.rs.
OPENROUTER_CUSTOM_INPUT = 65_536
_DIGITS_PER_GROUP = 5
# The pre-compaction fact is a neutral code word. "Read token.txt ... do not
# print it ... write the token from memory" read like secret handling, and
# xAI's usage-guideline check refused that follow-up before generation.
FACT_FILE = "codeword.txt"
FACT_FILES = (FACT_FILE, "acknowledged.txt")
# A child told an exact read count of a plain file stopped well short of its
# trigger. Its pages carry check words it writes here, one file per page.
CHILD_PAGE_DIR = "pages"
# A Codex child read 3 to 9 of the 30 KB exec results its trigger needed and
# then stopped, so a Codex child page is two halves read in one exec, one
# through `notify` and one through `text`. One `exec_command` result stops at
# 40,000 bytes (10,000 tokens at bytes/4); two halves stay under the 87,040
# byte round cap of a 272k window. Base64 is denser than the digit lines:
# gpt-6-luna counted 20,042 tokens for 29,380 bytes of it, against 14,721 for
# digits.
CODEX_CHILD_HALF_LINES = 26
CODEX_BASE64_BYTES_PER_TOKEN = 1.47
CHILD_PAGER = "page.py"
# `session log <root>.cN --json` pages newest-first. The compaction segment
# sits at position zero, so the tail has to cover the whole child history.
CHILD_HISTORY_TAIL = 65_536
# Spawn/wait receipts name a child on these tools. Tree frames are the other
# source; `session show` is the fallback when neither carries `cN`.
_CHILD_RECEIPT_TOOLS = frozenset({
    "spawn_agent", "wait_agent", "list_agents", "send_agent_message",
    "close_agent", "interrupt_agent", "recover_agent", "adopt_candidate",
})
CATALOG = (
    ("in_turn_compaction", "jsonl"),
    ("overflow", "jsonl"),
    ("kill_reopen", "jsonl"),
    ("child_compaction", "jsonl"),
)


def catalog_window(model: str) -> int | None:
    """Catalog context window for a route model, when this driver knows it.

    Spec 01 §6.7 and the user guide: `grok-4.7` is 256k, `grok-4.6` is 500k,
    Claude is 1M, OpenAI and Codex rows are 272k except `gpt-6.1-sol`, whose
    Codex catalog row reports `context_length=500000`. DeepSeek reports 1M, but
    Plak's OpenRouter custom route projects at most 65,536 input tokens
    (`OPENROUTER_CUSTOM_MAX_PROJECTED_INPUT`), which is the window that
    compacts. Other OpenRouter models are not in that list.
    """
    if model == "grok-4.7":
        return 256_000
    if model == "grok-4.6":
        return 500_000
    if model == "gpt-6.1-sol":
        return 500_000
    if model.startswith("deepseek/"):
        return OPENROUTER_CUSTOM_INPUT
    if model.startswith("claude-"):
        return 1_000_000
    if model.startswith("gpt-"):
        return 272_000
    return None


def small_window(window: int | None) -> bool:
    """A window of at most two read pages, where page-sized steps are too coarse."""
    return window is not None and window <= 2 * PAGE_TOKENS


def pages_for_window(window: int) -> int:
    """Full read pages whose bytes/4 measure exceeds `window`: at least 6, or 2 on a small window.

    A small window reaches its trigger inside the first page, and reading six
    pages to eof in small calls would outrun `ROUND_CAP`.
    """
    needed = (window + PAGE_TOKENS - 1) // PAGE_TOKENS
    return min(MAX_GROWTH_PAGES, max(2 if small_window(window) else 6, needed))


def default_pages(route: live.Route) -> int:
    """Per-route growth default. A dial overrides this."""
    window = catalog_window(route.model)
    if window is None:
        return 6
    return pages_for_window(window)


def growth_pages(route: live.Route | None = None) -> int:
    """Driver dial, else the route's default. Not a product setting."""
    raw = os.environ.get("PLAK_LIVE_GROWTH_PAGES", "").strip()
    if not raw:
        return default_pages(route or live.ROUTES["grok"])
    try:
        pages = int(raw)
    except ValueError as error:
        raise ScenarioFailure(HARNESS, "PLAK_LIVE_GROWTH_PAGES is not an integer") from error
    if not 1 <= pages <= MAX_GROWTH_PAGES:
        raise ScenarioFailure(HARNESS, f"PLAK_LIVE_GROWTH_PAGES must be 1..={MAX_GROWTH_PAGES}")
    return pages


def growth_timeout(pages: int) -> float:
    """Seconds for a growth turn. 6 pages stay at 900; larger windows scale."""
    return float(max(900, 150 * pages))


def child_timeout(pages: int) -> float:
    """Seconds for the child scenario. 6 pages stay at 1500."""
    return float(max(1500, 250 * pages))


def lines_per_page() -> int:
    return READ_PAGE_BYTES // LINE_BYTES


def write_bulk(path: Path, pages: int, pack: str = "grok", random_bytes=os.urandom) -> int:
    """Write `pages` full read-tool pages. Returns the line count.

    Lines are random, not a repeated character: a bytes/4 fence still sees
    every byte, and a provider count cannot collapse the file. Claude and Codex
    get decimal groups (`digit_line`), every other pack base64.
    """
    per_page = lines_per_page()
    total = per_page * pages
    with path.open("w") as handle:
        for _ in range(total):
            handle.write(bulk_line(pack, random_bytes) + "\n")
    return total


def bulk_line(pack: str, random_bytes=os.urandom) -> str:
    return digit_line(random_bytes) if pack in ("claude", "codex") else base64_line(random_bytes)


def new_check_word(random_bytes=os.urandom) -> str:
    return f"check-{random_bytes(3).hex()}"


def child_page_lines(pack: str, call_lines: int) -> int:
    """Bulk lines on one child page, after its header line."""
    return 2 * CODEX_CHILD_HALF_LINES if pack == "codex" else call_lines


def child_page_bytes_per_token(pack: str) -> float:
    return CODEX_BASE64_BYTES_PER_TOKEN if pack == "codex" else bulk_bytes_per_token(pack)


def child_pages(window: int | None, pack: str, call_lines: int,
                model: str | None = None) -> int:
    """Child pages whose text passes the compaction trigger, plus margin.

    The margin is `GROWTH_EXTRA_READS` sequential reads, at least one page.
    """
    window = window or OPENROUTER_CUSTOM_INPUT
    page_lines = child_page_lines(pack, call_lines)
    per_page = int(page_lines * LINE_BYTES / child_page_bytes_per_token(pack))
    extra = max(1, GROWTH_EXTRA_READS * call_lines // page_lines)
    return -(-compaction_trigger(window, model) // per_page) + extra


def page_header(index: int, count: int, word: str, size: int, pack: str = "grok") -> str:
    """A page's first line. Only the last page says it is last; the count is not stated."""
    if index == count:
        after = "This is the last page."
    elif pack == "codex":
        after = f"The next page is page {index + 1}."
    else:
        after = f"The next page starts at line {1 + index * size}."
    return f"Page {index}. Check word: {word}. {after}"


def write_child_pages(path: Path, pages: int, call_lines: int, pack: str = "grok",
                      random_bytes=os.urandom) -> list[str]:
    """Write `pages` child pages: one header line, then `child_page_lines` bulk lines.

    Returns each page's check word, in page order. The header is the only
    place a page's word appears, so `pages/<k>.txt` shows the child saw page k.
    A Codex child told the page count bailed after 3 of 37 pages, reporting
    the paging as incomplete: the count times the page size is past its
    window, and only compaction makes it fit. The header names the next page
    instead, and the last page alone says it is last. A Codex child reads
    through `page.py` beside `bulk.txt`.
    """
    page_lines = child_page_lines(pack, call_lines)
    size = page_lines + 1
    words = []
    with path.open("w") as handle:
        for index in range(1, pages + 1):
            word = new_check_word(random_bytes)
            words.append(word)
            handle.write(page_header(index, pages, word, size, pack) + "\n")
            for _ in range(page_lines):
                line = base64_line(random_bytes) if pack == "codex" else bulk_line(pack, random_bytes)
                handle.write(line + "\n")
    if pack == "codex":
        (path.parent / CHILD_PAGER).write_text(codex_pager(path.name))
    return words


def codex_pager(bulk: str) -> str:
    """`python3 page.py K H` prints half H (1 or 2) of page K; half 1 holds the header."""
    half = CODEX_CHILD_HALF_LINES
    return (
        "import sys\n"
        "from pathlib import Path\n"
        "page, half = int(sys.argv[1]), int(sys.argv[2])\n"
        f"lines = Path({bulk!r}).read_text().splitlines(keepends=True)\n"
        f"start = (page - 1) * {2 * half + 1} + (0 if half == 1 else {half + 1})\n"
        f"sys.stdout.write(''.join(lines[start:start + ({half + 1} if half == 1 else {half})]))\n"
    )


def pages_written(repo: Path, words: list[str]) -> int:
    """Pages whose `pages/<k>.txt` holds that page's check word."""
    return sum(text_equals(repo / CHILD_PAGE_DIR / f"{index}.txt", word)
               for index, word in enumerate(words, 1))


def base64_line(random_bytes=os.urandom) -> str:
    # 1100 bytes encodes to 1468 characters, under LINE_BYTES.
    return base64.b64encode(random_bytes(1100)).decode("ascii")


def digit_line(random_bytes=os.urandom) -> str:
    """1468 characters of random five-digit groups, the length of `base64_line`."""
    width = len(base64_line(random_bytes))
    digits = "".join(str(byte % 10) for byte in random_bytes(width))
    groups = [digits[index:index + _DIGITS_PER_GROUP]
              for index in range(0, len(digits), _DIGITS_PER_GROUP)]
    return " ".join(groups)[:width]


def growth_call_lines(pack: str, window: int | None = None) -> int:
    """Lines per sequential growth read for this pack and window.

    On a small window one call stays near an eighth of it (one token per byte
    is the observation measure), so a fold leaves room below the trigger: 20
    base64 lines are 29k bytes, nearly half of a 65,536 window.
    """
    lines = CLAUDE_GROWTH_CALL_LINES if pack == "claude" else GROWTH_CALL_LINES
    if small_window(window):
        lines = min(lines, max(2, window // 8 // LINE_BYTES))
    return lines


def pack_of(route: live.Route) -> str:
    """Process pack for this route. Codex and the OpenAI API are Code Mode."""
    if route.account in ("codex", "openai") or route.model.startswith("gpt-"):
        return "codex"
    if route.account in ("claude", "anthropic") or route.model.startswith("claude-"):
        return "claude"
    if route.account == "openrouter":
        return "pi"
    return "grok"


def _read_tool(pack: str) -> str:
    return {"codex": "exec_command", "claude": "Read", "pi": "read", "grok": "read_file"}[pack]


def new_code_word(random_bytes=os.urandom) -> str:
    return f"ZEBRA-{int.from_bytes(random_bytes(4), 'big') % 900_000 + 100_000}"


def plant_steps(pack: str = "grok") -> str:
    if pack == "codex":
        how = f"exec_command, printing {FACT_FILE} and no other file"
    else:
        how = f"the {_read_tool(pack)} tool on {FACT_FILE}"
    return (
        f"Read {FACT_FILE} with {how}. Write its exact contents, and nothing "
        "else, into acknowledged.txt."
    )


def plant_prompt(pack: str = "grok") -> str:
    return f"{plant_steps(pack)} Reply with exactly: ok"


def _page_bulk(pack: str, line_count: int, limit: int, reads: int | None = None) -> str:
    """How this pack pages `bulk.txt` without exceeding one call's budget.

    `reads` bounds the paging to that many calls instead of eof.
    """
    if reads is not None:
        tool = "exec_command, printing with python3" if pack == "codex" else f"the {_read_tool(pack)} tool"
        offsets = f"1, {1 + limit}, {1 + 2 * limit}, and so on up to {1 + (reads - 1) * limit}"
        # A Code Mode exec that runs several slices passes the round cap and
        # is clipped, so the context does not grow; one slice per exec.
        # Codex also shrank results with `max_output_tokens`, which keeps the
        # slice out of the context; the whole slice must come back.
        per_round = ("In Code Mode, make one exec per call, each running exactly one "
                     "exec_command without max_output_tokens and returning its whole output "
                     "with text(...)" if pack == "codex" else "Make one call per response")
        return (
            f"Page through bulk.txt using only {tool}, in exactly {reads} sequential calls of "
            f"{limit} lines each, at line offsets {offsets}. {per_round}, and wait for its "
            f"result before the next call. Do not stop before the {reads}th call."
        )
    if pack == "codex":
        return (
            "Print bulk.txt with exec_command only. This surface has no read tool. "
            "In Code Mode, make one exec per slice and wait for its result before the next exec. "
            f"Each call prints at most {limit} lines, using python3 to slice the file "
            f"starting at line 1 and then at each next offset of {limit}. "
            f"The file is {line_count} lines; do not stop early and do not print "
            "more than that many lines in one call."
        )
    tool = _read_tool(pack)
    return (
        f"Read bulk.txt from the first line through eof using only the {tool} tool, "
        f"not the shell. Each call must use limit {limit} and the next offset from "
        f"the previous result, starting at offset 1. Make one call per response and "
        f"wait for its result before the next call. The file is {line_count} lines; "
        "do not stop early."
    )


def growth_prompt(line_count: int, pack: str = "grok", call_lines: int | None = None) -> str:
    return (
        f"{_page_bulk(pack, line_count, call_lines or growth_call_lines(pack))} "
        f"When eof is true, {RECALL_STEP} Reply with exactly: done"
    )


RECALL_STEP = (
    "write the code word from the earlier turn into recalled.txt, exactly and alone, "
    f"from memory: do not open {FACT_FILE} or acknowledged.txt again."
)
CHILD_RECALL_STEP = (
    "write the code word you copied into acknowledged.txt into recalled.txt, exactly and "
    f"alone, from memory: do not open {FACT_FILE} or acknowledged.txt again."
)


def follow_prompt() -> str:
    return (
        "Write the code word from the earlier turn into followup.txt, exactly and "
        f"alone, from memory: do not open {FACT_FILE}, acknowledged.txt, or recalled.txt. "
        "Reply with exactly: done"
    )


def round_cap_bytes(window: int) -> int:
    """Spec 01 observation policy: text one model round may observe, in bytes.

    `clamp(floor(window * 4 * 8 / 100), 32768, 262144)`. A read beyond it is
    clipped and does not grow the context, so no single batch can overflow a
    large window.
    """
    return min(ROUND_CAP_MAX_BYTES, max(ROUND_CAP_MIN_BYTES, window * 4 * 8 // 100))


def overflow_plan(window: int | None, pack: str, line_count: int,
                  model: str | None = None) -> tuple[int, list[list[int]]]:
    """Lines per call and the offsets of each round's parallel batch.

    Each batch fills `OVERFLOW_CAP_SHARE` of the round cap, so nothing is
    clipped and every round adds its whole batch to the context. The rounds
    cover the compaction trigger at the pack's measured bytes per token, plus
    `OVERFLOW_EXTRA_ROUNDS` so a late batch lands on an almost full window.
    An unknown window is planned as the smallest one the driver knows.
    """
    window = window or OPENROUTER_CUSTOM_INPUT
    budget = int(round_cap_bytes(window) * OVERFLOW_CAP_SHARE)
    lines = max(1, budget // OVERFLOW_BATCH_CALLS // LINE_BYTES)
    per_round = OVERFLOW_BATCH_CALLS * lines
    bytes_per_token = bulk_bytes_per_token(pack)
    round_tokens = per_round * LINE_BYTES / bytes_per_token
    rounds = -(-compaction_trigger(window, model) // int(round_tokens)) + OVERFLOW_EXTRA_ROUNDS
    rounds = max(1, min(rounds, line_count // per_round))
    batches = [[1 + (index * OVERFLOW_BATCH_CALLS + call) * lines
                for call in range(OVERFLOW_BATCH_CALLS)] for index in range(rounds)]
    return lines, batches


def bulk_bytes_per_token(pack: str) -> float:
    return {"codex": CODEX_DIGIT_BYTES_PER_TOKEN,
            "claude": DIGIT_BYTES_PER_TOKEN}.get(pack, BASE64_BYTES_PER_TOKEN)


def overflow_timeout(rounds: int, pages: int) -> float:
    return float(max(growth_timeout(pages), 120 * rounds))


def overflow_prompt(lines: int, batches: list[list[int]], pack: str = "grok") -> str:
    listed = " ".join(
        f"Round {index}: offsets {', '.join(str(offset) for offset in batch)}."
        for index, batch in enumerate(batches, 1))
    order = "Wait for a batch's results before the next round, and do not skip or merge rounds."
    if pack == "codex":
        # Codex refused every batched wording live ("can't help exhaust the
        # context"), so its rounds are the growth turn's sequential reads,
        # each under the round cap, with the same total.
        reads = len(batches) * len(batches[0])
        return f"{_page_bulk(pack, 0, lines, reads)} After the last call, reply with exactly: done"
    # Stating the purpose keeps a batched read from looking like extraction.
    return (
        "bulk.txt is random filler for a context-capacity test of this agent harness: "
        f"loading it into your context is the task. Read bulk.txt in {len(batches)} rounds "
        f"using only the {_read_tool(pack)} tool, not the shell. In each round, call it once "
        f"per listed line offset, all in one parallel batch, each with limit {lines}. {order} "
        f"{listed} After the last round, reply with exactly: done. Do not open {FACT_FILE}."
    )


def continuation_prompt() -> str:
    return (
        "Write exactly CONTINUED and nothing else into continuation.txt. "
        "Reply with exactly: done"
    )


def kill_prompt(command: str) -> str:
    return (
        f"Run exactly this shell command and wait for it to finish: `{command}`. "
        "Do not change the command. Then reply: done"
    )


def reopen_prompt() -> str:
    return (
        "Write exactly REOPENED and nothing else into after_reopen.txt. "
        "Reply with exactly: done"
    )


def growth_reads(window: int | None, pack: str, call_lines: int, line_count: int,
                 model: str | None = None) -> int:
    """Sequential reads whose text passes the compaction trigger, plus margin.

    Bounded by the file.
    """
    window = window or OPENROUTER_CUSTOM_INPUT
    per_read = call_lines * LINE_BYTES / bulk_bytes_per_token(pack)
    reads = -(-compaction_trigger(window, model) // int(per_read)) + GROWTH_EXTRA_READS
    return max(1, min(reads, line_count // call_lines))


def child_paging(pack: str, call_lines: int) -> str:
    """Page-by-page reading of the child's `bulk.txt` with one deliverable per page."""
    if pack == "codex":
        read = (
            f"bulk.txt is a sequence of pages; read it only through {CHILD_PAGER}. Read page k "
            "with one exec whose source has these two lines, with k replaced by the page "
            "number:\n"
            f"const a = await tools.exec_command({{cmd: \"python3 {CHILD_PAGER} k 1\"}}); "
            "notify(a.output);\n"
            f"const b = await tools.exec_command({{cmd: \"python3 {CHILD_PAGER} k 2\"}}); "
            "text(b.output);\n"
            "Do not add max_output_tokens. Read page 1 first, and wait for each page's result "
            "before the next page."
        )
        nxt = "which page is next"
    else:
        size = call_lines + 1
        read = (
            f"bulk.txt is a sequence of pages of {size} lines each; page 1 starts at line 1. "
            f"Page through it using only the {_read_tool(pack)} tool. Read one page per call "
            f"with limit {size}, one call per response, and wait for its result before the "
            "next call."
        )
        nxt = "where the next page starts"
    return (
        f"{read} Each page's first line names that page's check word and {nxt}, or says "
        "it is the last page. After you have seen page k, write its check word, exactly and "
        f"alone, into {CHILD_PAGE_DIR}/k.txt; you may do that in the call that reads the next "
        "page. Keep paging until you have read the last page: your context is compacted "
        f"automatically when it fills, and the files in {CHILD_PAGE_DIR}/ record your "
        "progress. Do not read or search bulk.txt any other way."
    )


def child_prompt(pack: str = "grok", call_lines: int | None = None) -> str:
    instructions = (
        f"{plant_steps(pack)} "
        f"{child_paging(pack, call_lines or growth_call_lines(pack))} "
        f"Then {CHILD_RECALL_STEP} Reply with exactly: done"
    )
    return (
        "Spawn one worker child from the worker template, tier mutate, "
        "workspace=fork. Do not put any file's contents in the child prompt. "
        f"Give the child exactly this prompt: {json.dumps(instructions)}\n"
        "Wait for the child, adopt its candidate into this checkout, and reply: done"
    )


def hide_token(prompt: str, token: str) -> None:
    if token and token in prompt:
        raise ScenarioFailure(HARNESS, "prompt repeats the pre-compaction code word")


def lifecycle_windows(records: list[dict]) -> list[tuple[int, int]]:
    """Half-open index pairs of admitted..terminal lifecycle brackets."""
    windows = []
    stack: list[int] = []
    for index, record in enumerate(records):
        if record.get("kind") != "turn.lifecycle":
            continue
        phase = record.get("phase")
        if phase == "admitted":
            stack.append(index)
        elif phase not in (None, "admitted") and stack:
            windows.append((stack.pop(), index + 1))
    return windows


def _slice_records(records: list[dict], start: int, end: int) -> list[dict]:
    return records[start:end]


def scheduled_in_window(records: list[dict], start: int, end: int) -> list[dict]:
    """Committed compactions whose start reason is `scheduled`, inside one turn."""
    found = []
    reason = None
    for record in _slice_records(records, start, end):
        if record.get("kind") == "compact.started":
            reason = record.get("reason")
        elif record.get("kind") == "compact.finished":
            if reason == "scheduled" and record.get("outcome") == "committed":
                found.append(record)
            reason = None
    return found


def in_turn_compactions(records: list[dict]) -> list[dict]:
    found = []
    for start, end in lifecycle_windows(records):
        found.extend(scheduled_in_window(records, start, end))
    return found


def capacity_compactions(records: list[dict]) -> list[dict]:
    """Host capacity-recovery compacts (`compact.started` reason `capacity`)."""
    return [record for record in records
            if record.get("kind") == "compact.started" and record.get("reason") == "capacity"]


def capacity_recovery_committed(records: list[dict]) -> bool:
    """A capacity start followed later by a committed finish.

    The host logs `reason=capacity`, then the kernel logs the same compact as
    `manual` (the facade maps capacity recovery to a manual reason) before
    `compact.finished`. Pairing each start with the next finish would miss it.
    """
    seen = False
    for record in records:
        if record.get("kind") == "compact.started" and record.get("reason") == "capacity":
            seen = True
        elif (seen and record.get("kind") == "compact.finished"
              and record.get("outcome") == "committed"):
            return True
    return False


def prepared_peak(records: list[dict]) -> int | None:
    """Largest prepared input on `model.operation_usage` rows, if any were logged."""
    peak = None
    for record in records:
        if record.get("kind") != "model.operation_usage":
            continue
        for key in ("prepared_input_tokens", "peak_prepared_input_tokens"):
            value = record.get(key)
            if isinstance(value, int) and not isinstance(value, bool) and value > 0:
                peak = value if peak is None else max(peak, value)
    return peak


def compaction_trigger(window: int, model: str | None = None) -> int:
    """Rule trigger from spec 01 §10.3, without the unknown stable prefix.

    `grok-4.6` is an enrolled 80% row. Every other catalog window here uses
    `ceiling - max(ceil(ceiling/10), semantic summary bound + 4096)`, which is
    90% for those windows. Codex's catalog auto-compact limit is that 90%.
    """
    if model == "grok-4.6":
        return (window * 80) // 100
    tenth = (window + 9) // 10
    return max(0, window - max(tenth, _SUMMARY_HEADROOM))


def harness_bar(window: int, model: str | None = None) -> int:
    """Lower of the compaction trigger and the window minus one read page.

    A peak between the two has passed the point where a compact should exist.
    Calling that harness is not a false harness. The prefix term in §10.3 can
    only raise the real trigger, and this driver does not know it. A small
    window holds no whole page, so its step is an eighth of the window.
    """
    step = window // 8 if small_window(window) else PAGE_TOKENS
    return min(compaction_trigger(window, model), max(0, window - step))


def reached_trigger(records: list[dict], window: int | None, model: str | None = None) -> bool:
    """True when durable usage reached `harness_bar`.

    No usage row is not evidence: a turn that never got there stays a model
    miss, not a harness failure.
    """
    if window is None:
        return False
    peak = prepared_peak(records)
    return peak is not None and peak >= harness_bar(window, model)


def overflow_remedy(records: list[dict]) -> str | None:
    """Which overflow remedy the session log shows, if one committed.

    The rounds of batches are usually absorbed before the provider answers:
    the tool-group fence compacts (`scheduled`) once occupancy crosses the
    trigger, and a batch that would pass the window may first shrink via
    `context.placeholders`. The host's `capacity` compact runs only when the
    turn actually stops on ContextLimit. Spec 01 §10.3.
    """
    if capacity_recovery_committed(records):
        return "capacity-recovery compact"
    windows = bulk_windows(records)
    if any(scheduled_in_window(records, start, end) for start, end in windows):
        return "in-turn overflow compact"
    for start, end in windows:
        if any(record.get("kind") == "context.placeholders"
               for record in _slice_records(records, start, end)):
            return "tool-result placeholder shrink"
    return None


def tool_hits(records: list[dict], start: int, end: int, needles: tuple[str, ...]) -> list[dict]:
    hits = []
    for record in _slice_records(records, start, end):
        if record.get("kind") != "tool.started":
            continue
        blob = " ".join(str(record.get(key) or "")
                        for key in ("name", "exposed_name", "subject", "arguments", "detail"))
        if any(needle in blob for needle in needles):
            hits.append(record)
    return hits


def bulk_windows(records: list[dict]) -> list[tuple[int, int]]:
    return [window for window in lifecycle_windows(records)
            if tool_hits(records, window[0], window[1], ("bulk.txt",))]


def forbidden_reread(records: list[dict], start: int, end: int) -> list[dict]:
    """Any tool start that names a fact file inside [start, end).

    Grok's shell is `run_terminal_command`. grep and glob count too: the fact
    is in the subject, not only in a read-shaped tool name.
    """
    return tool_hits(records, start, end, FACT_FILES)


def reread_after_commit(records: list[dict], start: int, end: int) -> list[dict]:
    """Fact-file reads after a committed compaction in this turn, not before it."""
    cut = None
    for index in range(start, end):
        record = records[index]
        if record.get("kind") == "compact.finished" and record.get("outcome") == "committed":
            cut = index + 1
    if cut is None:
        return []
    return forbidden_reread(records, cut, end)


def assert_shrunk(record: dict) -> None:
    before = record.get("input_tokens_before")
    after = record.get("input_tokens_after")
    if before is None or after is None:
        return
    if not isinstance(before, int) or not isinstance(after, int) or after >= before:
        raise ScenarioFailure(HARNESS, "committed compaction did not shrink prepared input")


def abandonment(records: list[dict]) -> dict | None:
    for record in records:
        if record.get("kind") == "turn.abandoned":
            return record
    return None


# Spec 01 §5.5 / spec 02 §2.4. Code Mode can show exec as started before the
# cell's evaluation is durably prepared; that cut is nothing_started. A kill
# after the marker process is running is running_tool. Both are process_lost.
PROCESS_LOST_CLAIMS = frozenset({"nothing_started", "running_tool", "tool_round_completed"})
MARKER_OBSERVE_SECONDS = 15.0


def accepted_kill_abandonment(record: dict) -> bool:
    """True for a SIGKILL whose claim is running_tool or nothing_started."""
    return (record.get("cause") == "process_lost"
            and record.get("claim") in PROCESS_LOST_CLAIMS)


def marker_process_running(marker: str) -> bool:
    """The sleeper has exec'd: its argv contains `marker`.

    ToolStarted can precede that exec. Killing on the event alone is
    nothing_started, which is still a pass, but the usual claim is
    running_tool once this is true.
    """
    if not marker or any(char in marker for char in "\n\x00"):
        return False
    found = subprocess.run(
        ["pgrep", "-f", marker],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    return found.returncode == 0


def transcript_events(agent: live.Agent) -> list[dict]:
    """JSONL events already written for this agent, including child tree rows."""
    handle = getattr(agent, "transcript", None)
    path = Path(handle.name) if handle is not None else None
    if path is None or not path.is_file():
        return []
    events = []
    for line in path.read_text(errors="replace").splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(record, dict):
            events.append(record)
    return events


def _child_attempt_failed(record: dict) -> bool:
    """A projected child row that failed, faulted, or blocked."""
    if record.get("fault"):
        return True
    if record.get("status") in ("failed", "faulted"):
        return True
    return record.get("attempt") in ("blocked", "failed", "faulted")


def _describe_child(record: dict) -> str:
    """Status plus blocked quota, account, and fault text. Classification needs all of them."""
    parts = [f"child {record.get('agent_id')} status {record.get('status')}"]
    attempt = record.get("attempt")
    if isinstance(attempt, str) and attempt:
        parts.append(f"attempt {attempt}")
    blocked = record.get("blocked")
    if isinstance(blocked, dict):
        quota = blocked.get("quota")
        account = blocked.get("account")
        if isinstance(quota, str) and quota:
            parts.append(f"quota {quota}")
        if isinstance(account, str) and account:
            parts.append(f"account {account}")
    fault = record.get("fault")
    if isinstance(fault, str) and fault:
        parts.append(fault)
    failure = record.get("failure")
    if isinstance(failure, str) and failure:
        parts.append(failure)
    elif isinstance(failure, dict):
        kind = failure.get("kind")
        message = failure.get("message")
        if isinstance(kind, str) and kind:
            parts.append(f"failure {kind}")
        if isinstance(message, str) and message:
            parts.append(message)
    return " ".join(parts)


def _json_objects(text: str):
    """JSON objects in `text`, including a pretty-printed wait receipt after its arguments."""
    decoder = json.JSONDecoder()
    index = 0
    length = len(text)
    while index < length:
        start = text.find("{", index)
        if start < 0:
            return
        try:
            value, end = decoder.raw_decode(text, start)
        except json.JSONDecodeError:
            index = start + 1
            continue
        if isinstance(value, dict):
            yield value
        index = end if end > start else start + 1


def _receipt_objects(event: dict):
    """The event itself when it is a wait receipt, plus JSON carried in its result text."""
    if isinstance(event.get("agents"), list):
        yield event
    for key in ("detail", "result", "output"):
        raw = event.get(key)
        if isinstance(raw, dict):
            yield raw
        elif isinstance(raw, str) and "{" in raw:
            yield from _json_objects(raw)


def _receipt_child_failure(event: dict) -> str | None:
    """A failed `agents[]` row on a wait receipt, or None."""
    for obj in _receipt_objects(event):
        agents = obj.get("agents")
        if not isinstance(agents, list):
            continue
        for agent in agents:
            if isinstance(agent, dict) and _child_attempt_failed(agent):
                return _describe_child(agent)
    return None


# Root tool calls whose failure is the child's failure.
CHILD_TOOL_NAMES = frozenset({"spawn_agent", "wait_agent"})


def _retried_successfully(events: list[dict], index: int, failed: dict) -> bool:
    """A later call of the same tool on the same subject finished."""
    return any(
        isinstance(later, dict) and later.get("kind") == "tool"
        and later.get("name") == failed.get("name")
        and later.get("subject") == failed.get("subject")
        and later.get("state") in ("done", "succeeded")
        for later in events[index + 1:]
    )


def child_tool_failure(events: list[dict]) -> str | None:
    """A child tool failure visible on a supported channel, if one was projected.

    A failed `tool` event, a topology node at depth > 0 with `fault` or a
    failed or blocked attempt, or the same row on a wait receipt
    (`agents[].blocked`). Compact records are not on this channel. The text
    keeps `blocked.quota` and fault text so a provider refusal is not scored
    from the status word alone.
    """
    for index, event in enumerate(events):
        if not isinstance(event, dict):
            continue
        if event.get("kind") == "tool" and event.get("state") == "failed":
            receipt = _receipt_child_failure(event)
            if receipt:
                return receipt
            # A root call the model retried successfully (an adopt refused as
            # target_changed, then made with the current target) is the
            # model correcting itself, not a failure the scenario carries.
            if event.get("name") not in CHILD_TOOL_NAMES \
                    and _retried_successfully(events, index, event):
                continue
            subject = event.get("subject") or event.get("detail") or event.get("name") or "tool failed"
            return str(subject)
        if event.get("kind") == "tree":
            for node in event.get("nodes") or []:
                if not isinstance(node, dict) or int(node.get("depth") or 0) <= 0:
                    continue
                if _child_attempt_failed(node):
                    return _describe_child(node)
            continue
        receipt = _receipt_child_failure(event)
        if receipt:
            return receipt
    return None


def raise_child_tool_failure(text: str, lookup_faults: str | None = None) -> None:
    """Surface the child tool failure itself, not the missing session log.

    `rate_limited`, a quota exhaustion, and provider-unavailable fault text
    are `provider` (spec 05 §3.2). Any other child failure is `harness`.
    """
    detail = f"child tool failure: {' '.join(text.split())[:200]}"
    if (live.provider_child_text(text, lookup_faults=lookup_faults)
            or live.stream_validation_outcome(text, None) == live.PROVIDER):
        raise ScenarioFailure(live.PROVIDER, detail)
    raise ScenarioFailure(HARNESS, detail)


def text_equals(path: Path, expected: str) -> bool:
    if not path.is_file():
        return False
    return path.read_text().strip() == expected


def is_child_ordinal(value: object) -> bool:
    """True for `cN` as `session show`/`log` parse it: canonical decimal, no leading zero."""
    if not isinstance(value, str) or len(value) < 2 or not value.startswith("c"):
        return False
    ordinal = value[1:]
    if not ordinal.isascii() or not ordinal.isdigit():
        return False
    if ordinal != "0" and ordinal.startswith("0"):
        return False
    number = int(ordinal)
    return number <= 4_294_967_295 and str(number) == ordinal


def _ascii_words(text: str) -> list[str]:
    words: list[str] = []
    buf: list[str] = []
    for char in text:
        if char.isascii() and char.isalnum():
            buf.append(char)
        elif buf:
            words.append("".join(buf))
            buf = []
    if buf:
        words.append("".join(buf))
    return words


def child_ordinals(events: list[dict]) -> list[str]:
    """Child `cN` ids from JSONL `tree` frames and spawn/wait receipts.

    A tree node counts when it is not the root (`depth` > 0, or it names a
    parent). Receipt subjects are the wait id list (`c1,c2 · up to …`) and
    any id a spawn result preview carries. Other tool text is not scanned.
    """
    found: list[str] = []

    def add(value: object) -> None:
        if is_child_ordinal(value) and value not in found:
            found.append(value)

    for event in events:
        kind = event.get("kind")
        if kind == "tree":
            for node in event.get("nodes") or []:
                if not isinstance(node, dict):
                    continue
                depth = node.get("depth")
                if type(depth) is not int:
                    depth = 0
                if depth <= 0 and not node.get("parent_agent_id"):
                    continue
                add(node.get("agent_id"))
            continue
        if kind != "tool":
            continue
        name = event.get("name")
        if name not in _CHILD_RECEIPT_TOOLS and event.get("activity_kind") != "agent":
            continue
        for key in ("subject", "detail"):
            text = event.get(key)
            if isinstance(text, str):
                for word in _ascii_words(text):
                    add(word)
    return found


def child_ordinals_from_show(text: str) -> list[str]:
    """`Child          cN node=…` lines from `plak session show <root>`."""
    found: list[str] = []
    for line in text.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[0] == "Child" and is_child_ordinal(parts[1]):
            if parts[1] not in found:
                found.append(parts[1])
    return found


def is_committed_compaction_projection(record: dict) -> bool:
    """True for the `session log --json` line a committed compaction segment emits.

    The history projection gives that segment its own `kind`, `compaction`,
    and a short `text` that starts with `compacted ` (the selected-segment
    count and pass when the checkpoint's provenance names the segment).
    An empty assistant item is not that segment. The summary ciphertext and
    provider replay envelope are not on the line. `history_snapshot` is a
    Working-head header, not an item.
    """
    if record.get("kind") != "compaction":
        return False
    text = record.get("text")
    if not isinstance(text, str) or not text.startswith("compacted "):
        return False
    if not isinstance(record.get("entry_id"), str) or not record.get("entry_id"):
        return False
    for key in ("tool_calls", "tool_results"):
        if type(record.get(key)) is not int or record.get(key) != 0:
            return False
    return True


def _session_command(binary: Path, home: Path, args: list[str],
                     timeout: float) -> tuple[int, str, str]:
    """`plak session …` against `home`. A dead binary or a timeout is the channel failing."""
    env = os.environ.copy()
    env["PLAK_HOME"] = str(home)
    env.pop("PLAK_TEST_FAULTS", None)
    try:
        result = subprocess.run(
            [str(binary), "session", *args],
            env=env, capture_output=True, text=True, timeout=timeout,
        )
    except subprocess.TimeoutExpired as error:
        raise ScenarioFailure(live.PENDING, "session channel timed out") from error
    except OSError as error:
        raise ScenarioFailure(live.PENDING, f"session channel failed: {error}") from error
    return result.returncode, result.stdout, result.stderr


def _channel_detail(code: int, stdout: str, stderr: str) -> str:
    text = " ".join((stderr or stdout).split())
    return (text[:200] if text else f"exit {code}")


def child_ordinals_from_session_show(binary: Path, home: Path, root_id: str,
                                     timeout: float = 120) -> list[str]:
    """Child ordinals from `plak session show <root>`. Non-zero with no child line is pending.

    A recovery-required show still prints `Child` lines before it exits
    non-zero. Those ordinals are usable; a later log failure is its own channel
    error.
    """
    code, stdout, stderr = _session_command(binary, home, ["show", root_id], timeout)
    found = child_ordinals_from_show(stdout)
    if code != 0 and not found:
        raise ScenarioFailure(
            live.PENDING, f"session show channel failed: {_channel_detail(code, stdout, stderr)}")
    return found


def read_child_history(binary: Path, home: Path, root_id: str, ordinal: str,
                       timeout: float = 120) -> list[dict]:
    """`plak session log <root>.cN --json` items, including a Working `history_snapshot` header."""
    code, stdout, stderr = _session_command(
        binary, home,
        ["log", f"{root_id}.{ordinal}", "--json", "--tail", str(CHILD_HISTORY_TAIL)],
        timeout,
    )
    if code != 0:
        raise ScenarioFailure(
            live.PENDING,
            f"session log {ordinal} channel failed: {_channel_detail(code, stdout, stderr)}",
        )
    items = []
    for line in stdout.splitlines():
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            raise ScenarioFailure(
                live.PENDING, f"session log {ordinal} was not JSONL") from error
        if not isinstance(record, dict):
            raise ScenarioFailure(live.PENDING, f"session log {ordinal} was not JSONL")
        items.append(record)
    return items


def poll_records(home: Path, session_id: str, predicate, seconds: float = 30) -> list[dict]:
    deadline = time.monotonic() + seconds
    records = live.session_records(home, session_id)
    while not predicate(records) and time.monotonic() < deadline:
        time.sleep(1)
        records = live.session_records(home, session_id)
    return records


def require_completed(turn: live.Turn, what: str) -> None:
    import smoke
    smoke.require_completed(turn, what)


def commit_extra(repo: Path, message: str) -> None:
    subprocess.run(["git", "add", "-A"], cwd=repo, check=True)
    subprocess.run(["git", "-c", "user.name=smoke", "-c", "user.email=smoke@example.invalid",
                    "commit", "-qm", message], cwd=repo, check=True)


def close_agent(agent: live.Agent) -> None:
    try:
        agent.quit()
    except (ScenarioFailure, OSError, ValueError):
        try:
            agent.proc.wait(timeout=2)
        except (subprocess.TimeoutExpired, OSError):
            pass
        for handle in (getattr(agent, "transcript", None), getattr(agent, "stderr", None)):
            if handle is not None and not handle.closed:
                handle.close()
    proc = getattr(agent, "proc", None)
    if proc is None:
        return
    for handle in (proc.stdin, proc.stdout):
        if handle is not None and not handle.closed:
            try:
                handle.close()
            except OSError:
                pass


class LongSession:
    def __init__(self, route: live.Route, env: live.Env):
        import random

        self.route, self.env = route, env
        self.random_bytes = (os.urandom if env.fixture_seed is None
                             else random.Random(env.fixture_seed).randbytes)
        self.pages = growth_pages(route)
        self.window = catalog_window(route.model)
        self.call_lines = growth_call_lines(pack_of(route), self.window)

    def repo(self, name: str, token: str, bulk: bool) -> tuple[Path, int]:
        path = self.env.artifacts / name / "repo"
        live.git_repo(path, {FACT_FILE: token + "\n", "README.md": "seed\n"})
        lines = 0
        if bulk:
            lines = write_bulk(path / "bulk.txt", self.pages, pack_of(self.route), self.random_bytes)
            commit_extra(path, "bulk")
        return path, lines

    def home(self, name: str) -> Path:
        home = self.env.artifacts / name / "home"
        live.write_home(home, self.route, self.env.home)
        live.pin_max_model_rounds(home, ROUND_CAP)
        return home

    def agent(self, name: str, repo: Path, home: Path, args: list[str] | None = None) -> live.Agent:
        return live.Agent(self.env, repo, f"{name}/agent", args=args or [], home=home)

    def in_turn_compaction(self) -> str:
        token = new_code_word(self.random_bytes)
        repo, lines = self.repo("in_turn_compaction", token, bulk=True)
        home = self.home("in_turn_compaction")
        agent = self.agent("in_turn_compaction", repo, home)
        try:
            pack = pack_of(self.route)
            plant = agent.prompt(plant_prompt(pack), timeout=180)
            require_completed(plant, "fact turn")
            if not text_equals(repo / "acknowledged.txt", token):
                raise ScenarioFailure(MODEL, f"acknowledged.txt does not match {FACT_FILE}")
            growth = growth_prompt(lines, pack, self.call_lines)
            hide_token(growth, token)
            turn = agent.prompt(growth, timeout=growth_timeout(self.pages))
            require_completed(turn, "growth turn")
            records = poll_records(
                home, agent.session_id,
                lambda rows: any(scheduled_in_window(rows, start, end)
                                 for start, end in bulk_windows(rows)))
            self._require_in_turn(records, token, repo / "recalled.txt")
            follow = follow_prompt()
            hide_token(follow, token)
            follow_turn = agent.prompt(follow, timeout=180)
            require_completed(follow_turn, "follow-up")
            records = poll_records(
                home, agent.session_id,
                lambda rows: len(lifecycle_windows(rows)) >= 3)
            self._reject_follow_reread(records)
            if not text_equals(repo / "followup.txt", token):
                raise ScenarioFailure(MODEL, "follow-up did not write the pre-compaction code word")
        finally:
            close_agent(agent)
        return "in-turn scheduled compaction committed; fact survived; follow-up wrote followup.txt"

    def _require_in_turn(self, records: list[dict], token: str, recalled: Path) -> None:
        windows = bulk_windows(records)
        committed = [record for start, end in windows
                     for record in scheduled_in_window(records, start, end)]
        if not committed:
            if reached_trigger(records, self.window, self.route.model):
                raise ScenarioFailure(
                    HARNESS, "prepared input reached the compaction trigger without a committed compact")
            raise ScenarioFailure(MODEL, "growth turn did not reach the compaction trigger")
        for record in committed:
            assert_shrunk(record)
        for start, end in windows:
            if reread_after_commit(records, start, end):
                raise ScenarioFailure(MODEL, "fact file was re-read after compaction")
        if not text_equals(recalled, token):
            raise ScenarioFailure(MODEL, "recalled.txt does not match the pre-compaction code word")

    def _reject_follow_reread(self, records: list[dict]) -> None:
        windows = lifecycle_windows(records)
        if len(windows) < 3:
            return
        start, end = windows[-1]
        if forbidden_reread(records, start, end):
            raise ScenarioFailure(MODEL, "follow-up re-read the fact file")

    def overflow(self) -> str:
        token = new_code_word(self.random_bytes)
        repo, line_count = self.repo("overflow", token, bulk=True)
        home = self.home("overflow")
        agent = self.agent("overflow", repo, home)
        try:
            pack = pack_of(self.route)
            plant = agent.prompt(plant_prompt(pack), timeout=180)
            require_completed(plant, "fact turn")
            if not text_equals(repo / "acknowledged.txt", token):
                raise ScenarioFailure(MODEL, f"acknowledged.txt does not match {FACT_FILE}")
            lines, batches = overflow_plan(self.window, pack, line_count, self.route.model)
            prompt = overflow_prompt(lines, batches, pack)
            hide_token(prompt, token)
            turn = agent.prompt(prompt, timeout=overflow_timeout(len(batches), self.pages))
            records = poll_records(
                home, agent.session_id, lambda rows: overflow_remedy(rows) is not None)
            remedy = self._require_overflow(turn, records)
            if not text_equals(repo / "acknowledged.txt", token):
                raise ScenarioFailure(HARNESS, "overflow changed the pre-compaction fact file")
            follow = agent.prompt(continuation_prompt(), timeout=240)
            require_completed(follow, "continuation")
            if not text_equals(repo / "continuation.txt", "CONTINUED"):
                raise ScenarioFailure(MODEL, "continuation did not write continuation.txt")
        finally:
            close_agent(agent)
        return f"{remedy}; continuation wrote continuation.txt"

    def _require_overflow(self, turn: live.Turn, records: list[dict]) -> str:
        remedy = overflow_remedy(records)
        if remedy is not None:
            return remedy
        if turn.model_error_kind == "ContextLimit" and turn.phase == "failed":
            raise ScenarioFailure(HARNESS, "context limit produced no committed recovery compact")
        if not turn.completed:
            require_completed(turn, "overflow turn")
        if reached_trigger(records, self.window, self.route.model):
            raise ScenarioFailure(
                HARNESS, "prepared input reached the compaction trigger without a committed remedy")
        raise ScenarioFailure(MODEL, "overflow turn did not reach the compaction trigger")

    def kill_reopen(self) -> str:
        marker = "plak-long-kill-" + self.random_bytes(4).hex()
        # An argv token, not a `#` comment: the shell drops the comment before
        # exec, so `pkill -f` would not see it on the sleep process.
        command = f"python3 -c 'import time; time.sleep(45)' {marker}"
        repo, _lines = self.repo("kill_reopen", "unused", bulk=False)
        home = self.home("kill_reopen")
        agent = self.agent("kill_reopen", repo, home)
        started = {"tool": False, "pid": None}

        def kill_on_tool(event: dict) -> None:
            if started["tool"] or event.get("kind") != "tool" or event.get("state") != "running":
                return
            blob = " ".join(str(event.get(key) or "") for key in ("subject", "detail", "name"))
            if marker not in blob:
                return
            started["tool"] = True
            pid = event.get("pid") if isinstance(event.get("pid"), int) else event.get("process_id")
            if isinstance(pid, int) and pid > 1:
                started["pid"] = pid
            # Prefer the exec'd sleeper so the claim is running_tool. If it
            # never appears, kill anyway: nothing_started is the same cut.
            deadline = time.monotonic() + MARKER_OBSERVE_SECONDS
            while time.monotonic() < deadline:
                if marker_process_running(marker):
                    started["observed"] = True
                    break
                time.sleep(0.05)
            agent.signal(signal.SIGKILL)

        try:
            try:
                agent.prompt(kill_prompt(command), timeout=90, on_event=kill_on_tool)
            except ScenarioFailure as failure:
                if not started["tool"]:
                    raise
                # A timeout is not a kill. Only the pipe closing counts.
                if "exited during a turn" not in str(failure):
                    raise
            else:
                if not started["tool"]:
                    raise ScenarioFailure(MODEL, "the model never started the command")
                raise ScenarioFailure(HARNESS, "plak agent survived SIGKILL")
            session = agent.session_id
            close_agent(agent)
            self._reap(marker, started["pid"])
            agent = crash_triage.reopen(
                lambda: self.agent("kill_reopen", repo, home, args=["--session", session]),
                self.env.binary, repo, home, session)
            if agent.session_id != session:
                raise ScenarioFailure(HARNESS, "reopen attached a different session")
            follow = agent.prompt(reopen_prompt(), timeout=240)
            require_completed(follow, "follow-up after kill")
            claim = self._require_abandoned(home, session, agent)
            if not text_equals(repo / "after_reopen.txt", "REOPENED"):
                raise ScenarioFailure(MODEL, "reopened session did not write after_reopen.txt")
        finally:
            self._reap(marker, started["pid"])
            close_agent(agent)
        return (f"turn.abandoned {claim}/process_lost; "
                "reopen follow-up wrote after_reopen.txt")

    def _require_abandoned(self, home: Path, session: str, agent: live.Agent) -> str:
        # Process restart logs `turn.abandoned` only. `session.recovered` is
        # the in-place recovery record and is not part of this shape (spec 02 §2.4).
        records = poll_records(home, session, lambda rows: abandonment(rows) is not None)
        report = abandonment(records)
        if report is None:
            raise ScenarioFailure(HARNESS, "session log has no turn.abandoned record")
        claim = report.get("claim")
        if not accepted_kill_abandonment(report) or not isinstance(claim, str):
            raise ScenarioFailure(
                HARNESS,
                f"turn.abandoned claim={claim} cause={report.get('cause')}")
        matched = [event for event in agent.recovered
                   if event.get("claim") == claim and event.get("cause") == "process_lost"]
        if not matched:
            raise ScenarioFailure(HARNESS, f"reopen did not emit recovered {claim}/process_lost")
        return claim

    def child_compaction(self) -> str:
        token = new_code_word(self.random_bytes)
        repo, _lines = self.repo("child_compaction", token, bulk=False)
        pack = pack_of(self.route)
        pages = child_pages(self.window, pack, self.call_lines, self.route.model)
        words = write_child_pages(repo / "bulk.txt", pages, self.call_lines, pack,
                                  self.random_bytes)
        commit_extra(repo, "bulk")
        home = self.home("child_compaction")
        agent = self.agent("child_compaction", repo, home)
        try:
            prompt = child_prompt(pack, self.call_lines)
            hide_token(prompt, token)
            turn = agent.prompt(prompt, timeout=child_timeout(self.pages))
            require_completed(turn, "root turn")
            # Quit before the read so the child head is quiescent. `close_agent`
            # is idempotent with the finally below.
            close_agent(agent)
            events = transcript_events(agent)
            failure = child_tool_failure(events)
            if failure is not None:
                raise_child_tool_failure(failure, getattr(agent, "lookup_faults", None))
            session = agent.session_id
            if not isinstance(session, str) or not session:
                raise ScenarioFailure(HARNESS, "agent has no session id")
            ordinals = child_ordinals(events)
            if not ordinals:
                ordinals = child_ordinals_from_session_show(self.env.binary, home, session)
            if not ordinals:
                if agent.children:
                    raise ScenarioFailure(
                        live.PENDING,
                        "session show channel failed: spawned child is not addressable as cN",
                    )
                raise ScenarioFailure(MODEL, "root spawned no child")
            committed = False
            channel_error: ScenarioFailure | None = None
            for ordinal in ordinals:
                try:
                    items = read_child_history(self.env.binary, home, session, ordinal)
                except ScenarioFailure as failure:
                    if failure.outcome != live.PENDING:
                        raise
                    channel_error = failure
                    continue
                if any(is_committed_compaction_projection(item) for item in items):
                    committed = True
                    break
            written = pages_written(repo, words)
            if not committed:
                if channel_error is not None:
                    raise channel_error
                # The projection has no `model.operation_usage`, so a missing
                # segment is not evidence the trigger was reached.
                raise ScenarioFailure(
                    MODEL, "child history has no committed compaction segment "
                    f"(check words for {written}/{pages} pages)")
            if written != pages:
                raise ScenarioFailure(MODEL, f"child wrote check words for {written}/{pages} pages")
            if not text_equals(repo / "recalled.txt", token):
                raise ScenarioFailure(MODEL, "child did not report the code word in recalled.txt")
            if not text_equals(repo / "acknowledged.txt", token):
                raise ScenarioFailure(MODEL, "child did not write acknowledged.txt before compaction")
        finally:
            close_agent(agent)
        return (f"child compaction segment committed; {pages}/{pages} page check words; "
                f"recalled.txt matches {FACT_FILE}")

    @staticmethod
    def _reap(marker: str, pid: int | None) -> None:
        if isinstance(pid, int) and pid > 1:
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
        # The marker is an argv token of the sleeper, so it survives exec.
        subprocess.run(["pkill", "-f", marker], stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, check=False)


def play(route: live.Route, env: live.Env, name: str) -> str:
    runner = LongSession(route, env)
    method = getattr(runner, name, None)
    if method is None or name not in dict(CATALOG):
        raise ScenarioFailure(HARNESS, f"unknown long-session scenario {name}")
    return method()


def selected_catalog(only: str, run_all: bool) -> tuple[tuple[str, str], ...]:
    """The four bar scenarios, or the named subset.

    `--all` and `--only` are mutually exclusive at the parser. A bare run,
    with neither, is a usage error.
    """
    names = [name for name, _surface in CATALOG]
    if run_all:
        return CATALOG
    chosen = [name for name in only.split(",") if name]
    if not chosen:
        raise ValueError(
            "refusing to launch without --only or --all; scenarios: " + ", ".join(names))
    unknown = [name for name in chosen if name not in names]
    if unknown:
        raise ValueError("unknown long-session scenario: " + ", ".join(unknown))
    return tuple(item for item in CATALOG if item[0] in chosen)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0], allow_abbrev=False)
    parser.add_argument("route", nargs="?", default="grok", choices=sorted(live.ROUTES))
    parser.add_argument("--binary", type=Path, default=live.ROOT / "target/release/plak")
    parser.add_argument("--fixture-seed", type=int, help="replay randomized scenario inputs")
    parser.add_argument("--artifacts", type=Path, help="fresh attempt artifact directory")
    select = parser.add_mutually_exclusive_group()
    select.add_argument("--only", default="", help="comma-separated long-session scenario names")
    select.add_argument("--all", action="store_true",
                        help="run exactly the four bar scenarios")
    return parser


def main(argv: list[str]) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        selected = selected_catalog(args.only, args.all)
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 2
    route = live.ROUTES[args.route]
    stamp = time.strftime("%Y%m%d-%H%M%S")
    artifacts = args.artifacts or live.ROOT / "target" / "live" / f"long-{route.name}-{stamp}"
    env = live.Env(args.binary.resolve(), artifacts / "home", artifacts)
    env.fixture_seed = args.fixture_seed
    binary_start = live.binary_sha256(env.binary)
    live.write_home(env.home, route)
    results = []
    evidence = {}
    reported_pages = None
    for name, surface in selected:
        (artifacts / name).mkdir(parents=True, exist_ok=True)
        started = time.monotonic()
        usage_before = live.session_usage(env.home)
        segment_start = len(env.capture_segments)
        try:
            # A bad dial fails this scenario, not the report writer.
            reported_pages = growth_pages(route)
            detail, outcome = play(route, env, name), PASS
        except ScenarioFailure as failure:
            outcome, detail = failure.outcome, str(failure)
        except Exception as error:
            outcome, detail = HARNESS, f"driver error: {error!r}"
            (artifacts / name / "traceback.txt").write_text(traceback.format_exc())
        segments = env.capture_segments[segment_start:]
        import smoke
        complete = smoke.capture_complete(segments)
        evidence[name] = {"segments": [str(segment) for segment in segments],
                          "capture_complete": complete,
                          "captured_exchanges": sum(len(list(segment.glob("*.info")))
                                                    for segment in segments)}
        if outcome == PASS and not complete:
            outcome, detail = HARNESS, "raw exchange evidence missing or incomplete"
        usage = live.usage_delta(live.session_usage(env.home), usage_before)
        private = artifacts / name / "home"
        if private.is_dir():
            usage = usage.add(live.session_usage(private))
        result = live.Result(name, surface, outcome, detail, time.monotonic() - started,
                             usage.input_tokens, usage.output_tokens,
                             usage.cached_input_tokens, usage.reasoning_tokens)
        results.append(result)
        print(f"{result.outcome:8} {name}: {detail}", flush=True)
    binary_end = live.binary_sha256(env.binary)
    evidence_complete = all(item["capture_complete"] for item in evidence.values())
    header = live.route_report(route, bound=env.bound_compositions)
    (artifacts / "report.json").write_text(json.dumps(
        {**header, "binary": str(env.binary),
         "binary_sha256_start": binary_start, "binary_sha256_end": binary_end,
         "growth_pages": reported_pages, "round_cap": ROUND_CAP,
         "token_telemetry": live.sum_usage(
             live.TokenUsage(result.input_tokens, result.output_tokens,
                             result.cached_input_tokens, result.reasoning_tokens)
             for result in results),
         "evidence": evidence, "capture_complete": evidence_complete,
         "results": [result.__dict__ for result in results]}, indent=2) + "\n")
    print("\n" + live.table(results) + f"\n\nartifacts: {artifacts}")
    return 0 if (all(result.outcome == PASS for result in results)
                 and evidence_complete and binary_start == binary_end) else 1


if __name__ == "__main__":
    signal.signal(signal.SIGINT, signal.default_int_handler)
    raise SystemExit(main(sys.argv[1:]))
