"""Offline census: parse each source, diff, and regenerate the checked-in tables.

The fixture's `docs/*.md` pages are synthetic: each holds only the lines the
census parsers read. The OpenAI list deliberately omits the two
`gpt-daybreak-*` ids, which the census must report removed without dropping.
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "tests/tooling/fixtures/api_model_census"
SPEC = importlib.util.spec_from_file_location(
    "api_model_census", ROOT / "scripts/api_model_census.py"
)
assert SPEC and SPEC.loader
census = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = census
SPEC.loader.exec_module(census)


def _table(provider: str) -> dict:
    return census.load_table(census.table_paths(ROOT)[provider])


class DumpTests(unittest.TestCase):
    def test_document_pages_are_marked_synthetic(self):
        pages = sorted(FIXTURE.glob("*/docs/*.md"))
        self.assertTrue(pages)
        for path in pages:
            with self.subTest(page=path.relative_to(FIXTURE)):
                self.assertIn(
                    "Synthetic fixture for scripts/api_model_census.py; not provider documentation.",
                    path.read_text(),
                )

    def test_dump_matches_checked_in_bytes(self):
        for provider, path in census.table_paths(ROOT).items():
            table = census.load_table(path)
            self.assertEqual(census.dump_table(provider, table), path.read_text(), provider)


class ParseTests(unittest.TestCase):
    def test_openai_page_efforts_threshold_and_tools(self):
        luna = census.parse_openai_model_page(
            (FIXTURE / "openai/docs/gpt-6-luna.md").read_text()
        )
        self.assertEqual(luna["id"], "gpt-6-luna")
        self.assertEqual(luna["max_input_tokens"], 922_000)
        self.assertEqual(luna["max_tokens"], 128_000)
        self.assertEqual(
            luna["efforts"], ["none", "low", "medium", "high", "xhigh", "max"]
        )
        self.assertEqual(luna["price_threshold_input_tokens"], 272_000)
        self.assertTrue(luna["responses"])
        self.assertEqual(
            luna["hosted_features"], ["web_search", "code_execution", "tool_search"]
        )

    def test_openai_page_drops_minimal_and_keeps_api_efforts(self):
        gpt5 = census.parse_openai_model_page((FIXTURE / "openai/docs/gpt-5.md").read_text())
        self.assertEqual(gpt5["efforts"], ["low", "medium", "high"])
        self.assertIsNone(gpt5["price_threshold_input_tokens"])

    def test_openai_wrapped_effort_line_excludes_unsupported(self):
        sol = census.parse_openai_model_page(
            (FIXTURE / "openai/docs/gpt-6.1-sol.md").read_text()
        )
        self.assertEqual(sol["efforts"], ["low", "medium", "high", "xhigh", "max"])

    def test_openai_page_without_a_maximum_input_uses_the_context_window(self):
        gpt4o = census.parse_openai_model_page((FIXTURE / "openai/docs/gpt-4o.md").read_text())
        self.assertEqual(gpt4o["max_input_tokens"], 128_000)
        self.assertEqual(gpt4o["max_tokens"], 16_384)
        self.assertEqual(gpt4o["efforts"], [])

    def test_fast_mode_2x_is_not_a_price_threshold(self):
        text = "Fast mode is priced at 2x the applicable rates.\n"
        self.assertIsNone(census.parse_openai_model_page(text)["price_threshold_input_tokens"])

    def test_anthropic_row_and_docs(self):
        bundle = census.load_fixture(FIXTURE)
        haiku = bundle["anthropic"]["observed"]["claude-haiku-5-5"]
        self.assertEqual(haiku["price_threshold_input_tokens"], 100_000)
        self.assertEqual(haiku["efforts"], ["low", "medium", "high", "xhigh", "max"])
        self.assertTrue(haiku["adaptive_thinking"])
        self.assertIn("compaction", haiku["hosted_features"])
        sonnet = bundle["anthropic"]["observed"]["claude-sonnet-5"]
        self.assertNotIn("tool_search", sonnet["hosted_features"])
        self.assertIsNone(sonnet["price_threshold_input_tokens"])
        older = bundle["anthropic"]["observed"]["claude-haiku-4-5-20251001"]
        self.assertEqual(older["efforts"], [])
        self.assertFalse(older["adaptive_thinking"])
        self.assertIn("code_execution", older["hosted_features"])
        self.assertNotIn("hosted_ptc", older["hosted_features"])

    def test_xai_detail_doc_and_language_models_agree(self):
        bundle = census.load_fixture(FIXTURE)
        grok = bundle["xai"]["observed"]["grok-4.7"]
        self.assertEqual(grok["context_length"], 500_000)
        self.assertEqual(grok["long_context_threshold"], 200_000)
        self.assertEqual(grok["efforts"], ["low", "medium", "high", "xhigh"])
        build = bundle["xai"]["observed"]["grok-build-0.1"]
        self.assertEqual(build["efforts"], [])
        self.assertEqual(build["context_length"], 256_000)
        self.assertEqual(bundle["xai"]["notes"], [])

    def test_xai_doc_page(self):
        parsed = census.parse_xai_docs((FIXTURE / "xai/docs/grok-4.3.md").read_text())
        self.assertEqual(parsed["id"], "grok-4.3")
        self.assertEqual(parsed["context_length"], 1_000_000)
        self.assertEqual(parsed["efforts"], ["none", "low", "medium", "high", "xhigh"])
        self.assertEqual(parsed["long_context_threshold"], 200_000)

    def test_sources_that_disagree_are_not_adopted(self):
        detail = {
            "id": "grok-4.7",
            "context_length": 500_000,
            "long_context_threshold": 200_000,
            "efforts": ["low", "high"],
        }
        doc = {
            "id": "grok-4.7",
            "context_length": 1_000_000,
            "long_context_threshold": 200_000,
            "efforts": ["low", "high"],
        }
        observed, conflicts = census.observe_xai([detail], [], {"grok-4.7": doc})
        self.assertNotIn("context_length", observed["grok-4.7"])
        self.assertTrue(any(change.kind == "conflict" for change in conflicts))


class RegenerateTests(unittest.TestCase):
    def test_snapshot_refreshes_todays_tables(self):
        bundle = census.load_fixture(FIXTURE)
        removed = []
        for provider in ("openai", "anthropic", "xai"):
            table = _table(provider)
            updated, changes = census.regenerate(
                provider,
                table,
                bundle[provider]["observed"],
                bundle[provider]["listed"],
                include_new=False,
                drop_removed=False,
                promote=set(),
                reviewed_on="2099-01-01",
            )
            self.assertEqual(census.dump_table(provider, updated), census.dump_table(provider, table))
            self.assertFalse(any(change.kind == "changed" for change in changes), provider)
            self.assertFalse(any(change.kind == "blocked" for change in changes), provider)
            removed.extend(
                f"{provider} {change.model_id}"
                for change in changes
                if change.kind == "removed"
            )
        self.assertEqual(
            removed,
            [
                "openai gpt-daybreak-blue-latest",
                "openai gpt-daybreak-red-latest",
            ],
        )

    def test_new_ids_stay_needs_review_and_do_not_promote(self):
        bundle = census.load_fixture(FIXTURE)
        table = _table("openai")
        _, changes = census.regenerate(
            "openai",
            table,
            bundle["openai"]["observed"],
            bundle["openai"]["listed"],
            include_new=False,
            drop_removed=False,
            promote=set(),
            reviewed_on=None,
        )
        fresh = [change for change in changes if change.kind == "new"]
        self.assertGreater(len(fresh), 0)
        for change in fresh:
            self.assertNotIn("usable", change.detail)
            self.assertTrue(
                change.detail.startswith("needs-review") or change.detail.startswith("not-coding"),
                change.detail,
            )

    def test_include_new_records_a_non_admitting_id_only(self):
        table = {
            "revision": "openai.api-models.test",
            "reviewed_at": "2026-10-09",
            "sources": ["test"],
            "models": [],
        }
        observed = {
            "gpt-5.4-mini": {
                "id": "gpt-5.4-mini",
                "max_input_tokens": 1_050_000,
                "max_tokens": 128_000,
                "efforts": ["low"],
                "price_threshold_input_tokens": None,
                "codex_context_window": None,
                "responses": True,
                "hosted_features": [],
            },
            "gpt-6-luna": {
                "id": "gpt-6-luna",
                "max_input_tokens": 922_000,
                "max_tokens": 128_000,
                "efforts": ["none", "low"],
                "price_threshold_input_tokens": 272_000,
                "codex_context_window": 272_000,
                "responses": True,
                "hosted_features": ["web_search"],
            },
        }
        listed = ["gpt-5.4-mini", "gpt-6-luna"]
        updated, changes = census.regenerate(
            "openai",
            table,
            observed,
            listed,
            include_new=True,
            drop_removed=False,
            promote=set(),
            reviewed_on="2026-10-10",
        )
        self.assertEqual([row["id"] for row in updated["models"]], ["gpt-5.4-mini"])
        self.assertEqual(updated["reviewed_at"], "2026-10-10")
        blocked = [change for change in changes if change.model_id == "gpt-6-luna"]
        self.assertIn("would admit", blocked[0].detail)
        written = next(change for change in changes if change.model_id == "gpt-5.4-mini")
        self.assertIn("needs-review included", written.detail)

    def test_promote_is_explicit(self):
        table = {
            "revision": "anthropic.api-models.test",
            "reviewed_at": "2026-10-09",
            "sources": ["test"],
            "models": [],
        }
        row = {
            "id": "claude-opus-5-9",
            "max_input_tokens": 1_000_000,
            "max_tokens": 128_000,
            "efforts": ["low", "high"],
            "adaptive_thinking": True,
            "price_threshold_input_tokens": None,
            "lifecycle": "active",
            "hosted_features": [],
        }
        updated, _changes = census.regenerate(
            "anthropic",
            table,
            {"claude-opus-5-9": row},
            ["claude-opus-5-9"],
            include_new=True,
            drop_removed=False,
            promote={"claude-opus-5-9"},
            reviewed_on="2026-10-10",
        )
        self.assertEqual(updated["models"], [row])
        self.assertTrue(census.admitting("anthropic", row))

    def test_changed_window_is_reported(self):
        table = _table("xai")
        observed = {
            row["id"]: {
                "context_length": row["context_length"],
                "long_context_threshold": row["long_context_threshold"],
                "efforts": list(row["efforts"]),
            }
            for row in table["models"]
        }
        observed["grok-4.7"]["context_length"] = 600_000
        _updated, changes = census.regenerate(
            "xai",
            table,
            observed,
            [row["id"] for row in table["models"]],
            include_new=False,
            drop_removed=False,
            promote=set(),
            reviewed_on=None,
        )
        self.assertTrue(
            any(change.kind == "changed" and change.model_id == "grok-4.7" for change in changes)
        )

    def test_models_dev_does_not_change_the_row(self):
        table = _table("openai")
        remote = {"gpt-6-luna": {"context": 1, "output": 1}}
        lines = census.models_dev_lines("openai", table, remote)
        self.assertEqual(lines[0].kind, "models.dev")
        self.assertIn("report only", lines[0].detail)
        updated, _changes = census.regenerate(
            "openai",
            table,
            {},
            [row["id"] for row in table["models"]],
            include_new=False,
            drop_removed=False,
            promote=set(),
            reviewed_on="2099-01-01",
        )
        self.assertEqual(updated["models"], table["models"])


class SafetyTests(unittest.TestCase):
    def test_error_text_drops_team_ids_and_keys(self):
        body = (
            b'{"error":{"message":"nope team_id=team_abc123secret sk-live-secretvalue",'
            b'"team_id":"team_abc123secret"}}'
        )
        text = census.public_error(403, body)
        self.assertNotIn("team_abc123secret", text)
        self.assertNotIn("sk-live", text)
        self.assertIn("HTTP 403", text)

    def test_probe_sends_schema_valid_efforts_and_parses_both_errors(self):
        body = census.effort_probe_body("gpt-6-luna", "minimal")
        self.assertEqual(body["input"], "x")
        self.assertEqual(body["max_output_tokens"], 16)
        self.assertFalse(body["store"])
        self.assertEqual(
            [census.effort_probe_body("gpt-6-luna", effort)["reasoning"]["effort"] for effort in census.PROBE_ORDER],
            ["minimal", "none", "max", "xhigh"],
        )
        with self.assertRaises(census.CensusError):
            census.effort_probe_body("gpt-6-luna", "plak-census")
        with self.assertRaises(census.CensusError):
            census.effort_probe_body("gpt-6-luna", "ultra")

        global_error = (
            "Supported values are: 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', and 'max'."
        )
        model_error = (
            "Unsupported value: 'minimal' is not supported with the 'gpt-6-luna' model. "
            "Supported values are: 'none', 'low', 'medium', 'high', 'xhigh', and 'max'."
        )
        global_reading = census.parse_probe_message(global_error)
        assert global_reading is not None
        self.assertEqual(global_reading["scope"], "global")
        self.assertIsNone(global_reading["model"])
        self.assertIn("minimal", global_reading["efforts"])
        model_reading = census.parse_probe_message(model_error)
        assert model_reading is not None
        self.assertEqual(model_reading["scope"], "model")
        self.assertEqual(model_reading["model"], "gpt-6-luna")
        self.assertEqual(
            model_reading["efforts"],
            ["none", "low", "medium", "high", "xhigh", "max"],
        )
        self.assertNotIn("minimal", model_reading["efforts"])

        named = census.reduce_probe_attempts("gpt-6-luna", [(400, "minimal", model_error)])
        self.assertEqual(named["ladder"], ["none", "low", "medium", "high", "xhigh", "max"])
        self.assertEqual(named["used"], ["minimal"])
        self.assertTrue(named["stopped"])

        global_only = census.reduce_probe_attempts("gpt-6-luna", [(400, "minimal", global_error)])
        self.assertIsNone(global_only["ladder"])
        self.assertFalse(global_only["stopped"])

        mixed = census.reduce_probe_attempts(
            "gpt-6-luna",
            [(200, "minimal", ""), (400, "none", model_error)],
        )
        self.assertEqual(mixed["supported_200"], ["minimal"])
        self.assertEqual(mixed["ladder"], ["none", "low", "medium", "high", "xhigh", "max"])
        self.assertEqual(mixed["used"], ["minimal", "none"])

        partial = census.reduce_probe_attempts(
            "gpt-6-luna",
            [(200, effort, "") for effort in census.PROBE_ORDER],
        )
        self.assertIsNone(partial["ladder"])
        self.assertEqual(partial["supported_200"], list(census.PROBE_ORDER))
        self.assertFalse(partial["stopped"])

        wrapped = census._probe_message(
            json.dumps({"error": {"message": model_error, "code": "unsupported_value"}}).encode()
        )
        self.assertEqual(census.parse_probe_message(wrapped), model_reading)
        continued = census.reduce_probe_attempts(
            "gpt-6-luna",
            [(400, "minimal", global_error), (400, "none", model_error)],
        )
        self.assertEqual(continued["ladder"], named["ladder"])
        self.assertEqual(continued["used"], ["minimal", "none"])

        page = (FIXTURE / "openai/docs/gpt-6-luna.md").read_text()
        observed = census.observe_openai(["gpt-6-luna"], {"gpt-6-luna": page}, None, {"gpt-6-luna": ["low"]})
        self.assertEqual(observed["gpt-6-luna"]["efforts"][0], "none")

    def test_key_loader_does_not_echo_the_secret(self):
        secret = "sk-test-secret-value-not-real"
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / "secrets").mkdir()
            (home / "secrets" / "openai.key").write_text(secret + "\n")
            self.assertEqual(census.load_key("openai", {}, home), secret)
            with self.assertRaises(census.CensusError) as caught:
                census.load_key("openai", {"OPENAI_API_KEY": " \n"}, home)
            self.assertNotIn(secret, str(caught.exception))
            (home / "secrets" / "openai.key").write_text("line-one\nline-two\n")
            with self.assertRaises(census.CensusError) as caught:
                census.load_key("openai", {}, home)
            self.assertNotIn("line-one", str(caught.exception))

    def test_fixture_has_no_team_or_key_material(self):
        for path in FIXTURE.rglob("*"):
            if not path.is_file():
                continue
            text = path.read_text(errors="replace")
            self.assertNotIn("team_", text, path.name)
            self.assertNotRegex(text, r"sk-[A-Za-z0-9]{8,}")


class CommandTests(unittest.TestCase):
    def test_fixture_command_does_not_rewrite_tables(self):
        with tempfile.TemporaryDirectory() as tmp:
            tables = Path(tmp)
            overrides = []
            originals = {}
            for provider, path in census.table_paths(ROOT).items():
                dest = tables / f"{provider}.json"
                dest.write_text(path.read_text())
                originals[provider] = path.read_text()
                overrides.extend(["--table", f"{provider}={dest}"])
            result = subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "scripts/api_model_census.py"),
                    "--fixture",
                    str(FIXTURE),
                    "--write",
                    *overrides,
                ],
                cwd=ROOT,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("anthropic revision=", result.stdout)
            self.assertIn("  unchanged", result.stdout)
            self.assertNotIn("changed ", result.stdout)
            self.assertIn("removed gpt-daybreak-blue-latest", result.stdout)
            for provider, path in census.table_paths(ROOT).items():
                self.assertEqual((tables / f"{provider}.json").read_text(), originals[provider])
                self.assertEqual(path.read_text(), originals[provider])


if __name__ == "__main__":
    unittest.main()
