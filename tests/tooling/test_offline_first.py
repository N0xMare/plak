"""tools/release/offline_first.py: offline first, a stale index entry refreshed once."""

from __future__ import annotations

import importlib.util
import io
import subprocess
import unittest
from contextlib import redirect_stderr
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("offline_first", ROOT / "tools/release/offline_first.py")
assert SPEC is not None and SPEC.loader is not None
offline_first = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(offline_first)

MISMATCH = ('error: failed to select a version for the requirement `monty-typeshed = "^1.2.0"`\n'
            "candidate versions found which didn't match: 1.1.0\n")
CONFLICT = ("error: failed to select a version for `ruff_python_ast` which could resolve "
            "this conflict\n")


class FakeCargo:
    """Answers each call from a script of (returncode, stderr) by command shape."""

    def __init__(self, offline: list[tuple[int, str]], info: int = 0, online: int = 0):
        self.offline = list(offline)
        self.info = info
        self.online = online
        self.calls: list[list[str]] = []
        self.info_cwds: list[Path] = []

    def __call__(self, command, **kwargs):
        self.calls.append(list(command))
        if command[1] == "info":
            cwd = Path(kwargs["cwd"])
            # An empty directory with no Cargo.toml above it: no workspace patch
            # can answer the crate from a local path.
            assert cwd.is_dir() and not any(cwd.iterdir())
            assert not any((parent / "Cargo.toml").exists() for parent in (cwd, *cwd.parents))
            self.info_cwds.append(cwd)
            code, stderr = self.info, ""
        elif "--offline" in command:
            code, stderr = self.offline.pop(0)
        else:
            code, stderr = self.online, ""
        return subprocess.CompletedProcess(command, code, stderr=stderr)


def run(cargo: FakeCargo) -> int:
    with redirect_stderr(io.StringIO()):
        return offline_first.run(["cargo", "fetch"], runner=cargo)


class OfflineFirstTests(unittest.TestCase):
    def test_names_come_from_both_mismatch_spellings(self) -> None:
        self.assertEqual(offline_first.stale_index_names(MISMATCH + CONFLICT + MISMATCH),
                         ["monty-typeshed", "ruff_python_ast"])
        self.assertEqual(offline_first.stale_index_names("error: no matching package named"), [])

    def test_a_warm_cache_never_goes_online(self) -> None:
        cargo = FakeCargo([(0, "")])
        self.assertEqual(run(cargo), 0)
        self.assertEqual(cargo.calls, [["cargo", "fetch", "--offline"]])

    def test_a_stale_entry_is_refreshed_and_the_offline_step_retried_once(self) -> None:
        cargo = FakeCargo([(101, MISMATCH), (0, "")])
        self.assertEqual(run(cargo), 0)
        self.assertEqual(cargo.calls, [
            ["cargo", "fetch", "--offline"],
            ["cargo", "info", "--quiet", "--", "monty-typeshed"],
            ["cargo", "fetch", "--offline"],
        ])

    def test_the_refresh_runs_outside_the_callers_workspace(self) -> None:
        cargo = FakeCargo([(101, MISMATCH), (0, "")])
        self.assertEqual(run(cargo), 0)
        self.assertEqual(len(cargo.info_cwds), 1)
        self.assertNotEqual(cargo.info_cwds[0], Path.cwd())
        self.assertFalse(cargo.info_cwds[0].exists(), "the empty directory is removed")

    def test_names_must_start_with_a_letter(self) -> None:
        self.assertEqual(offline_first.stale_index_names(
            'failed to select a version for the requirement `-x = "1"`'), [])

    def test_a_retry_that_still_fails_runs_the_step_online(self) -> None:
        cargo = FakeCargo([(101, MISMATCH), (101, MISMATCH)], online=0)
        self.assertEqual(run(cargo), 0)
        self.assertEqual(cargo.calls[-1], ["cargo", "fetch"])
        self.assertEqual(sum(call[1] == "info" for call in cargo.calls), 1)

    def test_a_failed_refresh_skips_the_retry(self) -> None:
        cargo = FakeCargo([(101, MISMATCH)], info=101, online=7)
        self.assertEqual(run(cargo), 7)
        self.assertEqual(cargo.calls, [
            ["cargo", "fetch", "--offline"],
            ["cargo", "info", "--quiet", "--", "monty-typeshed"],
            ["cargo", "fetch"],
        ])

    def test_a_missing_input_goes_online_without_a_refresh(self) -> None:
        cargo = FakeCargo([(101, "error: failed to download `x`")], online=0)
        self.assertEqual(run(cargo), 0)
        self.assertEqual(cargo.calls, [["cargo", "fetch", "--offline"], ["cargo", "fetch"]])


if __name__ == "__main__":
    unittest.main()
