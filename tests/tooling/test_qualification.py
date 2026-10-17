"""Qualification retries use synthetic reports, never a binary or provider."""

import json
import sys
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests/live"))

import estimate
import live
import qualification
import jobs
import long_session
import smoke
import soak
import tui_soak


class QualificationTests(unittest.TestCase):
    def run_cell(self, outcomes):
        reports = []
        homes = []

        def attempt(path):
            path.mkdir()
            homes.append(path / "home")
            outcome = outcomes[len(reports)]
            report = {
                "capture_complete": True,
                "binary_sha256_start": "build", "binary_sha256_end": "build",
                "evidence": {"recall": {"capture_complete": True, "segments": ["capture"], "captured_exchanges": 1}},
                "results": [{"scenario": "recall", "outcome": outcome,
                             "detail": "capture proves fact was in input" if outcome == live.MODEL
                             else "synthetic " + outcome}],
            }
            reports.append(report)
            return (0 if outcome == live.PASS else 1), report

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "cell"
            result = qualification.run_cell(root, attempt)
            saved = json.loads((root / "report.json").read_text())
        self.assertEqual(result, saved)
        self.assertEqual(len(homes), len(set(homes)))
        return result, reports

    def test_model_then_pass_is_distinct_and_keeps_both_records(self):
        result, reports = self.run_cell([live.MODEL, live.PASS])
        self.assertEqual(result["outcome"], live.PASS)
        self.assertEqual(result["status"], "PASS after MODEL rerun")
        self.assertEqual([a["report"] for a in result["attempts"]], reports)
        self.assertIn("capture proves", result["attempts"][0]["reason"])

    def test_model_then_model_fails_and_never_tries_a_third_attempt(self):
        result, reports = self.run_cell([live.MODEL, live.MODEL, live.PASS])
        self.assertEqual(result["outcome"], live.MODEL)
        self.assertEqual(len(reports), 2)

    def test_harness_is_never_rerun(self):
        result, reports = self.run_cell([live.HARNESS, live.PASS])
        self.assertEqual(result["outcome"], live.HARNESS)
        self.assertEqual(len(reports), 1)

    def test_pending_is_never_rerun(self):
        result, reports = self.run_cell([live.PENDING, live.PASS])
        self.assertEqual(result["outcome"], live.PENDING)
        self.assertEqual(len(reports), 1)

    def test_provider_rule_is_unchanged_and_cannot_chain_model_rerun(self):
        for final in (live.PASS, live.PROVIDER, live.MODEL, live.HARNESS):
            with self.subTest(final=final):
                result, reports = self.run_cell([live.PROVIDER, final, live.PASS])
                self.assertEqual(result["outcome"], final)
                self.assertEqual(len(reports), 2)

    def test_model_rerun_is_final_even_when_provider(self):
        result, reports = self.run_cell([live.MODEL, live.PROVIDER, live.PASS])
        self.assertEqual(result["outcome"], live.PROVIDER)
        self.assertEqual(len(reports), 2)

    def test_clean_pass_is_not_retried(self):
        result, reports = self.run_cell([live.PASS])
        self.assertEqual(result["status"], "PASS")
        self.assertEqual(len(reports), 1)

    def test_missing_capture_or_changed_build_is_harness(self):
        for changes in ({"capture_complete": False}, {"binary_sha256_end": "other"}):
            report = {"capture_complete": True, "binary_sha256_start": "build",
                      "binary_sha256_end": "build", "results": [
                          {"outcome": live.MODEL, "detail": "cannot recall"}], **changes}
            self.assertEqual(qualification.score_report(1, report)[0], live.HARNESS)

    def test_dry_run_prints_same_command_and_extra_attempt_without_running(self):
        with patch("qualification.execute") as execute, patch("builtins.print") as output:
            self.assertEqual(qualification.main(["smoke", "codex", "--dry-run",
                                                  "--only", "compaction"]), 0)
        execute.assert_not_called()
        text = "\n".join(str(call.args) for call in output.call_args_list)
        self.assertIn("2 attempts", text)
        self.assertIn("MODEL", text)
        self.assertIn("PROVIDER", text)
        self.assertIn("compaction", text)

    def test_estimate_reserves_one_extra_attempt_explicitly(self):
        cell = estimate.Cell("mac", "smoke", "api", live.LIGHT, "model", "openai")
        rate = estimate.Rate(Decimal(1), Decimal(0), Decimal(1), Decimal(0))
        assumptions = estimate.Assumptions(smoke_input_tokens=1_000_000,
                                          smoke_output_tokens=0, contingency=Decimal(0))
        report = estimate.estimate_matrix([cell], {"model": rate}, assumptions)
        self.assertEqual(report.subtotal, Decimal(1))
        self.assertEqual(report.rerun_reserve, Decimal(1))
        self.assertEqual(report.total, Decimal(2))
        self.assertIn("rerun_reserve 1.000000", estimate.render(report))

    def test_real_smoke_driver_replays_route_build_prompt_in_fresh_home(self):
        seen = []

        def scenario(runner, name):
            segment = runner.env.artifacts / "capture"
            segment.mkdir(parents=True)
            (segment / "exchange.info").write_text("synthetic")
            runner.env.capture_segments.append(segment)
            seen.append((runner.route, runner.env.binary, runner.env.home,
                         runner.env.fixture_seed, name))
            if len(seen) == 1:
                raise live.ScenarioFailure(live.MODEL, "capture proves lost fact")
            return "fact recalled"

        with tempfile.TemporaryDirectory() as directory, \
                patch("smoke.selected_scenarios", return_value=(("compaction", "jsonl"),)), \
                patch("smoke.run_named", side_effect=scenario), \
                patch("smoke.capture_complete", return_value=True), \
                patch("live.write_home"), patch("live.binary_sha256", return_value="build"), \
                patch("live.capture_root_mismatch", return_value=None):
            binary = Path(directory) / "plak"
            result = qualification.run_cell(Path(directory) / "cell", lambda path:
                qualification.execute("smoke", "codex", binary, ["--only", "compaction"], 17, path))
        self.assertEqual(result["status"], "PASS after MODEL rerun")
        self.assertEqual(seen[0][:2], seen[1][:2])
        self.assertEqual(seen[0][3:], seen[1][3:])
        self.assertEqual(seen[0][3], 17)
        self.assertNotEqual(seen[0][2], seen[1][2])

    def test_randomized_long_session_and_jobs_inputs_replay(self):
        route = live.ROUTES["grok"]
        env = live.Env(Path("plak"), Path("home"), Path("artifacts"))
        env.fixture_seed = 123
        first = long_session.LongSession(route, env)
        second = long_session.LongSession(route, env)
        self.assertEqual(first.random_bytes(4), second.random_bytes(4))
        self.assertEqual(long_session.base64_line(first.random_bytes),
                         long_session.base64_line(second.random_bytes))
        self.assertEqual(long_session.digit_line(first.random_bytes),
                         long_session.digit_line(second.random_bytes))
        first_jobs = jobs.Jobs(route, env)
        second_jobs = jobs.Jobs(route, env)
        self.assertEqual(first_jobs.fresh_token("job"), second_jobs.fresh_token("job"))

    def test_best_effort_provider_waiver_does_not_apply_to_model_rerun(self):
        for initial, expected in ((live.PROVIDER, True), (live.MODEL, False)):
            result, _ = self.run_cell([initial, live.PROVIDER])
            self.assertEqual(qualification.qualifies(live.ROUTES["or-mimo"], result), expected)

    def test_driver_error_during_rerun_is_recorded_harness(self):
        with tempfile.TemporaryDirectory() as directory:
            report = {"capture_complete": True, "binary_sha256_start": "build",
                      "binary_sha256_end": "build", "results": [
                          {"outcome": live.MODEL, "task": "recall", "segment": "capture",
                           "captured_exchanges": 1, "evidence_status": "response",
                           "detail": "capture proves lost fact"}]}
            with patch("builtins.print"), patch("qualification.score_report",
                                                wraps=qualification.score_report):
                calls = []

                def attempt(path):
                    calls.append(path)
                    if len(calls) == 2:
                        raise OSError("driver report unavailable")
                    return 1, report

                result = qualification.run_cell(Path(directory) / "cell", attempt)
        self.assertEqual(result["outcome"], live.HARNESS)
        self.assertEqual(len(result["attempts"]), 2)
        self.assertIn("driver report unavailable", result["attempts"][1]["report"]["driver_error"])

    def test_soak_rerun_starts_on_retained_original_build(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "target/release/plak"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"original build")
            starts = []

            def execute(lane, route, used_binary, argv, seed, path):
                path.mkdir()
                starts.append(used_binary.read_bytes())
                start = live.binary_sha256(used_binary)
                used_binary.write_bytes(b"intentional rebuild")
                outcome = live.MODEL if len(starts) == 1 else live.PASS
                return (1 if outcome == live.MODEL else 0), {
                    "summary": {"capture_complete": True, "rebuild": "changed",
                                "binary_sha256_start": start,
                                "binary_sha256_end": live.binary_sha256(used_binary),
                                "binary_sha256_rebuilt": live.binary_sha256(used_binary)},
                    "records": [{"outcome": outcome, "task": "recall", "segment": "capture",
                                 "captured_exchanges": 1, "evidence_status": "response",
                                 "errors": ["capture proves lost fact"]}],
                }

            with patch.object(live, "ROOT", root), \
                    patch("qualification.execute", side_effect=execute), patch("builtins.print"):
                code = qualification.main(["soak", "codex", "--binary", str(binary)])
            self.assertEqual(code, 0)
            self.assertEqual(starts, [b"original build", b"original build"])

    def test_aborted_soak_keeps_model_or_provider_score_before_rebuild(self):
        for outcome in (live.MODEL, live.PROVIDER):
            report = {"summary": {"capture_complete": True, "rebuild": "failed",
                                  "rebuild_attempted": False,
                                  "binary_sha256_start": "build", "binary_sha256_end": "build"},
                      "records": [{"outcome": outcome, "task": "recall", "segment": "capture",
                                   "captured_exchanges": 1, "evidence_status": "response",
                                   "errors": ["capture-proven failure"]}]}
            self.assertEqual(qualification.score_report(1, report)[0], outcome)
            report["summary"]["rebuild_attempted"] = True
            self.assertEqual(qualification.score_report(1, report)[0], live.HARNESS)

class QualificationReviewTests(unittest.TestCase):

    def report(self, outcome=live.PASS):
        return {"summary": {"capture_complete": True, "binary_sha256_start": "start",
                            "binary_sha256_end": "start"},
                "records": [{"task": "repair", "outcome": outcome, "segment": "segment-001",
                             "captured_exchanges": 2, "evidence_status": "response",
                             "errors": ["answer oracle failed"]}]}

    def test_dead_end_is_harness_without_rerun(self):
        for changes in ({"dead_end": True}, {"outcome": "dead-end after stream:reset"},
                        {"dead_ends": 1}):
            with self.subTest(changes=changes), tempfile.TemporaryDirectory() as directory:
                report = self.report(live.MODEL)
                if "dead_ends" in changes:
                    report["summary"].update(changes)
                else:
                    report["records"][0].update(changes)
                attempt = Mock(side_effect=[(1, report), (0, self.report())])
                result = qualification.run_cell(Path(directory), attempt)
                self.assertEqual(result["outcome"], live.HARNESS)
                self.assertEqual(attempt.call_count, 1)

    def test_soak_model_suffix_with_exit_zero_is_model(self):
        report = self.report("model after stream:reset")
        outcome, reason = qualification.score_report(0, report)
        self.assertEqual(outcome, live.MODEL)
        for witness in ("repair", "response", "2", "segment-001"):
            self.assertIn(witness, reason)

    def test_model_without_capture_reference_is_harness(self):
        for field in ("task", "segment", "captured_exchanges"):
            report = self.report(live.MODEL)
            del report["records"][0][field]
            with self.subTest(field=field):
                self.assertEqual(qualification.score_report(0, report)[0], live.HARNESS)
        report = self.report(live.MODEL)
        report["records"][0]["evidence_status"] = "unexplained_no_response"
        self.assertEqual(qualification.score_report(0, report)[0], live.HARNESS)
        report = self.report()
        report["summary"]["model_misses"] = 1
        self.assertEqual(qualification.score_report(0, report)[0], live.HARNESS)

    def test_tui_model_miss_names_its_task_capture(self):
        report = self.report()
        report["summary"]["model_misses"] = 1
        report["records"][0].update(phase="completed", oracle=False)
        outcome, reason = qualification.score_report(0, report)
        self.assertEqual(outcome, live.MODEL)
        self.assertIn("repair", reason)
        self.assertIn("segment-001", reason)

    def test_soak_task_records_its_capture_segment(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = live.Env(root / "plak", root / "home", root)
            env._capture_root = root
            runner = soak.Soak(live.ROUTES["codex"], env, root)
            turn = live.Turn(phase="completed", text="wrong answer")
            agent = Mock(capture_dir=root / "segment-001", proc=Mock(pid=1),
                         prompt=Mock(return_value=turn))

            def open_agent(faults):
                runner.segment += 1
                return agent

            with patch.object(runner, "open", side_effect=open_agent), \
                    patch("soak.check", return_value=False), \
                    patch("soak.segment_evidence", return_value=(2, [], "response")):
                record = runner.run_task(soak.Task("repair", "fix the bug"), 1, None, False)
            self.assertEqual(record.outcome, live.MODEL)
            self.assertEqual(record.segment, str(agent.capture_dir))

    def test_scenario_model_reason_names_capture(self):
        report = self.report()
        report["results"] = [{"scenario": "recall", "outcome": live.MODEL,
                              "detail": "forgot captured fact"}]
        report["evidence"] = {"recall": {"capture_complete": True, "segments": ["capture"],
                                           "captured_exchanges": 3}}
        outcome, reason = qualification.score_report(1, report)
        self.assertEqual(outcome, live.MODEL)
        for witness in ("recall", "3", "capture", "forgot captured fact"):
            self.assertIn(witness, reason)
        del report["evidence"]["recall"]["segments"]
        self.assertEqual(qualification.score_report(1, report)[0], live.HARNESS)

    def test_real_long_session_model_rerun_keeps_capture_and_seed(self):
        seen = []

        def play(route, env, name):
            seen.append((env.fixture_seed, env.home))
            segment = env.artifacts / "capture"
            segment.mkdir()
            (segment / "exchange.info").write_text("synthetic")
            env.capture_segments.append(segment)
            if len(seen) == 1:
                raise live.ScenarioFailure(live.MODEL, "forgot captured fact")
            return "recalled"

        with tempfile.TemporaryDirectory() as directory, patch("builtins.print"), \
                patch("long_session.selected_catalog", return_value=(("compaction", "jsonl"),)), \
                patch("long_session.play", side_effect=play), patch("live.write_home"), \
                patch("live.binary_sha256", return_value="build"), \
                patch("smoke.capture_complete", return_value=True):
            result = qualification.run_cell(Path(directory) / "cell", lambda path:
                qualification.execute("long-session", "codex", Path("plak"), ["--all"], 17, path))
        self.assertEqual(result["status"], "PASS after MODEL rerun")
        self.assertEqual([seed for seed, _ in seen], [17, 17])
        self.assertNotEqual(seen[0][1], seen[1][1])
        self.assertIn("captured_exchanges=1", result["attempts"][0]["reason"])

    def test_score_harness_triggers_and_precedence(self):
        changes = [
            {"capture_complete": False}, {"binary_sha256_start": None},
            {"binary_sha256_end": None}, {"binary_sha256_end": "operator"},
            {"rebuild": "changed", "binary_sha256_end": "operator",
             "binary_sha256_rebuilt": "owned"},
            {"rebuild": "changed", "binary_sha256_end": "owned"},
            {"evidence_gaps": 1}, {"diagnostics_available": False},
            {"clean_quit": False}, {"rebuild": "failed", "rebuild_attempted": True},
            {"turn_failures": 1}, {"dead_ends": 1},
        ]
        for change in changes:
            with self.subTest(change=change):
                report = self.report(live.MODEL)
                report["summary"].update(change)
                self.assertEqual(qualification.score_report(0, report)[0], live.HARNESS)
        for top in ({"driver_error": "oops"},
                    {"evidence": {"repair": {"capture_complete": False}}},
                    {"records": []}):
            with self.subTest(top=top):
                self.assertEqual(qualification.score_report(0, {**self.report(), **top})[0],
                                 live.HARNESS)
        self.assertEqual(qualification.score_report(1, self.report())[0], live.HARNESS)
        precedence = [live.HARNESS, live.PENDING, live.PROVIDER, live.MODEL]
        for index, expected in enumerate(precedence):
            with self.subTest(expected=expected):
                report = self.report()
                report["records"] = [self.report(o)["records"][0]
                                     for o in reversed(precedence[index:])]
                self.assertEqual(qualification.score_report(1, report)[0], expected)
        report = self.report()
        report["summary"]["failure"] = {"outcome": live.PENDING, "detail": "pending"}
        self.assertEqual(qualification.score_report(1, report)[0], live.PENDING)

    def test_informational_waiver_never_waives_harness_or_pending(self):
        route = next(route for route in live.ROUTES.values()
                     if route.route_class == live.INFORMATIONAL)
        for outcome in (live.PASS, live.MODEL, live.PROVIDER, live.HARNESS, live.PENDING):
            with self.subTest(outcome=outcome):
                self.assertEqual(qualification.qualifies(route, {"outcome": outcome}),
                                 outcome in (live.PASS, live.MODEL, live.PROVIDER))

    def test_openrouter_contingency_includes_shared_rerun_reserve(self):
        cells = [estimate.Cell("mac", "smoke", "or", live.LIGHT, "model", "openrouter"),
                 estimate.Cell("mac", "smoke", "api", live.LIGHT, "model", "openai")]
        rate = estimate.Rate(Decimal(1), Decimal(0), Decimal(1), Decimal(0))
        assumptions = estimate.Assumptions(smoke_input_tokens=1_000_000,
                                          smoke_output_tokens=0, contingency=Decimal("0.25"),
                                          openrouter_ceiling=Decimal("2.49"))
        report = estimate.estimate_matrix(cells, {"model": rate}, assumptions)
        self.assertEqual(report.openrouter_with_contingency, Decimal("2.5"))
        self.assertTrue(report.exceeds_openrouter_ceiling)

    def test_execute_forwards_fixture_seed_for_each_seeded_lane(self):
        for lane in ("smoke", "long-session", "jobs"):
            with self.subTest(lane=lane), tempfile.TemporaryDirectory() as directory:
                path = Path(directory)
                driver = sys.modules[qualification.LANES[lane]]

                def main(argv):
                    self.assertEqual(argv, ["codex", "--binary", "plak", "--artifacts",
                                            str(path), "--only", "compaction",
                                            "--fixture-seed", "17"])
                    (path / "report.json").write_text("{}")
                    return 0

                with patch.object(driver, "main", side_effect=main):
                    qualification.execute(lane, "codex", Path("plak"),
                                          ["--only", "compaction"], 17, path)

    def test_each_lane_main_assigns_fixture_seed(self):
        class Observed(Exception):
            pass

        for driver, entry, options in ((smoke, "Smoke", []),
                                       (long_session, "play", ["--all"]),
                                       (jobs, "Jobs", [])):
            with self.subTest(driver=driver.__name__), tempfile.TemporaryDirectory() as directory:
                binary = Path(directory) / "plak"
                binary.write_bytes(b"not executed")

                def observe(route, env, *rest):
                    self.assertEqual(env.fixture_seed, 123)
                    raise Observed

                with patch("live.write_home"), patch.object(driver, entry, side_effect=observe), \
                        patch("live.capture_root_mismatch", return_value=None):
                    argv = ["codex", "--binary", str(binary), "--artifacts", directory,
                            "--fixture-seed", "123", *options]
                    if driver is long_session:
                        # play's exception is recorded by main, so inspect that report too.
                        driver.main(argv)
                        report = json.loads((Path(directory) / "report.json").read_text())
                        self.assertTrue(all("Observed" in r["detail"] for r in report["results"]))
                    else:
                        with self.assertRaises(Observed):
                            driver.main(argv)

    def cell_with_builds(self, lane="soak", *, before_first=False,
                         after_first=b"owned", corrupt=None, options=()):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "plak"
            binary.write_bytes(b"original")
            original_sha = live.binary_sha256(binary)
            starts = []
            seeds = []
            owned_sha = None

            def execute(lane, route, used_binary, argv, seed, path):
                nonlocal owned_sha
                path.mkdir()
                starts.append(used_binary.read_bytes())
                seeds.append(seed)
                report = self.report(live.MODEL if len(starts) == 1 else live.PASS)
                report["summary"]["binary_sha256_start"] = live.binary_sha256(used_binary)
                if lane == "soak":
                    used_binary.write_bytes(b"owned")
                    owned_sha = live.binary_sha256(used_binary)
                    report["summary"].update(rebuild="changed", binary_sha256_rebuilt=owned_sha)
                    used_binary.write_bytes(after_first)
                report["summary"]["binary_sha256_end"] = live.binary_sha256(used_binary)
                if corrupt == "before_restore":
                    (path.parent / "starting-plak").write_bytes(b"corrupted")
                return 0, report

            real_copy = qualification.shutil.copy2

            def copy(src, dst):
                result = real_copy(src, dst)
                if corrupt == "after_copy":
                    Path(dst).write_bytes(b"corrupted")
                return result

            seed_values = iter((17, 18, 19))

            def seed(bits):
                if before_first:
                    binary.write_bytes(b"operator")
                return next(seed_values)

            with patch.object(live, "ROOT", root), patch("builtins.print"), \
                    patch("qualification.execute", side_effect=execute), \
                    patch("qualification.shutil.copy2", side_effect=copy), \
                    patch("qualification.secrets.randbits", side_effect=seed):
                code = qualification.main([lane, "codex", "--binary", str(binary), *options])
            saved = json.loads(next((root / "target/live").glob("*/report.json")).read_text())
            return code, saved, starts, seeds, binary.read_bytes(), original_sha, owned_sha

    def test_pre_attempt_guard_spends_no_paid_attempt(self):
        code, report, starts, _, current, _, _ = self.cell_with_builds(before_first=True)
        self.assertEqual(code, 1)
        self.assertEqual(starts, [])
        self.assertEqual(current, b"operator")
        self.assertEqual(report["attempts"][0]["reason"],
                         "binary changed before attempt; no paid attempt")

    def test_operator_build_after_soak_rebuild_is_never_restored(self):
        code, report, starts, _, current, _, _ = self.cell_with_builds(after_first=b"operator")
        self.assertEqual(code, 1)
        self.assertEqual(report["outcome"], live.HARNESS)
        self.assertEqual(starts, [b"original"])
        self.assertEqual(current, b"operator")

    def test_operator_build_between_attempts_is_never_restored(self):
        real_score = qualification.score_report
        binary_path = None

        def score(code, report):
            # The scoring boundary follows the completed lane, before attempt 2.
            nonlocal binary_path
            result = real_score(code, report)
            if binary_path is None and report.get("summary", {}).get("rebuild") == "changed":
                binary_path = next(live.ROOT.glob("plak"))
                binary_path.write_bytes(b"operator")
            return result

        with patch("qualification.score_report", side_effect=score):
            code, report, starts, _, current, _, _ = self.cell_with_builds()
        self.assertEqual(code, 1)
        self.assertEqual(len(report["attempts"]), 2)
        self.assertEqual(starts, [b"original"])
        self.assertEqual(current, b"operator")

    def test_restore_and_seed_are_reported_for_both_attempts(self):
        code, report, starts, seeds, _, original_sha, owned_sha = self.cell_with_builds()
        self.assertEqual(code, 0)
        self.assertEqual(starts, [b"original", b"original"])
        self.assertEqual(seeds, [report["binding"]["fixture_seed"]] * 2)
        for index, attempt in enumerate(report["attempts"]):
            metadata = attempt["report"]
            self.assertEqual(metadata["starting_build_restored"], bool(index))
            self.assertEqual(metadata["binary_sha256_current"], owned_sha if index else original_sha)
            self.assertEqual(metadata["binary_sha256_restored"], original_sha if index else None)
            self.assertEqual(metadata["binary_sha256_intentional"], owned_sha if index else None)

    def test_retained_copy_is_verified_after_copy_and_before_restore(self):
        for corrupt in ("after_copy", "before_restore"):
            with self.subTest(corrupt=corrupt):
                code, report, starts, _, current, _, _ = self.cell_with_builds(corrupt=corrupt)
                self.assertEqual(code, 1)
                self.assertEqual(report["outcome"], live.HARNESS)
                self.assertEqual(starts, [] if corrupt == "after_copy" else [b"original"])
                self.assertEqual(current, b"original" if corrupt == "after_copy" else b"owned")
                self.assertIn("retained", report["attempts"][-1]["reason"])

    def test_seed_binding_replayed_on_non_soak_attempts(self):
        code, report, starts, seeds, _, _, _ = self.cell_with_builds(lane="smoke")
        self.assertEqual(code, 0)
        self.assertEqual(starts, [b"original", b"original"])
        self.assertEqual(seeds, [report["binding"]["fixture_seed"]] * 2)

    def test_forwarded_options_are_exact_and_dry_run_validates(self):
        for lane in qualification.LANES:
            for option in ("--art", "--binar", "--unknown", "--fixture-s"):
                with self.subTest(lane=lane, option=option), \
                        patch("qualification.execute") as execute, patch("builtins.print"):
                    self.assertEqual(qualification.main([lane, "codex", "--dry-run",
                                                          option, "value"]), 2)
                    execute.assert_not_called()
        for lane in qualification.LANES:
            driver = sys.modules[qualification.LANES[lane]]
            self.assertFalse(driver.build_parser().allow_abbrev)

    def test_lane_system_exit_is_reported_not_lost(self):
        with tempfile.TemporaryDirectory() as directory:
            attempt = Mock(side_effect=SystemExit(2))
            result = qualification.run_cell(Path(directory), attempt)
            self.assertEqual(result["outcome"], live.HARNESS)
            self.assertIn("SystemExit", result["attempts"][0]["report"]["driver_error"])
            self.assertEqual(attempt.call_count, 1)

    def test_invalid_forwarded_options_write_cell_report_without_launch(self):
        for lane in qualification.LANES:
            with self.subTest(lane=lane), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                binary = root / "plak"
                binary.write_bytes(b"never executed")
                with patch.object(live, "ROOT", root), patch("builtins.print"), \
                        patch("live.write_home") as launch:
                    self.assertEqual(qualification.main([lane, "codex", "--binary", str(binary),
                                                          "--art", "override"]), 1)
                launch.assert_not_called()
                report = json.loads(next((root / "target/live").glob("*/report.json")).read_text())
                self.assertEqual(report["outcome"], live.HARNESS)
                self.assertEqual(len(report["attempts"]), 1)
                self.assertIn("SystemExit", report["attempts"][0]["reason"])

    def test_owned_forwarded_options_spend_no_attempt(self):
        for option in ("--artifacts=override", "--fixture-seed=3", "--long-session"):
            with self.subTest(option=option):
                code, report, starts, _, _, _, _ = self.cell_with_builds(options=[option])
                self.assertEqual(code, 1)
                self.assertEqual(starts, [])
                self.assertEqual(report["outcome"], live.HARNESS)
                self.assertIn("owned by this driver", report["attempts"][0]["reason"])

    def test_tui_recovered_failed_turn_is_not_harness(self):
        for recovered in (True, False):
            with self.subTest(recovered=recovered), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "tests").mkdir()
                binary = root / "plak"
                binary.write_bytes(b"synthetic")
                phases = ["failed", "completed" if recovered else "failed", "completed"]
                clock = iter(range(0, 100000, 10000))

                def write_home(home, route):
                    log = home / "logs/plak.log"
                    log.parent.mkdir(parents=True)
                    log.write_text("synthetic diagnostics")
                    log.chmod(0o600)

                def repo(path, seed):
                    path.mkdir(parents=True)
                    (path / "followup.txt").write_text("sleep and reopen verified\n")
                    return path

                def tui(env, repo, path, **kwargs):
                    segment = kwargs["capture_dir"]
                    env.capture_segments.append(segment)
                    (segment / "1.info").write_text("synthetic")
                    (segment / "1.body").write_text("synthetic")
                    return Mock(pid=1, capture_dir=segment, quit=Mock(return_value=0))

                with patch.object(live, "ROOT", root), patch("live.write_home", side_effect=write_home), \
                        patch("live.git_repo", side_effect=repo), patch("live.Tui", side_effect=tui), \
                        patch("live.capture_integrity", return_value=True), \
                        patch("tui_soak.discover_session", return_value="session"), \
                        patch("tui_soak.time.monotonic", side_effect=lambda: next(clock)), \
                        patch("tui_soak.send_and_wait", side_effect=phases), \
                        patch("tui_soak.check_task", return_value=True), \
                        patch("tui_soak.raise_if_provider_terminal"), patch("builtins.print"):
                    code = tui_soak.main(["codex", "--binary", str(binary), "--artifacts",
                                          str(root / "attempt"), "--hours", "1"])
                report = json.loads((root / "attempt/report.json").read_text())
                self.assertEqual(report["summary"]["turn_failures"], 0 if recovered else 2)
                self.assertEqual(qualification.score_report(code, report)[0],
                                 live.PASS if recovered else live.HARNESS)

    def test_soak_records_digest_immediately_after_own_rebuild(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "tests").mkdir()
            binary = root / "plak"
            binary.write_bytes(b"original")
            real_digest = live.binary_sha256
            digests = []

            def digest(path):
                if len(digests) == 2:
                    path.write_bytes(b"operator")
                result = real_digest(path)
                digests.append(result)
                return result

            def rebuild(path, log):
                path.write_bytes(b"owned")
                return True

            def runner(route, env, repo):
                segment = env.capture_root / "segment-001"
                segment.mkdir(mode=0o700)
                env.capture_segments.append(segment)
                (segment / "1.info").write_text("synthetic")
                (segment / "1.body").write_text("synthetic")
                result = Mock(session=None)
                result.run_task.return_value = soak.Record("repair", 1, None, outcome="pass")
                return result
            clock = iter(range(0, 10000, 1000))
            with patch.object(live, "ROOT", root), patch("live.write_home"), \
                    patch("live.binary_sha256", side_effect=digest), \
                    patch("soak.prepare_repo", return_value=root), \
                    patch("soak.Soak", side_effect=runner), \
                    patch("soak.rebuild", side_effect=rebuild), \
                    patch("live.capture_integrity", return_value=True), \
                    patch("soak.time.monotonic", side_effect=lambda: next(clock)), \
                    patch("builtins.print"):
                code = soak.main(["codex", "--binary", str(binary), "--artifacts",
                                  str(root / "attempt"), "--hours", "1"])
            summary = json.loads((root / "attempt/report.json").read_text())["summary"]
            self.assertEqual(len(digests), 3)
            self.assertEqual(summary["binary_sha256_rebuilt"], digests[1])
            self.assertNotEqual(summary["binary_sha256_end"], summary["binary_sha256_rebuilt"])
            self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()
