"""Gate timing ledger: append, median warning, and the short-history silence."""

import json
from pathlib import Path
import tempfile
import unittest

import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/checks"))
import gate_times


def _sample(**overrides):
    row = {
        "commit": "abc123",
        "tier": "check",
        "group": "contracts",
        "seconds": 5,
        "exit_status": 0,
        "platform": "x86_64-unknown-linux-gnu",
        "cache": "warm",
    }
    row.update(overrides)
    return row


class GateTimesTests(unittest.TestCase):
    def test_append_writes_one_json_object_per_call(self):
        with tempfile.TemporaryDirectory() as temporary:
            ledger = Path(temporary) / ".gate-times.jsonl"
            gate_times.append_sample(ledger, _sample())
            gate_times.append_sample(ledger, _sample(group="static", seconds=1, cache="cold"))
            lines = ledger.read_text().splitlines()
            self.assertEqual(len(lines), 2)
            rows = [json.loads(line) for line in lines]
            self.assertEqual(set(rows[0]), set(gate_times.FIELDS))
            self.assertEqual(rows[0]["group"], "contracts")
            self.assertEqual(rows[1]["cache"], "cold")
            self.assertEqual(rows[1]["seconds"], 1)
            self.assertNotIn("\n", lines[0])

    def test_warns_when_the_latest_sample_exceeds_twice_the_median(self):
        samples = [
            _sample(seconds=5),
            _sample(seconds=5),
            _sample(seconds=11),
        ]
        self.assertEqual(
            gate_times.warning_lines(samples, platform=samples[0]["platform"], groups=["contracts"]),
            ["WARNING: contracts 11 s vs median 5 s: cold cache or regression"],
        )
        # Exactly twice the median is not a warning.
        samples[-1]["seconds"] = 10
        self.assertEqual(
            gate_times.warning_lines(samples, platform=samples[0]["platform"], groups=["contracts"]),
            [],
        )

    def test_no_warning_with_fewer_than_three_successful_samples(self):
        samples = [
            _sample(seconds=5),
            _sample(seconds=100),
        ]
        self.assertEqual(
            gate_times.warning_lines(samples, platform=samples[0]["platform"], groups=["contracts"]),
            [],
        )
        # Failures and other platform triples do not fill the window.
        padded = [
            _sample(seconds=5, exit_status=1),
            _sample(seconds=5, platform="aarch64-apple-darwin"),
            _sample(seconds=5, platform="aarch64-apple-darwin"),
            _sample(seconds=5),
            _sample(seconds=100),
        ]
        self.assertEqual(
            gate_times.warning_lines(padded, platform="x86_64-unknown-linux-gnu", groups=["contracts"]),
            [],
        )

    def test_cli_append_and_warn_round_trip(self):
        with tempfile.TemporaryDirectory() as temporary:
            ledger = Path(temporary) / "ledger.jsonl"
            for seconds in (5, 5, 277):
                status = gate_times.main([
                    "append", "--ledger", str(ledger), "--commit", "abc", "--tier", "check",
                    "--group", "contracts", "--seconds", str(seconds), "--exit-status", "0",
                    "--platform", "x86_64-unknown-linux-gnu", "--cache", "cold",
                ])
                self.assertEqual(status, 0)
            # Capture by calling warning_lines on what the CLI would read.
            lines = gate_times.warning_lines(
                gate_times.load_samples(ledger),
                platform="x86_64-unknown-linux-gnu",
                groups=["contracts"],
            )
            self.assertEqual(
                lines,
                ["WARNING: contracts 277 s vs median 5 s: cold cache or regression"],
            )


if __name__ == "__main__":
    unittest.main()
