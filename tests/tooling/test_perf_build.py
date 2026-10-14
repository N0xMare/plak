"""Contract tests for the benchmark-only supported build closures."""

from __future__ import annotations

import importlib.util
import copy
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]


def load_perf_build():  # type: ignore[no-untyped-def]
    """Import the standalone benchmark driver."""
    path = ROOT / "tools/perf/perf_build.py"
    spec = importlib.util.spec_from_file_location("perf_build", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["perf_build"] = module
    spec.loader.exec_module(module)
    return module


perf_build = load_perf_build()


def load_perf_validator():  # type: ignore[no-untyped-def]
    path = ROOT / "tools/perf/validate_perf_jsonl.py"
    spec = importlib.util.spec_from_file_location("perf_validator", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["perf_validator"] = module
    spec.loader.exec_module(module)
    return module


perf_validator = load_perf_validator()


class PerfBuildProfileTests(unittest.TestCase):
    """Pin the benchmark matrix without changing production Cargo features."""

    def test_required_public_closures_are_exact(self) -> None:
        self.assertEqual(
            perf_build.SUPPORTED_PUBLIC_BUILD_PROFILES,
            {
                "minimal": ("plakous", ("--lib",), ()),
                "subscriptions": (
                    "plakous",
                    ("--lib",),
                    ("native-subscriptions",),
                ),
                "sessions-sqlite": (
                    "plakous",
                    ("--lib",),
                    ("sessions-sqlite",),
                ),
                "plak": ("plak", ("--bin", "plak"), ()),
                "agent": ("plakous-agent", ("--lib",), ()),
                "tools": ("plakous-tools", ("--lib",), ()),
            },
        )
        self.assertEqual(
            perf_build.TRANSITIONAL_FACADE_BUILD_PROFILES,
            {
                "openai-platform": ("plakous", ("--lib",), ("openai",)),
                "xai-platform": ("plakous", ("--lib",), ("xai",)),
                "openrouter": ("plakous", ("--lib",), ("openrouter",)),
                "anthropic": ("plakous", ("--lib",), ("anthropic",)),
                "all-providers": (
                    "plakous",
                    ("--lib",),
                    (
                        "anthropic",
                        "native-subscriptions",
                        "openai",
                        "openrouter",
                        "xai",
                    ),
                ),
                "full": ("plakous", ("--lib",), ("full",)),
            },
        )

    def test_internal_topology_and_product_closures_are_exact(self) -> None:
        self.assertEqual(
            perf_build.COMPONENT_BUILD_PROFILES,
            {
                "codex-components": (
                    "plakous",
                    ("--lib",),
                    ("openai", "plakous-openai/native-auth"),
                ),
                "supergrok-components": (
                    "plakous",
                    ("--lib",),
                    ("plakous-xai/native-auth", "xai"),
                ),
            },
        )
        self.assertEqual(
            perf_build.INTERNAL_TOPOLOGY_BUILD_PROFILES,
            {
                **perf_build.COMPONENT_BUILD_PROFILES,
                "attempt": ("plakous-attempt", ("--lib",), ()),
                "responses-kernel": ("plakous-responses", ("--lib",), ()),
                "responses-host": (
                    "plakous-responses-dialect",
                    ("--lib",),
                    (),
                ),
                "messages": ("plakous-messages", ("--lib",), ()),
            },
        )
        self.assertEqual(
            perf_build.PRODUCT_TOPOLOGY_BUILD_PROFILES,
            {
                "plak-lib": ("plak", ("--lib",), ()),
            },
        )
        self.assertEqual(
            perf_build.BUILD_PROFILES,
            {
                **perf_build.SUPPORTED_PUBLIC_BUILD_PROFILES,
                **perf_build.TRANSITIONAL_FACADE_BUILD_PROFILES,
                **perf_build.INTERNAL_TOPOLOGY_BUILD_PROFILES,
                **perf_build.PRODUCT_TOPOLOGY_BUILD_PROFILES,
            },
        )
        self.assertEqual(len(perf_build.BUILD_PROFILES), 19)
        for profile in perf_build.SUPPORTED_PUBLIC_BUILD_PROFILES:
            self.assertEqual(
                perf_build.build_profile_scope(profile), "supported-public"
            )
        for profile in perf_build.TRANSITIONAL_FACADE_BUILD_PROFILES:
            self.assertEqual(
                perf_build.build_profile_scope(profile),
                "transitional-facade-baseline",
            )
        for profile in perf_build.INTERNAL_TOPOLOGY_BUILD_PROFILES:
            self.assertEqual(
                perf_build.build_profile_scope(profile),
                "internal-topology-baseline",
            )
        for profile in perf_build.PRODUCT_TOPOLOGY_BUILD_PROFILES:
            self.assertEqual(
                perf_build.build_profile_scope(profile),
                "product-topology-baseline",
            )

    def test_command_selects_the_profile_package_and_target(self) -> None:
        command = perf_build.cargo_command(
            "cargo",
            "build",
            "release",
            "plak",
            ("--bin", "plak"),
            (),
            "x86_64-unknown-linux-gnu",
            Path("/tmp/perf-build-test-target"),
            1,
        )
        self.assertIn("plak", command)
        self.assertEqual(command[command.index("--package") + 1], "plak")
        self.assertEqual(command[command.index("--bin") + 1], "plak")
        self.assertNotIn("--lib", command)
        self.assertIn("--no-default-features", command)

    def test_operation_semantics_distinguish_check_from_build_and_link(self) -> None:
        self.assertEqual(
            perf_build.CARGO_OPERATION_SEMANTICS,
            {
                "check": {
                    "workload": "cargo-check",
                    "selected_target_linked": False,
                    "scope": "type-check and compile dependencies without linking the selected package target",
                },
                "build": {
                    "workload": "cargo-build-link",
                    "selected_target_linked": True,
                    "scope": "compile and link the selected package target",
                },
            },
        )
        for operation in perf_build.CARGO_OPERATION_SEMANTICS:
            with self.subTest(operation=operation):
                command = perf_build.cargo_command(
                    "cargo",
                    operation,
                    "dev",
                    "plakous",
                    ("--lib",),
                    (),
                    "x86_64-unknown-linux-gnu",
                    Path("/tmp/perf-build-test-target"),
                    1,
                )
                self.assertEqual(
                    command[len(perf_build.profile_config_arguments("dev")) + 1],
                    operation,
                )

    def test_host_id_is_required_from_cli_or_environment(self) -> None:
        with (
            patch.dict(os.environ, {}, clear=True),
            patch.object(sys, "argv", ["perf_build.py"]),
            self.assertRaises(SystemExit) as missing,
        ):
            perf_build.parse_arguments()
        self.assertEqual(missing.exception.code, 2)

        with (
            patch.dict(
                os.environ, {"PLAKOUS_PERF_HOST_ID": "benchmark-worker-07"}, clear=True
            ),
            patch.object(sys, "argv", ["perf_build.py"]),
        ):
            self.assertEqual(
                perf_build.parse_arguments().host_id, "benchmark-worker-07"
            )

        with (
            patch.dict(
                os.environ, {"PLAKOUS_PERF_HOST_ID": "environment-host"}, clear=True
            ),
            patch.object(sys, "argv", ["perf_build.py", "--host-id", "cli-host"]),
        ):
            self.assertEqual(perf_build.parse_arguments().host_id, "cli-host")

    def test_host_id_validation_and_metadata_are_exact(self) -> None:
        for invalid in ("x" * 257, "host\nname", "host\rname"):
            with (
                self.subTest(invalid=repr(invalid)),
                patch.dict(os.environ, {}, clear=True),
                patch.object(sys, "argv", ["perf_build.py", "--host-id", invalid]),
                self.assertRaises(SystemExit) as rejected,
            ):
                perf_build.parse_arguments()
            self.assertEqual(rejected.exception.code, 2)

        with (
            patch("perf_build.platform.system", return_value="TestOS"),
            patch("perf_build.platform.release", return_value="1.2.3"),
            patch("perf_build.platform.machine", return_value="test-machine"),
            patch("perf_build.os.cpu_count", return_value=8),
        ):
            self.assertEqual(
                perf_build.host_identity("benchmark-worker-07"),
                {
                    "host_id": "benchmark-worker-07",
                    "system": "TestOS",
                    "release": "1.2.3",
                    "machine": "test-machine",
                    "cpu_count": 8,
                },
            )

    def test_every_profile_generates_its_exact_cross_field_command(self) -> None:
        for profile, (
            package,
            package_target,
            features,
        ) in perf_build.BUILD_PROFILES.items():
            with self.subTest(profile=profile):
                command = perf_build.cargo_command(
                    "cargo",
                    "build",
                    "dev",
                    package,
                    package_target,
                    features,
                    "x86_64-unknown-linux-gnu",
                    Path("/tmp/perf-build-test-target"),
                    2,
                )
                self.assertEqual(command[command.index("--package") + 1], package)
                if package_target == ("--lib",):
                    self.assertIn("--lib", command)
                    self.assertNotIn("--bin", command)
                else:
                    self.assertEqual(package_target, ("--bin", "plak"))
                    self.assertEqual(command[command.index("--bin") + 1], "plak")
                    self.assertNotIn("--lib", command)
                self.assertEqual(command.count("--features"), int(bool(features)))
                if features:
                    self.assertEqual(
                        command[command.index("--features") + 1], ",".join(features)
                    )
                self.assertIn("--no-default-features", command)
                self.assertIn("build", command)

        for _profile, (
            _package,
            _target,
            features,
        ) in perf_build.SUPPORTED_PUBLIC_BUILD_PROFILES.items():
            self.assertFalse(any("/" in feature for feature in features))
        self.assertTrue(
            set(perf_build.TRANSITIONAL_FACADE_BUILD_PROFILES).isdisjoint(
                perf_build.SUPPORTED_PUBLIC_BUILD_PROFILES
            )
        )

    def test_subscription_provider_profiles_enable_one_facade_provider(self) -> None:
        codex = perf_build.BUILD_PROFILES["codex-components"][2]
        supergrok = perf_build.BUILD_PROFILES["supergrok-components"][2]
        self.assertEqual(codex, ("openai", "plakous-openai/native-auth"))
        self.assertEqual(supergrok, ("plakous-xai/native-auth", "xai"))
        self.assertNotIn("native-subscriptions", codex + supergrok)

    def test_producer_and_validator_profile_contracts_are_identical(self) -> None:
        produced = {
            profile: (
                perf_build.build_profile_scope(profile),
                package,
                list(target),
                list(features),
            )
            for profile, (
                package,
                target,
                features,
            ) in perf_build.BUILD_PROFILES.items()
        }
        self.assertEqual(produced, perf_validator.V9_BUILD_PROFILE_CONTRACTS)
        self.assertEqual(perf_build.SCHEMA, "plakous.perf-build.v9")

    def test_closure_facts_are_sorted_deduplicated_and_content_addressed(self) -> None:
        raw = (
            b"plakous-tools v0.1.0|serde,default\n"
            b"serde v1.0.0|derive,std\n"
            b"serde v1.0.0|derive,std (*)\n"
        )
        with patch("perf_build.checked_stdout", return_value=raw):
            closure = perf_build.closure_provenance(
                "cargo",
                "plakous-tools",
                (),
                "x86_64-unknown-linux-gnu",
                {},
            )
        self.assertEqual(
            closure["entries"],
            ["plakous-tools v0.1.0|serde,default", "serde v1.0.0|derive,std"],
        )
        self.assertEqual(closure["resolved_root_features"], ["default", "serde"])
        expected = perf_build.sha256_bytes(
            json.dumps(
                closure["entries"], ensure_ascii=True, separators=(",", ":")
            ).encode()
        )
        self.assertEqual(closure["entries_sha256"], expected)

        metadata = {
            "toolchain": {"cargo": {"command": ["cargo", "-Vv"]}},
            "cargo": {
                "package": {"name": "plakous-tools"},
                "target": "x86_64-unknown-linux-gnu",
                "requested_features": [],
                "resolved_feature_closure": closure,
            },
        }
        perf_validator.validate_resolved_build_closure(metadata)

        unsorted = copy.deepcopy(metadata)
        unsorted["cargo"]["resolved_feature_closure"]["entries"] = list(
            reversed(closure["entries"])
        )
        with self.assertRaisesRegex(
            perf_validator.ValidationError, "sorted unique list"
        ):
            perf_validator.validate_resolved_build_closure(unsorted)

        wrong_command = copy.deepcopy(metadata)
        wrong_command["cargo"]["resolved_feature_closure"]["resolver_command"][-1] = (
            "{p}"
        )
        with self.assertRaisesRegex(perf_validator.ValidationError, "resolver_command"):
            perf_validator.validate_resolved_build_closure(wrong_command)

        wrong_root_features = copy.deepcopy(metadata)
        wrong_root_features["cargo"]["resolved_feature_closure"][
            "resolved_root_features"
        ] = ["serde"]
        with self.assertRaisesRegex(
            perf_validator.ValidationError, "normalized package root"
        ):
            perf_validator.validate_resolved_build_closure(wrong_root_features)

        missing_root = copy.deepcopy(metadata)
        missing_root_closure = missing_root["cargo"]["resolved_feature_closure"]
        missing_root_closure["entries"] = ["serde v1.0.0|derive,std"]
        missing_root_closure["entry_count"] = 1
        missing_root_closure["entries_sha256"] = perf_build.sha256_bytes(
            json.dumps(
                missing_root_closure["entries"],
                ensure_ascii=True,
                separators=(",", ":"),
            ).encode()
        )
        with self.assertRaisesRegex(
            perf_validator.ValidationError, "exactly one normalized package root"
        ):
            perf_validator.validate_resolved_build_closure(missing_root)

    def test_v5_run_identity_is_recomputed_from_canonical_metadata(self) -> None:
        metadata = {
            "schema": "plakous.perf-build.v5",
            "kind": "metadata",
            "baseline": {"identity": "campaign-baseline"},
            "cargo": {"build_profile": "minimal"},
        }
        encoded = json.dumps(
            metadata, sort_keys=True, ensure_ascii=True, separators=(",", ":")
        ).encode()
        metadata["run_identity"] = perf_build.sha256_bytes(encoded)
        perf_validator.validate_v5_run_identity(metadata)

        metadata["cargo"]["build_profile"] = "tools"
        with self.assertRaisesRegex(
            perf_validator.ValidationError, "canonical metadata"
        ):
            perf_validator.validate_v5_run_identity(metadata)

    def test_v5_measured_command_is_exact_and_ordered(self) -> None:
        target_directory = "/tmp/perf-build-exact-target"
        metadata = {
            "schema": "plakous.perf-build.v5",
            "toolchain": {"cargo": {"command": ["/toolchain/cargo", "-Vv"]}},
            "cargo": {
                "operation": "build",
                "package": {"name": "plakous"},
                "target_selection": ["--lib"],
                "requested_features": ["anthropic", "openai"],
                "target": "x86_64-unknown-linux-gnu",
                "cargo_profile": "dev",
                "target_unit_profile_settings": copy.deepcopy(
                    perf_validator.V5_TARGET_UNIT_PROFILE_SETTINGS["dev"]
                ),
                "jobs": 7,
            },
        }
        row = {"target_directory": target_directory}
        command = perf_validator.expected_v5_build_command(row, metadata)
        row["command"] = command
        perf_validator.validate_build_command(row, metadata)

        mutations = {
            "all-features": lambda value: value.insert(-8, "--all-features"),
            "extra-config": lambda value: value.__setitem__(
                slice(1, 1), ["--config", "profile.dev.opt-level=1"]
            ),
            "duplicate-config": lambda value: value.__setitem__(
                slice(1, 1), value[1:3]
            ),
            "selector": lambda value: value.__setitem__(
                value.index("--lib"), "--all-targets"
            ),
            "package": lambda value: value.__setitem__(
                value.index("plakous"), "plakous-tools"
            ),
            "jobs": lambda value: value.__setitem__(value.index("7"), "8"),
            "target-dir": lambda value: value.__setitem__(
                value.index(target_directory), f"{target_directory}-other"
            ),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                changed = copy.deepcopy(row)
                mutate(changed["command"])
                with self.assertRaisesRegex(
                    perf_validator.ValidationError, "exact ordered metadata"
                ):
                    perf_validator.validate_build_command(changed, metadata)

    def test_v7_campaign_requires_exact_profile_files_and_common_cohort(self) -> None:
        common = {
            "schema": "plakous.perf-build.v7",
            "kind": "metadata",
            "baseline": {"identity": "campaign-baseline"},
            "fixture": copy.deepcopy(perf_validator.V7_BUILD_FIXTURE),
            "host": {"host_id": "worker"},
            "git": {
                "commit": "a" * 40,
                "dirty": False,
                "status_sha256": f"sha256:{'b' * 64}",
                "tracked_patch_sha256": f"sha256:{'c' * 64}",
                "non_ignored_worktree_content_closure": {"identity": "source"},
            },
            "toolchain": {"cargo": "cargo", "rustc": "rustc"},
            "controlled_environment": {"RUSTC": "rustc"},
            "rustflags": {"values": []},
            "removed_environment_names": ["RUSTFLAGS"],
            "removed_environment_patterns": ["CARGO_PROFILE_*"],
            "sample_plan": {"cold_samples": 1, "warm_samples": 2},
        }

        def rows_for(profile: str) -> list[dict[str, object]]:
            metadata = copy.deepcopy(common)
            metadata["cargo"] = {
                "build_profile": profile,
                "operation": "build",
                "target": "x86_64-unknown-linux-gnu",
                "cargo_profile": "dev",
                "jobs": 8,
                "target_unit_profile_settings": copy.deepcopy(
                    perf_validator.V5_TARGET_UNIT_PROFILE_SETTINGS["dev"]
                ),
            }
            return [metadata, {"schema": "plakous.perf-build.v7", "kind": "sample"}]

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for profile in perf_validator.V7_BUILD_PROFILE_CONTRACTS:
                (root / f"{profile}.jsonl").touch()

            with (
                patch(
                    "perf_validator.load_rows",
                    side_effect=lambda path: rows_for(path.stem),
                ),
                patch("perf_validator.validate_build"),
            ):
                perf_validator.validate_v7_campaign(root)

                (root / "minimal.jsonl").unlink()
                with self.assertRaisesRegex(perf_validator.ValidationError, "missing="):
                    perf_validator.validate_v7_campaign(root)
                (root / "minimal.jsonl").touch()
                (root / "unexpected.jsonl").touch()
                with self.assertRaisesRegex(perf_validator.ValidationError, "extra="):
                    perf_validator.validate_v7_campaign(root)
                (root / "unexpected.jsonl").unlink()

            def mixed_rows(path: Path) -> list[dict[str, object]]:
                rows = rows_for(path.stem)
                if path.stem == "tools":
                    rows[0]["host"] = {"host_id": "other-worker"}
                return rows

            with (
                patch("perf_validator.load_rows", side_effect=mixed_rows),
                patch("perf_validator.validate_build"),
                self.assertRaisesRegex(
                    perf_validator.ValidationError, "host in tools.jsonl"
                ),
            ):
                perf_validator.validate_v7_campaign(root)

            mutations = {
                ("baseline", "identity"): "other-baseline",
                ("fixture",): {
                    "name": "other-fixture",
                    "revision": 5,
                },
                ("host",): {"host_id": "other-worker"},
                ("git",): {
                    "commit": "d" * 40,
                    "dirty": True,
                    "status_sha256": f"sha256:{'e' * 64}",
                    "tracked_patch_sha256": f"sha256:{'f' * 64}",
                    "non_ignored_worktree_content_closure": {
                        "identity": "other-source"
                    },
                },
                ("toolchain",): {"cargo": "other-cargo", "rustc": "rustc"},
                ("cargo", "target"): "aarch64-unknown-linux-gnu",
                ("cargo", "cargo_profile"): "release",
                ("cargo", "jobs"): 9,
                ("cargo", "target_unit_profile_settings"): {},
                ("controlled_environment",): {"RUSTC": "other-rustc"},
                ("rustflags",): {"values": ["-Ctarget-cpu=native"]},
                ("removed_environment_names",): ["RUSTFLAGS", "CARGO_TARGET_DIR"],
                ("removed_environment_patterns",): ["CARGO_TARGET_*_RUSTFLAGS"],
                ("sample_plan",): {"cold_samples": 1, "warm_samples": 3},
            }

            def set_path(
                value: dict[str, object], path: tuple[str, ...], replacement: object
            ) -> None:
                current = value
                for part in path[:-1]:
                    current = current[part]  # type: ignore[assignment,index]
                current[path[-1]] = replacement

            for value_path, replacement in mutations.items():
                with self.subTest(common_path=value_path):

                    def differing_rows(path: Path) -> list[dict[str, object]]:
                        rows = rows_for(path.stem)
                        if path.stem == "tools":
                            set_path(rows[0], value_path, replacement)
                        return rows

                    with (
                        patch("perf_validator.load_rows", side_effect=differing_rows),
                        patch("perf_validator.validate_build"),
                        self.assertRaisesRegex(
                            perf_validator.ValidationError,
                            f"{'.'.join(value_path)} in tools.jsonl",
                        ),
                    ):
                        perf_validator.validate_v7_campaign(root)

            def invalid_rows(
                path: Path,
                *,
                schema: str | None = None,
                operation: str | None = None,
                dry_run: bool = False,
            ) -> list[dict[str, object]]:
                rows = rows_for(path.stem)
                if path.stem == "tools":
                    if schema is not None:
                        rows[0]["schema"] = schema
                    if operation is not None:
                        rows[0]["cargo"]["operation"] = operation  # type: ignore[index]
                    if dry_run:
                        rows[1]["kind"] = "dry-run"
                return rows

            invalid_cases = (
                (
                    "schema",
                    lambda path: invalid_rows(path, schema="plakous.perf-build.v4"),
                    "is not v7",
                ),
                (
                    "operation",
                    lambda path: invalid_rows(path, operation="check"),
                    "cargo.operation",
                ),
                (
                    "dry-run",
                    lambda path: invalid_rows(path, dry_run=True),
                    "requires measured samples",
                ),
            )
            for name, side_effect, message in invalid_cases:
                with (
                    self.subTest(invalid=name),
                    patch("perf_validator.load_rows", side_effect=side_effect),
                    patch("perf_validator.validate_build"),
                    self.assertRaisesRegex(perf_validator.ValidationError, message),
                ):
                    perf_validator.validate_v7_campaign(root)

            git_tampers = {
                "commit": "d" * 40,
                "dirty": True,
                "status_sha256": f"sha256:{'e' * 64}",
                "tracked_patch_sha256": f"sha256:{'f' * 64}",
            }
            for git_field, replacement in git_tampers.items():
                with self.subTest(git_field=git_field):

                    def git_tampered_rows(path: Path) -> list[dict[str, object]]:
                        rows = rows_for(path.stem)
                        if path.stem == "tools":
                            rows[0]["git"][git_field] = replacement  # type: ignore[index]
                        return rows

                    with (
                        patch(
                            "perf_validator.load_rows",
                            side_effect=git_tampered_rows,
                        ),
                        patch("perf_validator.validate_build"),
                        self.assertRaisesRegex(
                            perf_validator.ValidationError,
                            "git in tools.jsonl",
                        ),
                    ):
                        perf_validator.validate_v7_campaign(root)

    def test_legacy_profile_contracts_keep_v3_v4_scope_names(self) -> None:
        self.assertEqual(
            perf_validator.LEGACY_BUILD_PROFILE_CONTRACTS["openai-platform"][0],
            "supported-public",
        )
        self.assertEqual(
            perf_validator.LEGACY_BUILD_PROFILE_CONTRACTS["codex-components"][0],
            "benchmark-component",
        )

    def test_v9_fixture_and_archived_contracts_are_exact(self) -> None:
        self.assertEqual(perf_build.FIXTURE, perf_validator.V9_BUILD_FIXTURE)
        self.assertEqual(len(perf_validator.V9_BUILD_PROFILE_CONTRACTS), 19)
        self.assertNotIn("paid-platform", perf_validator.V9_BUILD_PROFILE_CONTRACTS)
        self.assertNotIn("plak-app", perf_validator.V9_BUILD_PROFILE_CONTRACTS)
        self.assertEqual(len(perf_validator.V8_BUILD_PROFILE_CONTRACTS), 20)
        self.assertIn("paid-platform", perf_validator.V8_BUILD_PROFILE_CONTRACTS)
        self.assertNotIn("plak-app", perf_validator.V8_BUILD_PROFILE_CONTRACTS)
        self.assertEqual(len(perf_validator.V7_BUILD_PROFILE_CONTRACTS), 21)
        self.assertIn("plak-app", perf_validator.V7_BUILD_PROFILE_CONTRACTS)
        self.assertNotIn("dispatch", perf_validator.V7_BUILD_PROFILE_CONTRACTS)
        self.assertEqual(len(perf_validator.V6_BUILD_PROFILE_CONTRACTS), 22)
        self.assertIn("dispatch", perf_validator.V6_BUILD_PROFILE_CONTRACTS)
        self.assertEqual(len(perf_validator.V5_BUILD_PROFILE_CONTRACTS), 23)
        self.assertIn("agent-wrapper", perf_validator.V5_BUILD_PROFILE_CONTRACTS)
        self.assertIn("agent-impl", perf_validator.V5_BUILD_PROFILE_CONTRACTS)
        self.assertNotIn("agent", perf_validator.V5_BUILD_PROFILE_CONTRACTS)
        metadata = {"fixture": copy.deepcopy(perf_validator.V5_BUILD_FIXTURE)}
        perf_validator.validate_v5_fixture(metadata)
        for tampered in (
            {"name": "other-fixture", "revision": 5},
            {"name": "isolated-build-closure-matrix", "revision": 4},
        ):
            with (
                self.subTest(fixture=tampered),
                self.assertRaisesRegex(
                    perf_validator.ValidationError, "fixture must be"
                ),
            ):
                perf_validator.validate_v5_fixture({"fixture": tampered})

        perf_validator.validate_v5_sample_fixture({"fixture_revision": 5})
        with self.assertRaisesRegex(
            perf_validator.ValidationError, "fixture_revision must be 5"
        ):
            perf_validator.validate_v5_sample_fixture({"fixture_revision": 4})


if __name__ == "__main__":
    unittest.main()
