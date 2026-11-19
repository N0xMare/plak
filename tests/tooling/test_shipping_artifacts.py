import copy
import hashlib
import json
import os
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

from tests.tooling.shipping_fixture import ROOT, executable, manifest, manifests


class ShippingArtifacts(unittest.TestCase):
    def setUp(self):
        work = tempfile.TemporaryDirectory()
        self.addCleanup(work.cleanup)
        self.root = Path(work.name).resolve()
        self.home = self.root / "home"
        self.home.mkdir()
        self.artifacts = self.root / "artifacts"
        self.artifacts.mkdir()
        (self.artifacts / "plakd").write_text(executable())
        for name in manifests.NAMES[1:]:
            (self.artifacts / name).write_text(name)
        self.data = manifest(self.artifacts)
        self.manifest = self.artifacts / "manifest.json"
        self.manifest.write_bytes(manifests.canonical(self.data))

    def install(self, *args):
        return subprocess.run(
            ["sh", ROOT / "install.sh", *args],
            env={**os.environ, "HOME": str(self.home), "CARGO_HOME": str(self.home / ".cargo")},
            check=False,
            capture_output=True,
            text=True,
        )

    def test_closed_canonical_manifest_and_sidecar_identity(self):
        reordered = dict(reversed(list(self.data.items())))
        self.assertEqual(manifests.canonical(reordered), self.manifest.read_bytes())
        self.assertEqual(manifests.read_canonical(self.manifest.read_bytes()), self.data)
        for mutation in (
            {"unknown": "anything"},
            {"build_info": {**self.data["build_info"], "future_field": 1}},
            {"revision": "9" * 40},
        ):
            with self.assertRaises(ValueError):
                manifests.validate_manifest({**self.data, **mutation})
        for facts, error in (
            ({"test_hooks": True}, "must not include test hooks"),
            ({"test_hooks": 0}, "must not include test hooks"),
            ({"omitted_features": ["js"]}, "must not omit product features"),
        ):
            with self.subTest(facts=facts), self.assertRaisesRegex(ValueError, error):
                manifests.validate_build_info({**self.data["build_info"], **facts})
        with self.assertRaisesRegex(ValueError, "duplicate"):
            manifests.read_canonical(b'{"x":1,"x":2}\n')
        with self.assertRaisesRegex(ValueError, "noncanonical"):
            manifests.read_canonical(b'{"z": 1, "a": 2}\n')
        worker = "plakous-js-host"
        # Preserve the length so only the digest, display id and snapshot id may differ.
        (self.artifacts / worker).write_text("X" * len(worker))
        changed = manifest(self.artifacts)
        expected = copy.deepcopy(self.data)
        expected["files"][worker]["sha256"] = hashlib.sha256(b"X" * len(worker)).hexdigest()
        expected["files"][worker]["display_id"] = expected["files"][worker]["sha256"][:12]
        expected["snapshot_id"] = changed["snapshot_id"]
        self.assertNotEqual(self.data["snapshot_id"], changed["snapshot_id"])
        self.assertEqual(expected, changed)

    def test_path_scan_checks_each_shipping_binary_and_each_prefix(self):
        forbidden = (self.root / "physical checkout", self.root / "cargo-home")
        manifests.check_paths(self.artifacts, forbidden)
        for name in manifests.NAMES:
            path = self.artifacts / name
            before = path.read_bytes()
            for prefix in forbidden:
                with self.subTest(name=name, prefix=prefix):
                    path.write_bytes(before + os.fsencode(prefix))
                    with self.assertRaisesRegex(
                        ValueError, f"{name} contains a physical build path"
                    ):
                        manifests.check_paths(self.artifacts, forbidden)
            path.write_bytes(before)

    def test_mismatched_length_or_digest_writes_nothing_to_home(self):
        self.set_platform()
        path = self.artifacts / "plakous-py-host"
        for bad, message in (
            (b"x", "length mismatch"),
            (b"?" * path.stat().st_size, "digest mismatch"),
        ):
            with self.subTest(message=message):
                path.write_bytes(bad)
                result = self.install("--from", self.artifacts, "--no-link")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assertEqual(list(self.home.iterdir()), [])

    def test_no_link_is_idempotent_and_link_step_changes_only_owned_links(self):
        owned = self.home / ".local/lib/plak/release" / ("0" * 64) / "plakd"
        owned.parent.mkdir(parents=True)
        bindirs = [self.home / ".local/bin", self.home / ".cargo/bin"]
        for directory in bindirs:
            directory.mkdir(parents=True)
            (directory / "plakd").symlink_to(owned)
            (directory / "unrelated").symlink_to(self.root / "foreign")
        for name in ("binary-id", "state-key", "export-version"):
            (self.artifacts / name).write_text("poison from builder\n")
        # Use the host target for the same fixtures on both platforms.
        self.set_platform()
        first = self.install("--no-link", "--from", self.artifacts)
        self.assertEqual(first.returncode, 0, first.stderr)
        snapshot = Path(first.stdout.strip())
        self.assertEqual(snapshot.name, self.data["snapshot_id"])
        observed = {
            name: ((snapshot / name).stat().st_ino, (snapshot / name).read_text().strip())
            for name in ("binary-id", "state-key", "export-version")
        }
        self.assertEqual(
            {key: facts[1] for key, facts in observed.items()},
            {"binary-id": "fixture", "state-key": "r1.e1.p1.t1", "export-version": "7"},
        )
        second = self.install("--from", self.artifacts, "--no-link")
        self.assertEqual(second.stdout, first.stdout, second.stderr)
        for name, facts in observed.items():
            self.assertEqual((snapshot / name).stat().st_ino, facts[0])
        for directory in bindirs:
            self.assertEqual(
                {path.name: path.readlink() for path in directory.iterdir()},
                {"plakd": owned, "unrelated": self.root / "foreign"},
            )
        foreign = bindirs[1] / "plakous-py-host"
        foreign.write_text("foreign file")
        refused = self.install("link", snapshot.name)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("refusing to replace non-Plak", refused.stderr)
        self.assertEqual((bindirs[0] / "plakd").readlink(), owned)
        self.assertEqual(foreign.read_text(), "foreign file")
        foreign.unlink()
        linked = self.install("link", snapshot.name)
        self.assertEqual(linked.returncode, 0, linked.stderr)
        for directory in bindirs:
            self.assertEqual((directory / "plakd").readlink(), snapshot / "plakd")
            self.assertEqual((directory / "unrelated").readlink(), self.root / "foreign")

    def set_platform(self):
        if os.uname().sysname == "Darwin":
            self.data["platform"] = "aarch64-apple-darwin"
            self.manifest.write_bytes(manifests.canonical(self.data))

    def test_tarball_with_external_manifest_publishes_only_verified_files(self):
        self.set_platform()
        archive = self.root / "bundle.tar.gz"
        marker = self.artifacts / "binary-id"
        marker.write_text("not transferred")
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(self.artifacts, arcname="release")
            tar.add(marker, arcname="../outside-the-install")
        result = self.install("--from", archive, "--manifest", self.manifest, "--no-link")
        self.assertEqual(result.returncode, 0, result.stderr)
        installed = Path(result.stdout.strip())
        self.assertEqual((installed / "binary-id").read_text(), "fixture\n")
        self.assertFalse((self.root / "outside-the-install").exists())


class BuildInfoProbe(unittest.TestCase):
    def test_closed_read_only_probe_agrees_with_existing_probes(self):
        binary = os.environ.get("PLAK_TOOLING_BINARY")
        if not binary:
            self.skipTest("requires the real tooling binary from just steps tooling")
        with tempfile.TemporaryDirectory() as root:
            home = Path(root) / "not-created"
            state = Path(root) / "no-state"
            env = {**os.environ, "PLAK_HOME": str(home), "XDG_STATE_HOME": str(state)}

            def run(*args, check=True):
                return subprocess.run(
                    [binary, *args],
                    env=env,
                    check=check,
                    capture_output=True,
                    text=True,
                    timeout=15,
                )

            probe = manifests.read_canonical(run("version", "--build-info").stdout.encode())
            self.assertEqual(list(Path(root).iterdir()), [])
            home.mkdir()
            self.assertEqual(json.loads(run("version", "--build-info").stdout), probe)
            self.assertEqual(list(home.iterdir()), [])
            self.assertFalse(state.exists())
            version = run("version").stdout
            self.assertIs(probe["test_hooks"], "; test-hooks" in version)
            omitted = [f"-{name}" for name in probe["omitted_features"]]
            if omitted:
                self.assertIn("; features " + ",".join(omitted), version)
            else:
                self.assertNotIn("; features ", version)
            self.assertNotIn("--build-info", run("version", "--help").stdout)
            for flag in ("state-key", "export-version", "host-upgrade-probe", "reload-probe"):
                self.assertEqual(
                    run("version", "--build-info", f"--{flag}", check=False).returncode, 2
                )
            self.assertEqual(probe["state_key"], run("version", "--state-key").stdout.strip())
            self.assertEqual(
                probe["export"]["write"], int(run("version", "--export-version").stdout)
            )
            upgrade = json.loads(run("version", "--host-upgrade-probe").stdout)
            self.assertEqual(probe["export"]["read"], upgrade["export_versions"])
            self.assertEqual(
                probe["host_upgrade"],
                {"coordinator": upgrade["protocol"], "receipt": upgrade["receipt_version"]},
            )
            self.assertEqual(probe["runner"]["protocol"], upgrade["runner_protocol"])
            for key, field in (("revision", "source_revision"), ("flavor", "flavor")):
                self.assertEqual(probe[key], upgrade["runner_version"][field])
            wire = json.loads(run("wire", "schema").stdout)
            del wire["binary_id"]
            pin = hashlib.sha256(manifests.canonical(wire).removesuffix(b"\n")).hexdigest()
            self.assertEqual(probe["wire"], {"number": wire["wire_version"], "schema_pin": pin})
