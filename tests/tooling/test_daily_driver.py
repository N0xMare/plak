"""Read-only daily-driver evidence; all inputs are synthetic."""

import sys
import hashlib
import tempfile
import unittest
from contextlib import redirect_stdout, redirect_stderr
from io import StringIO
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tests" / "live"))
import daily_driver


def line(day, text, level="INFO"):
    return f"2026-10-{day:02d}T12:00:00Z {level} {text}\n"


class DailyDriverTests(unittest.TestCase):
    def test_build_boundaries_since_filter_and_distinct_root_child_bindings(self):
        logs = [
            line(1, 'plak.reload: exec installed plak executable=/private/maintainer/abcdef123456/plak carried=true'),
            line(2, 'session_runtime_binding{provider="grok" model="grok-4.7" session_id="root"}: plakous_hosted::agent::build: ready'),
            line(2, 'session_runtime_binding{provider="grok" model="grok-4.7" session_id="root"}: plakous_hosted::agent::build: ready'),
            line(3, 'session_runtime_binding{provider="grok" model="grok-4.7" session_id="root.c1"}: plakous_hosted::agent::build: ready'),
            line(3, 'plak.retry: retrying cause="http_status" detail="PRIVATE PROMPT"', "WARN"),
            line(3, 'plak.capture: PRIVATE PROMPT /private/token', "ERROR"),
            line(3, 'plak.reload: exec installed plak executable=/private/maintainer/123456abcdef/plak carried=false'),
            line(4, 'plak::store: PRIVATE PROMPT', "ERROR"),
        ]
        report = daily_driver.render(daily_driver.summarize(logs, since="2026-10-02"))
        self.assertIn("abcdef123456", report)
        self.assertIn("123456abcdef", report)
        self.assertIn("grok/grok-4.7", report)
        self.assertIn("root=1, child=1", report)
        self.assertIn("http_status=1", report)
        self.assertIn("WARN plak.retry=1", report)
        self.assertIn("ERROR plak.capture=1", report)
        self.assertNotIn("PRIVATE", report)
        self.assertNotIn("root.c1", report)
        self.assertNotIn("/private", report)
        self.assertIn("Active days", report)
        self.assertIn("Root routes", report)

    def test_version_ids_turns_and_recovery_counts(self):
        logs = [
            line(1, 'plak version: plak 0.1.0 (maintainer; binary abcdef123456)'),
            line(1, 'plak::runtime: turn.lifecycle turn_id="t1" phase="admitted"'),
            line(1, 'plak::runtime: turn.lifecycle turn_id="t1" phase="completed"'),
            line(1, 'plak::runtime: turn.abandoned turn_id="t2"'),
            line(1, 'plak::runtime: session.recovered turn_id="t3"'),
        ]
        build = daily_driver.summarize(logs)[0]
        self.assertEqual(build.build_id, "abcdef123456")
        self.assertEqual(build.turns, {"t1"})
        self.assertEqual(build.abandoned, 1)
        self.assertEqual(build.recovered, 1)

    def test_unknown_build_invalid_lines_and_since(self):
        report = daily_driver.render(daily_driver.summarize([
            "PRIVATE PROMPT WARN plak.retry: cause=secret\n",
            line(1, 'plak::store: warning', "WARN"),
            line(3, 'plak::store: warning', "WARN"),
        ], since="2026-10-02"))
        self.assertIn("unknown", report)
        self.assertIn("WARN plak::store=1", report)
        self.assertNotIn("secret", report)
        self.assertNotIn("PRIVATE", report)
        with self.assertRaises(ValueError):
            daily_driver.summarize([], since="not-a-date")

    def test_untrusted_field_values_are_not_printed(self):
        report = daily_driver.render(daily_driver.summarize([
            line(1, 'session_runtime_binding{provider="SECRET" model="SECRET" session_id="s"}: plak::store: hi'),
            line(1, 'plak.retry: cause="SECRET"', "WARN"),
            line(1, 'plak.reload: exec installed plak executable=/PRIVATE/SECRET/plak'),
        ]))
        self.assertNotIn("SECRET", report)
        self.assertNotIn("PRIVATE", report)

    def test_snapshot_hash_is_binary_digest_not_directory_name_and_missing_is_unknown(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            snapshot = "abcdef123456"
            binary = home / ".local/lib/plak/maintainer" / snapshot / "plak"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"synthetic executable")
            expected = hashlib.sha256(binary.read_bytes()).hexdigest()[:12]
            with patch.object(daily_driver.Path, "home", return_value=home):
                self.assertEqual(daily_driver.snapshot_binary_id(snapshot), expected)
                self.assertIsNone(daily_driver.snapshot_binary_id("123456abcdef"))
                self.assertIsNone(daily_driver.snapshot_binary_id("../../PRIVATE"))
                builds = daily_driver.summarize([
                    line(1, f'plak.reload: exec installed plak executable=/PRIVATE/maintainer/{snapshot}/plak'),
                    line(2, f'plak.start: startup binary={expected} state_key=r22.e24.p30.t4'),
                ])
            self.assertEqual(builds[-1].binary_id, expected)
            self.assertEqual(builds[-1].build_id, snapshot)

    def test_startup_binary_preferred_over_snapshot_attribution(self):
        with patch.object(daily_driver, "snapshot_binary_id", return_value="abcdef123456"):
            builds = daily_driver.summarize([
                line(1, 'plak.reload: exec installed plak executable=/PRIVATE/maintainer/123456abcdef/plak'),
                line(2, 'plak.start: binary=111111111111 state_key=r22.e24.p30.t4'),
                line(2, 'plak::store: warning', "WARN"),
            ])
        self.assertEqual(builds[-1].binary_id, "111111111111")
        self.assertEqual(builds[-1].problems[("WARN", "plak::store")], 1)

    def test_cli_since_reads_without_changing_log_and_redacts_file_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "PRIVATE.log"
            source.write_text(line(1, "plak::store: PRIVATE PROMPT", "ERROR") +
                              line(2, "plak.start: binary=abcdef123456 state_key=r22.e24.p30.t4"))
            before = source.read_bytes()
            output = StringIO()
            with redirect_stdout(output):
                self.assertEqual(daily_driver.main(["--log", str(source), "--since", "2026-10-02"]), 0)
            self.assertEqual(source.read_bytes(), before)
            self.assertIn("abcdef123456", output.getvalue())
            self.assertNotIn("PRIVATE", output.getvalue())
            self.assertNotIn("ERROR plak::store", output.getvalue())
            errors = StringIO()
            with redirect_stderr(errors):
                self.assertEqual(daily_driver.main(["--log", str(source.parent / "PRIVATE-missing")]), 2)
            self.assertNotIn("PRIVATE", errors.getvalue())
            self.assertNotIn(directory, errors.getvalue())
