"""The cohort verification receipt misses unless every keyed input still matches."""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]


def load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


cache = load("package_verification_cache", ROOT / "tools/release/package_verification_cache.py")


class PackageVerificationCache(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name)
        self.stage = self.scratch / "stage"
        self.stage.mkdir()
        (self.stage / "Cargo.lock").write_text("# lock\n", encoding="utf-8")
        (self.stage / "src").mkdir()
        (self.stage / "src/lib.rs").write_text("fn kept() {}\n", encoding="utf-8")
        self.order = self.scratch / "order"
        self.order.write_text("plakous 0.0.3\n", encoding="utf-8")
        self.receipt = self.scratch / "receipt"
        self.cargo_home = self.scratch / "cargo-home"
        self.cargo_home.mkdir()
        self.env = patch.dict(os.environ, {"CARGO_HOME": str(self.cargo_home)}, clear=False)
        self.env.start()
        self.addCleanup(self.env.stop)

    def test_a_changed_field_with_a_stale_checksum_misses(self) -> None:
        body = cache.receipt_body(self.stage, self.order)
        cache.write_receipt(self.receipt, body)
        self.assertEqual(cache.load_receipt(self.receipt), body)
        payload = json.loads(self.receipt.read_text(encoding="utf-8"))
        payload["toolchain"] = "0" * 64
        self.receipt.write_text(json.dumps(payload), encoding="utf-8")
        self.assertIsNone(cache.load_receipt(self.receipt))

    def test_a_non_object_receipt_misses_before_it_is_treated_as_a_dict(self) -> None:
        self.receipt.write_text("[]", encoding="utf-8")
        self.assertIsNone(cache.load_receipt(self.receipt))
        self.receipt.write_text("null", encoding="utf-8")
        self.assertIsNone(cache.load_receipt(self.receipt))

    def test_toolchain_rustflags_and_ancestor_config_change_the_body(self) -> None:
        first = cache.receipt_body(self.stage, self.order)
        real = subprocess.check_output

        def rustc(command, **kwargs):
            if command[:1] == ["rustc"]:
                return "rustc 0.0.0-e6\n"
            return real(command, **kwargs)

        with patch.object(subprocess, "check_output", side_effect=rustc):
            self.assertNotEqual(
                cache.receipt_body(self.stage, self.order)["toolchain"], first["toolchain"]
            )
        with patch.dict(os.environ, {"RUSTFLAGS": "--cfg e6_receipt"}):
            changed = cache.receipt_body(self.stage, self.order)
        self.assertNotEqual(changed["env"], first["env"])
        with patch.dict(os.environ, {"CARGO_ENCODED_RUSTFLAGS": "e6"}):
            self.assertNotEqual(cache.receipt_body(self.stage, self.order)["env"], first["env"])
        with patch.dict(os.environ, {"CARGO_BUILD_TARGET": "x86_64-unknown-linux-gnu"}):
            self.assertNotEqual(cache.receipt_body(self.stage, self.order)["env"], first["env"])
        with patch.dict(os.environ, {"CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS": "-Dwarnings"}):
            self.assertNotEqual(cache.receipt_body(self.stage, self.order)["env"], first["env"])
        with patch.dict(os.environ, {"CARGO_TERM_COLOR": "always"}):
            self.assertEqual(cache.receipt_body(self.stage, self.order)["env"], first["env"])
        ancestor = self.stage.parent / ".cargo" / "config.toml"
        ancestor.parent.mkdir()
        ancestor.write_text("[build]\n", encoding="utf-8")
        self.assertNotEqual(cache.receipt_body(self.stage, self.order)["configs"], first["configs"])
        (self.cargo_home / "config.toml").write_text("[net]\n", encoding="utf-8")
        self.assertNotEqual(cache.receipt_body(self.stage, self.order)["configs"], first["configs"])

    def test_the_receipt_holds_hashes_and_no_credential_or_environment_value(self) -> None:
        first = cache.receipt_body(self.stage, self.order)
        secrets = {
            "CARGO_REGISTRY_TOKEN": "e6-registry-token-value",
            "CARGO_REGISTRIES_MIRROR_TOKEN": "e6-mirror-token-value",
            "CARGO_REGISTRY_SECRET_KEY": "e6-secret-key-value",
            "CARGO_REGISTRIES_MIRROR_CREDENTIAL_PROVIDER": "e6-provider-value",
        }
        with patch.dict(os.environ, secrets):
            self.assertEqual(cache.receipt_body(self.stage, self.order), first)
            with patch.dict(os.environ, {"RUSTFLAGS": "--cfg e6_raw_env_value"}):
                body = cache.receipt_body(self.stage, self.order)
        self.assertNotEqual(body["env"], first["env"])
        for field, value in body.items():
            self.assertRegex(value, "^[0-9a-f]{64}$", field)
        cache.write_receipt(self.receipt, body)
        recorded = self.receipt.read_bytes()
        for value in [*secrets.values(), "e6_raw_env_value", "plakous 0.0.3"]:
            self.assertNotIn(value.encode(), recorded)

    def test_each_staged_and_cohort_input_changes_its_field(self) -> None:
        def field(name: str) -> str:
            return cache.receipt_body(self.stage, self.order)[name]

        source = self.stage / "src/lib.rs"
        staged = field("staged")
        source.write_text("fn changed() {}\n", encoding="utf-8")
        self.assertNotEqual(field("staged"), staged)
        staged = field("staged")
        source.chmod(0o755)
        self.assertNotEqual(field("staged"), staged)
        staged = field("staged")
        (self.stage / "src/empty").mkdir()
        self.assertNotEqual(field("staged"), staged)
        staged = field("staged")
        target = self.stage / "src/copy.rs"
        target.write_bytes(source.read_bytes())
        link = self.stage / "src/link.rs"
        link.symlink_to("lib.rs")
        linked = field("staged")
        self.assertNotEqual(linked, staged)
        link.unlink()
        link.symlink_to("copy.rs")
        self.assertNotEqual(field("staged"), linked)
        lock = field("lock")
        (self.stage / "Cargo.lock").write_text("# other lock\n", encoding="utf-8")
        self.assertNotEqual(field("lock"), lock)
        lock = field("lock")
        (self.stage / "Cargo.lock").unlink()
        self.assertEqual(field("lock"), "absent")
        self.assertNotEqual(lock, "absent")
        cohort = field("cohort")
        self.order.write_text("plakous 0.0.4\n", encoding="utf-8")
        self.assertNotEqual(field("cohort"), cohort)

    def test_a_recorded_body_hits_until_the_toolchain_changes(self) -> None:
        body = cache.receipt_body(self.stage, self.order)
        cache.write_receipt(self.receipt, body)
        self.assertTrue(cache.matches(body, self.receipt))
        real = subprocess.check_output

        def rustc(command, **kwargs):
            if command[:1] == ["cargo"]:
                return "cargo 0.0.0-e6\n"
            return real(command, **kwargs)

        with patch.object(subprocess, "check_output", side_effect=rustc):
            later = cache.receipt_body(self.stage, self.order)
        self.assertFalse(cache.matches(later, self.receipt))


if __name__ == "__main__":
    unittest.main()
