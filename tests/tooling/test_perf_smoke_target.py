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
        source = (ROOT / "tools/perf/perf-smoke.sh").read_text()
        prefix, marker, _ = source.partition('plak_bin="$root/plak"')
        self.assertTrue(marker, "perf script lost the executable-copy boundary")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "tools/perf").mkdir(parents=True)
            (root / "tools/gate").mkdir(parents=True)
            (root / "tools/build").mkdir(parents=True)
            (root / "bin").mkdir()
            (root / "tmp").mkdir()
            script = root / "tools/perf/perf-smoke.sh"
            script.write_text(prefix)
            for name in ("test-tmpdir.sh", "test-env.sh"):
                destination = root / "tools/gate" / name
                destination.write_bytes((ROOT / "tools/gate" / name).read_bytes())
                destination.chmod(0o755)
            builder = root / "tools/build/build-plak.sh"
            builder.write_bytes((ROOT / "tools/build/build-plak.sh").read_bytes())
            builder.chmod(0o755)
            (root / "tools/build/check-artifact-linkage.py").write_text(
                "import sys\nraise SystemExit(0)\n"
            )
            cargo = root / "bin" / "cargo"
            cargo.write_text(
                "#!" + sys.executable + "\n"
                "import os, pathlib, sys\n"
                "assert os.environ['CARGO_PROFILE_RELEASE_STRIP'] == 'debuginfo'\n"
                "expected = [\n"
                " ['build', '--locked', '-p', 'plak', '--features', 'plak/test-hooks,plak/test-support', '--profile', 'release'],\n"
                " ['build', '--locked', '-p', 'plakous-js-runtime', '-p', 'plakous-py-worker', '--profile', 'release'],\n"
                "]\n"
                "log = pathlib.Path(os.environ['CARGO_LOG'])\n"
                "seen = log.read_text().splitlines() if log.exists() else []\n"
                "assert sys.argv[1:] == expected[len(seen)], (sys.argv[1:], seen)\n"
                "log.write_text('\\n'.join(seen + [' '.join(sys.argv[1:])]) + '\\n')\n"
                "base = pathlib.Path(os.environ['CARGO_TARGET_DIR'])\n"
                "target = os.environ.get('CARGO_BUILD_TARGET')\n"
                "output = base / target / 'release' if target else base / 'release'\n"
                "output.mkdir(parents=True, exist_ok=True)\n"
                "for binary in ('plak', 'plakous-js-host', 'plakous-py-host'):\n"
                "    path = output / binary\n"
                "    path.write_bytes(binary.encode() + b' release executable')\n"
                "    path.chmod(0o755)\n"
            )
            cargo.chmod(0o700)
            for target in ("x86_64-unknown-linux-musl", None):
                with self.subTest(target=target):
                    (root / "cargo-log").unlink(missing_ok=True)
                    environment = dict(
                        os.environ,
                        PATH=str(root / "bin") + os.pathsep + os.environ["PATH"],
                        CARGO_TARGET_DIR=str(root / "custom target"),
                        CARGO_LOG=str(root / "cargo-log"),
                        PLAK_TEST_TMPDIR=str(root / "tmp"),
                    )
                    if target:
                        environment["CARGO_BUILD_TARGET"] = target
                    else:
                        environment.pop("CARGO_BUILD_TARGET", None)
                    result = subprocess.run(
                        ["bash", str(script)],
                        cwd=root,
                        env=environment,
                        capture_output=True,
                        text=True,
                        timeout=20,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    for binary in ("plak", "plakous-js-host", "plakous-py-host"):
                        self.assertEqual(
                            (root / "tmp" / "perf-smoke" / binary).read_bytes(),
                            binary.encode() + b" release executable",
                        )


if __name__ == "__main__":
    unittest.main()
