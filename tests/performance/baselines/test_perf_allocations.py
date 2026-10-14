from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock


MODULE_PATH = Path(__file__).with_name("perf_allocations.py")
SPEC = importlib.util.spec_from_file_location("perf_allocations", MODULE_PATH)
assert SPEC and SPEC.loader
perf = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(perf)


MEMUSAGE_STDERR = """\
Memory usage summary: heap total: 12,345, heap peak: 4,567, stack peak: 8,901
         total calls   total memory   failed calls
 malloc|          7          10000              0
realloc|          2           1000              0  (nomove:1, dec:0, free:0)
 calloc|          3           1345              0
   free|          9          10000
"""


class MemusageParserTests(unittest.TestCase):
    def test_parses_real_summary_and_allocation_calls(self) -> None:
        sample = perf.parse_memusage(MEMUSAGE_STDERR)
        self.assertEqual(sample["allocation_calls"], 12)
        self.assertEqual(sample["heap_total_bytes"], 12345)
        self.assertEqual(sample["heap_peak_bytes"], 4567)
        self.assertEqual(sample["stack_peak_bytes"], 8901)

    def test_rejects_missing_or_duplicate_summary(self) -> None:
        with self.assertRaises(perf.ProbeError):
            perf.parse_memusage("no summary")
        with self.assertRaises(perf.ProbeError):
            perf.parse_memusage(MEMUSAGE_STDERR + MEMUSAGE_STDERR)

    def test_rejects_failed_allocations(self) -> None:
        with self.assertRaises(perf.ProbeError):
            perf.parse_memusage(MEMUSAGE_STDERR.replace("10000              0", "10000              1", 1))


class ContractTests(unittest.TestCase):
    def test_stable_host_id_is_required(self) -> None:
        for value in (None, "x", "localhost", "<stable-host-id>", "spaces fail"):
            environment = {} if value is None else {"PLAKOUS_PERF_HOST_ID": value}
            with self.assertRaises(perf.ProbeError):
                perf.stable_host_id(environment)
        self.assertEqual(perf.stable_host_id({"PLAKOUS_PERF_HOST_ID": "lab-amd-01"}), "lab-amd-01")

    def test_delta_is_explicitly_derived_from_zero_n_pair(self) -> None:
        common = {
            "schema": perf.SCHEMA, "kind": "raw-sample", "pair_id": "pair",
            "workload": "core-event-round", "benchmark": "perf_core", "operation": "event-round",
            "scenario": "short", "scale": 8,
        }
        zero = {**common, "sample_id": "zero", "iteration_count": 0,
                "metrics": {"allocation_calls": 10, "heap_total_bytes": 100, "heap_peak_bytes": 80}}
        measured = {**common, "sample_id": "n", "iteration_count": 5,
                    "metrics": {"allocation_calls": 35, "heap_total_bytes": 600, "heap_peak_bytes": 120}}
        row = perf.delta_row(zero, measured)
        self.assertEqual(row["deltas"], {"allocation_calls": 25, "heap_total_bytes": 500, "heap_peak_bytes": 40})
        self.assertEqual(row["per_iteration"]["allocation_calls"], 5)
        self.assertIn("non-additive", row["peak_caveat"])

    def test_exact_row_validation_rejects_extra_fields(self) -> None:
        row = {
            "schema": perf.SCHEMA, "kind": "derived-delta", "pair_id": "p",
            "workload": "core-event-round", "benchmark": "perf_core", "operation": "event-round",
            "scenario": "short", "scale": 8,
            "iteration_count": 1, "zero_sample_id": "z", "measured_sample_id": "n",
            "derivation": "x", "deltas": {"allocation_calls": 1, "heap_total_bytes": 2, "heap_peak_bytes": 3},
            "per_iteration": {"allocation_calls": 1.0, "heap_total_bytes": 2.0, "heap_peak_bytes": 3.0},
            "qualification": "host-qualified-unratified-derived-observation", "peak_caveat": "x",
            "invented": True,
        }
        with self.assertRaises(perf.ProbeError):
            perf.validate_row(row)

    @mock.patch.object(perf.subprocess, "run")
    def test_measure_invokes_memusage_and_validates_probe_confirmation(self, run: mock.Mock) -> None:
        confirmation = '{"schema":"plakous.perf-isolated-workload.v1","bench":"perf_core","operation":"event-round","history_turns":10,"iterations":4,"completed_iterations":4}\n'
        run.return_value = subprocess.CompletedProcess([], 0, stdout=confirmation, stderr=MEMUSAGE_STDERR)
        row = perf.measure(Path("/snapshot/perf_core"), "core-event-round", "perf_core", "event-round", "short", 10, 4, "pair", "sample")
        arguments = run.call_args.args[0]
        self.assertEqual(arguments[:3], ["/usr/bin/memusage", "--", "/snapshot/perf_core"])
        self.assertEqual(arguments[3:], ["--isolated-probe", "event-round", "10", "4"])
        self.assertEqual(row["scope"], "startup-inclusive process total from one isolated benchmark invocation")
        run.return_value = subprocess.CompletedProcess([], 0, stdout="unexpected", stderr=MEMUSAGE_STDERR)
        with self.assertRaises(perf.ProbeError):
            perf.measure(Path("/snapshot/perf_core"), "core-event-round", "perf_core", "event-round", "short", 10, 4, "pair", "sample")

    @mock.patch.object(perf, "command")
    def test_discovery_uses_cargo_json_executable(self, command: mock.Mock) -> None:
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "perf_core-x"
            executable.write_bytes(b"executable")
            executable.chmod(0o755)
            command.return_value = subprocess.CompletedProcess(
                [], 0,
                stdout=json.dumps({
                    "reason": "compiler-artifact",
                    "target": {"name": "perf_core", "kind": ["bench"]},
                    "executable": str(executable),
                }) + "\n",
                stderr="",
            )
            path, arguments = perf.discover_benchmark(Path("/repo"), "perf_core")
            self.assertEqual(path, Path(directory).resolve() / "perf_core-x")
        self.assertIn("--message-format=json", arguments)
        self.assertIn("--no-run", arguments)

    def test_snapshot_rejects_symlink_and_freezes_regular_executable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.write_bytes(b"executable")
            source.chmod(0o755)
            link = root / "link"
            link.symlink_to(source)
            with self.assertRaises(perf.ProbeError):
                perf.snapshot_executable(link, root / "rejected")
            destination = root / "snapshot"
            binding = perf.snapshot_executable(source, destination)
            source.write_bytes(b"changed")
            self.assertEqual(destination.read_bytes(), b"executable")
            self.assertEqual(binding["sha256"], perf.sha256(destination))


if __name__ == "__main__":
    unittest.main()
