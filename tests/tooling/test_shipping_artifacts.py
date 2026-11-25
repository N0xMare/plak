import copy
import hashlib
import json
import os
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

from tests.tooling.test_release import Server, release_tarball
from tests.tooling.shipping_fixture import (
    LINUX,
    REVISION,
    ROOT,
    TOOLCHAIN,
    executable,
    manifest,
    manifests,
)


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

    def install(self, *args, **overrides):
        return subprocess.run(
            ["sh", ROOT / "install.sh", *args],
            env={
                **os.environ,
                "HOME": str(self.home),
                "CARGO_HOME": str(self.home / ".cargo"),
                "PLAK_TEST_SNAPSHOT_FIXTURE": "1",
                **overrides,
            },
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
        self.native_artifact()
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
        self.native_artifact()
        first = self.install("--no-link", "--from", self.artifacts)
        self.assertEqual(first.returncode, 0, first.stderr)
        snapshot = Path(first.stdout.strip())
        self.assertEqual(snapshot.name, self.data["snapshot_id"])
        observed = {
            name: ((snapshot / name).stat().st_ino, (snapshot / name).read_text().strip())
            for name in ("binary-id", "state-key", "export-version")
        }
        identity = observed["binary-id"][1].split()
        binary = (snapshot / "plakd").stat()
        self.assertEqual(
            identity,
            [
                self.data["files"]["plakd"]["display_id"],
                str(binary.st_dev),
                str(binary.st_ino),
                str(binary.st_size),
                str(binary.st_mtime_ns),
            ],
        )
        self.assertEqual(observed["state-key"][1], self.data["build_info"]["state_key"])
        self.assertEqual(
            observed["export-version"][1], str(self.data["build_info"]["export"]["write"])
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

    def native_artifact(self):
        binary = os.environ.get("PLAK_TOOLING_BINARY")
        if not binary:
            self.skipTest("requires the real tooling binary from just steps tooling")
        info = json.loads(subprocess.check_output([binary, "version", "--build-info"]))
        self.assertTrue(info["test_hooks"])
        info.update(flavor="release", revision=REVISION, test_hooks=False)
        shutil.copyfile(binary, self.artifacts / "plakd")
        (self.artifacts / "plakd").chmod(0o700)
        platform = "aarch64-apple-darwin" if os.uname().sysname == "Darwin" else LINUX
        self.data = manifests.make_manifest(
            self.artifacts, platform, REVISION, info["product_version"], "3" * 64, TOOLCHAIN, info
        )
        self.manifest.write_bytes(manifests.canonical(self.data))

    def test_tarball_with_external_manifest_publishes_only_verified_files(self):
        self.native_artifact()
        archive = self.root / "bundle.tar.gz"
        marker = self.artifacts / "binary-id"
        marker.write_text("not transferred")
        with tarfile.open(archive, "w:gz", compresslevel=1) as tar:
            tar.add(self.artifacts, arcname="release")
            tar.add(marker, arcname="../outside-the-install")
        result = self.install("--from", archive, "--manifest", self.manifest, "--no-link")
        self.assertEqual(result.returncode, 0, result.stderr)
        installed = Path(result.stdout.strip())
        self.assertEqual(
            (installed / "binary-id").read_text().split()[0],
            self.data["files"]["plakd"]["display_id"],
        )
        self.assertFalse((self.root / "outside-the-install").exists())

    def test_local_bootstrap_digest_must_match_before_either_path_executes(self):
        binary = self.artifacts / "plakd"
        binary.write_text('#!/bin/sh\ntouch "$HOME/ran"\n')
        binary.chmod(0o700)
        archive = self.root / "bundle.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(self.artifacts, arcname="release")
        for source in (self.artifacts, archive):
            with self.subTest(source=source.name):
                result = self.install("--from", source, "--manifest", self.manifest)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("plakd digest mismatch; installer image was not run", result.stderr)
                self.assertEqual(list(self.home.iterdir()), [])

    def test_release_tarball_runs_the_native_installer_without_touching_cargo(self):
        self.native_artifact()
        version = self.data["product_version"]
        stem = f"plakd-{version}-{self.data['platform']}"
        site = self.root / "site"
        downloads = site / f"releases/download/v{version}"
        downloads.mkdir(parents=True)
        archive = downloads / f"{stem}.tar.gz"
        with tarfile.open(archive, "w:gz", compresslevel=1) as tar:
            tar.add(self.artifacts, arcname=stem)
        release_tarball.write_sums(downloads)
        server = Server(site)
        self.addCleanup(server.close)
        cargo = self.home / ".cargo/bin"
        cargo.mkdir(parents=True)
        foreign = cargo / "plakd"
        foreign.write_text("foreign")
        result = self.install(
            PLAK_RELEASE_BASE=server.url, PLAK_VERSION=version, PLAK_SKIP_ATTESTATION="1"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("attestation check skipped", result.stderr)
        snapshot = self.home / ".local/lib/plak/release" / self.data["snapshot_id"]
        self.assertEqual((self.home / ".local/bin/plakd").readlink(), snapshot / "plakd")
        self.assertEqual(
            (snapshot / "state-key").read_text().strip(), self.data["build_info"]["state_key"]
        )
        self.assertEqual(list(cargo.iterdir()), [foreign])
        self.assertEqual(foreign.read_text(), "foreign")

    def test_snapshot_shares_workers_preserves_old_images_and_refuses_unsafe_layout(self):
        self.native_artifact()
        first = self.install("--from", self.artifacts, "--no-link")
        self.assertEqual(first.returncode, 0, first.stderr)
        initial = Path(first.stdout.strip())
        js = (initial / "plakous-js-host").resolve()
        py = (initial / "plakous-py-host").resolve()
        self.assertEqual(js.stat().st_mode & 0o777, 0o555)
        self.assertEqual(py.stat().st_mode & 0o777, 0o555)
        js.chmod(0o755)
        refused = self.install("--from", self.artifacts, "--no-link")
        self.assertIn("unexpected snapshot executable owner or mode", refused.stderr)
        js.chmod(0o555)
        lib = initial.parent.parent
        lib.chmod(0o777)
        refused = self.install("--from", self.artifacts, "--no-link")
        self.assertIn("writable installation ancestor refused", refused.stderr)
        self.assertEqual(lib.stat().st_mode & 0o777, 0o777)
        lib.chmod(0o755)
        (self.artifacts / "plakous-py-host").write_bytes(b"replacement worker")
        previous = self.data
        self.data = manifests.make_manifest(
            self.artifacts,
            previous["platform"],
            REVISION,
            previous["product_version"],
            "3" * 64,
            TOOLCHAIN,
            previous["build_info"],
        )
        self.manifest.write_bytes(manifests.canonical(self.data))
        new = self.install("--from", self.artifacts, "--no-link")
        self.assertEqual(new.returncode, 0, new.stderr)
        new = Path(new.stdout.strip())
        self.assertNotEqual(new, initial)
        self.assertEqual((new / "plakous-js-host").resolve(), js)
        self.assertEqual((initial / "plakous-py-host").resolve(), py)
        self.assertNotEqual((new / "plakous-py-host").resolve(), py)
        self.assertTrue((initial / "plakd").exists())


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
            self.assertEqual(wire.pop("binary_id"), probe["binary_id"])
            pin = hashlib.sha256(manifests.canonical(wire).removesuffix(b"\n")).hexdigest()
            self.assertEqual(probe["wire"], {"number": wire["wire_version"], "schema_pin": pin})
