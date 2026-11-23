"""A strict shard union needs the exact cuts and independent campaigns."""

from collections import Counter
import gzip
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools/dev"))
import crash_sweep_shards as sweep  # noqa: E402

COMMIT = "a" * 40
TREE = "b" * 40
TARGET = "x86_64-unknown-linux-musl"
FORMATS = {"state_key": "r1.e2.p3.t4"}


def receipt(i, n):
    names = sweep.plan(n)[i - 1]
    indexed, groups = sweep.expected_rows(names)
    return {
        "shard": f"{i}/{n}",
        "commit": COMMIT,
        "tree": TREE,
        "target": TARGET,
        "inventory": sweep.fingerprint(),
        "formats": FORMATS,
        "passed_at": 100 + i,
        "indexed": [
            dict(
                shape=shape,
                store=store,
                store_ordinal=cut,
                repeat=1,
                outcome="COMPLETED_ONCE",
                invariants=True,
            )
            for shape, store, cut in sorted(indexed)
        ],
        "runners": sorted(names & sweep.RUNNERS),
        "wire": [
            dict(
                family=family,
                child=child,
                group=group,
                rows=[
                    dict(shape=family, child=bool(child), store=store, cut=cut, invariants=True)
                    for store, cut in sorted(rows)
                ],
            )
            for (family, child, group), rows in sorted(groups.items())
        ],
    }


def merge(rows, directory):
    paths = []
    for i, row in enumerate(rows):
        path = directory / f"{i}.json.gz"
        with gzip.open(path, "wt", encoding="utf-8") as stream:
            json.dump(row, stream)
        paths.append(path)
    return sweep.merge(
        paths,
        directory / "merged",
        commit=COMMIT,
        tree=TREE,
        target=TARGET,
        formats=FORMATS,
        before_publish=lambda: None,
    )


class StrictSweepShardTests(unittest.TestCase):
    def test_each_shape_and_terminal_campaign_is_owned_once(self):
        for n in (1, 2, 5, 7, len(sweep.INDEXED)):
            with self.subTest(shards=n):
                plan = sweep.plan(n)
                self.assertEqual(plan, sweep.plan(n))
                self.assertTrue(all(plan))
                self.assertEqual(
                    Counter(name for names in plan for name in names),
                    Counter(set(sweep.INDEXED) | set(sweep.TERMINAL_SHAPES)),
                )
                self.assertEqual(sum(len(sweep.expected_rows(names)[0]) for names in plan), 2111)

    def test_merge_is_order_independent_and_preserves_invariant_rows(self):
        with tempfile.TemporaryDirectory() as scratch:
            directory = Path(scratch)
            counts = merge([receipt(i, 5) for i in (4, 3, 1, 5, 2)], directory)
            self.assertEqual(counts["indexed_cuts"], 2111)
            self.assertEqual(counts["indexed_shapes"], 50)
            self.assertEqual(counts["runner_cases"], 12)
            self.assertEqual(counts["wire_groups"], 32)
            self.assertEqual(counts["wire_invariant_rows"], 187)
            self.assertEqual(counts["passed_at"], 101)
            self.assertEqual(len((directory / "merged/sweep.jsonl").read_text().splitlines()), 2111)
            self.assertEqual(len((directory / "merged/wire.jsonl").read_text().splitlines()), 187)

    def test_missing_duplicate_or_mixed_shards_do_not_yield_evidence(self):
        first, second = receipt(1, 2), receipt(2, 2)
        for rows, message in (
            ([first], "need all 2 receipts"),
            ([first, first], "shard receipts"),
            ([first, receipt(2, 3)], "need all 3 receipts"),
            ([first, dict(second, tree="c" * 40)], "sweep identity"),
            ([dict(row, commit="c" * 40) for row in (first, second)], "sweep identity"),
            (
                [dict(row, target="aarch64-apple-darwin") for row in (first, second)],
                "sweep identity",
            ),
            ([first, dict(second, inventory="stale")], "fingerprint mismatch"),
            ([first, dict(second, formats={})], "stored format identities"),
            ([first, dict(second, passed_at=True)], "invalid shard completion time"),
            ([first, dict(second, passed_at=10**12)], "invalid shard completion time"),
        ):
            with self.subTest(error=message), tempfile.TemporaryDirectory() as scratch:
                with self.assertRaisesRegex(ValueError, message):
                    merge(rows, Path(scratch))
                self.assertFalse((Path(scratch) / "merged").exists())

    def test_missing_cases_and_rows_or_failed_invariants_refuse_merge(self):
        damages = (
            ("indexed cuts", lambda row: row["indexed"].pop()),
            ("indexed cuts", lambda row: row["indexed"].append(row["indexed"][0])),
            ("strict cut failed", lambda row: row["indexed"][0].update(invariants=False)),
            ("strict cut failed", lambda row: row["indexed"][0].update(outcome="REFUSED{known}")),
            ("runner cases", lambda row: row["runners"].pop()),
            ("wire groups", lambda row: row["wire"].pop()),
            ("missing=", lambda row: row["wire"][0]["rows"].pop()),
            ("unexpected=", lambda row: row["wire"][0]["rows"][0].update(cut=100)),
            (
                "wire invariant failed",
                lambda row: row["wire"][0]["rows"][0].update(invariants=False),
            ),
        )
        for message, corrupt in damages:
            with self.subTest(error=message), tempfile.TemporaryDirectory() as scratch:
                row = receipt(1, 1)
                corrupt(row)
                with self.assertRaisesRegex(ValueError, message):
                    merge([row], Path(scratch))
