"""Parse content-free `plak telemetry sql --json` into Harbor usage fields."""

from __future__ import annotations

import json
import hashlib
import os
import sqlite3
import stat
import tempfile
from contextlib import ExitStack, closing
from dataclasses import dataclass, replace
from pathlib import Path
from urllib.parse import quote


TELEMETRY_COLUMNS = (
    "input_tokens",
    "cached_input_tokens",
    "output_tokens",
    "reasoning_tokens",
    "cost_micros",
    "cost_status",
    "row_count",
    "missing_input_rows",
    "missing_cache_rows",
    "missing_output_rows",
    "missing_cost_rows",
    "cache_write_tokens",
    "missing_cache_write_rows",
    "compaction_rows",
    "observed_turn_cost_micros",
    "missing_compaction_cost_rows",
    "compaction_coverage_mismatches",
    "missing_turn_cost_rows",
    "endpoint_compact_workflows",
)

# `plak telemetry sql --json` emits arrays of string cells, not objects.
# Bare aggregates over zero turns still yield one SQLite row; HAVING drops it.
def _usage_sql(
    session_filter: str, *, include_compactions: bool = True,
    compaction_cost_columns: bool = True, endpoint_workflows: str = "0",
) -> str:
    compaction_filter = "" if include_compactions else " AND 0"
    compaction_cost = "cost_micros" if compaction_cost_columns else "NULL"
    compaction_cost_status = "cost_status" if compaction_cost_columns else "'unreported_compaction'"
    return f"""
WITH selected_session AS (SELECT ({session_filter}) AS id),
usage_rows AS (
  SELECT input_tokens, cached_input_tokens, cache_write_tokens,
         output_tokens, reasoning_tokens, cost_micros, cost_status,
         0 AS compaction
  FROM turns WHERE session_id = (SELECT id FROM selected_session)
  UNION ALL
  SELECT input_tokens, cached_input_tokens, cache_write_tokens,
         output_tokens, reasoning_tokens, {compaction_cost} AS cost_micros,
         {compaction_cost_status} AS cost_status, 1 AS compaction
  FROM compactions WHERE session_id = (SELECT id FROM selected_session){compaction_filter}
),
turn_compaction_coverage AS (
  SELECT SUM(CASE WHEN t.compactions IS NULL OR t.compactions < 0 OR
                       t.compactions != (
                         SELECT COUNT(*) FROM compactions c
                         WHERE c.session_id = t.session_id AND c.turn_id = t.turn_id
                       ) THEN 1 ELSE 0 END) AS mismatches
  FROM turns t WHERE t.session_id = (SELECT id FROM selected_session)
)
SELECT
  SUM(input_tokens) AS input_tokens,
  SUM(cached_input_tokens) AS cached_input_tokens,
  SUM(output_tokens) AS output_tokens,
  SUM(reasoning_tokens) AS reasoning_tokens,
  SUM(cost_micros) AS cost_micros,
  CASE WHEN SUM(CASE WHEN cost_status IN ('known', 'reported')
                         AND cost_micros IS NOT NULL THEN 0 ELSE 1 END) = 0
       THEN CASE WHEN MIN(cost_status) = MAX(cost_status)
                 THEN MIN(cost_status) ELSE 'mixed_reported' END
       ELSE 'partial' END AS cost_status,
  COUNT(*) AS row_count,
  SUM(CASE WHEN input_tokens IS NULL THEN 1 ELSE 0 END) AS missing_input_rows,
  SUM(CASE WHEN cached_input_tokens IS NULL THEN 1 ELSE 0 END) AS missing_cache_rows,
  SUM(CASE WHEN output_tokens IS NULL THEN 1 ELSE 0 END) AS missing_output_rows,
  SUM(CASE WHEN cost_status IN ('known', 'reported')
                AND cost_micros IS NOT NULL THEN 0 ELSE 1 END) AS missing_cost_rows,
  SUM(cache_write_tokens) AS cache_write_tokens,
  SUM(CASE WHEN cache_write_tokens IS NULL THEN 1 ELSE 0 END) AS missing_cache_write_rows,
  SUM(compaction) AS compaction_rows,
  SUM(CASE WHEN compaction = 0 THEN cost_micros END) AS observed_turn_cost_micros,
  SUM(CASE WHEN compaction = 1 AND (cost_status NOT IN ('known', 'reported')
               OR cost_micros IS NULL) THEN 1 ELSE 0 END) AS missing_compaction_cost_rows,
  COALESCE((SELECT mismatches FROM turn_compaction_coverage), 0) +
    (SELECT COUNT(*) FROM compactions c
     WHERE c.session_id = (SELECT id FROM selected_session)
       AND c.turn_id IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM turns t
                       WHERE t.session_id = c.session_id AND t.turn_id = c.turn_id))
    AS compaction_coverage_mismatches,
  SUM(CASE WHEN compaction = 0 AND (cost_status NOT IN ('known', 'reported')
               OR cost_micros IS NULL) THEN 1 ELSE 0 END) AS missing_turn_cost_rows,
  {endpoint_workflows} AS endpoint_compact_workflows
FROM usage_rows
HAVING COUNT(*) > 0;
"""


TELEMETRY_SQL = _usage_sql("""SELECT substr(locator, 1, instr(locator, '/') - 1)
  FROM product.sessions WHERE id = '{session_id}'""",
  endpoint_workflows="""(SELECT COUNT(*) FROM product.workflows
  WHERE session_id = '{session_id}' AND kind = 'compact')""")
ARCHIVE_SQL = _usage_sql("?")
ARCHIVE_V7_SQL = _usage_sql("?", compaction_cost_columns=False)
ARCHIVE_TURN_SQL = _usage_sql("?", include_compactions=False, compaction_cost_columns=False)


class UsageError(ValueError):
    pass


@dataclass(frozen=True)
class UsageTotals:
    n_input_tokens: int | None
    n_cache_tokens: int | None
    n_output_tokens: int | None
    n_reasoning_tokens: int | None
    cost_usd: float | None
    cost_status: str
    resolved: bool
    n_cache_write_tokens: int | None = None
    missing_buckets: tuple[str, ...] = ()
    usage_status: str = "complete"
    compaction_rows: int = 0
    observed_turn_cost_usd: float | None = None
    observed_cost_usd: float | None = None
    missing_compaction_cost_rows: int = 0
    missing_turn_cost_rows: int = 0
    endpoint_compact_workflows: int = 0


@dataclass(frozen=True)
class ArchivedPlakUsage:
    usage: UsageTotals | None
    status: str
    source_sha256: dict[str, str]
    product_session_id: str | None = None
    runtime_session_id: str | None = None
    turn_usage: UsageTotals | None = None


def _archive_copy(path: Path, target: Path) -> str:
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_size > 64 * 1024 * 1024:
        raise UsageError("archive database is not a bounded regular file")
    digest = hashlib.sha256()
    target.parent.mkdir(parents=True, exist_ok=True)
    with path.open("rb") as source, target.open("wb") as copied:
        opened = os.fstat(source.fileno())
        if not stat.S_ISREG(opened.st_mode) or opened.st_ino != info.st_ino:
            raise UsageError("archive source changed during open")
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
            copied.write(chunk)
        after_read = os.fstat(source.fileno())
    after_path = path.lstat()
    if any(
        (before.st_ino, before.st_size, before.st_mtime_ns)
        != (after.st_ino, after.st_size, after.st_mtime_ns)
        for before, after in ((info, after_read), (info, after_path))
    ):
        raise UsageError("archive source changed during copy")
    return digest.hexdigest()


def _archive_db(path: Path) -> sqlite3.Connection:
    uri = f"file:{quote(str(path.absolute()), safe='/')}?mode=ro"
    connection = sqlite3.connect(uri, uri=True)
    connection.execute("PRAGMA query_only = ON")
    return connection


def read_archived_plak_usage(agent_dir: Path) -> ArchivedPlakUsage:
    """Recover one trial's telemetry only with an exact product-session binding.

    Archived Plak homes have used both root and nested database locations.
    Every connection is read-only and only the selected trial's two local databases
    are considered. Missing or ambiguous evidence never supplies zero tokens.
    """
    home = agent_dir / "plak-home"
    layouts = (
        *(
            (state / "product.sqlite3", home / "telemetry" / "telemetry.sqlite3")
            for state in sorted((home / "state").glob("r*.e*.p*.t*"))
        ),
        (home / "state" / "product.sqlite3", home / "telemetry" / "telemetry.sqlite3"),
        (home / "product.sqlite3", home / "telemetry.sqlite3"),
    )
    # All current product-state layouts share the same telemetry path. It
    # cannot identify which product DB owns a trial; only a present product DB
    # selects a layout, and two product DBs remain genuinely ambiguous.
    present = [(product, telemetry) for product, telemetry in layouts if product.exists()]
    if not present:
        return ArchivedPlakUsage(None, "missing_db", {})
    if len(present) != 1:
        return ArchivedPlakUsage(None, "ambiguous_db_layout", {})
    product_path, telemetry_path = present[0]
    if not product_path.exists() or not telemetry_path.exists():
        return ArchivedPlakUsage(None, "missing_db", {})

    hashes: dict[str, str] = {}
    try:
        with ExitStack() as stack:
            private = Path(stack.enter_context(tempfile.TemporaryDirectory()))
            product_copy = private / "product" / "product.sqlite3"
            telemetry_copy = private / "telemetry" / "telemetry.sqlite3"
            for label, source, target in (
                ("product", product_path, product_copy),
                ("telemetry", telemetry_path, telemetry_copy),
            ):
                hashes[label] = _archive_copy(source, target)
                for suffix in ("-wal", "-shm"):
                    sidecar = source.with_name(source.name + suffix)
                    if sidecar.exists():
                        hashes[label + suffix.replace("-", "_")] = _archive_copy(
                            sidecar, target.with_name(target.name + suffix)
                        )
            product = _archive_db(product_copy)
            stack.enter_context(closing(product))
            telemetry = _archive_db(telemetry_copy)
            stack.enter_context(closing(telemetry))
            sessions = product.execute("SELECT id, locator FROM sessions LIMIT 2").fetchall()
            if len(sessions) != 1:
                return ArchivedPlakUsage(None, "ambiguous_session" if sessions else "missing_session", hashes)
            product_id, locator = sessions[0]
            if not isinstance(product_id, str) or not isinstance(locator, str) or "/" not in locator:
                return ArchivedPlakUsage(None, "invalid_session", hashes)
            stdout_path = agent_dir / "plak-stdout.txt"
            if stdout_path.exists():
                stdout_info = stdout_path.lstat()
                if not stat.S_ISREG(stdout_info.st_mode) or stdout_info.st_size > 8 * 1024 * 1024:
                    return ArchivedPlakUsage(None, "unverified_stdout", hashes)
                with stdout_path.open("r", encoding="utf-8", errors="replace") as stdout:
                    text = stdout.read(8 * 1024 * 1024 + 1)
                if len(text) > 8 * 1024 * 1024:
                    return ArchivedPlakUsage(None, "unverified_stdout", hashes)
                declared = parse_session_id_lines(text)
                if len(set(declared)) != 1 or declared[0] != product_id:
                    return ArchivedPlakUsage(None, "session_mismatch", hashes)
            runtime_id = locator.split("/", 1)[0]
            if not runtime_id:
                return ArchivedPlakUsage(None, "invalid_session", hashes)
            endpoint_workflows = product.execute(
                "SELECT COUNT(*) FROM workflows WHERE session_id = ? AND kind = 'compact'",
                (product_id,),
            ).fetchone()[0]
            ids = telemetry.execute("SELECT DISTINCT session_id FROM turns LIMIT 2").fetchall()
            if not ids:
                return ArchivedPlakUsage(None, "missing_turns", hashes, product_id, runtime_id)
            if len(ids) != 1 or ids[0][0] != runtime_id:
                return ArchivedPlakUsage(None, "ambiguous_turns", hashes, product_id, runtime_id)
            compaction_ids = telemetry.execute(
                "SELECT DISTINCT session_id FROM compactions LIMIT 2"
            ).fetchall()
            if any(row[0] != runtime_id for row in compaction_ids):
                return ArchivedPlakUsage(None, "ambiguous_compactions", hashes, product_id, runtime_id)
            schema = telemetry.execute("PRAGMA user_version").fetchone()[0]
            compaction_columns = {
                column[1] for column in telemetry.execute("PRAGMA table_info(compactions)")
            }
            has_cost_columns = {"cost_micros", "cost_status"} <= compaction_columns
            if (schema, has_cost_columns) not in ((7, False), (8, True)):
                return ArchivedPlakUsage(None, "unsupported_telemetry_schema", hashes, product_id, runtime_id)
            query = ARCHIVE_SQL if schema == 8 else ARCHIVE_V7_SQL
            row = telemetry.execute(query, (runtime_id,)).fetchone()
            if row is None:
                return ArchivedPlakUsage(None, "missing_turns", hashes, product_id, runtime_id)
            usage = parse_telemetry_json(json.dumps([list(row)]))
            if endpoint_workflows:
                usage = replace(
                    usage, resolved=False, usage_status="partial", cost_usd=None,
                    cost_status="partial_scope",
                    missing_buckets=(*usage.missing_buckets, "endpoint_compaction_scope_unverified"),
                    endpoint_compact_workflows=endpoint_workflows,
                )
            turn_row = telemetry.execute(ARCHIVE_TURN_SQL, (runtime_id,)).fetchone()
            turn_usage = parse_telemetry_json(json.dumps([list(turn_row)])) if turn_row else None
            return ArchivedPlakUsage(
                usage,
                "complete" if usage.resolved else "partial_usage",
                hashes,
                product_id,
                runtime_id,
                turn_usage,
            )
    except (OSError, sqlite3.Error, UsageError, ValueError):
        return ArchivedPlakUsage(None, "invalid_db", hashes)


def parse_session_id_lines(stdout: str) -> tuple[str, ...]:
    ids: list[str] = []
    for line in stdout.splitlines():
        if line.startswith("session_id="):
            value = line.split("=", 1)[1].strip()
            if value:
                ids.append(value)
    return tuple(ids)


def parse_session_id_line(stdout: str) -> str:
    ids = parse_session_id_lines(stdout)
    if not ids:
        raise UsageError("plak run stdout has no session_id= line")
    return ids[0]


def _unresolved(*, cost_status: str = "missing", missing_buckets: tuple[str, ...] = ("usage",)) -> UsageTotals:
    return UsageTotals(
        n_input_tokens=None,
        n_cache_tokens=None,
        n_output_tokens=None,
        n_reasoning_tokens=None,
        cost_usd=None,
        cost_status=cost_status,
        resolved=False,
        missing_buckets=missing_buckets,
        usage_status="missing",
    )


def _as_int(value: object) -> int | None:
    if value is None or value == "":
        return None
    if type(value) is int:
        return value if value >= 0 else None
    if isinstance(value, str) and value.strip().isascii() and value.strip().isdigit():
        return int(value.strip())
    return None


def _row_fields(row: object) -> dict[str, object]:
    if isinstance(row, dict):
        return row
    if isinstance(row, list):
        if len(row) < 6:
            raise UsageError("telemetry row is missing columns")
        return dict(zip(TELEMETRY_COLUMNS, row, strict=False))
    raise UsageError("telemetry row is not an object or array")


def parse_telemetry_json(payload: str) -> UsageTotals:
    try:
        rows = json.loads(payload)
    except json.JSONDecodeError as exc:
        raise UsageError(f"telemetry JSON is not parseable: {exc}") from exc
    if not isinstance(rows, list) or not rows:
        return _unresolved()
    fields = _row_fields(rows[0])

    input_tokens = _as_int(fields.get("input_tokens"))
    cached = _as_int(fields.get("cached_input_tokens"))
    cache_write = _as_int(fields.get("cache_write_tokens"))
    output_tokens = _as_int(fields.get("output_tokens"))
    reasoning = _as_int(fields.get("reasoning_tokens"))
    cost_micros = _as_int(fields.get("cost_micros"))
    cost_status = str(fields.get("cost_status") or "unknown")
    row_count = _as_int(fields.get("row_count"))
    compaction_rows = _as_int(fields.get("compaction_rows")) or 0
    observed_turn_cost_micros = _as_int(fields.get("observed_turn_cost_micros"))
    missing_compaction_cost_rows = _as_int(fields.get("missing_compaction_cost_rows")) or 0
    missing_turn_cost_rows = _as_int(fields.get("missing_turn_cost_rows")) or 0
    endpoint_compact_workflows = _as_int(fields.get("endpoint_compact_workflows")) or 0
    compaction_coverage_mismatches = _as_int(fields.get("compaction_coverage_mismatches"))
    missing = []
    for column, bucket, source in (
        ("missing_input_rows", "input", "input_tokens"),
        ("missing_cache_rows", "cache_read", "cached_input_tokens"),
        ("missing_output_rows", "output", "output_tokens"),
        ("missing_cache_write_rows", "cache_write", "cache_write_tokens"),
    ):
        # A legacy SUM without coverage counters can hide unmeasured rows.
        if row_count is None or _as_int(fields.get(column)) != 0 or _as_int(fields.get(source)) is None:
            missing.append(bucket)
    if compaction_coverage_mismatches is None or compaction_coverage_mismatches:
        missing.append("compaction_coverage")
    if endpoint_compact_workflows:
        missing.append("endpoint_compaction_scope_unverified")
    token_counts = (input_tokens, cached, cache_write, output_tokens, reasoning)
    if row_count is None and all(value in (None, 0) for value in token_counts) and cost_status in {
        "unknown",
        "missing",
    }:
        return _unresolved(cost_status="missing")
    if input_tokens is None and output_tokens is None:
        return _unresolved()
    cost_usd = None
    if row_count is not None and cost_status in {"known", "reported", "mixed_reported"} and cost_micros is not None and _as_int(fields.get("missing_cost_rows")) == 0:
        cost_usd = cost_micros / 1_000_000
    if row_count is None and cost_status in {"known", "reported"}:
        # Historical six-column aggregates cannot prove every contributing row.
        cost_usd = None
        cost_status = "unverified_aggregate"
    if row_count is not None and (
        "compaction_coverage" in missing or "endpoint_compaction_scope_unverified" in missing
    ):
        cost_usd = None
        cost_status = "partial_scope"
    return UsageTotals(
        n_input_tokens=input_tokens,
        n_cache_tokens=cached,
        n_output_tokens=output_tokens,
        n_reasoning_tokens=reasoning,
        cost_usd=cost_usd,
        cost_status=cost_status,
        resolved=not missing,
        n_cache_write_tokens=cache_write,
        missing_buckets=tuple(missing),
        usage_status="partial" if missing else "complete",
        compaction_rows=compaction_rows,
        observed_turn_cost_usd=(observed_turn_cost_micros / 1_000_000
                                if observed_turn_cost_micros is not None else None),
        observed_cost_usd=(cost_micros / 1_000_000 if cost_micros is not None else None),
        missing_compaction_cost_rows=missing_compaction_cost_rows,
        missing_turn_cost_rows=missing_turn_cost_rows,
        endpoint_compact_workflows=endpoint_compact_workflows,
    )


def telemetry_query(session_id: str) -> str:
    if "'" in session_id or ";" in session_id:
        raise UsageError("session id is not a UUID-shaped token")
    return TELEMETRY_SQL.format(session_id=session_id)


def _claude_prompt_totals(
    input_tokens: int | None,
    cache_read: int | None,
    cache_write: int | None,
    output_tokens: int | None,
) -> UsageTotals:
    """Claude remainder `input_tokens` plus cache buckets is the prompt total.

    Harbor `n_input_tokens` is that prompt. `n_cache_tokens` is cache_read
    (a subset). `n_cache_write_tokens` is cache_creation; Harbor has no write
    field, so callers put it on metadata.
    """
    missing = tuple(name for name, value in (("input", input_tokens), ("cache_read", cache_read), ("cache_write", cache_write), ("output", output_tokens)) if value is None)
    if input_tokens is None and output_tokens is None:
        return _unresolved()
    prompt = None if any(value is None for value in (input_tokens, cache_read, cache_write)) else input_tokens + cache_read + cache_write
    return UsageTotals(
        n_input_tokens=prompt,
        n_cache_tokens=cache_read,
        n_output_tokens=output_tokens,
        n_reasoning_tokens=None,
        cost_usd=None,
        cost_status="unknown",
        resolved=not missing,
        n_cache_write_tokens=cache_write,
        missing_buckets=missing,
        usage_status="partial" if missing else "complete",
    )


def parse_pmux_json(payload: str) -> UsageTotals:
    """Map `pmux --output json run` StatefulResult usage onto Harbor buckets."""
    try:
        body = json.loads(payload)
    except json.JSONDecodeError as exc:
        raise UsageError(f"pmux JSON is not parseable: {exc}") from exc
    if not isinstance(body, dict):
        return _unresolved()
    usage = body.get("usage")
    if not isinstance(usage, dict):
        return _unresolved()
    bucket = usage.get("combined")
    if not isinstance(bucket, dict):
        bucket = usage.get("main")
    if not isinstance(bucket, dict):
        return _unresolved()
    return _claude_prompt_totals(
        _as_int(bucket.get("input_tokens")),
        _as_int(bucket.get("cache_read_input_tokens")),
        _as_int(bucket.get("cache_creation_input_tokens")),
        _as_int(bucket.get("output_tokens")),
    )


def parse_pmux_jsonl(payload: str) -> UsageTotals:
    """Sum assistant `usage` records from Claude Code JSONL by message identity.

    Claude Code streams a growing usage snapshot for one assistant message, so
    the LAST snapshot per message id is that message's total and the totals are
    summed across ids. Identity is the message id plus `requestId` when the
    line carries one; two distinct messages with byte-identical usage are two
    requests, not one. A usage record without an id is unattributable, so the
    whole payload resolves as unresolved rather than being guessed at.
    """
    snapshots: dict[
        tuple[str, str], tuple[int | None, int | None, int | None, int | None]
    ] = {}
    for line in payload.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            body = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(body, dict) or body.get("type") != "assistant":
            continue
        message = body.get("message")
        if not isinstance(message, dict):
            continue
        usage = message.get("usage")
        if not isinstance(usage, dict):
            continue
        identity = _assistant_identity(body, message)
        if identity is None:
            return _unresolved()
        snapshots[identity] = (
            _as_int(usage.get("input_tokens")),
            _as_int(usage.get("output_tokens")),
            _as_int(usage.get("cache_read_input_tokens")),
            _as_int(usage.get("cache_creation_input_tokens")),
        )
    if not snapshots:
        return _unresolved()
    records = list(snapshots.values())

    def summed(index: int) -> int | None:
        values = [row[index] for row in records if row[index] is not None]
        if len(values) != len(records):
            return None
        return int(sum(values))

    return _claude_prompt_totals(
        summed(0),
        summed(2),
        summed(3),
        summed(1),
    )


def _assistant_identity(
    body: dict[str, object], message: dict[str, object]
) -> tuple[str, str] | None:
    """Identity of one assistant request, or None when the id is missing."""
    raw_id = message.get("id")
    if not isinstance(raw_id, str) or not raw_id.strip():
        return None
    request = body.get("requestId")
    if not isinstance(request, str) or not request.strip():
        request = message.get("requestId")
    if not isinstance(request, str) or not request.strip():
        request = ""
    return (raw_id.strip(), request.strip())


def merge_usage(*parts: UsageTotals) -> UsageTotals:
    if not parts:
        return _unresolved()
    if len(parts) == 1:
        return parts[0]
    if all(part.usage_status == "missing" for part in parts):
        return _unresolved()

    def summed(attr: str) -> int | None:
        values = [
            getattr(part, attr)
            for part in parts
            if getattr(part, attr) is not None
        ]
        if not values:
            return None
        return int(sum(values))

    def summed_cost(attr: str) -> float | None:
        values = [getattr(part, attr) for part in parts if getattr(part, attr) is not None]
        return float(sum(values)) if values else None

    costs = [part.cost_usd for part in parts if part.cost_usd is not None]
    all_costs = len(costs) == len(parts)
    status = "mixed_reported" if all_costs else "partial"
    if all_costs and len({part.cost_status for part in parts}) == 1:
        status = parts[0].cost_status
    missing = tuple(sorted({bucket for part in parts for bucket in part.missing_buckets} | ({"session"} if any(not part.resolved for part in parts) else set())))
    complete = all(part.resolved for part in parts)
    return UsageTotals(
        n_input_tokens=summed("n_input_tokens"),
        n_cache_tokens=summed("n_cache_tokens"),
        n_output_tokens=summed("n_output_tokens"),
        n_reasoning_tokens=summed("n_reasoning_tokens"),
        cost_usd=sum(costs) if all_costs else None,
        cost_status=status,
        resolved=complete,
        n_cache_write_tokens=summed("n_cache_write_tokens"),
        missing_buckets=missing,
        usage_status="partial" if missing else "complete",
        compaction_rows=sum(part.compaction_rows for part in parts),
        observed_turn_cost_usd=summed_cost("observed_turn_cost_usd"),
        observed_cost_usd=summed_cost("observed_cost_usd"),
        missing_compaction_cost_rows=sum(part.missing_compaction_cost_rows for part in parts),
        missing_turn_cost_rows=sum(part.missing_turn_cost_rows for part in parts),
        endpoint_compact_workflows=sum(part.endpoint_compact_workflows for part in parts),
    )
