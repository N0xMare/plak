"""The perf smoke copies Cargo's built executable from the selected target path."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class PerfSmokeTargetTests(unittest.TestCase):
    def test_explicit_build_target_selects_triple_subdirectory(self):
        source = (ROOT / "scripts" / "perf-smoke.sh").read_text()
        prefix, marker, _ = source.partition('plak_bin="$root/plak"')
        self.assertTrue(marker, "perf script lost the executable-copy boundary")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "scripts").mkdir()
            (root / "bin").mkdir()
            (root / "tmp").mkdir()
            script = root / "scripts" / "perf-smoke.sh"
            script.write_text(prefix)
            (root / "scripts" / "test-tmpdir.sh").write_bytes(
                (ROOT / "scripts" / "test-tmpdir.sh").read_bytes())
            cargo = root / "bin" / "cargo"
            cargo.write_text("#!" + sys.executable + "\n"
                             "import os, pathlib, sys\n"
                             "assert sys.argv[1:] == ['build', '--release', '--locked', '--workspace', '--all-features']\n"
                             "base = pathlib.Path(os.environ['CARGO_TARGET_DIR'])\n"
                             "target = os.environ.get('CARGO_BUILD_TARGET')\n"
                             "output = base / target / 'release' if target else base / 'release'\n"
                             "output.mkdir(parents=True, exist_ok=True)\n"
                             "for binary in ('plak', 'plakous-js-host', 'plakous-py-host'):\n"
                             "    (output / binary).write_bytes(binary.encode() + b' release executable')\n")
            cargo.chmod(0o700)
            for target in ("x86_64-unknown-linux-musl", None):
                with self.subTest(target=target):
                    environment = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ["PATH"],
                                       CARGO_TARGET_DIR=str(root / "custom target"),
                                       PLAK_TEST_TMPDIR=str(root / "tmp"))
                    if target:
                        environment["CARGO_BUILD_TARGET"] = target
                    else:
                        environment.pop("CARGO_BUILD_TARGET", None)
                    result = subprocess.run(["bash", str(script)], cwd=root, env=environment,
                                            capture_output=True, text=True, timeout=20)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    for binary in ("plak", "plakous-js-host", "plakous-py-host"):
                        self.assertEqual((root / "tmp" / "perf-smoke" / binary).read_bytes(),
                                         binary.encode() + b" release executable")


if __name__ == "__main__":
    unittest.main()
