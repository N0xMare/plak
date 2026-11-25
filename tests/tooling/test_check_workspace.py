"""Unit tests for the streamlined `tools/gate/check_workspace.py` policy checker."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/gate"))

import check_workspace as checker  # noqa: E402


class CheckWorkspacePolicyTests(unittest.TestCase):
    def test_live_workspace_passes(self) -> None:
        self.assertEqual(checker._check_workspace(ROOT), 0)

    def test_lints_forbid_unsafe(self) -> None:
        self.assertEqual(
            checker.validate_local_production_lints({"pkg": {"lints": {"workspace": True}}}),
            [],
        )
        errors = checker.validate_local_production_lints({"pkg": {}})
        self.assertEqual(len(errors), 1)
        self.assertIn("workspace lint inheritance", errors[0])

    def test_commit_trace_forbids_sql_returning(self) -> None:
        clean = checker.validate_commit_trace_sql(
            "plakous-store", {"src/sqlite.rs": 'let q = "INSERT INTO t VALUES (1)";'}
        )
        self.assertEqual(clean, [])
        bad = checker.validate_commit_trace_sql(
            "plakous-store",
            {"src/sqlite.rs": 'let q = "INSERT INTO t VALUES (1) RETURNING id";'},
        )
        self.assertEqual(len(bad), 1)
        self.assertIn("RETURNING", bad[0])
        test_only = checker.validate_commit_trace_sql(
            "plakous-store",
            {
                "src/sqlite.rs": (
                    '#[cfg(test)] mod tests { fn t() { let _ = "INSERT INTO t RETURNING id"; } }'
                )
            },
        )
        self.assertEqual(test_only, [])

    def test_neutral_sources_forbid_provider_names(self) -> None:
        self.assertEqual(
            checker.validate_neutral_sources("plakous-model", {"src/lib.rs": "pub struct Foo;"}),
            [],
        )
        bad = checker.validate_neutral_sources(
            "plakous-model", {"src/lib.rs": 'pub const PROVIDER: &str = "openai";'}
        )
        self.assertEqual(len(bad), 1)
        self.assertIn("openai", bad[0])

    def test_tracked_hygiene_catches_home_paths_and_private_names(self) -> None:
        self.assertEqual(checker.hygiene_findings("a.rs", b'let p = "/home/user/proj";'), [])
        bad_home = checker.hygiene_findings("a.rs", b'let p = "/ho' + b'me/alice/proj";')
        self.assertEqual(len(bad_home), 1)
        self.assertIn("/ho" + "me/alice", bad_home[0])
        bad_priv = checker.hygiene_findings(
            "a.rs", b"host = secretbox.local\n", frozenset({b"secretbox"})
        )
        self.assertEqual(len(bad_priv), 1)
        self.assertIn("secretbox", bad_priv[0])


if __name__ == "__main__":
    unittest.main()
