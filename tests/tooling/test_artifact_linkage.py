"""Tests for the built-artifact linkage gate in `check_workspace.py`.

The gate can only run where a release build exists, so the decision itself is
exercised here on fabricated symbol listings: a clean product build, a `plak`
that links the engine, and three executables that are the same file.
"""

from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/checks"))


def load_module(name: str, path: Path):  # type: ignore[no-untyped-def]
    """Import one repository script without making scripts a package."""
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


workspace_policy = load_module(
    "check_workspace_linkage", ROOT / "tools/checks/check_workspace.py"
)

# A worker listing as Mach-O prints it, and as ELF prints it. Both must count.
MACH_O_WORKER_SYMBOLS = """\
0000000100001234 T __ZN13rquickjs_core7runtime7Runtime3new17hdeadbeefE
0000000100005678 T _JS_NewRuntime
0000000100009abc T _JS_FreeRuntime
0000000100009abd T __ZN12rquickjs_sys3ffi5bindE
"""
ELF_WORKER_SYMBOLS = """\
0000000000401234 T _ZN13rquickjs_core7runtime7Runtime3new17hdeadbeefE
0000000000405678 T JS_NewRuntime
0000000000409abc T JS_FreeRuntime
"""
# The sidecar client. "rquickjs" appears in Plakous' own type and constructor
# names, which is exactly why a substring match on the crate name proves
# nothing; none of these lines is an engine marker.
CLIENT_ONLY_SYMBOLS = """\
0000000100001234 T __ZN7plakous12js_code_mode27RquickjsCodeModeIntegration6attachE
0000000100005678 T __ZN7plakous19rquickjs_sidecar_v117hcafef00dE
0000000100009abc T _main
"""
PYTHON_WORKER_SYMBOLS = """\
0000000100001234 T __ZN5monty4pool6WorkerE
0000000100005678 T _main
"""

DISTINCT_DIGESTS = {
    "monty": "a" * 64,
    "plak": "b" * 64,
    "plakous-js-host": "c" * 64,
}


class ArtifactLinkageFindings(unittest.TestCase):
    """The linkage rules, applied to listings that need no release build."""

    def test_monty_runtime_leaks_are_refused_on_both_platforms(self) -> None:
        for marker in ("_ZN5monty8bytecode2vmE", "__ZN19monty_type_checking5checkE",
                       "_RNvCsabc_11monty_alloc4Heap", "_RNvCsabc_19monty_type_checking5check",
                       "_RNvNtCsabc_11monty_proto6worker4main"):
            with self.subTest(marker=marker):
                findings = workspace_policy.artifact_linkage_findings(
                    {"plak": CLIENT_ONLY_SYMBOLS + marker,
                     "plakous-js-host": MACH_O_WORKER_SYMBOLS,
                     "monty": PYTHON_WORKER_SYMBOLS}, DISTINCT_DIGESTS)
                self.assertEqual(len(findings), 1, findings)
                self.assertIn("plak links the Monty runtime", findings[0])

    def test_monty_worker_is_the_positive_control(self) -> None:
        findings = workspace_policy.artifact_linkage_findings(
            {"plak": CLIENT_ONLY_SYMBOLS, "plakous-js-host": MACH_O_WORKER_SYMBOLS,
             "monty": CLIENT_ONLY_SYMBOLS}, DISTINCT_DIGESTS)
        self.assertEqual(len(findings), 1, findings)
        self.assertIn("monty does not link the Monty runtime", findings[0])

    def test_monty_adapter_names_do_not_count_as_runtime(self) -> None:
        findings = workspace_policy.artifact_linkage_findings(
            {"plak": "__ZN13json_to_monty5applyE\n__ZN11monty_proto6client4sendE",
             "plakous-js-host": MACH_O_WORKER_SYMBOLS,
             "monty": PYTHON_WORKER_SYMBOLS}, DISTINCT_DIGESTS)
        self.assertEqual(findings, [])

    def test_a_clean_product_build_reports_no_finding(self) -> None:
        for label, worker in (
            ("mach-o", MACH_O_WORKER_SYMBOLS),
            ("elf", ELF_WORKER_SYMBOLS),
        ):
            with self.subTest(platform=label):
                findings = workspace_policy.artifact_linkage_findings(
                    {
                        "plak": CLIENT_ONLY_SYMBOLS,
                        "plakous-js-host": worker,
                        "monty": PYTHON_WORKER_SYMBOLS,
                    },
                    DISTINCT_DIGESTS,
                )
                self.assertEqual(findings, [])

    def test_the_engine_linked_into_plak_is_a_violation(self) -> None:
        findings = workspace_policy.artifact_linkage_findings(
            {
                "plak": CLIENT_ONLY_SYMBOLS + MACH_O_WORKER_SYMBOLS,
                "plakous-js-host": MACH_O_WORKER_SYMBOLS,
                "monty": PYTHON_WORKER_SYMBOLS,
            },
            DISTINCT_DIGESTS,
        )
        self.assertEqual(len(findings), 1, findings)
        self.assertIn("plak links the QuickJS engine", findings[0])

    def test_a_worker_without_the_engine_is_a_violation(self) -> None:
        findings = workspace_policy.artifact_linkage_findings(
            {
                "plak": CLIENT_ONLY_SYMBOLS,
                "plakous-js-host": CLIENT_ONLY_SYMBOLS,
                "monty": PYTHON_WORKER_SYMBOLS,
            },
            DISTINCT_DIGESTS,
        )
        self.assertEqual(len(findings), 1, findings)
        self.assertIn("does not link the QuickJS engine", findings[0])

    def test_executables_that_share_a_digest_are_not_separate_artifacts(self) -> None:
        findings = workspace_policy.artifact_linkage_findings(
            {
                "plak": CLIENT_ONLY_SYMBOLS,
                "plakous-js-host": MACH_O_WORKER_SYMBOLS,
                "monty": PYTHON_WORKER_SYMBOLS,
            },
            {name: "d" * 64 for name in DISTINCT_DIGESTS},
        )
        self.assertEqual(len(findings), 1, findings)
        self.assertIn("not separate artifacts", findings[0])

    def test_a_listing_that_could_not_be_read_is_a_violation(self) -> None:
        findings = workspace_policy.artifact_linkage_findings(
            {
                "plak": None,
                "plakous-js-host": MACH_O_WORKER_SYMBOLS,
                "monty": PYTHON_WORKER_SYMBOLS,
            },
            DISTINCT_DIGESTS,
            "llvm-nm",
        )
        self.assertEqual(
            findings, ["plak: could not list symbols with llvm-nm"]
        )

    def test_the_client_crate_name_alone_is_not_an_engine_marker(self) -> None:
        self.assertEqual(
            workspace_policy.engine_symbol_markers(CLIENT_ONLY_SYMBOLS), set()
        )


class StaleReleaseBuild(unittest.TestCase):
    """A binary older than the tree it is evidence for is refused, not passed."""

    def build(self, root: Path, name: str, offset: float, newest: float) -> Path:
        """Write one executable whose mtime sits `offset` from the source stamp."""
        path = root / name
        path.write_bytes(name.encode())
        os.utime(path, (newest + offset, newest + offset))
        return path

    def test_only_the_binaries_older_than_the_newest_source_are_stale(self) -> None:
        newest = 1_700_000_000.0
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = {
                "plak": self.build(root, "plak", -10_000, newest),
                "plakous-js-host": self.build(root, "plakous-js-host", 10, newest),
                "monty": self.build(root, "monty", -1, newest),
            }
            with patch.object(
                workspace_policy, "newest_tracked_source_mtime", lambda _root: newest
            ):
                stale = workspace_policy.stale_release_artifacts(root, artifacts)
        self.assertEqual(stale, ["monty", "plak"])

    def test_an_undecidable_source_stamp_calls_nothing_stale(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifacts = {"plak": self.build(root, "plak", -10_000, 1_700_000_000.0)}
            with patch.object(
                workspace_policy, "newest_tracked_source_mtime", lambda _root: None
            ):
                stale = workspace_policy.stale_release_artifacts(root, artifacts)
        self.assertEqual(stale, [])

    def test_a_stale_binary_is_reported_as_not_checked_rather_than_passing(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            release = root / "target" / "release"
            release.mkdir(parents=True)
            for name in workspace_policy.PRODUCT_EXECUTABLES:
                (release / name).write_bytes(name.encode())
            with patch.object(
                workspace_policy,
                "newest_tracked_source_mtime",
                lambda _root: 2_000_000_000.0,
            ):
                errors, note = workspace_policy.artifact_linkage_errors(root)
        self.assertEqual(errors, [])
        self.assertIn("not checked: stale release build", note)


class MandatoryArtifactLinkage(unittest.TestCase):
    def test_cli_reads_raw_symbols_and_refuses_leaks_or_failed_listing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in workspace_policy.PRODUCT_EXECUTABLES:
                (root / name).write_text(name)
            tools = root / "bin"
            tools.mkdir()
            nm = tools / "nm"
            for parent, status, success in (("_main", 0, True),
                                            ("__ZN19monty_type_checking5checkE", 0, False),
                                            ("", 2, False)):
                nm.write_text(
                    '#!/bin/sh\ncase "$1" in\n'
                    f' */plak) echo "{parent}"; exit {status} ;;\n'
                    ' */monty) echo "__ZN5monty2vmE" ;;\n'
                    ' */plakous-js-host) echo "_JS_NewRuntime" ;;\nesac\n'
                )
                nm.chmod(0o700)
                result = subprocess.run(
                    [sys.executable, str(ROOT / "tools/build/check-artifact-linkage.py"), str(root)],
                    env=dict(os.environ, PATH=str(tools)), capture_output=True, text=True,
                    check=False,
                )
                self.assertEqual(result.returncode == 0, success, result.stderr)
                if success:
                    self.assertIn("plak sha256", result.stdout)

    def test_missing_artifacts_fail_closed(self):
        errors, _ = workspace_policy.verify_artifact_linkage({})
        self.assertEqual(errors, ["missing product artifacts: plak, plakous-js-host, monty"])

    def test_missing_symbol_tool_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            artifacts = {}
            for name in workspace_policy.PRODUCT_EXECUTABLES:
                artifacts[name] = Path(directory) / name
                artifacts[name].write_text(name)
            with patch.object(workspace_policy, "symbol_tool", return_value=None):
                errors, _ = workspace_policy.verify_artifact_linkage(artifacts)
        self.assertEqual(errors, ["artifact linkage requires nm or llvm-nm on PATH"])


if __name__ == "__main__":
    unittest.main()
