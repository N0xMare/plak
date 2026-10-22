"""Offline checks for the live catalog, token telemetry and dry-run estimator."""

import json
import sys
import tempfile
import unittest
from decimal import Decimal
from io import StringIO
from pathlib import Path
from contextlib import redirect_stderr, redirect_stdout

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests" / "live"))

import estimate  # noqa: E402
import live  # noqa: E402


class CatalogTests(unittest.TestCase):
    def test_route_classes_and_endpoints_are_closed(self):
        for route in live.ROUTES.values():
            self.assertIn(route.route_class, live.ROUTE_CLASSES, route.name)
            self.assertTrue(route.host and route.path.startswith("/"), route.name)

    def test_xai_api_home_copies_the_shared_key_file(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            source = base / "source"
            (source / "secrets").mkdir(parents=True)
            (source / "secrets" / "xai.key").write_text("fixture-value\n")
            (source / "secrets" / "xai-api.key").write_text("wrong-slot\n")
            home = base / "home"
            live.write_home(home, live.ROUTES["api-xai"], source)
            copied = home / "secrets" / "xai.key"
            self.assertEqual(copied.read_text(), "fixture-value\n")
            self.assertFalse((home / "secrets" / "xai-api.key").exists())
            self.assertEqual(copied.stat().st_mode & 0o777, 0o600)
            agent = (home / "agents" / "root-agent.toml").read_text()
            self.assertIn('account = "xai-api"', agent)
            self.assertIn('model = "grok-4.7"', agent)
            self.assertIn('effort = "low"', agent)

class TelemetryTests(unittest.TestCase):
    def test_session_usage_sums_turn_usage_only(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            log = home / "state" / "r1" / "sessions" / "abc.events.jsonl"
            log.parent.mkdir(parents=True)
            log.write_text("\n".join(json.dumps(record) for record in (
                {"kind": "turn.lifecycle", "phase": "completed",
                 "input_tokens": 9, "output_tokens": 9},
                {"kind": "turn.usage", "input_tokens": 100, "output_tokens": 7,
                 "reasoning_tokens": 3, "cached_input_tokens": 40},
                {"kind": "turn.usage", "input_tokens": 5, "output_tokens": "nope"},
            )) + "\nnot json\n\"just a string\"\n")
            usage = live.session_usage(home)
            self.assertEqual(usage, live.TokenUsage(105, 7, 40, 3))
            self.assertEqual(live.session_usage(home / "missing"), live.TokenUsage())

    def test_jsonl_turn_event_keeps_cached_and_reasoning(self):
        turn = live.Turn()
        live.apply_usage_event(turn, {"kind": "turn", "input_tokens": 10, "output_tokens": 4,
                                      "cached_input_tokens": 6, "reasoning_tokens": 2,
                                      "cache_write_tokens": 99})
        live.apply_usage_event(turn, {"kind": "assistant", "input_tokens": 100})
        live.apply_usage_event(turn, {"kind": "turn", "input_tokens": True, "output_tokens": -1})
        self.assertEqual(turn.usage(), live.TokenUsage(10, 4, 6, 2))

    def test_usage_delta_is_what_one_scenario_added(self):
        before = live.TokenUsage(10, 1, 4, 0)
        after = live.TokenUsage(25, 3, 4, 2)
        self.assertEqual(live.usage_delta(after, before).as_dict(), {
            "input_tokens": 15, "output_tokens": 2,
            "cached_input_tokens": 0, "reasoning_tokens": 2,
        })


class EstimateTests(unittest.TestCase):
    def assumptions(self) -> estimate.Assumptions:
        return estimate.Assumptions(
            smoke_input_tokens=1_000_000,
            smoke_output_tokens=500_000,
            smoke_cache_share=Decimal("0.25"),
            smoke_cache_write_tokens=100_000,
            contingency=Decimal("0.25"),
        )

    def cell(self, name: str, model: str, route_class: str = live.LIGHT,
             account: str = "openrouter", lane: str = "smoke") -> estimate.Cell:
        return estimate.Cell("mac", lane, name, route_class, model, account)

    def test_price_splits_cached_input_and_applies_contingency_once(self):
        rates = {"priced": estimate.Rate(Decimal(2), Decimal(4), Decimal("0.5"), Decimal(1))}
        report = estimate.estimate_matrix(
            [self.cell("a", "priced"), self.cell("b", "missing")],
            rates, self.assumptions())
        self.assertEqual(report.subtotal, Decimal("3.725"))
        self.assertEqual(report.rerun_reserve, Decimal("3.725"))
        self.assertEqual(report.contingency_usd, Decimal("1.86250"))
        self.assertEqual(report.total, Decimal("9.31250"))
        self.assertEqual(report.cells[0].usd, Decimal("3.725"))
        self.assertEqual(report.cells[0].with_contingency(report.contingency), report.total)
        self.assertEqual(report.cells[0].charge.cached_input_tokens, 250_000)
        self.assertIsNone(report.cells[1].usd)
        self.assertEqual(report.cells[1].billing, "unpriced")
        self.assertEqual(report.unpriced_models, ("missing",))
        rendered = estimate.render(report)
        self.assertIn("total 9.312500", rendered)
        self.assertIn("unpriced missing", rendered)
        self.assertIn("openrouter_subtotal 3.725000", rendered)

    def test_cache_share_outside_zero_to_one_is_refused(self):
        assumptions = estimate.Assumptions(smoke_cache_share=Decimal("1.1"), contingency=Decimal(0))
        with self.assertRaisesRegex(estimate.EstimateError, "cache share"):
            estimate.estimate_matrix(
                [self.cell("a", "priced")],
                {"priced": estimate.Rate(Decimal(1), Decimal(1), Decimal(1), Decimal(0))},
                assumptions)

    def test_subscription_stays_quota_when_the_card_names_the_model(self):
        rate = estimate.Rate(Decimal(1), Decimal(2), Decimal(0), Decimal(0))
        rates = {"grok-4.7": rate, "gpt-6-luna": rate, "claude-haiku-5-5": rate}
        assumptions = estimate.Assumptions(smoke_input_tokens=1_000_000, smoke_output_tokens=0,
                                          smoke_cache_share=Decimal(0), contingency=Decimal(0))
        report = estimate.estimate_matrix([
            self.cell("grok", "grok-4.7", account="grok"),
            self.cell("api-xai", "grok-4.7", account="xai-api"),
            self.cell("api-openai", "gpt-6-luna", account="openai"),
            self.cell("api-openai-medium", "gpt-6-luna", account="openai"),
            self.cell("api-anthropic", "claude-haiku-5-5", account="anthropic"),
        ], rates, assumptions)
        self.assertEqual(report.cells[0].billing, "quota")
        self.assertIsNone(report.cells[0].usd)
        self.assertEqual([item.usd for item in report.cells[1:]],
                         [Decimal(1), Decimal(1), Decimal(1), Decimal(1)])
        self.assertEqual(report.subtotal, Decimal(4))
        self.assertEqual(report.openrouter_subtotal, Decimal(0))
        self.assertIn("grok-4.7", report.subscription_models)
        self.assertNotIn("grok-4.7", report.unpriced_models)

    def test_openrouter_ceiling_warns_without_dropping_cells(self):
        rates = {"deepseek/deepseek-v4.1-flash-1017": estimate.Rate(
            Decimal(1), Decimal(0), Decimal(0), Decimal(0))}
        assumptions = estimate.Assumptions(
            smoke_input_tokens=1_000_000, smoke_output_tokens=0,
            smoke_cache_share=Decimal(0), contingency=Decimal(0),
            openrouter_ceiling=Decimal("0.5"))
        cells = [
            self.cell("deepseek", "deepseek/deepseek-v4.1-flash-1017"),
            self.cell("codex", "gpt-6-luna", account="codex"),
        ]
        report = estimate.estimate_matrix(cells, rates, assumptions)
        self.assertEqual(report.openrouter_subtotal, Decimal(1))
        self.assertTrue(report.exceeds_openrouter_ceiling)
        self.assertEqual(len(report.cells), 2)
        self.assertIn("warning: openrouter estimate exceeds ceiling", estimate.render(report))

    def test_lean_plan_has_one_soak_two_extra_crashes_and_optional_half_hour_tui(self):
        with self.assertRaisesRegex(estimate.EstimateError, "soak-row"):
            estimate.planned_cells()
        for invalid in ("nope", "api-openai", "claude"):
            with self.assertRaises(estimate.EstimateError):
                estimate.planned_cells(soak_row=invalid)
        cells = estimate.planned_cells(soak_row="grok")
        self.assertEqual([(c.platform, c.route_name) for c in cells if c.lane == "soak"],
                         [("mac", "grok")])
        self.assertFalse(any(c.lane == "tui-soak" for c in cells))
        crashes = [c for c in cells if c.lane == "crash-jobs"]
        self.assertEqual(len(crashes), 5)
        for c in crashes:
            self.assertEqual(estimate.lane_charge(c).input_tokens,
                             (estimate.JOBS_INPUT * 2 + 5) // 11)
        optional = estimate.planned_cells(soak_row="grok", tui_rows=("codex", "deepseek"))
        self.assertEqual([c.route_name for c in optional if c.lane == "tui-soak"],
                         ["codex", "deepseek"])
        self.assertEqual(estimate.lane_charge(next(c for c in optional if c.lane == "tui-soak")).input_tokens,
                         estimate.TUI_INPUT // 4)
        for rows in (("nope",), ("grok", "grok"), ("api-openai",)):
            with self.assertRaises(estimate.EstimateError):
                estimate.planned_cells(soak_row="grok", tui_rows=rows)

    def test_long_session_scales_the_measured_deepseek_cell(self):
        assumptions = estimate.Assumptions()

        def charge(route: str) -> estimate.Charge:
            cell = next(item for item in estimate.planned_cells(soak_row="deepseek")
                        if item.route_name == route and item.lane == "long-session")
            return estimate.lane_charge(cell, assumptions)

        def scaled(total: int, model: str) -> int:
            return estimate.scaled_long_session(
                total, estimate.input_window(model), assumptions.long_session_measured_window)

        deepseek = "deepseek/deepseek-v4.1-flash-1017"
        self.assertEqual(charge("deepseek").input_tokens, 750_415)
        self.assertEqual(charge("deepseek").output_tokens, 4_673)
        self.assertEqual(charge("deepseek").cached_input_tokens,
                         int((Decimal(750_415) * Decimal("0.22")).to_integral_value()))
        self.assertEqual(charge("deepseek-messages").input_tokens, charge("deepseek").input_tokens)
        self.assertEqual(charge("grok").input_tokens, scaled(750_415, "grok-4.7"))
        self.assertEqual(charge("codex").input_tokens, scaled(750_415, "gpt-6-luna"))
        self.assertEqual(charge("claude-sonnet55-low").input_tokens,
                         scaled(750_415, "claude-sonnet-5-5"))
        self.assertLess(charge("grok").input_tokens, charge("deepseek").input_tokens)
        jobs = estimate.lane_charge(self.cell("codex", "gpt-6-luna", account="codex", lane="jobs"))
        self.assertEqual(jobs.input_tokens, estimate.JOBS_INPUT)
        self.assertEqual(jobs.output_tokens, estimate.JOBS_OUTPUT)
        self.assertEqual(estimate.JOBS_INPUT, (241_039 // 12) * 11)

    def test_rate_card_is_read_from_a_local_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rates.json"
            path.write_text(json.dumps({
                "source": "https://openrouter.ai/api/v1/models",
                "unit": "USD per million tokens",
                "models": {"not-a-route": {
                    "input": 1, "output": 2, "cache_read": 0, "cache_write": 0,
                    "context_length": 99,
                }},
            }))
            rates = estimate.load_rate_card(path)
            self.assertEqual(rates["not-a-route"].input, Decimal(1))
            path.write_text(json.dumps({"unit": "USD", "models": {}}))
            with self.assertRaisesRegex(estimate.EstimateError, "unit"):
                estimate.load_rate_card(path)
            buffer = StringIO()
            with redirect_stdout(buffer), redirect_stderr(StringIO()):
                code = estimate.main(["--rate-card", str(path.parent / "missing.json"), "--soak-row", "deepseek"])
            self.assertEqual(code, 2)
            path.write_text(json.dumps({
                "unit": "USD per million tokens",
                "models": {"not-a-route": {
                    "input": 1, "output": 1, "cache_read": 0, "cache_write": 0,
                }},
            }))
            buffer = StringIO()
            with redirect_stdout(buffer):
                code = estimate.main([
                    "--rate-card", str(path), "--soak-row", "deepseek", "--contingency", "0",
                    "--smoke-input", "0", "--smoke-output", "0",
                ])
            self.assertEqual(code, 0)
            text = buffer.getvalue()
            self.assertIn("total 0.000000", text)
            self.assertIn("deepseek/deepseek-v4.1-flash-1017", text)
            self.assertIn("api-openai-medium", text)
            self.assertIn("claude-haiku-5-5", text)
            self.assertIn("grok-4.7", text)
            self.assertNotIn("gpt-5.6-luna", text)


if __name__ == "__main__":
    unittest.main()
