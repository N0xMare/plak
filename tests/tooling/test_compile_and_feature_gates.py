"""Focused regression tests for public-compile and feature gate hardening."""

from __future__ import annotations

import importlib.util
import json
import io
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]


def load_module(name: str, path: Path):  # type: ignore[no-untyped-def]
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


compile_gate = load_module(
    "compile_gate_hardening", ROOT / "tools/checks/check_public_compile.py"
)
feature_gate = load_module(
    "feature_gate_hardening", ROOT / "tools/checks/check_features.py"
)


class FeatureClosureCacheTests(unittest.TestCase):
    def test_hit_reuses_tree_and_metadata_without_subprocesses(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cache.json"
            commands = (["cargo", "tree", "-p", "plakous"], ["cargo", "metadata"])
            cache = feature_gate.CargoOutputCache(path, "digest")
            with mock.patch.object(feature_gate.subprocess, "check_output", return_value="output") as run:
                for command in commands:
                    self.assertEqual(cache.output(command, ROOT), "output")
                self.assertEqual(run.call_count, 2)
            cache.save()
            hit = feature_gate.CargoOutputCache(path, "digest")
            self.assertTrue(hit.hit)
            with mock.patch.object(feature_gate.subprocess, "check_output") as output, \
                 mock.patch.object(feature_gate.subprocess, "run") as run:
                for command in commands:
                    self.assertEqual(hit.output(command, ROOT), "output")
                output.assert_not_called()
                run.assert_not_called()

    def test_mismatch_and_corrupt_cache_recompute(self):
        for contents in ('{"digest":"old","outputs":{}}', 'not json', '[]',
                         '{"digest":"digest","outputs":{"key":null}}'):
            with self.subTest(contents=contents), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "cache.json"
                path.write_text(contents)
                cache = feature_gate.CargoOutputCache(path, "digest")
                self.assertFalse(cache.hit)
                with mock.patch.object(feature_gate.subprocess, "check_output", return_value="fresh") as run:
                    self.assertEqual(cache.output(["cargo", "tree"], ROOT), "fresh")
                    run.assert_called_once()
                cache.save()
                self.assertTrue(feature_gate.CargoOutputCache(path, "digest").hit)

    def test_digest_tracks_every_input_and_inventory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            paths = ["Cargo.lock", "Cargo.toml", "crates/example/Cargo.toml",
                     ".cargo/config", ".cargo/config.toml", "nested/.cargo/config.toml", "rust-toolchain.toml",
                     "tools/checks/check_features.py", "tools/checks/plakous_policy.py"]
            for name in paths:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("original")
            inventory = "Cargo.toml\0crates/example/Cargo.toml\0nested/.cargo/config.toml\0"
            with mock.patch.object(feature_gate.subprocess, "check_output", side_effect=lambda command, **_: "cargo 1" if command[0] == "cargo" else inventory):
                baseline = feature_gate.cache_digest(root)
                for name in paths:
                    with self.subTest(input=name):
                        path = root / name
                        path.write_text("changed")
                        self.assertNotEqual(feature_gate.cache_digest(root), baseline)
                        path.write_text("original")
                (root / "source.rs").write_text("not a Cargo input")
                self.assertEqual(feature_gate.cache_digest(root), baseline)
                (root / "Cargo.lock").unlink()
                self.assertNotEqual(feature_gate.cache_digest(root), baseline)
                (root / "Cargo.lock").write_text("original")
                (root / ".cargo/config.extra").write_text("new")
                self.assertNotEqual(feature_gate.cache_digest(root), baseline)
                (root / ".cargo/config.extra").unlink()
                inventory += "new/Cargo.toml\0"
                self.assertNotEqual(feature_gate.cache_digest(root), baseline)
            with mock.patch.object(feature_gate.subprocess, "check_output", side_effect=["cargo 2", "Cargo.toml\0crates/example/Cargo.toml\0nested/.cargo/config.toml\0"]):
                self.assertNotEqual(feature_gate.cache_digest(root), baseline)

    def test_cache_path_respects_target_dir(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            self.assertEqual(feature_gate.cache_path(), ROOT / "target/feature-closure-cache.json")
        with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": "custom-target"}):
            self.assertEqual(feature_gate.cache_path(), ROOT / "custom-target/feature-closure-cache.json")
        with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": "/custom-target"}):
            self.assertEqual(feature_gate.cache_path(), Path("/custom-target/feature-closure-cache.json"))
        with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": ""}):
            self.assertEqual(feature_gate.cache_path(), ROOT / "target/feature-closure-cache.json")

    def test_digest_unavailable_when_git_inventory_fails(self):
        failed = subprocess.CalledProcessError(128, ["git", "ls-files", "-z"])
        with mock.patch.object(feature_gate.subprocess, "check_output",
                               side_effect=["cargo 1", failed]):
            self.assertIsNone(feature_gate.cache_digest(ROOT))

    def test_missing_git_inventory_bypasses_cache_and_queries_cargo_directly(self):
        command = ["cargo", "metadata"]

        def workspace(root):
            for _ in range(2):
                self.assertEqual(feature_gate.cargo_output(command, root), "direct")
            return mock.Mock(packages={})

        with mock.patch.object(feature_gate.sys, "argv", ["check_features.py"]), \
             mock.patch.object(feature_gate, "cache_digest", return_value=None), \
             mock.patch.object(feature_gate, "CargoOutputCache") as cache, \
             mock.patch.object(feature_gate, "run_cargo_output", return_value="direct") as run, \
             mock.patch.object(feature_gate, "cached_workspace", side_effect=workspace), \
             mock.patch.object(feature_gate, "digest_packages", return_value=set()), \
             mock.patch.object(feature_gate, "validate_feature_contracts", return_value=[]), \
             mock.patch.object(feature_gate, "validate_excluded_closures", return_value=[]), \
             mock.patch.object(feature_gate, "validate_test_only_dependency_features", return_value=[]), \
             mock.patch.object(feature_gate, "check_facade_profiles", return_value=[]), \
             mock.patch.object(feature_gate.sys, "stdout", new_callable=io.StringIO) as output:
            self.assertEqual(feature_gate.main(), 0)
            cache.assert_not_called()
            self.assertEqual(run.call_count, 2)
            self.assertIn("bypass", output.getvalue())
            self.assertIn("git ls-files failed", output.getvalue())
            self.assertIsNone(feature_gate.CARGO_OUTPUTS)

    def test_build_and_no_cache_bypass_cache(self):
        for flag in ("--build", "--no-cache"):
            with self.subTest(flag=flag), \
                 mock.patch.object(feature_gate.sys, "argv", ["check_features.py", flag]), \
                 mock.patch.object(feature_gate, "CargoOutputCache") as cache, \
                 mock.patch.object(feature_gate, "cache_digest") as digest, \
                 mock.patch.object(feature_gate, "cached_workspace", return_value=mock.Mock(packages={})), \
                 mock.patch.object(feature_gate, "digest_packages", return_value=set()), \
                 mock.patch.object(feature_gate, "validate_feature_contracts", return_value=[]), \
                 mock.patch.object(feature_gate, "validate_excluded_closures", return_value=[]), \
                 mock.patch.object(feature_gate, "validate_test_only_dependency_features", return_value=[]), \
                 mock.patch.object(feature_gate, "check_facade_profiles", return_value=[]) as facade, \
                 mock.patch.object(feature_gate.sys, "stdout", new_callable=io.StringIO) as output:
                self.assertEqual(feature_gate.main(), 0)
                cache.assert_not_called()
                digest.assert_not_called()
                self.assertIn("bypass", output.getvalue())
                # --build checks every facade subset itself.
                self.assertEqual(facade.called, flag == "--no-cache")

    def test_failed_query_is_not_cached(self):
        with tempfile.TemporaryDirectory() as directory:
            cache = feature_gate.CargoOutputCache(Path(directory) / "cache.json", "digest")
            with mock.patch.object(feature_gate.subprocess, "check_output", side_effect=subprocess.CalledProcessError(1, "cargo")):
                with self.assertRaises(subprocess.CalledProcessError):
                    cache.output(["cargo", "tree"], ROOT)
            self.assertEqual(cache.outputs, {})



class FacadeProfileTests(unittest.TestCase):
    MANIFESTS = {"plakous": {"features": {"default": [], "full": ["b", "a"], "a": [], "b": []}}}

    @staticmethod
    def message(level: str, rendered: str) -> str:
        return json.dumps({"reason": "compiler-message",
                           "message": {"level": level, "rendered": rendered}})

    def check(self, results):  # type: ignore[no-untyped-def]
        with mock.patch.object(feature_gate.subprocess, "run", side_effect=results) as run, \
             mock.patch.object(feature_gate.sys, "stderr", new_callable=io.StringIO) as stderr, \
             mock.patch.object(feature_gate.sys, "stdout", new_callable=io.StringIO):
            errors = feature_gate.check_facade_profiles(self.MANIFESTS, ROOT)
        return errors, [call.args[0] for call in run.call_args_list], stderr.getvalue()

    def test_checks_the_empty_set_and_each_public_feature_alone(self):
        clean = subprocess.CompletedProcess([], 0, json.dumps({"reason": "build-finished"}), "")
        errors, commands, _ = self.check([clean] * 3)
        self.assertEqual(errors, [])
        base = ["cargo", "check", "--locked", "--offline", "-p", "plakous", "--no-default-features"]
        tail = ["--lib", "--message-format=json"]
        self.assertEqual(commands, [base + tail, base + ["--features", "a"] + tail,
                                    base + ["--features", "b"] + tail])

    def test_compile_errors_and_warnings_fail_with_rendered_diagnostics(self):
        failed = subprocess.CompletedProcess([], 101, self.message("error", "error[E0433]: x\n"), "")
        warned = subprocess.CompletedProcess([], 0, "\n".join([
            "not json", self.message("note", "note: ignored\n"), self.message("warning", "warning: y\n"),
        ]), "")
        aborted = subprocess.CompletedProcess([], 101, "", "error: no matching package\n")
        errors, _, stderr = self.check([failed, warned, aborted])
        self.assertEqual(errors, ["plakous [minimal]: cargo check failed",
                                  "plakous [a]: cargo check reported 1 warning(s)",
                                  "plakous [b]: cargo check failed"])
        self.assertIn("error[E0433]: x", stderr)
        self.assertIn("warning: y", stderr)
        self.assertNotIn("note: ignored", stderr)
        self.assertIn("no matching package", stderr)

    def test_missing_facade_is_an_error(self):
        self.assertEqual(feature_gate.check_facade_profiles({}, ROOT),
                         ["plakous: package missing from workspace metadata"])

class PublicCompileGateTests(unittest.TestCase):
    def setUp(self):
        patch = mock.patch.dict(os.environ, {'CARGO_BUILD_TARGET': 'test-target'})
        patch.start()
        self.addCleanup(patch.stop)

    def test_contract_cache_is_lane_and_target_specific(self):
        with tempfile.TemporaryDirectory() as directory:
            lane = Path(directory) / 'lane'
            for target in ('x86_64-unknown-linux-gnu', 'x86_64-unknown-linux-musl'):
                with self.subTest(target=target), mock.patch.dict(os.environ, {
                    'CARGO_TARGET_DIR': str(lane), 'CARGO_BUILD_TARGET': target,
                }, clear=True):
                    environment = compile_gate.contract_environment()
                    self.assertEqual(environment['CARGO_TARGET_DIR'],
                                     str(lane.resolve().parent / '.public-contracts' / lane.name / target))
                    self.assertEqual(environment['CARGO_BUILD_TARGET'], target)
                    self.assertEqual(environment, compile_gate.contract_environment())

    def test_contract_cache_defaults_to_checkout_and_rustc_host(self):
        with mock.patch.dict(os.environ, {}, clear=True), \
             mock.patch.object(compile_gate.subprocess, 'check_output',
                               return_value='rustc 1.99.0\nhost: aarch64-apple-darwin\n'):
            environment = compile_gate.contract_environment()
        self.assertEqual(environment['CARGO_TARGET_DIR'],
                         str(ROOT / '.public-contracts/target/aarch64-apple-darwin'))
        self.assertNotIn('CARGO_BUILD_TARGET', environment)

    def test_contract_cache_override_uses_persistent_container_mount_and_triple(self):
        target = 'x86_64-unknown-linux-musl'
        for image in ('image-a', 'next-image'):
            root = f'/qual/target/public-contracts-{image}'
            with self.subTest(image=image), mock.patch.dict(os.environ, {
                'CARGO_TARGET_DIR': '/qual/target', 'CARGO_BUILD_TARGET': target,
                'PLAK_PUBLIC_CONTRACTS_DIR': root,
            }, clear=True):
                environment = compile_gate.contract_environment()
                self.assertEqual(environment['CARGO_TARGET_DIR'], f'{root}/{target}')
                self.assertEqual(environment, compile_gate.contract_environment())


    def diagnostic(
        self, binary: str, *, code: str | None = "EXPECTED", line: int | None = None
    ) -> str:
        if code == "EXPECTED":
            code = compile_gate.AUTHORITY_NEGATIVE_ERROR_CODES[binary]
        expected_line = compile_gate.AUTHORITY_NEGATIVE_ERROR_LINES[binary]
        line = line or expected_line
        source = compile_gate.AUTHORITY_NEGATIVE_SOURCES[binary].splitlines()[
            expected_line - 1
        ]
        marker = compile_gate.AUTHORITY_NEGATIVE_MARKERS[binary]
        start = source.index(marker) + 1
        return json.dumps(
            {
                "reason": "compiler-message",
                "message": {
                    "code": None if code is None else {"code": code},
                    "message": compile_gate.AUTHORITY_NEGATIVE_ERROR_MESSAGES.get(
                        binary, "fixture diagnostic"
                    ),
                    "spans": [
                        {
                            "file_name": f"tests/fixtures/authority-negative/src/bin/{binary}.rs",
                            "is_primary": True,
                            "line_start": line,
                            "text": [
                                {
                                    "text": source,
                                    "highlight_start": start,
                                    "highlight_end": start + len(marker),
                                }
                            ],
                        }
                    ],
                },
            }
        )

    def test_negative_fixture_inventory_and_content_are_pinned(self) -> None:
        self.assertTrue(compile_gate.validate_supported_inventory_policy())
        self.assertEqual(
            set(compile_gate.AUTHORITY_NEGATIVE_BINS),
            set(compile_gate.AUTHORITY_NEGATIVE_SOURCES),
        )
        self.assertEqual(
            set(compile_gate.AUTHORITY_NEGATIVE_BINS),
            set(compile_gate.AUTHORITY_NEGATIVE_ERROR_CODES),
        )

    def test_oracle_accepts_only_import_diagnostic_at_pinned_span(self) -> None:
        binary = "driver_authority"
        self.assertTrue(
            compile_gate.has_authority_diagnostic(self.diagnostic(binary), binary)
        )
        self.assertFalse(
            compile_gate.has_authority_diagnostic(
                self.diagnostic(binary, code="E0277"), binary
            )
        )
        self.assertFalse(
            compile_gate.has_authority_diagnostic(self.diagnostic(binary, line=3), binary)
        )

    def test_oracle_pins_private_import_code_where_rustc_distinguishes_it(self) -> None:
        binary = "component_checkpoint"
        self.assertEqual(
            compile_gate.AUTHORITY_NEGATIVE_ERROR_CODES[binary], "E0603"
        )
        self.assertTrue(
            compile_gate.has_authority_diagnostic(
                self.diagnostic(binary, code="E0603"), binary
            )
        )

    def test_supported_types_remain_nameable_but_not_constructible(self) -> None:
        for binary in ("issued_invocation", "terminal_result"):
            with self.subTest(binary=binary):
                self.assertEqual(
                    compile_gate.AUTHORITY_NEGATIVE_ERROR_LINES[binary], 4
                )
                self.assertEqual(
                    compile_gate.AUTHORITY_NEGATIVE_ERROR_CODES[binary], None
                )
                self.assertTrue(
                    compile_gate.has_authority_diagnostic(self.diagnostic(binary), binary)
                )

    def test_supported_secret_stays_nameable_without_raw_exposure(self) -> None:
        binary = "credential_bytes"
        self.assertEqual(compile_gate.AUTHORITY_NEGATIVE_ERROR_LINES[binary], 5)
        self.assertEqual(compile_gate.AUTHORITY_NEGATIVE_ERROR_CODES[binary], "E0599")
        self.assertTrue(
            compile_gate.has_authority_diagnostic(self.diagnostic(binary), binary)
        )

    def test_xai_public_credentials_cannot_invoke_private_execution_methods(self) -> None:
        for binary in ("xai_platform_authority", "xai_platform_grant", "xai_platform_bearer"):
            with self.subTest(binary=binary):
                self.assertEqual(compile_gate.AUTHORITY_NEGATIVE_ERROR_CODES[binary], "E0624")
                self.assertEqual(compile_gate.AUTHORITY_NEGATIVE_ERROR_LINES[binary], 4)
                self.assertTrue(compile_gate.has_authority_diagnostic(self.diagnostic(binary), binary))
                self.assertFalse(compile_gate.has_authority_diagnostic(
                    self.diagnostic(binary, code="E0432"), binary
                ))

    def test_all_fixture_cargo_commands_are_locked_and_offline(self) -> None:
        for _label, command in (
            compile_gate.LOCK_PREFLIGHT
            + compile_gate.SUPPORTED_POSITIVE
            + compile_gate.DEPENDENCY_POSITIVE
            + compile_gate.AUTHORITY_NEGATIVE
        ):
            self.assertIn("--locked", command)
            self.assertIn("--offline", command)

    def test_fetch_prefetches_every_fixture_locked_without_compiling(self):
        for _ in range(2):
            with mock.patch.object(compile_gate.sys, 'argv', ['check_public_compile.py', '--fetch']), \
                 mock.patch.object(compile_gate.subprocess, 'run', return_value=
                    subprocess.CompletedProcess([], 0, stdout='')) as runner, \
                 mock.patch('sys.stdout', new_callable=io.StringIO):
                self.assertEqual(compile_gate.main(), 0)
            commands = [call.args[0] for call in runner.call_args_list]
            self.assertEqual(commands, [
                ('cargo', 'fetch', '--manifest-path', command[3], '--locked')
                for _, command in compile_gate.LOCK_PREFLIGHT
            ])
            self.assertEqual(len(commands), 6)

    def test_fetch_failure_stops_before_other_fixtures(self):
        with mock.patch.object(compile_gate.sys, 'argv', ['check_public_compile.py', '--fetch']), \
             mock.patch.object(compile_gate.subprocess, 'run', return_value=
                subprocess.CompletedProcess([], 73, stdout='fetch failed')) as runner, \
             mock.patch('sys.stdout', new_callable=io.StringIO):
            self.assertEqual(compile_gate.main(), 73)
        self.assertEqual(runner.call_count, 1)

    def test_fetch_cannot_silently_skip_fixtures_with_only(self):
        with mock.patch.object(compile_gate.sys, 'argv', ['check_public_compile.py',
                '--fetch', '--only', 'plakous-model']), \
             mock.patch.object(compile_gate.subprocess, 'run') as runner, \
             mock.patch('sys.stderr', new_callable=io.StringIO):
            with self.assertRaises(SystemExit) as error:
                compile_gate.main()
        self.assertEqual(error.exception.code, 2)
        runner.assert_not_called()

    def test_json_display_keeps_diagnostics_and_drops_artifacts(self) -> None:
        output = "\n".join(
            (
                json.dumps({"reason": "compiler-artifact", "target": {}}),
                json.dumps(
                    {
                        "reason": "compiler-message",
                        "message": {"rendered": "error[E0432]: denied\n"},
                    }
                ),
                "cargo summary",
            )
        )
        self.assertEqual(
            compile_gate.compiler_output_for_display(output),
            "error[E0432]: denied\ncargo summary",
        )

    def batch_output(self, binaries):
        records = []
        for binary in binaries:
            record = json.loads(self.diagnostic(binary))
            record['target'] = dict(name=binary, kind=['bin'])
            records.append(json.dumps(record))
        return '\n'.join(records)

    def run_negative_batch(self, output, *args, returncode=101):
        with mock.patch.object(compile_gate.sys, 'argv', ['check_public_compile.py',
                '--negative-only', '--only', 'negative-authority', *args]), \
             mock.patch.object(compile_gate.subprocess, 'run', return_value=
                subprocess.CompletedProcess([], returncode, stdout=output)) as runner, \
             mock.patch('sys.stdout', new_callable=io.StringIO), \
             mock.patch('sys.stderr', new_callable=io.StringIO):
            status = compile_gate.main()
        return status, runner.call_args_list

    def test_negative_batch_checks_every_bin_with_one_locked_offline_invocation(self):
        status, calls = self.run_negative_batch(self.batch_output(compile_gate.AUTHORITY_NEGATIVE_BINS))
        self.assertEqual(status, 0)
        self.assertEqual(len(calls), 1)
        command = calls[0].args[0]
        for flag in ('--keep-going', '--locked', '--offline', '--message-format=json'):
            self.assertIn(flag, command)
        self.assertEqual([command[i + 1] for i, arg in enumerate(command) if arg == '--bin'],
                         list(compile_gate.AUTHORITY_NEGATIVE_BINS))

    def test_negative_batch_rejects_missing_wrong_target_or_unexpected_success(self):
        output = self.batch_output(compile_gate.AUTHORITY_NEGATIVE_BINS)
        missing = '\n'.join(output.splitlines()[1:])
        first = json.loads(output.splitlines()[0])
        first['target']['name'] = 'some_dependency'
        wrong_target = json.dumps(first) + '\n' + missing
        for broken, returncode in ((missing, 101), (wrong_target, 101), (output, 0)):
            with self.subTest(returncode=returncode, output=broken[:50]):
                status, _ = self.run_negative_batch(broken, returncode=returncode)
                self.assertNotEqual(status, 0)

    def test_batch_selection_and_per_pin_archives_are_preserved(self):
        binary = 'credential_bytes'
        label = 'negative-authority-credential-bytes'
        with tempfile.TemporaryDirectory() as archive:
            with mock.patch.object(compile_gate.sys, 'argv', ['check_public_compile.py',
                    '--only', label, '--archive-dir', archive]), \
                 mock.patch.object(compile_gate.subprocess, 'run', return_value=
                    subprocess.CompletedProcess([], 101, stdout=self.batch_output([binary]))) as runner, \
                 mock.patch('sys.stdout', new_callable=io.StringIO):
                self.assertEqual(compile_gate.main(), 0)
            self.assertEqual(runner.call_count, 1)
            self.assertEqual([runner.call_args.args[0][i + 1]
                              for i, arg in enumerate(runner.call_args.args[0]) if arg == '--bin'],
                             [binary])
            self.assertIn('exit_code=101', (Path(archive) / f'{label}.log').read_text())

    def test_positive_batch_merges_only_identical_feature_and_fixture_groups(self):
        pins = (compile_gate.SUPPORTED_POSITIVE + compile_gate.EXAMPLE_POSITIVE
                + compile_gate.DEPENDENCY_POSITIVE + compile_gate.CONSUMER_MANIFEST_POSITIVE)
        batches = compile_gate.batch_passing_commands(pins)
        self.assertEqual(sorted(label for labels, _ in batches for label in labels),
                         sorted(label for label, _ in pins))
        self.assertEqual(len(batches), len(pins) - 6)
        for labels, command in batches:
            if len(labels) > 1:
                self.assertEqual(command[1], 'run')
        # An isolated compile pin must still compile under its original feature set.
        self.assertEqual(compile_gate.batch_passing_commands((pins[0],)),
                         (((pins[0][0],), pins[0][1]),))


class FeatureGateTests(unittest.TestCase):
    def test_cargo_commands_are_locked_and_offline(self) -> None:
        command = feature_gate.cargo_command("tree", ("openai",), False)
        self.assertIn("--locked", command)
        self.assertIn("--offline", command)

    def test_workspace_inventory_is_derived_from_offline_metadata(self) -> None:
        metadata = {
            "packages": [
                {"id": "one", "name": "plakous"},
                {"id": "two", "name": "plakous-code-mode"},
            ],
            "workspace_members": ["one", "two"],
        }
        with mock.patch.object(
            feature_gate.subprocess,
            "check_output",
            return_value=json.dumps(metadata),
        ) as check_output:
            self.assertEqual(
                feature_gate.workspace_package_names(),
                {"plakous", "plakous-code-mode"},
            )
        command = check_output.call_args.args[0]
        self.assertIn("--locked", command)
        self.assertIn("--offline", command)

    def test_test_only_dependency_features_are_read_from_normal_edges(self) -> None:
        tree = (
            "plak v0.1.0 (/repo/bin/plak) default\n"
            "plakous-http v0.1.0 (/repo/crates/common/http) fault-injection\n"
            "plakous-http v0.1.0 (/repo/crates/common/http) fault-injection (*)\n"
            "tokio v1.53.1 io-util,rt\n"
        )
        with mock.patch.object(
            feature_gate.subprocess, "check_output", return_value=tree
        ) as check_output:
            self.assertEqual(
                feature_gate.resolved_dependency_features("plak", ("test-hooks",), "plakous-http"),
                {"fault-injection"},
            )
        command = check_output.call_args.args[0]
        self.assertIn("normal,build", command)
        self.assertEqual(command[command.index("--features") + 1], "test-hooks")

    def test_a_default_build_enabling_fault_injection_is_an_error(self) -> None:
        def tree(command, **_):  # type: ignore[no-untyped-def]
            return "plakous-http v0.1.0 (/repo/crates/common/http) fault-injection\n"

        with (
            mock.patch.object(feature_gate.subprocess, "check_output", side_effect=tree),
            mock.patch.dict(
                feature_gate.TEST_ONLY_DEPENDENCY_FEATURES,
                {("plak", "test-hooks"): (("plakous-http", "fault-injection"),)},
                clear=True,
            ),
        ):
            errors = feature_gate.validate_test_only_dependency_features()
        self.assertEqual(
            errors,
            [
                "plak [default]: enables plakous-http/fault-injection, which only "
                "`test-hooks` may enable",
                "plak [maintainer]: enables plakous-http/fault-injection, which only "
                "`test-hooks` may enable",
            ],
        )

    def test_test_clock_cannot_enter_supported_closures(self) -> None:
        def features(package, profile, dependency):
            return {"test-clock"} if dependency in {"plakous", "plakous-openai", "plakous-openrouter"} else set()

        with mock.patch.object(feature_gate, "resolved_dependency_features", side_effect=features):
            errors = feature_gate.validate_test_only_dependency_features()
        for name in ("plakous", "plakous-openai", "plakous-openrouter"):
            self.assertTrue(any(f"{name}/test-clock" in error for error in errors))
        self.assertTrue(any("plak [maintainer]" in error for error in errors))
        self.assertTrue(any("plakous [full]" in error for error in errors))
        self.assertNotIn("test-clock", feature_gate.optional_features({"features": {"test-clock": [], "auth": []}}))

    def test_the_real_plak_gates_fault_injection_behind_test_hooks(self) -> None:
        self.assertEqual(feature_gate.validate_test_only_dependency_features(), [])

    def test_commit_trace_inventory_is_test_only(self) -> None:
        pins = feature_gate.TEST_ONLY_DEPENDENCY_FEATURES
        self.assertIn(("plakous-store", "commit-trace"), pins[("plak", "test-hooks")])
        self.assertIn(
            ("plakous-recursive-subagents-store", "commit-trace"),
            pins[("plak", "test-hooks")],
        )
        self.assertEqual(
            pins[("plakous-recursive-subagents-store", "commit-trace")],
            (("plakous-store", "commit-trace"),),
        )

    def test_commit_trace_is_absent_from_default_and_release_feature_closures(self) -> None:
        for profile in ((), ("maintainer",)):
            for dependency in ("plakous-store", "plakous-recursive-subagents-store"):
                with self.subTest(profile=profile, dependency=dependency):
                    self.assertNotIn(
                        "commit-trace",
                        feature_gate.resolved_dependency_features("plak", profile, dependency),
                    )
        command = feature_gate.cargo_command("tree", (), True, "plakous-recursive-subagents-store")
        command.extend(["-e", "normal,build", "--depth", "1", "--prefix", "none"])
        output = subprocess.check_output(command, cwd=ROOT, text=True)
        self.assertNotIn("plakous-store ", output)

    def test_expected_native_closure_includes_non_plakous_internal_packages(self) -> None:
        manifests = feature_gate.load_workspace_manifests()
        closure = feature_gate.expected_closure(
            "plakous", ("native-subscriptions",), False, manifests
        )
        self.assertIn("plakous-code-mode", closure)
        self.assertIn("plakous-py-eval", closure)
        self.assertNotIn("plak-compose-protocol", closure)

    def test_feature_profiles_are_per_package_and_capped(self) -> None:
        manifests = feature_gate.load_workspace_manifests()
        facade = feature_gate.optional_features(manifests["plakous"])
        self.assertEqual(len(feature_gate.feature_sets(facade, True)), 2 ** len(facade) + 2)
        self.assertLessEqual(len(facade), feature_gate.MAX_OPTIONAL_FEATURES)
        store = feature_gate.optional_features(manifests["plakous-store"])
        self.assertEqual(len(feature_gate.feature_sets(store, False)), 2 ** len(store) + 1)
        self.assertEqual(feature_gate.cargo_command("tree", (), True, "plak")[:6],
                         ["cargo", "tree", "--locked", "--offline", "-p", "plak"])

    def test_maintainer_feature_is_local_to_the_product(self) -> None:
        manifests = feature_gate.load_workspace_manifests()
        self.assertNotIn("maintainer", manifests["plakous"]["features"])
        self.assertNotIn("maintainer", manifests["plakous-hosted"]["features"])
        self.assertEqual(manifests["plak"]["features"]["maintainer"], [])
        self.assertIn("maintainer", feature_gate.optional_features(manifests["plak"]))


if __name__ == "__main__":
    unittest.main()
