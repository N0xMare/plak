"""No-network tests for the serial OpenRouter Harbor budget watcher."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

WATCH_PATH = Path(__file__).resolve().parents[2] / "eval/scripts/openrouter_watch.py"
spec = importlib.util.spec_from_file_location("openrouter_watch", WATCH_PATH)
watch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(watch)


class FakeHarbor:
    returncode = 0

    def __init__(self):
        self.polls = 0

    def poll(self):
        self.polls += 1
        return None if self.polls == 1 else self.returncode


class OpenRouterWatchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.key = self.root / "key"
        self.key.write_text("test-secret\n")
        self.key.chmod(0o600)
        self.baseline = self.root / "baseline.json"
        self.jobs = self.root / "jobs"
        self.report = self.jobs / "watch.json"
        self.report.parent.mkdir()
        self.binding = mock.patch.object(watch, "source_binding", return_value={"source_commit": "a" * 40})
        self.binding.start()
        self.addCleanup(self.binding.stop)

    def args(self, mode, report=None, held=None):
        values = ["--key-file", str(self.key), "--baseline", str(self.baseline)]
        if report is not None:
            values.extend(["--report", str(report)])
        if held is not None:
            values.extend(["--held-usd", held])
        return values + [mode]

    def command(self):
        return ["--", "harbor", "run", "--jobs-dir", str(self.jobs)]

    def test_immutable_shared_baseline_and_regressed_second_run(self):
        with mock.patch.object(watch, "usage", return_value=(watch.Decimal("25.00"), watch.Decimal("75"))):
            self.assertEqual(watch.main(self.args("init")), 0)
            with self.assertRaises(FileExistsError):
                watch.main(self.args("init"))
        baseline_bytes = self.baseline.read_bytes()
        fake = FakeHarbor()
        readings = iter([(watch.Decimal("25.10"), watch.Decimal("74.90")),
                         (watch.Decimal("25.20"), watch.Decimal("74.80")),
                         (watch.Decimal("25.20"), watch.Decimal("74.80"))])
        with (mock.patch.object(watch, "usage", side_effect=lambda _key: next(readings)),
              mock.patch.object(watch, "docker_ids", return_value=set()),
              mock.patch.object(watch.subprocess, "Popen", return_value=fake) as launch,
              mock.patch.object(watch.time, "sleep")):
            result = watch.main(self.args("run", self.report) + self.command())
        self.assertEqual(result, 0)
        self.assertTrue(launch.call_args.kwargs["start_new_session"])
        self.assertEqual(self.baseline.read_bytes(), baseline_bytes)
        self.assertEqual(json.loads(self.report.read_text())["status"], "harbor_exited")
        observations = (self.root / "baseline.json.observations.jsonl").read_text().splitlines()
        self.assertEqual([json.loads(row)["usage_usd"] for row in observations],
                         ["25.10", "25.20", "25.20"])
        with (mock.patch.object(watch, "usage", return_value=(watch.Decimal("25.15"), watch.Decimal("74"))),
              mock.patch.object(watch, "docker_ids", return_value=set()),
              mock.patch.object(watch.subprocess, "Popen", return_value=FakeHarbor()) as launch,
              mock.patch.object(watch.time, "sleep")):
            self.assertEqual(watch.main(self.args("run", self.root / "second.json") + self.command()), 0)
            launch.assert_called_once()
        state = watch.load_state(self.baseline, watch.Decimal("25.00"))
        self.assertEqual(state.highwater, watch.Decimal("25.20"))
        self.assertEqual(state.regression_reserve, watch.Decimal("0.05"))

    def test_regressed_sample_is_journaled_with_permanent_reserve(self):
        with mock.patch.object(watch, "usage", return_value=(watch.Decimal("25.00"),
                                                              watch.Decimal("75.00"))):
            watch.main(self.args("init"))
        baseline_bytes = self.baseline.read_bytes()
        watch.append_observation(self.baseline, watch.transition(
            watch.load_state(self.baseline, watch.Decimal("25.00")), watch.Decimal("25.20")))
        journal = self.root / "baseline.json.observations.jsonl"
        fake = FakeHarbor()
        readings = [(watch.Decimal("25.25"), watch.Decimal("74.75")),
                    (watch.Decimal("25.22"), watch.Decimal("74.78")),
                    (watch.Decimal("25.22"), watch.Decimal("74.78"))]
        with (mock.patch.object(watch, "usage", side_effect=readings),
              mock.patch.object(watch, "docker_ids", return_value=set()),
              mock.patch.object(watch.subprocess, "Popen", return_value=fake),
              mock.patch.object(watch.time, "sleep")):
            self.assertEqual(watch.main(self.args("run", self.report, "1.22585088") + self.command()), 0)
        self.assertEqual(self.baseline.read_bytes(), baseline_bytes)
        self.assertEqual([json.loads(line)["usage_usd"] for line in journal.read_text().splitlines()],
                         ["25.20", "25.25", "25.25", "25.25"])
        self.assertEqual([json.loads(line)["regression_reserve_usd"] for line in journal.read_text().splitlines()],
                         ["0", "0", "0.03", "0.03"])
        report = json.loads(self.report.read_text())
        self.assertEqual(report["status"], "harbor_exited")
        self.assertEqual(report["usage_usd"], "25.25")
        self.assertEqual(report["held_usd"], "1.22585088")
        self.assertEqual(report["regression_reserve_usd"], "0.03")

    def test_soft_stop_fails_before_launch(self):
        with mock.patch.object(watch, "usage", return_value=(watch.Decimal("8.00"), watch.Decimal("17"))):
            watch.main(self.args("init"))
        with (mock.patch.object(watch, "usage", return_value=(watch.Decimal("16.00"), watch.Decimal("9"))),
              mock.patch.object(watch.subprocess, "Popen") as launch):
            with self.assertRaisesRegex(ValueError, "soft stop"):
                watch.main(self.args("run", self.report) + self.command())
            launch.assert_not_called()

    def test_existing_report_is_refused_before_child_or_docker_probe(self):
        with mock.patch.object(watch, "usage", return_value=(watch.Decimal("1"), watch.Decimal("24"))):
            watch.main(self.args("init"))
        self.report.write_text("prior receipt")
        with (mock.patch.object(watch, "usage") as meter,
              mock.patch.object(watch, "docker_ids") as docker,
              mock.patch.object(watch.subprocess, "Popen") as launch):
            with self.assertRaisesRegex(ValueError, "already exists"):
                watch.main(self.args("run", self.report) + self.command())
            meter.assert_not_called()
            docker.assert_not_called()
            launch.assert_not_called()

    def test_external_holds_reduce_both_start_thresholds_without_rebasing(self):
        with mock.patch.object(watch, "usage", return_value=(watch.Decimal("25"), watch.Decimal("75"))):
            watch.main(self.args("init"))
        baseline_bytes = self.baseline.read_bytes()
        for current, remaining in [("31.8", "68.2"), ("25.1", "3.2")]:
            with (self.subTest(current=current, remaining=remaining),
                  mock.patch.object(watch, "usage", return_value=(watch.Decimal(current), watch.Decimal(remaining))),
                  mock.patch.object(watch.subprocess, "Popen") as launch):
                with self.assertRaisesRegex(ValueError, "soft stop"):
                    watch.main(self.args("run", self.report, "1.22585088") + self.command())
                launch.assert_not_called()
                self.assertEqual(self.baseline.read_bytes(), baseline_bytes)

    def test_held_cost_threshold_during_run_stops_owned_work_and_retains_liability(self):
        with mock.patch.object(watch, "usage", return_value=(watch.Decimal("25"), watch.Decimal("75"))):
            watch.main(self.args("init"))
        fake = FakeHarbor()
        readings = [(watch.Decimal("31.7"), watch.Decimal("68.3")),
                    (watch.Decimal("31.8"), watch.Decimal("68.2"))]
        with (mock.patch.object(watch, "usage", side_effect=readings),
              mock.patch.object(watch, "docker_ids", return_value=set()),
              mock.patch.object(watch.subprocess, "Popen", return_value=fake),
              mock.patch.object(watch, "stop_owned", return_value=True) as stopped,
              mock.patch.object(watch, "stop_owned_docker", return_value=True) as docker_stop,
              mock.patch.object(watch.time, "sleep")):
            with self.assertRaisesRegex(ValueError, "arm incomplete"):
                watch.main(self.args("run", self.report, "1.22585088") + self.command())
        stopped.assert_called_once_with(fake)
        docker_stop.assert_called_once()
        report = json.loads(self.report.read_text())
        self.assertEqual(report["status"], "incomplete")
        self.assertEqual(report["held_usd"], "1.22585088")
        self.assertTrue(report["joined"])
        self.assertTrue(report["docker_cleanup_proven"])

    def test_transitions_accumulate_each_drop_including_bounce_and_below_baseline(self):
        state = watch.WatchState(watch.Decimal("10"), watch.Decimal("10"), watch.Decimal("0"))
        for raw in ("11", "10.5", "11", "9.4", "9.4", "10"):
            state = watch.transition(state, watch.Decimal(raw))
        self.assertEqual(state.highwater, watch.Decimal("11"))
        self.assertEqual(state.raw, watch.Decimal("10"))
        self.assertEqual(state.regression_reserve, watch.Decimal("2.1"))
        self.assertTrue(state.extended)

    def test_replay_accepts_only_legacy_prefix_then_exact_extended_transitions(self):
        journal = self.root / "baseline.json.observations.jsonl"
        self.baseline.write_text('{"usage_usd":"10"}')
        self.baseline.chmod(0o600)
        journal.write_text('{"usage_usd":"10.1"}\n{"usage_usd":"10.2"}\n')
        journal.chmod(0o600)
        state = watch.load_state(self.baseline, watch.Decimal("10"))
        self.assertEqual(state.raw, watch.Decimal("10.2"))
        next_state = watch.transition(state, watch.Decimal("10.15"))
        watch.append_observation(self.baseline, next_state)
        self.assertEqual(watch.load_state(self.baseline, watch.Decimal("10")), next_state)
        self.assertEqual(journal.read_text().splitlines()[:2],
                         ['{"usage_usd":"10.1"}', '{"usage_usd":"10.2"}'])
        with journal.open("a") as output:
            output.write('{"usage_usd":"10.3"}\n')
        with self.assertRaisesRegex(ValueError, "invalid"):
            watch.load_state(self.baseline, watch.Decimal("10"))

    def test_replay_refuses_missing_partial_and_corrupt_journal(self):
        journal = self.root / "baseline.json.observations.jsonl"
        self.baseline.write_text('{"usage_usd":"10"}')
        self.baseline.chmod(0o600)
        with self.assertRaisesRegex(ValueError, "missing"):
            watch.load_state(self.baseline, watch.Decimal("10"))
        for content, pattern in [
            ('{"usage_usd":"10.2"}', "partial tail"),
            ('{"usage_usd":"10.2","usage_usd":"10.3"}\n', "duplicate"),
            ('{"usage_usd":"10.2","raw_usage_usd":"10.1","regression_reserve_usd":"0"}\n',
             "invalid"),
        ]:
            with self.subTest(content=content):
                journal.write_text(content)
                journal.chmod(0o600)
                with self.assertRaisesRegex(ValueError, pattern):
                    watch.load_state(self.baseline, watch.Decimal("10"))
        journal.write_text("")
        watch.append_observation(self.baseline, watch.transition(
            watch.load_state(self.baseline, watch.Decimal("10")), watch.Decimal("9")))
        row = json.loads(journal.read_text())
        row["regression_reserve_usd"] = "0"
        journal.write_text(json.dumps(row) + "\n")
        with self.assertRaisesRegex(ValueError, "transition"):
            watch.load_state(self.baseline, watch.Decimal("10"))

    def test_both_thresholds_include_regression_reserve(self):
        state = watch.WatchState(watch.Decimal("7.8"), watch.Decimal("7.8"),
                                 watch.Decimal("0.1"), True)
        with self.assertRaisesRegex(ValueError, "soft stop"):
            watch.check_budget(state, watch.Decimal("1"), watch.Decimal("1.1"),
                               watch.Decimal("50"))
        state = watch.WatchState(watch.Decimal("1.2"), watch.Decimal("1.2"),
                                 watch.Decimal("0.1"), True)
        with self.assertRaisesRegex(ValueError, "soft stop"):
            watch.check_budget(state, watch.Decimal("1"), watch.Decimal("0.2"),
                               watch.Decimal("2.3"))

    def test_rejected_import_is_hash_bound_and_one_time(self):
        with mock.patch.object(watch, "usage", return_value=(watch.Decimal("25"), watch.Decimal("75"))):
            watch.main(self.args("init"))
        baseline, baseline_sha = watch.load_baseline(self.baseline, b"test-secret")
        watch.append_observation(self.baseline, watch.transition(
            watch.load_state(self.baseline, baseline), watch.Decimal("25.25")))
        stop = self.root / "stop-report.json"
        stop.write_text(json.dumps({"status": "incomplete",
                                    "reason": "OpenRouter usage meter regressed",
                                    "baseline_sha256": baseline_sha,
                                    "usage_usd": "25.25",
                                    "meter_regression": {"observed_usd": "25.22",
                                                         "prior_highwater_usd": "25.25",
                                                         "baseline_usd": "25"}}))
        stop.chmod(0o600)
        import hashlib
        digest = hashlib.sha256(stop.read_bytes()).hexdigest()
        journal = self.root / "baseline.json.observations.jsonl"
        prior_digest = hashlib.sha256(journal.read_bytes()).hexdigest()
        prior_rows = len(journal.read_text().splitlines())
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            watch.import_rejected_observation(self.baseline, baseline, baseline_sha, stop,
                                               "0" * 64, prior_digest, prior_rows)
        with self.assertRaisesRegex(ValueError, "predecessor journal changed"):
            watch.import_rejected_observation(self.baseline, baseline, baseline_sha, stop,
                                               digest, "0" * 64, prior_rows)
        state = watch.import_rejected_observation(self.baseline, baseline, baseline_sha,
                                                  stop, digest, prior_digest, prior_rows)
        self.assertEqual(state.regression_reserve, watch.Decimal("0.03"))
        self.assertEqual(state.highwater, watch.Decimal("25.25"))
        self.assertEqual(state.imported_receipt_sha256, digest)
        self.assertEqual(watch.load_state(self.baseline, baseline), state)
        with self.assertRaisesRegex(ValueError, "already imported"):
            watch.import_rejected_observation(self.baseline, baseline, baseline_sha,
                                               stop, digest, prior_digest, prior_rows)

    def test_reserve_threshold_sample_is_durable_before_owned_cleanup(self):
        with mock.patch.object(watch, "usage", return_value=(watch.Decimal("25"), watch.Decimal("75"))):
            watch.main(self.args("init"))
        fake = FakeHarbor()
        readings = [(watch.Decimal("31.7"), watch.Decimal("68.3")),
                    (watch.Decimal("31.6"), watch.Decimal("68.4"))]
        with (mock.patch.object(watch, "usage", side_effect=readings),
              mock.patch.object(watch, "docker_ids", return_value=set()),
              mock.patch.object(watch.subprocess, "Popen", return_value=fake),
              mock.patch.object(watch, "stop_owned", return_value=True) as stopped,
              mock.patch.object(watch, "stop_owned_docker", return_value=True) as docker_stop,
              mock.patch.object(watch.time, "sleep")):
            with self.assertRaisesRegex(ValueError, "arm incomplete"):
                watch.main(self.args("run", self.report, "1.22585088") + self.command())
        stopped.assert_called_once_with(fake)
        docker_stop.assert_called_once()
        state = watch.load_state(self.baseline, watch.Decimal("25"))
        self.assertEqual(state.highwater, watch.Decimal("31.7"))
        self.assertEqual(state.regression_reserve, watch.Decimal("0.1"))
        self.assertEqual(json.loads(self.report.read_text())["regression_reserve_usd"], "0.1")

    def test_successful_run_records_holds_without_settling_them(self):
        with mock.patch.object(watch, "usage", return_value=(watch.Decimal("25"), watch.Decimal("75"))):
            watch.main(self.args("init"))
        baseline_bytes = self.baseline.read_bytes()
        with (mock.patch.object(watch, "usage", return_value=(watch.Decimal("25.2"), watch.Decimal("74.8"))),
              mock.patch.object(watch, "docker_ids", return_value=set()),
              mock.patch.object(watch.subprocess, "Popen", return_value=FakeHarbor()),
              mock.patch.object(watch.time, "sleep")):
            self.assertEqual(watch.main(self.args("run", self.report, "1.22585088") + self.command()), 0)
        report = json.loads(self.report.read_text())
        self.assertEqual(report["held_usd"], "1.22585088")
        self.assertEqual(report["budget_spent_usd"], "0.2")
        self.assertEqual(self.baseline.read_bytes(), baseline_bytes)

    def test_invalid_held_costs_are_refused_before_meter_or_launch(self):
        for held in ("", "-1", "NaN", "Infinity", "invalid"):
            with (self.subTest(held=held), contextlib.redirect_stderr(io.StringIO()),
                  mock.patch.object(watch, "usage") as meter,
                  mock.patch.object(watch.subprocess, "Popen") as launch):
                with self.assertRaises(SystemExit):
                    watch.main(self.args("run", self.report, held) + self.command())
                meter.assert_not_called()
                launch.assert_not_called()

    def test_key_file_must_be_private_regular_and_not_symlink(self):
        self.key.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "private regular"):
            watch.key_bytes(self.key)
        self.key.chmod(0o600)
        link = self.root / "link"
        link.symlink_to(self.key)
        with self.assertRaises(OSError):
            watch.key_bytes(link)

    def test_meter_uses_only_current_key_get_and_parses_decimal(self):
        class Reply:
            status = 200

            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

            def read(self, _limit):
                return b'{"data":{"usage":0.1234567890123456789,"limit_remaining":24.5}}'

        opener = mock.Mock()
        opener.open.return_value = Reply()
        with mock.patch.object(watch, "build_opener", return_value=opener) as built:
            usage, remaining = watch.usage(b"test-secret")
        self.assertIs(built.call_args.args[0], watch.NoRedirect)
        request = opener.open.call_args.args[0]
        self.assertEqual(request.full_url, "https://openrouter.ai/api/v1/key")
        self.assertEqual(request.get_method(), "GET")
        self.assertEqual(str(usage), "0.1234567890123456789")
        self.assertEqual(str(remaining), "24.5")

    def test_redirect_is_refused_before_credentials_can_follow(self):
        with self.assertRaisesRegex(ValueError, "redirected"):
            watch.NoRedirect().redirect_request(None, None, 302, "Found", {}, "https://other.example")

    def test_docker_stop_targets_only_new_containers_in_owned_trial_project(self):
        trial = self.jobs / "run-time" / "task__AbCd123"
        trial.mkdir(parents=True)
        (trial / "config.json").write_text(json.dumps({"trial_name": trial.name,
                                                         "trials_dir": str(trial.parent)}))
        old_id, new_id = "a" * 12, "b" * 12
        with (mock.patch.object(watch, "docker_ids", side_effect=[{old_id, new_id},
                                                                    {new_id}, set()]) as queried,
              mock.patch.object(watch.subprocess, "run") as command):
            self.assertTrue(watch.stop_owned_docker(self.jobs, {old_id}))
        self.assertEqual(queried.call_count, 3)
        self.assertIn(new_id, command.call_args.args[0])
        self.assertNotIn(old_id, command.call_args.args[0])
        self.assertFalse(watch.stop_owned_docker(self.root / "missing", set()))

    def test_post_launch_meter_io_error_stops_owned_harbor_and_marks_incomplete(self):
        with mock.patch.object(watch, "usage", return_value=(watch.Decimal("1"), watch.Decimal("24"))):
            watch.main(self.args("init"))
        fake = FakeHarbor()
        readings = iter([(watch.Decimal("1"), watch.Decimal("24")), OSError("meter offline")])

        def next_reading(_key):
            reading = next(readings)
            if isinstance(reading, Exception):
                raise reading
            return reading

        with (mock.patch.object(watch, "usage", side_effect=next_reading),
              mock.patch.object(watch, "docker_ids", return_value=set()),
              mock.patch.object(watch.subprocess, "Popen", return_value=fake),
              mock.patch.object(watch, "stop_owned", return_value=True) as stopped,
              mock.patch.object(watch, "stop_owned_docker", return_value=False) as docker_stop,
              mock.patch.object(watch.time, "sleep")):
            with self.assertRaisesRegex(ValueError, "arm incomplete"):
                watch.main(self.args("run", self.report) + self.command())
        stopped.assert_called_once_with(fake)
        docker_stop.assert_called_once()
        report = json.loads(self.report.read_text())
        self.assertEqual(report["status"], "incomplete")
        self.assertFalse(report["docker_cleanup_proven"])
        self.assertEqual(report["reason"], "OSError")

    def test_owned_process_group_is_interrupted_and_joined(self):
        process = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"],
                                   start_new_session=True, stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL)
        try:
            self.assertTrue(watch.stop_owned(process))
            self.assertIsNotNone(process.poll())
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()


if __name__ == "__main__":
    unittest.main()
