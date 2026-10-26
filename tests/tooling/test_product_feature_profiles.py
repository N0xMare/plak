"""Linear product feature-off witnesses and warning-denied compile coverage."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("product_features", ROOT / "tools/checks/check_features.py")
assert spec and spec.loader
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class ProductFeatureProfilesTests(unittest.TestCase):
    def manifest(self):
        return {"features": {"default": ["git-cli", *gate.PRODUCT_FEATURES]}}

    def test_six_off_witnesses_do_not_reenable_an_omitted_feature(self):
        profiles = gate.product_off_profiles(self.manifest())
        self.assertEqual(len(profiles), 6)
        for feature, (name, selected, default) in zip(gate.PRODUCT_FEATURES, profiles):
            self.assertEqual(name, f"without-{feature}")
            self.assertNotIn(feature, selected)
            self.assertFalse(default)
            if feature in {"tui", "images"}:
                self.assertNotIn("clipboard", selected)
        self.assertEqual(profiles[-1][1], ("git-cli",))

    def test_compile_covers_bin_and_lib_and_warning_failure_is_not_cached(self):
        warning = json.dumps({"reason": "compiler-message", "message": {
            "level": "warning", "rendered": "warning: unused feature-off binding\n"}})
        results = [subprocess.CompletedProcess([], 0, warning, "")] + [
            subprocess.CompletedProcess([], 0, "", "") for _ in range(7)]
        with mock.patch.object(gate.subprocess, "run", side_effect=results) as run:
            errors = gate.check_product_profiles({"plak": self.manifest()}, operation="check", target="reviewed-triple")
        self.assertEqual(len(errors), 1)
        self.assertIn("without-tui", errors[0])
        self.assertEqual(run.call_count, 8)
        for call in run.call_args_list:
            command = call.args[0]
            for flag in ["--locked", "--offline", "--no-default-features", "--lib", "--bins", "--target", "reviewed-triple"]:
                self.assertIn(flag, command)
            if command[1] == "clippy":
                self.assertEqual(command[-3:], ["--", "-D", "warnings"])
            else:
                self.assertIn("--message-format=json", command)
            features = command[command.index("--features") + 1].split(",")
            self.assertNotEqual(set(features), {"git-cli", "tui", "clipboard", "images", "mcp"})
            self.assertNotEqual(set(features), {"git-cli", "tui", "clipboard", "images", "python"})

    def test_complete_external_closure_pins_catch_a_planted_headless_ratatui_edge(self):
        with mock.patch.object(gate, "full_closure", return_value={"ratatui"}):
            errors = gate.validate_excluded_closures()
        self.assertEqual(len(errors), 1)
        self.assertIn("ratatui", errors[0])
        self.assertIn("git-cli+images+python+mcp", errors[0])


if __name__ == "__main__":
    unittest.main()
