"""Offline accounting regressions for Harbor result projection."""

import json
import pathlib
import sqlite3
import sys
import tempfile
import unittest
from contextlib import closing

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "eval"))

from harbor_adapter.pricing import CanonicalUsage, from_harbor_context, implied_usd  # noqa: E402
from harbor_adapter.timing import span_seconds  # noqa: E402
from harbor_adapter.usage import telemetry_query, merge_usage, parse_pmux_json, parse_pmux_jsonl, parse_telemetry_json, read_archived_plak_usage  # noqa: E402


class UsageCompletenessTests(unittest.TestCase):
    def test_sql_aggregate_retains_missing_row_evidence(self) -> None:
        row = ["10", "2", "4", "0", "123", "partial", "2", "1", "0", "0", "1"]
        result = parse_telemetry_json(json.dumps([row]))
        self.assertEqual(result.n_input_tokens, 10)
        self.assertFalse(result.resolved)
        self.assertIn("input", result.missing_buckets)
        self.assertIsNone(result.cost_usd)

    def test_merge_keeps_observed_counts_but_not_complete_claim(self) -> None:
        known = parse_telemetry_json(json.dumps([["10", "2", "4", "0", "100", "known", "1", "0", "0", "0", "0", "0", "0"]]))
        merged = merge_usage(known, parse_telemetry_json("[]"))
        self.assertEqual(merged.n_input_tokens, 10)
        self.assertFalse(merged.resolved)
        self.assertEqual(merged.usage_status, "partial")
        self.assertIsNone(merged.cost_usd)

    def test_priced_and_unpriced_rows_have_no_aggregate_bill(self) -> None:
        row = ["20", "0", "6", "0", "100", "partial", "2", "0", "0", "0", "1", "0", "0", "0", "100", "0", "0"]
        result = parse_telemetry_json(json.dumps([row]))
        self.assertTrue(result.resolved)
        self.assertIsNone(result.cost_usd)
        self.assertEqual(result.cost_status, "partial")

    def test_sql_reads_cache_writes_and_preserves_partial_write_coverage(self) -> None:
        with closing(sqlite3.connect(":memory:")) as db, db:
            db.execute("ATTACH DATABASE ':memory:' AS product")
            db.execute("CREATE TABLE product.sessions (id TEXT, locator TEXT)")
            db.execute("CREATE TABLE product.workflows (session_id TEXT, kind TEXT)")
            db.execute("INSERT INTO product.sessions VALUES ('product-id', 'runtime-id/main')")
            db.execute("CREATE TABLE turns (session_id TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER, cost_micros INTEGER, cost_status TEXT, cache_write_tokens INTEGER)")
            db.execute("CREATE TABLE compactions (session_id TEXT, outcome TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER, cache_write_tokens INTEGER, cost_micros INTEGER, cost_status TEXT)")
            db.execute("ALTER TABLE turns ADD COLUMN turn_id TEXT")
            db.execute("ALTER TABLE turns ADD COLUMN compactions INTEGER")
            db.execute("ALTER TABLE compactions ADD COLUMN turn_id TEXT")
            db.execute("INSERT INTO turns VALUES ('runtime-id', 100, 20, 10, 2, 1000, 'reported', 30, 'turn-one', 0)")
            def usage():
                row = db.execute(telemetry_query("product-id")).fetchone()
                return parse_telemetry_json(json.dumps([list(row)]))
            complete = usage()
            self.assertTrue(complete.resolved)
            self.assertEqual(complete.usage_status, "complete")
            self.assertEqual(complete.n_cache_write_tokens, 30)
            self.assertEqual(complete.n_input_tokens, 100)
            self.assertEqual(complete.cost_usd, .001)
            db.execute("INSERT INTO turns VALUES ('runtime-id', 50, 0, 5, 0, NULL, 'unknown', NULL, 'turn-two', 0)")
            partial = usage()
            self.assertFalse(partial.resolved)
            self.assertEqual(partial.n_cache_write_tokens, 30)
            self.assertEqual(partial.n_input_tokens, 150)
            self.assertEqual(partial.missing_buckets, ("cache_write",))
            self.assertIsNone(partial.cost_usd)

    def test_live_sql_adds_schema8_compaction_usage_and_cost(self) -> None:
        with closing(sqlite3.connect(":memory:")) as db, db:
            db.execute("ATTACH DATABASE ':memory:' AS product")
            db.execute("CREATE TABLE product.sessions (id TEXT, locator TEXT)")
            db.execute("CREATE TABLE product.workflows (session_id TEXT, kind TEXT)")
            db.execute("INSERT INTO product.sessions VALUES ('product-id', 'runtime-id/main')")
            db.execute("CREATE TABLE turns (session_id TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, cache_write_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER, cost_micros INTEGER, cost_status TEXT)")
            db.execute("CREATE TABLE compactions (session_id TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, cache_write_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER, cost_micros INTEGER, cost_status TEXT)")
            db.execute("ALTER TABLE turns ADD COLUMN turn_id TEXT")
            db.execute("ALTER TABLE turns ADD COLUMN compactions INTEGER")
            db.execute("ALTER TABLE compactions ADD COLUMN turn_id TEXT")
            db.execute("INSERT INTO turns VALUES ('runtime-id', 100, 20, 30, 10, 2, 1000, 'reported', 'turn-one', 1)")
            db.execute("INSERT INTO compactions VALUES ('runtime-id', 7, 2, 0, 3, 1, 200, 'reported', 'turn-one')")
            row = db.execute(telemetry_query("product-id")).fetchone()
            usage = parse_telemetry_json(json.dumps([list(row)]))
            self.assertTrue(usage.resolved)
            self.assertEqual((usage.n_input_tokens, usage.n_cache_tokens,
                              usage.n_cache_write_tokens, usage.n_output_tokens),
                             (107, 22, 30, 13))
            self.assertEqual(usage.cost_usd, .0012)
            self.assertEqual(usage.observed_turn_cost_usd, .001)
            self.assertEqual(usage.compaction_rows, 1)
            db.execute("INSERT INTO product.workflows VALUES ('product-id', 'compact')")
            guarded = parse_telemetry_json(json.dumps([list(db.execute(telemetry_query("product-id")).fetchone())]))
            self.assertFalse(guarded.resolved)
            self.assertIn("endpoint_compaction_scope_unverified", guarded.missing_buckets)

    def test_compaction_usage_is_separate_from_turn_and_failed_call_is_partial(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            _, telemetry = ArchivedPlakUsageTests()._make_home(agent)
            with closing(sqlite3.connect(telemetry)) as db, db:
                db.execute("UPDATE turns SET cost_micros=1000, cost_status='reported'")
                db.execute("UPDATE turns SET compactions=1")
                db.execute("INSERT INTO compactions VALUES ('runtime-id', 'committed', 7, 2, 3, 1, 0, 'turn-id')")
            recovered = read_archived_plak_usage(agent)
            self.assertEqual(recovered.status, "complete")
            self.assertEqual(recovered.usage.n_input_tokens, 107)
            self.assertEqual(recovered.usage.n_output_tokens, 13)
            self.assertEqual(recovered.usage.compaction_rows, 1)
            self.assertIsNone(recovered.usage.cost_usd)
            self.assertEqual(recovered.usage.observed_turn_cost_usd, .001)
            self.assertEqual(recovered.turn_usage.n_input_tokens, 100)
            with closing(sqlite3.connect(telemetry)) as db, db:
                db.execute("UPDATE turns SET compactions=2")
                db.execute("INSERT INTO compactions VALUES ('runtime-id', 'failed', NULL, NULL, NULL, NULL, NULL, 'turn-id')")
            partial = read_archived_plak_usage(agent)
            self.assertEqual(partial.status, "partial_usage")
            self.assertEqual(partial.usage.n_input_tokens, 107)
            self.assertIn("input", partial.usage.missing_buckets)
            self.assertIn("output", partial.usage.missing_buckets)
            self.assertIsNone(partial.usage.cost_usd)

    def test_legacy_sum_retains_observations_without_claiming_coverage(self) -> None:
        result = parse_telemetry_json('[["10", "2", "4", "0", "100", "known"]]')
        self.assertEqual(result.n_input_tokens, 10)
        self.assertEqual(result.n_output_tokens, 4)
        self.assertFalse(result.resolved)
        self.assertEqual(result.usage_status, "partial")
        self.assertEqual(set(result.missing_buckets), {"input", "output", "cache_read", "cache_write", "compaction_coverage"})
        self.assertEqual(result.cost_status, "unverified_aggregate")
        self.assertIsNone(result.cost_usd)

    def test_claude_missing_cache_write_does_not_become_zero(self) -> None:
        usage = parse_pmux_json(json.dumps({"usage": {"combined": {"input_tokens": 10, "cache_read_input_tokens": 2, "output_tokens": 3}}}))
        self.assertFalse(usage.resolved)
        self.assertIsNone(usage.n_input_tokens)
        self.assertIn("cache_write", usage.missing_buckets)

    def test_claude_jsonl_partial_request_bucket_survives(self) -> None:
        lines = [
            {"type": "assistant", "message": {"id": "a", "usage": {"input_tokens": 2, "cache_read_input_tokens": 1, "cache_creation_input_tokens": 0, "output_tokens": 3}}},
            {"type": "assistant", "message": {"id": "b", "usage": {"input_tokens": 2, "cache_read_input_tokens": 1, "output_tokens": 3}}},
        ]
        usage = parse_pmux_jsonl("\n".join(map(json.dumps, lines)))
        self.assertFalse(usage.resolved)
        self.assertIsNone(usage.n_cache_write_tokens)


class PricingTests(unittest.TestCase):
    def test_claude_cache_write_is_removed_from_standard_input(self) -> None:
        cost = implied_usd("claude-opus-5-5", CanonicalUsage(input_tokens=100, cache_read=20, cache_write=30, output_tokens=10))
        expected = (50 * 4 + 20 * 0.2 + 30 * 5 + 10 * 20) / 1_000_000
        self.assertAlmostEqual(cost.usd, expected)
        self.assertEqual(cost.basis, "frozen_standard_proxy")

    def test_missing_cache_bucket_is_unresolved(self) -> None:
        cost = from_harbor_context("claude-opus-5-5", n_input_tokens=100, n_cache_tokens=0, n_cache_write_tokens=None, n_output_tokens=10)
        self.assertIsNone(cost.usd)
        self.assertEqual(cost.status, "unresolved_usage")

    def test_canonical_usage_requires_explicit_cache_inapplicability(self) -> None:
        cost = implied_usd("claude-opus-5-5", CanonicalUsage(input_tokens=100, output_tokens=10))
        self.assertIsNone(cost.usd)
        self.assertEqual(cost.status, "unresolved_usage")

    def test_unpriced_cache_write_is_not_hidden_by_other_priced_buckets(self) -> None:
        cost = implied_usd("grok-4.6", CanonicalUsage(input_tokens=100, cache_read=2, cache_write=3, output_tokens=5))
        self.assertIsNone(cost.usd)
        self.assertEqual(cost.status, "unpriced_cache_write")


class TimingTests(unittest.TestCase):
    def test_invalid_spans_are_null(self) -> None:
        for block in (
            {"started_at": "2026-01-01T01:00:00", "finished_at": "2026-01-01T01:00:01Z"},
            {"started_at": "2026-01-01T01:00:01Z", "finished_at": "2026-01-01T01:00:00Z"},
        ):
            self.assertIsNone(span_seconds(block))


class ArchivedPlakUsageTests(unittest.TestCase):
    def _make_home(self, agent: pathlib.Path, *, nested: bool = True) -> tuple[pathlib.Path, pathlib.Path]:
        home = agent / "plak-home"
        product = home / "state" / "product.sqlite3" if nested else home / "product.sqlite3"
        telemetry = home / "telemetry" / "telemetry.sqlite3" if nested else home / "telemetry.sqlite3"
        product.parent.mkdir(parents=True)
        telemetry.parent.mkdir(parents=True, exist_ok=True)
        with closing(sqlite3.connect(product)) as db, db:
            db.execute("CREATE TABLE sessions (id TEXT, locator TEXT)")
            db.execute("CREATE TABLE workflows (session_id TEXT, kind TEXT)")
            db.execute("INSERT INTO sessions VALUES ('product-id', 'runtime-id/main')")
        with closing(sqlite3.connect(telemetry)) as db, db:
            db.execute("PRAGMA user_version = 7")
            db.execute("CREATE TABLE turns (session_id TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER, cost_micros INTEGER, cost_status TEXT, cache_write_tokens INTEGER, turn_id TEXT, compactions INTEGER)")
            db.execute("CREATE TABLE compactions (session_id TEXT, outcome TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER, cache_write_tokens INTEGER, turn_id TEXT)")
            db.execute("INSERT INTO turns VALUES ('runtime-id', 100, 20, 10, 2, NULL, 'unknown', 30, 'turn-id', 0)")
        return product, telemetry

    def test_session_bound_usage_and_source_hashes(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            product, telemetry = self._make_home(agent)
            before = (product.read_bytes(), telemetry.read_bytes())
            result = read_archived_plak_usage(agent)
            self.assertEqual(result.status, "complete")
            self.assertEqual(result.product_session_id, "product-id")
            self.assertEqual(result.runtime_session_id, "runtime-id")
            self.assertEqual(result.usage.n_cache_write_tokens, 30)
            self.assertEqual(set(result.source_sha256), {"product", "telemetry"})
            self.assertEqual(before, (product.read_bytes(), telemetry.read_bytes()))

    def test_versioned_product_state_uses_shared_telemetry_without_inventing_a_second_layout(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            product, _ = self._make_home(agent)
            versioned = product.parent / "r1.e1.p1.t1" / "product.sqlite3"
            versioned.parent.mkdir()
            product.rename(versioned)
            recovered = read_archived_plak_usage(agent)
            self.assertEqual(recovered.status, "complete")
            self.assertEqual(recovered.usage.n_input_tokens, 100)
            product.write_bytes(versioned.read_bytes())
            self.assertEqual(read_archived_plak_usage(agent).status, "ambiguous_db_layout")

    def test_schema8_compaction_cost_is_counted_once(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            _, telemetry = self._make_home(agent)
            with closing(sqlite3.connect(telemetry)) as db, db:
                db.execute("ALTER TABLE compactions ADD COLUMN cost_micros INTEGER")
                db.execute("ALTER TABLE compactions ADD COLUMN cost_status TEXT")
                db.execute("PRAGMA user_version = 8")
                db.execute("UPDATE turns SET cost_micros=1000, cost_status='reported'")
                db.execute("UPDATE turns SET compactions=1")
                db.execute("INSERT INTO compactions VALUES ('runtime-id', 'committed', 7, 2, 3, 1, 0, 'turn-id', 200, 'reported')")
            complete = read_archived_plak_usage(agent)
            self.assertEqual(complete.status, "complete")
            self.assertEqual(complete.usage.n_input_tokens, 107)
            self.assertEqual(complete.usage.cost_usd, .0012)
            self.assertEqual(complete.usage.missing_compaction_cost_rows, 0)
            with closing(sqlite3.connect(telemetry)) as db, db:
                db.execute("UPDATE compactions SET cost_micros=NULL, cost_status='unknown'")
            partial_cost = read_archived_plak_usage(agent)
            self.assertEqual(partial_cost.status, "complete")
            self.assertTrue(partial_cost.usage.resolved)
            self.assertIsNone(partial_cost.usage.cost_usd)
            self.assertEqual(partial_cost.usage.observed_cost_usd, .001)
            self.assertEqual(partial_cost.usage.missing_compaction_cost_rows, 1)

    def test_turn_compaction_count_catches_missing_row_even_with_postturn_row(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            _, telemetry = self._make_home(agent)
            with closing(sqlite3.connect(telemetry)) as db, db:
                db.execute("UPDATE turns SET compactions=1, cost_micros=1000, cost_status='reported'")
                db.execute("INSERT INTO compactions VALUES ('runtime-id', 'committed', 7, 0, 2, 0, 0, NULL)")
            missing = read_archived_plak_usage(agent)
            self.assertEqual(missing.status, "partial_usage")
            self.assertEqual(missing.usage.n_input_tokens, 107)
            self.assertIn("compaction_coverage", missing.usage.missing_buckets)
            self.assertIsNone(missing.usage.cost_usd)
            self.assertEqual(missing.usage.observed_cost_usd, .001)
            with closing(sqlite3.connect(telemetry)) as db, db:
                db.execute("INSERT INTO compactions VALUES ('runtime-id', 'committed', 3, 0, 1, 0, 0, 'turn-id')")
            complete = read_archived_plak_usage(agent)
            self.assertEqual(complete.status, "complete")
            self.assertEqual(complete.usage.n_input_tokens, 110)

    def test_archived_endpoint_workflow_prevents_complete_claim(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            product, _ = self._make_home(agent)
            with closing(sqlite3.connect(product)) as db, db:
                db.execute("INSERT INTO workflows VALUES ('product-id', 'compact')")
            recovered = read_archived_plak_usage(agent)
            self.assertEqual(recovered.status, "partial_usage")
            self.assertEqual(recovered.usage.n_input_tokens, 100)
            self.assertIn("endpoint_compaction_scope_unverified", recovered.usage.missing_buckets)

    def test_wrong_or_extra_session_is_not_attributed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            _, telemetry = self._make_home(agent)
            with closing(sqlite3.connect(telemetry)) as db, db:
                db.execute("INSERT INTO turns VALUES ('other', 1, 0, 1, 0, NULL, 'unknown', 0, 'other-turn', 0)")
            result = read_archived_plak_usage(agent)
            self.assertEqual(result.status, "ambiguous_turns")
            self.assertIsNone(result.usage)

    def test_stdout_identity_must_match_product_session(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            self._make_home(agent)
            (agent / "plak-stdout.txt").write_text("session_id=other\n", encoding="utf-8")
            result = read_archived_plak_usage(agent)
            self.assertEqual(result.status, "session_mismatch")
            self.assertIsNone(result.usage)

    def test_late_stdout_identity_is_not_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            self._make_home(agent)
            (agent / "plak-stdout.txt").write_text(
                "session_id=product-id\n" + "x" * 70_000 + "\nsession_id=other\n",
                encoding="utf-8",
            )
            result = read_archived_plak_usage(agent)
            self.assertEqual(result.status, "session_mismatch")
            self.assertIsNone(result.usage)

    def test_negative_token_bucket_is_partial(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            _, telemetry = self._make_home(agent)
            with closing(sqlite3.connect(telemetry)) as db, db:
                db.execute("UPDATE turns SET input_tokens = -1")
            result = read_archived_plak_usage(agent)
            self.assertEqual(result.status, "partial_usage")
            self.assertIsNone(result.usage.n_input_tokens)
            self.assertIn("input", result.usage.missing_buckets)

    def test_wal_backed_turns_are_read_from_private_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            _, telemetry = self._make_home(agent)
            db = sqlite3.connect(telemetry)
            try:
                db.execute("PRAGMA journal_mode=WAL")
                db.execute("PRAGMA wal_autocheckpoint=0")
                db.execute("INSERT INTO turns VALUES ('runtime-id', 1, 0, 1, 0, NULL, 'unknown', 0, 'turn-two', 0)")
                db.commit()
                result = read_archived_plak_usage(agent)
                self.assertEqual(result.status, "complete")
                self.assertEqual(result.usage.n_input_tokens, 101)
                self.assertIn("telemetry_wal", result.source_sha256)
            finally:
                db.close()

    def test_missing_and_partial_evidence_stays_unresolved(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            agent = pathlib.Path(temporary) / "agent"
            self.assertEqual(read_archived_plak_usage(agent).status, "missing_db")
            _, telemetry = self._make_home(agent, nested=False)
            with closing(sqlite3.connect(telemetry)) as db, db:
                db.execute("UPDATE turns SET cache_write_tokens = NULL")
            result = read_archived_plak_usage(agent)
            self.assertEqual(result.status, "partial_usage")
            self.assertFalse(result.usage.resolved)
            self.assertIsNone(result.usage.n_cache_write_tokens)


if __name__ == "__main__":
    unittest.main()
