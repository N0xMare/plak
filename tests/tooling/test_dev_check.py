"""The gate tiers select their layers, and every tier that runs Rust tests
supplies a real-worker prerequisite without guessing Cargo paths."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
GROUPS = ('static', 'builds', 'rustdoc', 'tests-parallel', 'tests-serial',
          'tests-durability', 'contracts', 'tooling', 'package')
CHECK_GROUPS = ('static', 'builds', 'tests-parallel', 'tests-serial', 'contracts', 'tooling')
PLATFORM_GROUPS = ('static', 'builds', 'tests-parallel', 'tests-serial', 'tests-durability')


class DevCheckWorkerTests(unittest.TestCase):
    def test_platform_and_check_are_ordered_subsets_of_full_with_independent_exclusions(self):
        self.assertLessEqual(set(PLATFORM_GROUPS), set(GROUPS))
        self.assertLessEqual(set(CHECK_GROUPS), set(GROUPS))
        self.assertNotIn('contracts', PLATFORM_GROUPS)
        self.assertIn('builds', CHECK_GROUPS)
        self.assertNotIn('tests-durability', CHECK_GROUPS)
        self.assertIn('tests-durability', PLATFORM_GROUPS)
        self.assertEqual(GROUPS[GROUPS.index('tests-serial') + 1], 'tests-durability')
        self.assertLessEqual({'tests-parallel', 'tests-serial', 'contracts', 'tooling'}, set(CHECK_GROUPS))
        for tier, groups in ((('--platform',), PLATFORM_GROUPS), ((), CHECK_GROUPS),
                             (('--full',), GROUPS)):
            with self.subTest(tier=tier):
                self.assertEqual(list(groups), [g for g in GROUPS if g in groups])
                self.log.unlink(missing_ok=True)
                result, calls, scripts = self.run_gate(*tier)
                self.assertEqual(result.returncode, 0, result.stderr)
                if 'tests-parallel' in groups:
                    self.assert_nextest_home_is_isolated(calls)
                self.assertEqual([line for line in result.stdout.splitlines()
                                  if line.startswith('== group:')],
                                 [f'== group: {group} ==' for group in groups])
                for checker in ('check_workspace.py', 'check_features.py', 'check_public_compile.py'):
                    self.assertEqual(any(s[0].endswith(checker) for s in scripts),
                                     tier != ('--platform',))

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "scripts").mkdir()
        shutil.copyfile(ROOT / "scripts/dev-check.sh", self.root / "scripts/dev-check.sh")
        shutil.copyfile(ROOT / "scripts/dev-tools.sh", self.root / "scripts/dev-tools.sh")
        shutil.copyfile(ROOT / "scripts/test-tmpdir.sh", self.root / "scripts/test-tmpdir.sh")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "calls.jsonl"
        self.worker = self.root / "custom target" / "host-triple" / "debug" / "monty"
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ['PATH'],
                        GATE_CALLS=str(self.log), GATE_WORKER=str(self.worker),
                        CARGO_TARGET_DIR=str(self.root / 'custom target'),
                        CARGO_BUILD_TARGET='host-triple')
        self.env.pop('PLAK_MONTY_WORKER', None)
        # Keep gate scratch inside the fixture instead of the shared per-checkout root.
        self.env['PLAK_TEST_TMPDIR'] = str(self.root / 'test-tmp')
        # The gate must create the isolated product home itself. An ambient
        # PLAK_HOME would look like the non-test cargo commands inherited one.
        self.env.pop('PLAK_HOME', None)
        self.env.pop('RUSTFLAGS', None)
        for key in [key for key in self.env if key.startswith('CARGO_TARGET_') and key.endswith('_RUSTFLAGS')]:
            del self.env[key]
        self.executable('cargo', r'''import json, os, sys
from pathlib import Path
args = sys.argv[1:]
if args == ['nextest', '--version']:
    print('cargo-nextest 0.9.100')
with open(os.environ['GATE_CALLS'], 'a') as log:
    log.write(json.dumps(dict(args=args, worker=os.environ.get('PLAK_MONTY_WORKER'),
                             target=os.environ.get('CARGO_BUILD_TARGET'),
                             target_dir=os.environ.get('CARGO_TARGET_DIR'),
                             plak_home=os.environ.get('PLAK_HOME'),
                             rustdocflags=os.environ.get('RUSTDOCFLAGS'),
                             rustflags=os.environ.get('RUSTFLAGS'),
                             target_rustflags=os.environ.get(
                                 'CARGO_TARGET_HOST_TRIPLE_RUSTFLAGS'))) + '\n')
failure = json.loads(os.environ.get('GATE_CARGO_FAILURES', '{}')).get(args[0], 0)
if failure and args != ['nextest', '--version']:
    raise SystemExit(failure)
if args[0] == 'test':
    if os.environ.get('GATE_BUILD_FAIL'):
        raise SystemExit(1)
    worker = Path(os.environ['GATE_WORKER'])
    worker.parent.mkdir(parents=True, exist_ok=True)
    worker.write_text('#!/bin/sh\nexit 0\n')
    worker.chmod(0o700)
    print(json.dumps(dict(reason='compiler-artifact', executable=str(worker) + '-harness',
                         target=dict(name='monty', kind=['bin']), profile=dict(test=True))))
    print(json.dumps(dict(reason='compiler-artifact', executable=str(worker),
                         target=dict(name='monty', kind=['bin']), profile=dict(test=False))))
''')
        self.executable('python3', r'''import json, os, sys
if sys.argv[1] == '-':
    os.execv(sys.executable, [sys.executable, *sys.argv[1:]])
with open(os.environ['GATE_CALLS'], 'a') as log:
    log.write(json.dumps(dict(python=sys.argv[1:])) + '\n')
failure = json.loads(os.environ.get('GATE_PYTHON_FAILURES', '{}')).get(
    os.path.basename(sys.argv[1]), 0)
if failure:
    raise SystemExit(failure)
if sys.argv[1].endswith('changed_packages.py'):
    print(os.environ.get('GATE_CHANGED', ''), end='')
''')

        publish = self.root / 'scripts/publish.sh'
        publish.write_text('#!' + sys.executable + r'''
import json, os, sys
with open(os.environ['GATE_CALLS'], 'a') as log:
    log.write(json.dumps(dict(publish=sys.argv[1:], offline=os.environ.get('CARGO_NET_OFFLINE'))) + '\n')
raise SystemExit(int(os.environ.get('GATE_PUBLISH_FAILURE', '0')))
''')
        publish.chmod(0o700)

    def executable(self, name, source):
        path = self.bin / name
        path.write_text('#!' + sys.executable + '\n' + source)
        path.chmod(0o700)

    def run_gate(self, *args):
        result = subprocess.run(['bash', str(self.root / 'scripts/dev-check.sh'), *args],
                                env=self.env, text=True, capture_output=True, timeout=15)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return result, [c for c in calls if 'args' in c], [c['python'] for c in calls if 'python' in c]

    def commit_fixture(self):
        """Make the fixture a clean git tree; gate scratch and logs are ignored."""
        (self.root / '.gitignore').write_text('calls.jsonl\n/test-tmp/\n/custom target/\n')
        git = ['git', '-C', str(self.root), '-c', 'user.name=t', '-c', 'user.email=t@t']
        subprocess.run([*git, 'init', '-q'], check=True)
        subprocess.run([*git, 'add', '-A'], check=True)
        subprocess.run([*git, 'commit', '-q', '-m', 'fixture'], check=True)
        return subprocess.run([*git, 'rev-parse', 'HEAD^{tree}'], check=True, text=True,
                              capture_output=True).stdout.strip()

    def publish_calls(self):
        lines = self.log.read_text().splitlines() if self.log.exists() else []
        return [call for call in map(json.loads, lines) if 'publish' in call]

    @property
    def evidence(self):
        return self.root / 'custom target' / 'gate-evidence' / 'full'

    def assert_feature_builds_deny_warnings(self, calls):
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual(len(builds), 2)
        for build in builds:
            self.assertIn('-D warnings', build.get('rustflags') or '')
        ambient = self.env.get('RUSTFLAGS') or ''
        if '-D warnings' in ambient:
            return
        for call in calls:
            if call['args'][0] == 'build':
                continue
            self.assertNotIn(
                '-D warnings',
                call.get('rustflags') or '',
                call['args'][:3],
            )

    def assert_nextest_home_is_isolated(self, calls):
        lanes = self.lanes(calls)
        self.assertGreaterEqual(len(lanes), 1)
        for lane in lanes:
            home = lane['plak_home']
            self.assertTrue(
                home and home.endswith('/plak-isolated-home'),
                home,
            )
            mode = os.stat(home).st_mode & 0o777
            self.assertEqual(mode, 0o700, home)
        for call in calls:
            if call['args'][:2] == ['nextest', 'run']:
                continue
            self.assertIsNone(
                call['plak_home'],
                f"non-test cargo inherited PLAK_HOME: {call['args'][:3]}",
            )

    @staticmethod
    def lanes(calls):
        return [c for c in calls if c['args'][:2] == ['nextest', 'run']]

    @staticmethod
    def profiles(calls):
        return [c['args'][c['args'].index('--profile') + 1] for c in DevCheckWorkerTests.lanes(calls)]

    @staticmethod
    def filtersets(calls):
        return [c['args'][c['args'].index('-E') + 1] if '-E' in c['args'] else None
                for c in DevCheckWorkerTests.lanes(calls)]

    def test_full_gate_builds_and_passes_exact_cargo_artifact_to_both_lanes(self):
        result, calls, _ = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'test']
        self.assertEqual(len(builds), 1)
        self.assertEqual(builds[0]['target'], 'host-triple')
        self.assertEqual(builds[0]['target_dir'], self.env['CARGO_TARGET_DIR'])
        lanes = [c for c in calls if c['args'][:2] == ['nextest', 'run']]
        self.assertEqual(len(lanes), 2)
        self.assertTrue(all(c['worker'] == str(self.worker.resolve()) for c in lanes))
        self.assert_nextest_home_is_isolated(calls)

    def test_explicit_relative_worker_is_resolved_without_building(self):
        self.worker.parent.mkdir(parents=True)
        self.worker.write_text('#!/bin/sh\nexit 0\n')
        self.worker.chmod(0o700)
        self.env['PLAK_MONTY_WORKER'] = str(self.worker.relative_to(self.root))
        result, calls, _ = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(c['args'][0] == 'test' for c in calls))
        lanes = [c for c in calls if c['args'][:2] == ['nextest', 'run']]
        self.assertEqual([c['worker'] for c in lanes], [str(self.worker.resolve())] * 2)

    def test_invalid_override_fails_before_any_test_lane(self):
        directory = self.root / 'directory'
        directory.mkdir()
        ordinary = self.root / 'non-executable'
        ordinary.write_text('not executable')
        for value in ('', 'missing', str(directory), str(ordinary)):
            with self.subTest(value=value):
                self.env['PLAK_MONTY_WORKER'] = value
                self.log.unlink(missing_ok=True)
                result, calls, _ = self.run_gate()
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(any(c['args'][0] == 'test' or
                                     c['args'][:2] == ['nextest', 'run'] for c in calls))

    def test_failed_build_cannot_run_worker_tests(self):
        self.env['GATE_BUILD_FAIL'] = '1'
        result, calls, _ = self.run_gate()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(c['args'][:2] == ['nextest', 'run'] for c in calls))

    def test_static_reports_every_failed_step_and_later_groups_still_run(self):
        self.env['GATE_CARGO_FAILURES'] = json.dumps({'fmt': 3, 'clippy': 4})
        self.env['GATE_PYTHON_FAILURES'] = json.dumps({
            'check_workspace.py': 5, 'check_features.py': 6, 'run_tooling_tests.py': 7,
        })
        result, calls, scripts = self.run_gate('--steps', 'static,contracts,tooling')
        self.assertEqual(result.returncode, 1)
        self.assertEqual([c['args'][0] for c in calls if c['args'][0] != 'nextest'],
                         ['fmt', 'clippy'])
        self.assertEqual([Path(s[0]).name for s in scripts if s[0].endswith('.py')],
                         ['check_workspace.py', 'check_features.py', 'check_public_compile.py',
                          'run_tooling_tests.py'])
        self.assertIn('failed static steps: fmt workspace features clippy', result.stdout)
        self.assertIn('failed groups: static tooling', result.stdout)
        self.assertEqual([line.split()[2] for line in result.stdout.splitlines()
                          if line.startswith('== time: ')], ['static', 'contracts', 'tooling'])
        self.assertNotIn('gate passed', result.stdout)
    def test_failed_test_lanes_do_not_skip_contracts_or_tooling(self):
        self.env['GATE_CARGO_FAILURES'] = json.dumps({'nextest': 8})
        result, calls, scripts = self.run_gate()
        self.assertEqual(result.returncode, 8)
        self.assertEqual(len(self.lanes(calls)), 2)
        self.assertEqual(sum(c['args'][0] == 'test' for c in calls), 1)
        for name in ('check_public_compile.py', 'run_tooling_tests.py'):
            self.assertTrue(any(s[0].endswith(name) for s in scripts))
        self.assertIn('failed groups: tests-parallel tests-serial', result.stdout)
        self.assertNotIn('gate passed', result.stdout)

    def test_default_tier_lints_builds_and_tests_the_whole_workspace_only(self):
        result, calls, scripts = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([c['args'][0] for c in calls if c['args'][0] not in {'nextest'}],
                         ['fmt', 'clippy', 'build', 'build', 'test'])
        builds = [c['args'] for c in calls if c['args'][0] == 'build']
        self.assertIn('--workspace', builds[0])
        self.assertNotIn('--all-features', builds[0])
        self.assertEqual(builds[1][builds[1].index('--features') + 1], 'maintainer')
        self.assert_feature_builds_deny_warnings(calls)
        self.assertEqual(self.profiles(calls), ['offline', 'serial'])
        self.assertEqual(self.filtersets(calls), [None, None])
        for checker in ('check_public_compile.py', 'check_workspace.py', 'check_features.py'):
            self.assertTrue(any(s[0].endswith(checker) for s in scripts))
        self.assertEqual(sum(s[0].endswith('run_tooling_tests.py') for s in scripts), 1)

    def test_feature_builds_deny_warnings_on_the_flags_cargo_will_use(self):
        self.env['RUSTFLAGS'] = '-C link-arg=-fuse-ld=mold'
        result, calls, _ = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual(
            [c['rustflags'] for c in builds],
            ['-C link-arg=-fuse-ld=mold -D warnings', '-C link-arg=-fuse-ld=mold -D warnings'],
        )
        for call in calls:
            if call['args'][0] == 'build':
                continue
            self.assertEqual(call['rustflags'], '-C link-arg=-fuse-ld=mold', call['args'][:3])

        self.log.unlink()
        self.env['CARGO_TARGET_HOST_TRIPLE_RUSTFLAGS'] = '-C link-arg=-fuse-ld=mold'
        self.env['RUSTFLAGS'] = 'should-not-be-used'
        result, calls, _ = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual(
            [c['target_rustflags'] for c in builds],
            ['-C link-arg=-fuse-ld=mold -D warnings', '-C link-arg=-fuse-ld=mold -D warnings'],
        )
        self.assertTrue(all(c['rustflags'] == 'should-not-be-used' for c in builds))
        for call in calls:
            if call['args'][0] == 'build':
                continue
            self.assertEqual(call['target_rustflags'], '-C link-arg=-fuse-ld=mold', call['args'][:3])
            self.assertEqual(call['rustflags'], 'should-not-be-used')

    def test_full_tier_adds_rustdoc_before_tests_and_structure_after(self):
        result, calls, scripts = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        order = [c['args'][0] for c in calls if c['args'][0] != 'nextest' or c['args'][1] == 'run']
        self.assertEqual(order, ['fmt', 'clippy', 'build', 'build', 'doc', 'test', 'nextest', 'nextest', 'nextest'])
        doc = next(c for c in calls if c['args'][0] == 'doc')
        self.assertEqual(doc['rustdocflags'], '-D warnings')
        for flag in ('--workspace', '--no-deps', '--all-features', '--locked'):
            self.assertIn(flag, doc['args'])
        ran = [s[0] for s in scripts]
        for checker in ('tools/checks/check_public_compile.py', 'tools/checks/check_workspace.py',
                        'tools/checks/check_features.py', 'tools/checks/run_tooling_tests.py'):
            self.assertIn(checker, ran)
    def test_platform_tier_checks_features_and_both_lanes_without_linux_checks(self):
        result, calls, scripts = self.run_gate('--platform')
        self.assertEqual(result.returncode, 0, result.stderr)
        order = [c['args'][0] for c in calls if c['args'][0] != 'nextest' or c['args'][1] == 'run']
        self.assertEqual(order, ['fmt', 'clippy', 'build', 'build', 'test', 'nextest', 'nextest', 'nextest'])
        self.assertEqual(self.profiles(calls), ['offline', 'serial', 'durability'])
        checks = [c['args'] for c in calls if c['args'][0] == 'build']
        self.assertEqual(checks, [
            ['build', '--locked', '--workspace', '--lib', '--bins', '--target', 'host-triple'],
            ['build', '--locked', '-p', 'plak', '--features', 'maintainer', '--bins',
             '--target', 'host-triple'],
        ])
        for checker in ('check_public_compile.py', 'check_workspace.py', 'check_features.py',
                        'run_tooling_tests.py'):
            self.assertFalse(any(s[0].endswith(checker) for s in scripts))
        for call in calls:
            if call['args'][0] in {'clippy', 'build', 'test'} or call['args'][:2] == ['nextest', 'run']:
                self.assertIn('--target', call['args'])
                self.assertEqual(call['args'][call['args'].index('--target') + 1], 'host-triple')

    def test_full_tier_checks_default_and_maintainer_feature_sets(self):
        result, calls, _ = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        checks = [c['args'] for c in calls if c['args'][0] == 'build']
        self.assertEqual(checks, [
            ['build', '--locked', '--workspace', '--lib', '--bins', '--target', 'host-triple'],
            ['build', '--locked', '-p', 'plak', '--features', 'maintainer', '--bins',
             '--target', 'host-triple'],
        ])

    def test_build_target_is_explicit_for_every_compile_and_test_invocation(self):
        result, calls, _ = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        for call in calls:
            args = call['args']
            if args[0] in {'clippy', 'build', 'check', 'doc', 'test'} or args[:2] == ['nextest', 'run']:
                with self.subTest(args=args):
                    self.assertIn('--target', args)
                    self.assertEqual(args[args.index('--target') + 1], 'host-triple')

    def test_unconfigured_target_adds_neither_target_nor_empty_arguments(self):
        self.env.pop('CARGO_BUILD_TARGET')
        result, calls, _ = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        for call in calls:
            self.assertNotIn('--target', call['args'])
            self.assertNotIn('', call['args'])

    def test_fast_tier_runs_the_parallel_lane_over_touched_packages_and_dependents(self):
        self.env['GATE_CHANGED'] = 'plakous-model\nplak\n'
        result, calls, scripts = self.run_gate('--fast')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(['tools/checks/changed_packages.py'], scripts)
        scope = 'rdeps(=plakous-model) | rdeps(=plak)'
        # The serial budget lane is left to the default tier.
        self.assertEqual(self.profiles(calls), ['offline'])
        self.assertEqual(self.filtersets(calls), [scope])
        self.assertIn('--no-tests=pass', self.lanes(calls)[0]['args'])
        self.assertFalse(any(c['args'][0] == 'doc' for c in calls))

    def test_fast_tier_takes_explicit_packages_without_reading_the_diff(self):
        result, calls, scripts = self.run_gate('--fast', 'plakous-store')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(['tools/checks/changed_packages.py'], scripts)
        self.assertEqual(self.filtersets(calls), ['rdeps(=plakous-store)'])

    def test_fast_tier_with_a_workspace_input_runs_the_whole_suite(self):
        self.env['GATE_CHANGED'] = '--workspace\n'
        result, calls, _ = self.run_gate('--fast')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.profiles(calls), ['offline'])
        self.assertEqual(self.filtersets(calls), [None])

    def test_fast_tier_without_rust_changes_still_runs_static(self):
        self.env['PLAK_MONTY_WORKER'] = 'missing'
        result, calls, _ = self.run_gate('--fast')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([c['args'][0] for c in calls if c['args'][0] != 'nextest'], ['fmt', 'clippy'])
        self.assertEqual(self.lanes(calls), [])

    def test_unknown_argument_is_refused(self):
        for args in (('--quick',), ('--changed',), ('--full', 'plak'), ('--platform', 'plak')):
            with self.subTest(args=args):
                result = subprocess.run(['bash', str(self.root / 'scripts/dev-check.sh'), *args],
                                        env=self.env, text=True, capture_output=True, timeout=15)
                self.assertEqual(result.returncode, 2)

    def test_steps_static_checks_structure_right_after_fmt(self):
        result, calls, scripts = self.run_gate('--steps', 'static')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([c['args'][0] for c in calls if c['args'][0] != 'nextest'],
                         ['fmt', 'clippy'])
        self.assertIn(['tools/checks/check_workspace.py'], scripts)
        self.assertIn(['tools/checks/check_features.py'], scripts)
        self.assertEqual(self.lanes(calls), [])
        records = [json.loads(line) for line in self.log.read_text().splitlines()]
        fmt = next(i for i, row in enumerate(records) if row.get('args', [''])[0] == 'fmt')
        self.assertEqual(records[fmt + 1]['python'], ['tools/checks/check_workspace.py'])
        self.assertEqual(records[fmt + 2]['python'], ['tools/checks/check_features.py'])
        self.assertEqual(records[fmt + 3]['args'][0], 'clippy')

    def test_full_equals_every_group_in_canonical_order(self):
        full, full_calls, full_scripts = self.run_gate('--full')
        self.log.unlink()
        steps, step_calls, step_scripts = self.run_gate('--steps', ','.join(reversed(GROUPS)))
        self.assertEqual(full.returncode, 0, full.stderr)
        self.assertEqual(steps.returncode, 0, steps.stderr)
        self.assertEqual(full_calls, step_calls)
        self.assertEqual(full_scripts, step_scripts)
        headings = [line for line in full.stdout.splitlines() if line.startswith('== group:')]
        self.assertEqual(headings, [f'== group: {group} ==' for group in GROUPS])

    def test_steps_serial_only_runs_the_serial_lane_and_worker_prerequisite(self):
        result, calls, _ = self.run_gate('--steps', 'tests-serial')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.profiles(calls), ['serial'])
        self.assertIn('--test-threads=1', self.lanes(calls)[0]['args'])
        self.assertEqual([c['args'][0] for c in calls if c['args'][0] != 'nextest'], ['test'])

    def test_bad_step_groups_fail_before_builds(self):
        for groups in ('', 'static,typo', ',static', 'static,'):
            with self.subTest(groups=groups):
                self.log.unlink(missing_ok=True)
                result, calls, _ = self.run_gate('--steps', groups)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertFalse(any(c['args'][0] != 'nextest' for c in calls))

    def test_check_omits_durability_and_platform_runs_it_serially(self):
        check, check_calls, _ = self.run_gate()
        self.assertEqual(check.returncode, 0, check.stderr)
        self.assertEqual(self.profiles(check_calls), ['offline', 'serial'])
        self.log.unlink()
        platform, platform_calls, _ = self.run_gate('--platform')
        self.assertEqual(platform.returncode, 0, platform.stderr)
        self.assertEqual(self.profiles(platform_calls), ['offline', 'serial', 'durability'])
        durability = self.lanes(platform_calls)[-1]
        self.assertIn('--test-threads=1', durability['args'])
        self.log.unlink()
        result, calls, _ = self.run_gate('--steps', 'tests-durability')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.profiles(calls), ['durability'])
        self.assertIn('--test-threads=1', self.lanes(calls)[0]['args'])
        self.assertEqual([c['args'][0] for c in calls if c['args'][0] != 'nextest'], ['test'])

    def test_package_group_runs_the_offline_publish_dry_run_in_a_git_tree(self):
        self.commit_fixture()
        result, calls, _ = self.run_gate('--steps', 'package')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.publish_calls(), [dict(publish=['--gate'], offline='true')])
        self.assertEqual([c['args'] for c in calls], [['nextest', '--version']])

    def test_package_group_is_skipped_outside_a_git_work_tree(self):
        result, _, _ = self.run_gate('--steps', 'package')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('skip package: not a git work tree', result.stdout)
        self.assertEqual(self.publish_calls(), [])

    def test_failed_package_dry_run_fails_the_gate(self):
        self.commit_fixture()
        self.env['GATE_PUBLISH_FAILURE'] = '3'
        result, _, _ = self.run_gate('--steps', 'package')
        self.assertEqual(result.returncode, 3)
        self.assertIn('failed groups: package', result.stdout)

    def test_only_a_passing_full_run_on_a_clean_tree_records_evidence(self):
        tree = self.commit_fixture()
        for tier in (('--steps', 'package'), ()):
            result, _, _ = self.run_gate(*tier)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(self.evidence.exists(), tier)
        self.env['GATE_PUBLISH_FAILURE'] = '1'
        result, _, _ = self.run_gate('--full')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.evidence.exists())
        del self.env['GATE_PUBLISH_FAILURE']
        (self.root / 'untracked').write_text('x')
        result, _, _ = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('no evidence: not a clean git tree', result.stdout)
        self.assertFalse(self.evidence.exists())
        (self.root / 'untracked').unlink()
        result, _, _ = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.evidence.read_text(), tree + '\n')
