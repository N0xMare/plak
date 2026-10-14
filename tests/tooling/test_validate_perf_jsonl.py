#!/usr/bin/env python3
"""Focused unit tests for performance evidence identity validation."""

from __future__ import annotations

import copy
import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[2] / "tools/perf/validate_perf_jsonl.py"
SPEC = importlib.util.spec_from_file_location("validate_perf_jsonl", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
VALIDATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VALIDATOR)


class PerformanceIdentityTests(unittest.TestCase):
    def test_rust_perf_host_id_is_bounded_and_non_placeholder(self) -> None:
        self.assertEqual(
            VALIDATOR.stable_host_id({"host": {"host_id": "benchmark-worker-07"}}),
            "benchmark-worker-07",
        )
        for value in (
            None,
            "",
            "ab",
            "host",
            "LOCALHOST",
            "unknown",
            "stable-host-id",
            "<stable-host-id>",
            "contains space",
            "x" * 129,
        ):
            with (
                self.subTest(value=value),
                self.assertRaises(VALIDATOR.ValidationError),
            ):
                VALIDATOR.stable_host_id({"host": {"host_id": value}})

    def test_each_rust_perf_validator_requires_host_id(self) -> None:
        cases = (
            (VALIDATOR.validate_core, "plakous.perf-core.v4"),
            (VALIDATOR.validate_project, "plakous.perf-project.v6"),
            (VALIDATOR.validate_store, "plakous.perf-store.v6"),
        )
        for validator, schema in cases:
            for host in ({}, {"host_id": None}, {"host_id": ""}):
                with (
                    self.subTest(schema=schema, host=host),
                    self.assertRaises(VALIDATOR.ValidationError),
                ):
                    validator([{"schema": schema, "host": host}])

    def test_tagged_sha256_requires_exact_lowercase_digest(self) -> None:
        valid = {"digest": "sha256:" + "0a" * 32}
        self.assertEqual(VALIDATOR.tagged_sha256(valid, "digest"), valid["digest"])
        for value in (
            "sha256:x",
            "sha256:" + "A0" * 32,
            "sha512:" + "00" * 32,
            "sha256:" + "00" * 31,
            None,
        ):
            with (
                self.subTest(value=value),
                self.assertRaises(VALIDATOR.ValidationError),
            ):
                VALIDATOR.tagged_sha256({"digest": value}, "digest")

    def test_tagged_sha256_rejects_missing_binding(self) -> None:
        with self.assertRaises(VALIDATOR.ValidationError):
            VALIDATOR.tagged_sha256({}, "digest")


if __name__ == "__main__":
    unittest.main()
