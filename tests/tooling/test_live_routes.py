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
import jobs  # noqa: E402
import live  # noqa: E402
import long_session  # noqa: E402
import smoke  # noqa: E402


class CatalogTests(unittest.TestCase):
    def test_route_classes_and_endpoints_are_closed(self):
        for route in live.ROUTES.values():
            self.assertIn(route.route_class, live.ROUTE_CLASSES, route.name)
            self.assertTrue(route.host and route.path.startswith("/"), route.name)

    def test_full_rows_are_the_owner_matrix(self):
        full = {name for name, route in live.ROUTES.items()
                if route.planned and route.route_class == live.FULL}
        self.assertEqual(full, {
            "codex", "grok", "claude-sonnet55-low", "deepseek", "deepseek-messages",
        })
        self.assertEqual(live.ROUTES["codex"].model, "gpt-6-luna")
        self.assertEqual(live.ROUTES["grok"].model, "grok-4.7")
        self.assertEqual(live.ROUTES["claude-sonnet55-low"].model, "claude-sonnet-5-5")
        deepseek = live.ROUTES["deepseek"]
        messages = live.ROUTES["deepseek-messages"]
        self.assertEqual(deepseek.model, "deepseek/deepseek-v4.1-flash")
        self.assertEqual(messages.model, deepseek.model)
        self.assertIsNone(deepseek.protocol)
        self.assertEqual(messages.protocol, "messages")
        self.assertEqual(deepseek.path, "/api/v1/chat/completions")
        self.assertEqual(messages.path, "/api/v1/messages")

    def test_subscription_light_rows_use_owner_spellings(self):
        expected = {
            "codex-luna-medium": ("codex", "gpt-6-luna", "medium"),
            "codex-sol": ("codex", "gpt-6.1-sol", "low"),
            "codex-astra": ("codex", "gpt-6-astra", "low"),
            "grok-46": ("grok", "grok-4.6", "low"),
            "claude-haiku55-low": ("claude", "claude-haiku-5-5", "low"),
            "claude-haiku55-medium": ("claude", "claude-haiku-5-5", "medium"),
            "claude-sonnet55-medium": ("claude", "claude-sonnet-5-5", "medium"),
            "claude-opus-low": ("claude", "claude-opus-5-5", "low"),
            "claude-opus-medium": ("claude", "claude-opus-5-5", "medium"),
            "claude-fable": ("claude", "claude-fable-5-1", "low"),
        }
        for name, (account, model, effort) in expected.items():
            route = live.ROUTES[name]
            self.assertEqual((route.account, route.model, route.effort), (account, model, effort))
            self.assertEqual(route.route_class, live.LIGHT)
            self.assertTrue(route.planned)
            self.assertIsNone(route.protocol)

    def test_openrouter_informational_and_light_ids(self):
        informational = {
            "or-nemotron-free": "nvidia/nemotron-3-ultra-550b-a55b:free",
            "or-laguna-free": "poolside/laguna-s-2.1:free",
            "or-lfm-free": "liquid/lfm-2.5-2.6b:free",
            "or-ling": "inclusionai/ling-3.0-flash-vl",
            "or-qwen38": "qwen/qwen3.8-27b",
        }
        light = {
            "or-mimo": "xiaomi/mimo-v2.6-flash",
            "or-glm": "z-ai/glm-5.3-flash",
            "or-qwen37": "qwen/qwen3.7-flash",
        }
        for name, model in informational.items():
            route = live.ROUTES[name]
            self.assertEqual(route.model, model)
            self.assertEqual(route.route_class, live.INFORMATIONAL)
            self.assertIsNone(route.protocol)
            self.assertEqual(route.path, "/api/v1/chat/completions")
        for name, model in light.items():
            route = live.ROUTES[name]
            self.assertEqual(route.model, model)
            self.assertEqual(route.route_class, live.LIGHT)
            self.assertIsNone(route.protocol)
        messages = [route.name for route in live.ROUTES.values()
                    if route.protocol == "messages"]
        self.assertEqual(messages, ["deepseek-messages"])
        self.assertFalse(live.ROUTES["or-ling"].model.endswith(":free"))
        self.assertFalse(live.ROUTES["or-qwen38"].model.endswith(":free"))

    def test_api_light_routes_are_the_enrolled_choices(self):
        expected = {
            "api-openai": ("openai", "gpt-6-luna", "low", "api.openai.com", "/v1/responses",
                           "OPENAI_API_KEY", "openai.key"),
            "api-openai-medium": ("openai", "gpt-6-luna", "medium", "api.openai.com",
                                  "/v1/responses", "OPENAI_API_KEY", "openai.key"),
            "api-anthropic": ("anthropic", "claude-haiku-5-5", "medium", "api.anthropic.com",
                              "/v1/messages", "ANTHROPIC_API_KEY", "anthropic.key"),
            "api-xai": ("xai-api", "grok-4.7", "low", "api.x.ai", "/v1/responses",
                        "XAI_API_KEY", "xai.key"),
        }
        for name, (account, model, effort, host, path, env, filename) in expected.items():
            route = live.ROUTES[name]
            self.assertEqual(route.flags(), ["--account", account, "--model", model,
                                             "--effort", effort])
            self.assertEqual((route.host, route.path), (host, path))
            self.assertEqual(route.route_class, live.LIGHT)
            self.assertTrue(route.planned)
            self.assertIsNone(route.protocol)
            self.assertEqual(live.API_KEY_ENV[account], env)
            self.assertEqual(live.api_key_filename(account), filename)
        self.assertEqual(
            live.fault_plan(live.ROUTES["api-anthropic"], "429", retry_after=2),
            "host=api.anthropic.com,path=/v1/messages,nth=1,fault=429,retry-after=2")

    def test_legacy_sonnet5_routes_stay_runnable_but_unplanned(self):
        for name in ("claude", "claude-sonnet-medium"):
            route = live.ROUTES[name]
            self.assertEqual(route.model, "claude-sonnet-5")
            self.assertFalse(route.planned)
            self.assertEqual(route.route_class, live.LIGHT)

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

    def test_route_report_names_the_bar_composition(self):
        report = live.route_report(live.ROUTES["codex"])
        self.assertEqual(report["composition"], "plakous-default")
        self.assertEqual(live.route_report(live.ROUTES["codex"], composition="py-ptc")["composition"],
                         "py-ptc")

    def test_readme_names_every_route(self):
        text = (ROOT / "tests" / "live" / "README.md").read_text()
        for route in live.ROUTES.values():
            self.assertIn(f"`{route.name}`", text, route.name)
            self.assertIn(route.model, text, route.name)

    def test_bar_names_per_row_rerun_and_the_full_cell_commands(self):
        spec = (ROOT / "docs" / "spec" / "05-testing-and-evidence.md").read_text()
        readme = (ROOT / "tests" / "live" / "README.md").read_text()
        self.assertNotIn("whole matrix again", spec)
        self.assertIn("carries the earlier pass", spec)
        self.assertIn("class depth", spec)
        self.assertIn("pack and family render pins", spec)
        for stem in ("codex", "grok", "claude", "openrouter-chat", "openrouter-messages",
                     "openai-api", "anthropic-api", "xai-api"):
            self.assertIn(f"`{stem}`", spec)
        commands = (
            "python3 tests/live/qualification.py smoke <route>",
            "python3 tests/live/qualification.py long-session <route> --all",
            "python3 tests/live/qualification.py jobs <route>",
            "python3 tests/live/qualification.py jobs <route> --only crash_jobs",
            "python3 tests/live/qualification.py soak <route> --hours 1",
            "just daily-driver --since <date>",
        )
        for command in commands:
            self.assertIn(command, spec)
            self.assertIn(command, readme)
        for text in (spec, readme):
            text = " ".join(text.split())
            self.assertIn("three crash_jobs runs", text)
            self.assertIn("least-used full row only", text)
            self.assertIn("per release candidate, not per install", text)
            self.assertIn("TUI review replaces it", text)
        for name in ("api-openai", "api-openai-medium", "api-anthropic", "api-xai"):
            self.assertIn(f"`{name}`", spec)
            self.assertIn("not best-effort", spec)


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
        rates = {"deepseek/deepseek-v4.1-flash": estimate.Rate(
            Decimal(1), Decimal(0), Decimal(0), Decimal(0))}
        assumptions = estimate.Assumptions(
            smoke_input_tokens=1_000_000, smoke_output_tokens=0,
            smoke_cache_share=Decimal(0), contingency=Decimal(0),
            openrouter_ceiling=Decimal("0.5"))
        cells = [
            self.cell("deepseek", "deepseek/deepseek-v4.1-flash"),
            self.cell("codex", "gpt-6-luna", account="codex"),
        ]
        report = estimate.estimate_matrix(cells, rates, assumptions)
        self.assertEqual(report.openrouter_subtotal, Decimal(1))
        self.assertTrue(report.exceeds_openrouter_ceiling)
        self.assertEqual(len(report.cells), 2)
        self.assertIn("warning: openrouter estimate exceeds ceiling", estimate.render(report))

    def test_linux_cells_are_full_rows_and_api_light_only(self):
        linux = {(cell.route_name, cell.lane)
                 for cell in estimate.planned_cells(soak_row="deepseek") if cell.platform == "linux"}
        self.assertEqual(linux, {
            ("codex", "smoke"),
            ("grok", "smoke"),
            ("claude-sonnet55-low", "smoke"),
            ("deepseek", "smoke"),
            ("deepseek-messages", "smoke"),
            ("api-openai", "smoke"),
            ("api-openai-medium", "smoke"),
            ("api-anthropic", "smoke"),
            ("api-xai", "smoke"),
        })
        mac = [cell for cell in estimate.planned_cells(soak_row="deepseek")
               if cell.platform == "mac" and cell.route_name == "codex"]
        self.assertEqual([cell.lane for cell in mac],
                         ["smoke", "long-session", "jobs", "crash-jobs"])
        self.assertTrue(all(cell.lane == "smoke" for cell in estimate.planned_cells(soak_row="deepseek")
                            if cell.platform == "linux"))
        informational = [cell for cell in estimate.planned_cells(soak_row="deepseek")
                         if cell.route_name == "or-nemotron-free"]
        self.assertEqual([(cell.platform, cell.lane) for cell in informational], [("mac", "smoke")])
        self.assertTrue(all(cell.route_name not in ("claude", "claude-sonnet-medium")
                            for cell in estimate.planned_cells(soak_row="deepseek")))

    def test_defaults_match_the_recorded_volumes_and_the_drivers(self):
        assumptions = estimate.Assumptions()
        self.assertEqual(len(smoke.Smoke.SCENARIOS), 12)
        self.assertEqual(len(jobs.SCENARIOS), estimate.JOBS_SCENARIOS)
        names = {name for name, _surface in long_session.CATALOG}
        self.assertEqual(names, {"in_turn_compaction", "overflow", "kill_reopen", "child_compaction"})
        self.assertTrue(set(estimate.GROWTH_SCENARIOS) <= names)
        self.assertEqual(assumptions.smoke_input_tokens, 241_039)
        self.assertEqual(assumptions.smoke_output_tokens, 5_500)
        self.assertEqual(assumptions.smoke_cache_share, Decimal("0.64"))
        self.assertEqual(assumptions.soak_input_tokens, 5_600_000)
        self.assertEqual(assumptions.soak_output_tokens, 60_000)
        self.assertEqual(assumptions.soak_cache_share, Decimal("0.9"))
        self.assertEqual(assumptions.tui_soak_input_tokens, 8_600_000)
        self.assertEqual(assumptions.tui_cache_share, Decimal("0.9"))
        self.assertEqual(assumptions.long_session_input_tokens, 750_415)
        self.assertEqual(assumptions.long_session_output_tokens, 4_673)
        self.assertEqual(assumptions.long_session_cache_share, Decimal("0.22"))
        self.assertEqual(assumptions.jobs_cache_share, Decimal("0.64"))

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

        deepseek = "deepseek/deepseek-v4.1-flash"
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

    def test_rate_card_is_local_and_the_script_does_not_fetch(self):
        source = (ROOT / "tests" / "live" / "estimate.py").read_text()
        for banned in ("urllib", "requests", "urlopen", "socket", "2026-10-08-openrouter-rates"):
            self.assertNotIn(banned, source)
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
            self.assertIn("deepseek/deepseek-v4.1-flash", text)
            self.assertIn("api-openai-medium", text)
            self.assertIn("claude-haiku-5-5", text)
            self.assertIn("grok-4.7", text)
            self.assertNotIn("gpt-5.6-luna", text)


if __name__ == "__main__":
    unittest.main()
