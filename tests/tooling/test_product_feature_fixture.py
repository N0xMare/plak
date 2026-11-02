"""Feature-fixture processes stay off the operator subscription store."""

from __future__ import annotations

import importlib.util
import os
import stat
import subprocess
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "product_feature_fixture", ROOT / "tools/gate/product_feature_fixture.py"
)
assert SPEC and SPEC.loader
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)

OPERATOR = {
    "HOME": "/operator/real-home",
    "PLAK_HOME": "/operator/real-home/.plak",
    "XDG_STATE_HOME": "/operator/real-state",
    "XDG_CONFIG_HOME": "/operator/real-config",
    "XDG_DATA_DIRS": "/operator/share",
    "OPENAI_API_KEY": "sk-operator",
}


def _assert_isolated(test: unittest.TestCase, env: dict[str, str]) -> None:
    root = Path(env["PLAK_HOME"]).parent
    test.assertEqual(Path(env["HOME"]), root / "operator-home")
    test.assertEqual(Path(env["XDG_STATE_HOME"]), root / "xdg" / "state-home")
    test.assertEqual(Path(env["XDG_CONFIG_HOME"]), root / "xdg" / "config-home")
    test.assertEqual(Path(env["XDG_DATA_HOME"]), root / "xdg" / "data-home")
    test.assertEqual(Path(env["XDG_CACHE_HOME"]), root / "xdg" / "cache-home")
    test.assertEqual(Path(env["XDG_RUNTIME_DIR"]), root / "xdg" / "runtime-dir")
    test.assertTrue(str(env["XDG_DATA_DIRS"]).startswith(str(root)))
    for name in ("HOME", "PLAK_HOME", "XDG_STATE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_DIRS"):
        test.assertNotEqual(env[name], OPERATOR[name])
        test.assertNotIn("/operator/", env[name])
    test.assertNotIn("OPENAI_API_KEY", env)
    for path in (Path(env["HOME"]), Path(env["PLAK_HOME"]), Path(env["XDG_STATE_HOME"])):
        test.assertTrue(path.is_dir(), path)
        test.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700)


class FixtureIsolationTests(unittest.TestCase):
    def test_route_fixture_accepts_in_a_private_home(self):
        def run(*_args, **kwargs):
            _assert_isolated(self, kwargs["env"])
            self.assertEqual(kwargs["input"], FIXTURE.ACCEPTANCE)
            return subprocess.CompletedProcess([], 1, "", "refused")

        with (
            mock.patch.dict(os.environ, OPERATOR, clear=False),
            mock.patch.object(FIXTURE.subprocess, "run", side_effect=run) as calls,
        ):
            errors = FIXTURE.check_route("writer", "reduced")
        self.assertEqual(calls.call_count, 1)
        self.assertTrue(any("uncompiled route fixture creation" in error for error in errors))
        command = calls.call_args.args[0]
        self.assertIn("create route fixture", command)

    def test_canonical_fixture_creation_accepts_in_a_private_home(self):
        def run(*_args, **kwargs):
            _assert_isolated(self, kwargs["env"])
            return subprocess.CompletedProcess([], 1, "", "refused")

        with (
            mock.patch.dict(os.environ, OPERATOR, clear=False),
            mock.patch.object(FIXTURE.subprocess, "run", side_effect=run) as calls,
        ):
            errors = FIXTURE.check("writer", "reduced", "server")
        creations = [call for call in calls.call_args_list if "create fixture" in call.args[0]]
        self.assertEqual(len(creations), 2)
        for call in creations:
            self.assertEqual(call.kwargs["input"], FIXTURE.ACCEPTANCE)
        for call in calls.call_args_list:
            if "create fixture" not in call.args[0]:
                self.assertEqual(call.kwargs["input"], "")
        self.assertGreaterEqual(calls.call_count, 2)
        self.assertTrue(any("python canonical fixture creation" in error for error in errors))
        self.assertTrue(any("mcp canonical fixture creation" in error for error in errors))

    def test_image_diagnostic_uses_the_private_home(self):
        def popen(*_args, **kwargs):
            _assert_isolated(self, kwargs["env"])
            raise RuntimeError("stop after capturing the environment")

        with (
            mock.patch.dict(os.environ, OPERATOR, clear=False),
            mock.patch.object(FIXTURE.subprocess, "Popen", side_effect=popen),
        ):
            with self.assertRaises(RuntimeError):
                FIXTURE.image_diagnostic("binary")


if __name__ == "__main__":
    unittest.main()
