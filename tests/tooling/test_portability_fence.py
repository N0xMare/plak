"""The portable-crate fence rejects native-only dependencies and names the path."""

from __future__ import annotations

import importlib.util
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/gate"))


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


checker = load_module("check_workspace_portability", ROOT / "tools/gate/check_workspace.py")

CLEAN = """\
0plakous-model v0.0.3 (/src/plakous-model) feats=[]
1uuid v1.28.0 feats=[js,serde,v7]
2wasm-bindgen v0.2.129 feats=[std]
1tokio v1.53.0 feats=[macros,rt,sync]
"""

LIBC = """\
0plakous-model v0.0.3 (/src/plakous-model) feats=[]
1uuid v1.28.0 feats=[serde,v7]
2getrandom v0.4.3 feats=[]
3libc v0.2.190 feats=[]
"""

# A proc-macro runs on the host; its own closure is not built for the target.
PROC_MACRO = """\
0plakous-model v0.0.3 (/src/plakous-model) feats=[]
1derive-helper v0.1.0 (proc-macro) feats=[]
2libc v0.2.190 feats=[]
3rustix v1.0.0 feats=[]
1serde v1.0.229 feats=[]
"""

NIX = """\
0plakous-model v0.0.3 (/src/plakous-model) feats=[]
1helper v0.1.0 feats=[]
2nix v0.28.0 feats=[resource]
"""

TOKIO = """\
0plakous-model v0.0.3 (/src/plakous-model) feats=[]
1tokio v1.53.0 feats=[fs,macros,net,rt,rt-multi-thread,signal,sync]
1socket2 v0.5.0 feats=[] (*)
"""


class PortabilityFenceTests(unittest.TestCase):
    def test_the_portable_set_is_the_model_and_the_runner_protocol(self):
        self.assertEqual(checker.PORTABLE_CRATES, ("plakous-model", "plakous-runner-protocol"))

    def test_portable_crates_flag_prints_only_that_set(self):
        buffer = StringIO()
        with (
            patch.object(sys, "argv", ["check_workspace.py", "--portable-crates"]),
            redirect_stdout(buffer),
        ):
            self.assertEqual(checker.main(), 0)
        self.assertEqual(
            buffer.getvalue().splitlines(), ["plakous-model", "plakous-runner-protocol"]
        )
        with patch.object(sys, "argv", ["check_workspace.py", "--nope"]):
            self.assertEqual(checker.main(), 2)

    def test_libc_is_banned_and_notify_is_banned_ahead_of_the_lock(self):
        nodes, errors = checker.parse_feature_tree(LIBC)
        self.assertEqual(errors, [])
        self.assertEqual(
            checker.closure_findings("plakous-model", "default", nodes),
            [
                "plakous-model [default]: libc v0.2.190 "
                "via plakous-model -> uuid -> getrandom -> libc"
            ],
        )
        names = set(re.findall(r'^name = "([^"]+)"', (ROOT / "Cargo.lock").read_text(), re.M))
        self.assertEqual(checker.BANNED_NATIVE_CRATES - names, {"notify"})
        self.assertIn("notify", checker.BANNED_NATIVE_CRATES)

    def test_tree_arguments_are_the_normal_closure_at_both_resolutions(self):
        default = checker.portable_tree_args("plakous-model", all_features=False)
        everything = checker.portable_tree_args("plakous-model", all_features=True)
        self.assertEqual(
            default[:6],
            ["-p", "plakous-model", "-e", "normal", "--target", "wasm32-unknown-unknown"],
        )
        self.assertEqual(everything[:6], default[:6])
        self.assertNotIn("--all-features", default)
        self.assertIn("--all-features", everything)
        self.assertNotIn("dev", default)

    def test_a_clean_closure_passes_and_a_banned_crate_names_the_path(self):
        nodes, errors = checker.parse_feature_tree(CLEAN)
        self.assertEqual(errors, [])
        self.assertEqual(checker.closure_findings("plakous-model", "default", nodes), [])
        nodes, errors = checker.parse_feature_tree(NIX)
        self.assertEqual(errors, [])
        self.assertEqual(
            checker.closure_findings("plakous-model", "default", nodes),
            ["plakous-model [default]: nix v0.28.0 via plakous-model -> helper -> nix"],
        )

    def test_banned_tokio_features_are_named_together(self):
        nodes, errors = checker.parse_feature_tree(TOKIO)
        self.assertEqual(errors, [])
        self.assertEqual(
            checker.closure_findings("plakous-model", "all-features", nodes),
            [
                "plakous-model [all-features]: tokio v1.53.0 features "
                "fs, net, rt-multi-thread, signal via plakous-model -> tokio",
                "plakous-model [all-features]: socket2 v0.5.0 via plakous-model -> socket2",
            ],
        )

    def test_a_proc_macro_subtree_is_host_code_outside_the_fence(self):
        nodes, errors = checker.parse_feature_tree(PROC_MACRO)
        self.assertEqual(errors, [])
        self.assertEqual([node.proc_macro for node in nodes], [False, True, False, False, False])
        self.assertEqual(checker.closure_findings("plakous-model", "default", nodes), [])

    def test_an_unparsed_tree_and_a_tree_that_starts_elsewhere_fail_closed(self):
        _nodes, errors = checker.parse_feature_tree("not a cargo tree line\n")
        self.assertEqual(errors, ["unparsed cargo tree line 1: not a cargo tree line"])
        nodes, errors = checker.parse_feature_tree("0other v1.0.0 feats=[]\n")
        self.assertEqual(errors, [])
        self.assertEqual(
            checker.closure_findings("plakous-model", "default", nodes),
            ["plakous-model [default]: cargo tree did not start at the crate"],
        )

    def test_the_live_plakous_model_closure_is_portable(self):
        workspace = checker.load_workspace(ROOT)
        self.assertEqual(checker.portable_closure_errors(workspace), [])

    def test_the_fence_resolves_the_portable_target_not_the_host(self):
        # Two local crates carry banned names. The wasm-only one is in the
        # wasm closure; the native-only one is not, on any host.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copyfile(ROOT / "rust-toolchain.toml", root / "rust-toolchain.toml")
            (root / "Cargo.toml").write_text(
                '[workspace]\nmembers = ["portable", "nix", "rustix"]\nresolver = "2"\n'
            )
            for name in ("nix", "rustix"):
                self.fixture_crate(root, name, "")
            self.fixture_crate(
                root,
                "portable",
                '[target.\'cfg(target_arch = "wasm32")\'.dependencies]\n'
                'nix = { path = "../nix" }\n'
                '[target.\'cfg(not(target_arch = "wasm32"))\'.dependencies]\n'
                'rustix = { path = "../rustix" }\n',
            )
            subprocess.run(
                ["cargo", "generate-lockfile", "--offline"],
                cwd=root,
                check=True,
                capture_output=True,
            )
            workspace = SimpleNamespace(root=root, packages={"portable": object()})
            with patch.object(checker, "PORTABLE_CRATES", ("portable",)):
                errors = checker.portable_closure_errors(workspace)
        self.assertEqual(
            errors,
            [
                "portable [default]: nix v0.0.1 via portable -> nix",
                "portable [all-features]: nix v0.0.1 via portable -> nix",
            ],
        )

    @staticmethod
    def fixture_crate(root: Path, name: str, dependencies: str) -> None:
        (root / name / "src").mkdir(parents=True)
        (root / name / "src/lib.rs").write_text("")
        (root / name / "Cargo.toml").write_text(
            f'[package]\nname = "{name}"\nversion = "0.0.1"\nedition = "2021"\n\n{dependencies}'
        )

    def test_a_reported_tree_names_the_crate_the_dependency_and_the_path(self):
        seen: list[list[str]] = []

        def fake(_root, spec, _errors):
            seen.append(spec)
            if "--all-features" in spec:
                return CLEAN
            return NIX

        workspace = SimpleNamespace(root=ROOT, packages={"plakous-model": object()})
        with (
            patch.object(checker, "PORTABLE_CRATES", ("plakous-model",)),
            patch.object(checker, "cargo_tree_output", side_effect=fake),
        ):
            errors = checker.portable_closure_errors(workspace)
        self.assertEqual(
            errors,
            ["plakous-model [default]: nix v0.28.0 via plakous-model -> helper -> nix"],
        )
        self.assertEqual([spec[-1] != "--all-features" for spec in seen], [True, False])


if __name__ == "__main__":
    unittest.main()
