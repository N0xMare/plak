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
GROUPS = (
    'static',
    'builds',
    'variants',
    'rustdoc',
    'tests-parallel',
    'tests-serial',
    'tests-durability',
    'contracts',
    'tooling',
    'package',
)
CHECK_GROUPS = ('static', 'builds', 'tests-parallel', 'tests-serial', 'contracts', 'tooling')
PLATFORM_GROUPS = ('static', 'builds', 'tests-parallel', 'tests-serial', 'tests-durability')
SUPPORTED = ('plakous', 'plakous-agent', 'plakous-model', 'plakous-tools', 'plak')
# The static group's clippy runs: the workspace, then each supported package alone.
CLIPPIES = ['clippy'] * (1 + len(SUPPORTED))
# The builds group, in order: workspace defaults, each supported package's own
# resolution, then the maintainer parent build the linkage check reads.
BUILDS = [
    ['build', '--locked', '--workspace', '--lib', '--bins', '--target', 'host-triple'],
    *(
        ['build', '--locked', '-p', package, '--lib', '--bins', '--target', 'host-triple']
        for package in SUPPORTED
    ),
    [
        'build',
        '--locked',
        '-p',
        'plak',
        '--features',
        'maintainer',
        '--bins',
        '--target',
        'host-triple',
    ],
]


class DevCheckWorkerTests(unittest.TestCase):
    def test_platform_and_check_are_ordered_subsets_of_full_with_independent_exclusions(self):
        self.assertLessEqual(set(PLATFORM_GROUPS), set(GROUPS))
        self.assertLessEqual(set(CHECK_GROUPS), set(GROUPS))
        self.assertNotIn('contracts', PLATFORM_GROUPS)
        self.assertIn('builds', CHECK_GROUPS)
        self.assertNotIn('tests-durability', CHECK_GROUPS)
        self.assertIn('tests-durability', PLATFORM_GROUPS)
        self.assertEqual(GROUPS[GROUPS.index('tests-serial') + 1], 'tests-durability')
        self.assertLessEqual(
            {'tests-parallel', 'tests-serial', 'contracts', 'tooling'}, set(CHECK_GROUPS)
        )
        for tier, groups in (
            (('--platform',), PLATFORM_GROUPS),
            ((), CHECK_GROUPS),
            (('--full',), GROUPS),
        ):
            with self.subTest(tier=tier):
                self.assertEqual(list(groups), [g for g in GROUPS if g in groups])
                self.log.unlink(missing_ok=True)
                result, calls, scripts = self.run_gate(*tier)
                self.assertEqual(result.returncode, 0, result.stderr)
                if 'tests-parallel' in groups:
                    self.assert_nextest_home_is_isolated(calls)
                self.assertEqual(
                    [line for line in result.stdout.splitlines() if line.startswith('== group:')],
                    [f'== group: {group} ==' for group in groups],
                )
                for checker in (
                    'check_workspace.py',
                    'check_features.py',
                    'check_public_compile.py',
                ):
                    self.assertEqual(
                        any(
                            s[0].endswith(checker)
                            and '--product-builds' not in s
                            and '--portable-crates' not in s
                            and '--wasm-libraries' not in s
                            for s in scripts
                        ),
                        tier != ('--platform',),
                    )
                self.assertEqual(
                    sum('--product-builds' in s for s in scripts), int('variants' in groups)
                )

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "tools/gate").mkdir(parents=True)
        (self.root / "tools/release").mkdir(parents=True)
        for name in (
            "dev-check.sh",
            "dev-tools.sh",
            "dev-tools-prefix.sh",
            "test-tmpdir.sh",
            "test-env.sh",
            "clippy.sh",
        ):
            destination = self.root / "tools/gate" / name
            shutil.copyfile(ROOT / "tools/gate" / name, destination)
            destination.chmod(0o755)
        shutil.copyfile(ROOT / "tools/gate/vcs_listing.py", self.root / "tools/gate/vcs_listing.py")
        shutil.copyfile(
            ROOT / "tools/gate/test-processes.py", self.root / "tools/gate/test-processes.py"
        )
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "calls.jsonl"
        self.worker = self.root / "custom target" / "host-triple" / "debug" / "plakous-py-host"
        # The gate probes the wasm target in the sysroot; other rustc calls are real.
        self.sysroot = self.root / 'sysroot'
        (self.sysroot / 'lib/rustlib/wasm32-unknown-unknown/lib').mkdir(parents=True)
        real_rustc = shutil.which('rustc')
        self.env = dict(
            os.environ,
            PATH=str(self.bin) + os.pathsep + os.environ['PATH'],
            GATE_CALLS=str(self.log),
            GATE_WORKER=str(self.worker),
            CARGO_TARGET_DIR=str(self.root / 'custom target'),
            CARGO_BUILD_TARGET='host-triple',
            GATE_SYSROOT=str(self.sysroot),
            GATE_REAL_RUSTC=real_rustc or '',
        )
        self.env.pop('PLAK_PY_HOST', None)
        self.env.pop('PLAK_JS_HOST', None)
        # The offline musl container exports this; the package group must decide it itself.
        self.env.pop('CARGO_NET_OFFLINE', None)
        # Keep gate scratch inside the fixture instead of the shared per-checkout root.
        self.env['PLAK_TEST_TMPDIR'] = str(self.root / 'test-tmp')
        # The gate must create the isolated product home itself. An ambient
        # PLAK_HOME would look like the non-test cargo commands inherited one.
        self.env.pop('PLAK_HOME', None)
        self.env.pop('RUSTFLAGS', None)
        self.env.pop('CARGO_ENCODED_RUSTFLAGS', None)
        self.env.pop('CARGO_BUILD_RUSTFLAGS', None)
        for key in [
            key
            for key in self.env
            if key.startswith('CARGO_TARGET_') and key.endswith('_RUSTFLAGS')
        ]:
            del self.env[key]
        self.executable(
            'cargo',
            r'''import json, os, sys
from pathlib import Path
args = sys.argv[1:]
if args == ['nextest', '--version']:
    print('cargo-nextest 0.9.100')
with open(os.environ['GATE_CALLS'], 'a') as log:
    log.write(json.dumps(dict(args=args, worker=os.environ.get('PLAK_PY_HOST'),
                             js_worker=os.environ.get('PLAK_JS_HOST'),
                             target=os.environ.get('CARGO_BUILD_TARGET'),
                             target_dir=os.environ.get('CARGO_TARGET_DIR'),
                             plak_home=os.environ.get('PLAK_HOME'),
                             rustdocflags=os.environ.get('RUSTDOCFLAGS'),
                             rustflags=os.environ.get('RUSTFLAGS'),
                             encoded_rustflags=os.environ.get('CARGO_ENCODED_RUSTFLAGS'),
                             rustc_wrapper=os.environ.get('RUSTC_WRAPPER'),
                             git_global=os.environ.get('GIT_CONFIG_GLOBAL'),
                             git_nosystem=os.environ.get('GIT_CONFIG_NOSYSTEM'),
                             target_rustflags=os.environ.get(
                                 'CARGO_TARGET_HOST_TRIPLE_RUSTFLAGS'))) + '\n')
if args[0] == 'clippy' and os.environ.get('GATE_CLIPPY_WAITS_FOR'):
    import time
    deadline = time.monotonic() + 5
    while not Path(os.environ['GATE_CLIPPY_WAITS_FOR']).exists():
        if time.monotonic() > deadline:
            raise SystemExit('clippy ran alone')
        time.sleep(0.02)
if args[0] == 'doc' and os.environ.get('GATE_DOC_WAITS_FOR'):
    import time
    deadline = time.monotonic() + 5
    while not Path(os.environ['GATE_DOC_WAITS_FOR']).exists():
        if time.monotonic() > deadline:
            raise SystemExit('doc ran alone')
        time.sleep(0.02)
failure = json.loads(os.environ.get('GATE_CARGO_FAILURES', '{}')).get(args[0], 0)
if args == ['fetch', '--locked', '--offline'] and os.environ.get('GATE_COLD_CACHE'):
    failure = 1
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
                         target=dict(name='plakous-py-host', kind=['bin']), profile=dict(test=True))))
    print(json.dumps(dict(reason='compiler-artifact', executable=str(worker),
                         target=dict(name='plakous-py-host', kind=['bin']), profile=dict(test=False))))
    js_worker = worker.with_name('plakous-js-host')
    js_worker.write_text('#!/bin/sh\nexit 0\n')
    js_worker.chmod(0o700)
    print(json.dumps(dict(reason='compiler-artifact', executable=str(js_worker),
                         target=dict(name='plakous-js-host', kind=['bin']), profile=dict(test=False))))
''',
        )
        self.executable(
            'ruff',
            "import sys\nif '--version' in sys.argv:\n    print('ruff 0.17.3')\nraise SystemExit(0)\n",
        )
        self.executable(
            'python3',
            r'''import json, os, sys
if sys.argv[1] == '-' or sys.argv[1].endswith(('vcs_listing.py', 'test-processes.py')):
    os.execv(sys.executable, [sys.executable, *sys.argv[1:]])
with open(os.environ['GATE_CALLS'], 'a') as log:
    log.write(json.dumps(dict(python=sys.argv[1:], git_global=os.environ.get('GIT_CONFIG_GLOBAL'),
                              target_dir=os.environ.get('CARGO_TARGET_DIR'),
                              rustflags=os.environ.get('RUSTFLAGS'),
                              encoded_rustflags=os.environ.get('CARGO_ENCODED_RUSTFLAGS'),
                              rustc_wrapper=os.environ.get('RUSTC_WRAPPER'),
                              target_rustflags=os.environ.get('CARGO_TARGET_HOST_TRIPLE_RUSTFLAGS'),
                              git_nosystem=os.environ.get('GIT_CONFIG_NOSYSTEM'))) + '\n')
if sys.argv[1].endswith('check_public_compile.py') and os.environ.get('GATE_DOC_WAITS_FOR'):
    open(os.environ['GATE_DOC_WAITS_FOR'], 'w').close()
failure = json.loads(os.environ.get('GATE_PYTHON_FAILURES', '{}')).get(
    os.path.basename(sys.argv[1]), 0)
if failure:
    raise SystemExit(failure)
if sys.argv[1].endswith('changed_packages.py'):
    print(os.environ.get('GATE_CHANGED', ''), end='')
if sys.argv[1].endswith('check_workspace.py') and '--wasm-libraries' in sys.argv:
    print('plakous-model')
''',
        )
        self.executable(
            'rustc',
            r'''import os, sys
if sys.argv[1:] == ['--print', 'sysroot']:
    print(os.environ['GATE_SYSROOT'])
    raise SystemExit(0)
real = os.environ['GATE_REAL_RUSTC']
if not real:
    raise SystemExit(127)
os.execv(real, [real, *sys.argv[1:]])
''',
        )

        publish = self.root / 'tools/release/publish.sh'
        publish.write_text(
            '#!'
            + sys.executable
            + r'''
import json, os, sys
with open(os.environ['GATE_CALLS'], 'a') as log:
    log.write(json.dumps(dict(publish=sys.argv[1:], offline=os.environ.get('CARGO_NET_OFFLINE'))) + '\n')
if os.environ.get('GATE_CLIPPY_WAITS_FOR'):
    open(os.environ['GATE_CLIPPY_WAITS_FOR'], 'w').close()
raise SystemExit(int(os.environ.get('GATE_PUBLISH_FAILURE', '0')))
'''
        )
        publish.chmod(0o700)

    def executable(self, name, source):
        path = self.bin / name
        path.write_text('#!' + sys.executable + '\n' + source)
        path.chmod(0o700)

    def run_gate(self, *args):
        result = subprocess.run(
            ['bash', str(self.root / 'tools/gate/dev-check.sh'), *args],
            env=self.env,
            text=True,
            capture_output=True,
            timeout=15,
        )
        calls = (
            [json.loads(line) for line in self.log.read_text().splitlines()]
            if self.log.exists()
            else []
        )
        self.python_calls = [c for c in calls if 'python' in c]
        return (
            result,
            [c for c in calls if 'args' in c],
            [c['python'] for c in calls if 'python' in c],
        )

    def commit_fixture(self):
        """Make the fixture a clean git tree; gate scratch and logs are ignored."""
        (self.root / '.gitignore').write_text('calls.jsonl\n/test-tmp/\n/custom target/\n')
        git = ['git', '-C', str(self.root), '-c', 'user.name=t', '-c', 'user.email=t@t']
        subprocess.run([*git, 'init', '-q'], check=True)
        subprocess.run([*git, 'add', '-A'], check=True)
        subprocess.run([*git, 'commit', '-q', '-m', 'fixture'], check=True)
        return subprocess.run(
            [*git, 'rev-parse', 'HEAD^{tree}'], check=True, text=True, capture_output=True
        ).stdout.strip()

    def publish_calls(self):
        lines = self.log.read_text().splitlines() if self.log.exists() else []
        return [call for call in map(json.loads, lines) if 'publish' in call]

    def test_variants_forward_the_target_and_warning_flags_without_running_other_groups(self):
        self.env['RUSTFLAGS'] = 'unused-base-flags'
        self.env['CARGO_TARGET_HOST_TRIPLE_RUSTFLAGS'] = '-C target-feature=+crt-static'
        result, calls, scripts = self.run_gate('--steps', 'variants')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call['args'] for call in calls], [['nextest', '--version']])
        self.assertIn(
            ['tools/gate/check_features.py', '--product-builds', '--target', 'host-triple'], scripts
        )
        call = next(call for call in self.python_calls if '--product-builds' in call['python'])
        self.assertEqual(call['target_rustflags'], '-C target-feature=+crt-static')
        self.assertEqual(call['rustflags'], 'unused-base-flags -D warnings')

    def test_variants_keep_the_compiler_wrapper_without_disabling_other_groups(self):
        self.env['RUSTC_WRAPPER'] = 'sccache'
        result, calls, _ = self.run_gate('--steps', 'builds,variants,rustdoc')
        self.assertEqual(result.returncode, 0, result.stderr)
        variants = next(call for call in self.python_calls if '--product-builds' in call['python'])
        self.assertEqual(variants['rustc_wrapper'], 'sccache')
        builds = [call for call in calls if call['args'][0] == 'build']
        self.assertEqual(len(builds), len(BUILDS))
        self.assertTrue(all(call['rustc_wrapper'] == 'sccache' for call in builds))
        rustdoc = next(call for call in calls if call['args'][0] == 'doc')
        self.assertEqual(rustdoc['rustc_wrapper'], 'sccache')

    @property
    def evidence(self):
        return self.root / 'custom target' / 'gate-evidence' / 'full'

    @property
    def package_evidence(self):
        return self.root / 'custom target' / 'gate-evidence' / 'package'

    def assert_feature_builds_deny_warnings(self, calls):
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual(len(builds), len(BUILDS))
        for build in builds:
            self.assertIn('-D warnings', build.get('rustflags') or '')
        ambient = self.env.get('RUSTFLAGS') or ''
        if '-D warnings' in ambient:
            return
        for call in calls:
            if call['args'][0] == 'build' or self.is_wasm_check(call):
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
    def is_wasm_check(call):
        return call['args'][0] == 'check' and 'wasm32-unknown-unknown' in call['args']

    @staticmethod
    def profiles(calls):
        return [
            c['args'][c['args'].index('--profile') + 1] for c in DevCheckWorkerTests.lanes(calls)
        ]

    @staticmethod
    def filtersets(calls):
        return [
            c['args'][c['args'].index('-E') + 1] if '-E' in c['args'] else None
            for c in DevCheckWorkerTests.lanes(calls)
        ]

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
        self.assertTrue(
            all(
                c['js_worker'] == str(self.worker.with_name('plakous-js-host').resolve())
                for c in lanes
            )
        )
        self.assert_nextest_home_is_isolated(calls)

    def test_explicit_relative_worker_is_resolved_without_building(self):
        self.worker.parent.mkdir(parents=True)
        self.worker.write_text('#!/bin/sh\nexit 0\n')
        self.worker.chmod(0o700)
        self.env['PLAK_PY_HOST'] = str(self.worker.relative_to(self.root))
        self.env['PLAK_JS_HOST'] = str(self.worker.relative_to(self.root))
        result, calls, _ = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(c['args'][0] == 'test' for c in calls))
        lanes = [c for c in calls if c['args'][:2] == ['nextest', 'run']]
        self.assertEqual([c['worker'] for c in lanes], [str(self.worker.resolve())] * 2)
        self.assertEqual([c['js_worker'] for c in lanes], [str(self.worker.resolve())] * 2)

    def test_invalid_override_fails_before_any_test_lane(self):
        directory = self.root / 'directory'
        directory.mkdir()
        ordinary = self.root / 'non-executable'
        ordinary.write_text('not executable')
        self.worker.parent.mkdir(parents=True)
        self.worker.write_text('#!/bin/sh\nexit 0\n')
        self.worker.chmod(0o700)
        self.env['PLAK_JS_HOST'] = str(self.worker)
        for value in ('', 'missing', str(directory), str(ordinary)):
            with self.subTest(value=value):
                self.env['PLAK_PY_HOST'] = value
                self.log.unlink(missing_ok=True)
                result, calls, _ = self.run_gate()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('PLAK_PY_HOST', result.stderr)
                self.assertFalse(
                    any(
                        c['args'][0] == 'test' or c['args'][:2] == ['nextest', 'run'] for c in calls
                    )
                )

    def test_invalid_js_override_fails_before_any_test_lane(self):
        self.worker.parent.mkdir(parents=True)
        self.worker.write_text('#!/bin/sh\nexit 0\n')
        self.worker.chmod(0o700)
        self.env['PLAK_PY_HOST'] = str(self.worker)
        for value in ('', 'missing', str(self.root)):
            with self.subTest(value=value):
                self.env['PLAK_JS_HOST'] = value
                self.log.unlink(missing_ok=True)
                result, calls, _ = self.run_gate()
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(any(c['args'][:2] == ['nextest', 'run'] for c in calls))

    def test_failed_build_cannot_run_worker_tests(self):
        self.env['GATE_BUILD_FAIL'] = '1'
        result, calls, _ = self.run_gate()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(c['args'][:2] == ['nextest', 'run'] for c in calls))

    def test_static_reports_every_failed_step_and_later_groups_still_run(self):
        self.env['GATE_CARGO_FAILURES'] = json.dumps({'fmt': 3, 'clippy': 4})
        self.env['GATE_PYTHON_FAILURES'] = json.dumps(
            {
                'check_workspace.py': 5,
                'check_features.py': 6,
                'gate_evidence.py': 9,
                'run_tooling_tests.py': 7,
            }
        )
        result, calls, scripts = self.run_gate('--steps', 'static,contracts,tooling')
        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            [c['args'][0] for c in calls if c['args'][0] != 'nextest'], ['fmt', 'fetch', *CLIPPIES]
        )
        self.assertEqual(
            [Path(s[0]).name for s in scripts if s[0].endswith('.py')],
            [
                'check_workspace.py',
                'check_features.py',
                'gate_evidence.py',
                'check_public_compile.py',
                'run_tooling_tests.py',
            ],
        )
        self.assertIn(
            'failed static steps: fmt workspace features evidence clippy '
            + ' '.join(f'clippy:{package}' for package in SUPPORTED),
            result.stdout,
        )
        self.assertIn('failed groups: static tooling', result.stdout)
        self.assertEqual(
            [
                line.split()[2]
                for line in result.stdout.splitlines()
                if line.startswith('== time: ')
            ],
            ['static', 'contracts', 'tooling'],
        )
        self.assertNotIn('gate passed', result.stdout)

    def test_contracts_and_package_run_beside_rustdoc_when_no_test_lane_is_selected(self):
        # The fake rustdoc waits for the contracts checker to have started:
        # run one after the other, rustdoc would fail.
        self.commit_fixture()
        self.env['GATE_DOC_WAITS_FOR'] = str(self.root / 'contracts-started')
        result, calls, scripts = self.run_gate(
            '--steps', 'static,rustdoc,contracts,tooling,package'
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            [
                line.split()[2]
                for line in result.stdout.splitlines()
                if line.startswith('== time: ')
            ],
            ['static', 'rustdoc', 'contracts', 'package', 'tooling'],
        )
        self.assertEqual(len(self.publish_calls()), 1)
        self.assertEqual(scripts.count(['tools/gate/check_public_compile.py']), 1)
        # Tooling waits for the overlapping groups to end.
        self.assertLess(
            scripts.index(['tools/gate/check_public_compile.py']),
            next(
                i for i, script in enumerate(scripts) if script[0].endswith('run_tooling_tests.py')
            ),
        )

    def test_package_starts_with_the_gate_beside_static(self):
        # The fake clippy waits for the publish dry run to have started.
        self.commit_fixture()
        self.env['GATE_CLIPPY_WAITS_FOR'] = str(self.root / 'package-started')
        result, calls, scripts = self.run_gate('--steps', 'static,contracts,package')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            [
                line.split()[2]
                for line in result.stdout.splitlines()
                if line.startswith('== time: ')
            ],
            ['static', 'contracts', 'package'],
        )
        records = [json.loads(line) for line in self.log.read_text().splitlines()]
        last_clippy = max(
            i for i, row in enumerate(records) if row.get('args', [''])[0] == 'clippy'
        )
        contracts = next(
            i
            for i, row in enumerate(records)
            if row.get('python') == ['tools/gate/check_public_compile.py']
        )
        # Contracts still waits for static to end.
        self.assertGreater(contracts, last_clippy)

    def test_contracts_and_package_run_after_a_selected_test_lane(self):
        self.commit_fixture()
        result, calls, scripts = self.run_gate('--steps', 'tests-serial,contracts,package')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        records = [json.loads(line) for line in self.log.read_text().splitlines()]
        lane = max(i for i, row in enumerate(records) if row.get('args', [''])[0] == 'nextest')
        contracts = next(
            i
            for i, row in enumerate(records)
            if row.get('python') == ['tools/gate/check_public_compile.py']
        )
        publish = next(i for i, row in enumerate(records) if 'publish' in row)
        self.assertGreater(contracts, lane)
        self.assertGreater(publish, contracts)

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
        self.assertEqual(
            [c['args'][0] for c in calls if c['args'][0] not in {'nextest'}],
            ['fmt', 'fetch', *CLIPPIES, *['build'] * len(BUILDS), 'check', 'test'],
        )
        builds = [c['args'] for c in calls if c['args'][0] == 'build']
        self.assertIn('--workspace', builds[0])
        self.assertNotIn('--all-features', builds[0])
        self.assertEqual(builds[-1][builds[-1].index('--features') + 1], 'maintainer')
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
            ['-C link-arg=-fuse-ld=mold -D warnings'] * len(BUILDS),
        )
        for call in calls:
            if call['args'][0] == 'build' or self.is_wasm_check(call):
                continue
            self.assertEqual(call['rustflags'], '-C link-arg=-fuse-ld=mold', call['args'][:3])

        self.log.unlink()
        self.env['CARGO_TARGET_HOST_TRIPLE_RUSTFLAGS'] = '-C link-arg=-fuse-ld=mold'
        self.env['RUSTFLAGS'] = 'should-not-be-used'
        result, calls, _ = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual(
            [c['rustflags'] for c in builds],
            ['should-not-be-used -D warnings'] * len(BUILDS),
        )
        self.assertTrue(all(c['target_rustflags'] == '-C link-arg=-fuse-ld=mold' for c in builds))
        for call in calls:
            if call['args'][0] == 'build' or self.is_wasm_check(call):
                continue
            self.assertEqual(
                call['target_rustflags'], '-C link-arg=-fuse-ld=mold', call['args'][:3]
            )
            self.assertEqual(call['rustflags'], 'should-not-be-used')

        self.log.unlink()
        self.env['CARGO_ENCODED_RUSTFLAGS'] = 'linker\x1fmold'
        result, calls, _ = self.run_gate('--steps', 'builds')
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual(
            [c['encoded_rustflags'] for c in builds],
            ['linker\x1fmold\x1f-D\x1fwarnings'] * len(BUILDS),
        )
        self.assertTrue(all(c['rustflags'] == 'should-not-be-used' for c in builds))
        self.assertTrue(all(c['target_rustflags'] == '-C link-arg=-fuse-ld=mold' for c in builds))
        self.env.pop('CARGO_ENCODED_RUSTFLAGS')

        # No rustflags environment variable: configured target rustflags (not
        # build.rustflags) are copied through, and an explicit RUSTFLAGS still
        # replaces them.
        self.log.unlink()
        self.env.pop('RUSTFLAGS', None)
        self.env.pop('CARGO_TARGET_HOST_TRIPLE_RUSTFLAGS', None)
        cargo_home = self.root / 'cargo-home'
        cargo_home.mkdir()
        (cargo_home / 'config.toml').write_text(
            '[build]\nrustflags = ["-C", "link-arg=-fuse-ld=gold"]\n',
            encoding='utf-8',
        )
        (self.root / '.cargo').mkdir()
        (self.root / '.cargo' / 'config.toml').write_text(
            '[target.host-triple]\n'
            'rustflags = ["-C", "link-arg=-B/opt/mold/bin", "-C", "link-arg=-fuse-ld=mold"]\n'
            '[build]\n'
            'rustflags = ["-C", "link-arg=-fuse-ld=bfd"]\n',
            encoding='utf-8',
        )
        self.env['CARGO_HOME'] = str(cargo_home)
        result, calls, _ = self.run_gate('--steps', 'builds')
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual(
            [c['rustflags'] for c in builds],
            ['-C link-arg=-B/opt/mold/bin -C link-arg=-fuse-ld=mold -D warnings'] * len(BUILDS),
        )
        self.log.unlink()
        (self.root / '.cargo' / 'config.toml').write_text(
            '[target.\'cfg(target_os = "other")\']\nrustflags = ["-C", "link-arg=-fuse-ld=lld"]\n',
            encoding='utf-8',
        )
        result, calls, _ = self.run_gate('--steps', 'builds')
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual(
            [c['rustflags'] for c in builds],
            ['-C link-arg=-fuse-ld=gold -D warnings'] * len(BUILDS),
        )
        self.log.unlink()
        self.env['RUSTFLAGS'] = '-C link-arg=-fuse-ld=lld'
        result, calls, _ = self.run_gate('--steps', 'builds')
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual(
            [c['rustflags'] for c in builds],
            ['-C link-arg=-fuse-ld=lld -D warnings'] * len(BUILDS),
        )
        # An empty RUSTFLAGS is set, so it replaces configured rustflags.
        self.log.unlink()
        self.env['RUSTFLAGS'] = ''
        result, calls, _ = self.run_gate('--steps', 'builds')
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual([c['rustflags'] for c in builds], ['-D warnings'] * len(BUILDS))

    def test_configured_rustflags_are_read_once_and_build_env_is_build_rustflags(self):
        cargo = self.root / '.cargo'
        cargo.mkdir()
        (cargo / 'config.toml').write_text(
            '[build]\n'
            'rustflags = ["-C", "link-arg=-B/opt/mold/bin", "-C", "link-arg=-fuse-ld=mold"]\n',
            encoding='utf-8',
        )
        # CARGO_HOME is also an ancestor `.cargo` of the gate's cwd.
        self.env['CARGO_HOME'] = str(cargo)
        result, calls, _ = self.run_gate('--steps', 'builds')
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'build']
        once = '-C link-arg=-B/opt/mold/bin -C link-arg=-fuse-ld=mold -D warnings'
        self.assertEqual([c['rustflags'] for c in builds], [once] * len(BUILDS))

        self.log.unlink()
        self.env['CARGO_BUILD_RUSTFLAGS'] = '-C link-arg=-fuse-ld=lld'
        result, calls, _ = self.run_gate('--steps', 'builds')
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual(
            [c['rustflags'] for c in builds],
            [
                '-C link-arg=-B/opt/mold/bin -C link-arg=-fuse-ld=mold -C link-arg=-fuse-ld=lld -D warnings'
            ]
            * len(BUILDS),
        )

        self.log.unlink()
        (cargo / 'config.toml').write_text(
            '[target.host-triple]\nrustflags = ["-C", "link-arg=-fuse-ld=gold"]\n',
            encoding='utf-8',
        )
        result, calls, _ = self.run_gate('--steps', 'builds')
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'build']
        self.assertEqual(
            [c['rustflags'] for c in builds],
            ['-C link-arg=-fuse-ld=gold -D warnings'] * len(BUILDS),
        )

    def test_full_tier_adds_rustdoc_before_tests_and_structure_after(self):
        result, calls, scripts = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        order = [c['args'][0] for c in calls if c['args'][0] != 'nextest' or c['args'][1] == 'run']
        self.assertEqual(
            order,
            [
                'fmt',
                'fetch',
                *CLIPPIES,
                *['build'] * len(BUILDS),
                'check',
                *['doc'] * (1 + len(SUPPORTED)),
                'test',
                'nextest',
                'nextest',
                'nextest',
                'nextest',
                'nextest',
            ],
        )
        docs = [c for c in calls if c['args'][0] == 'doc']
        soaks = [c for c in calls if '--run-ignored' in c['args']]
        self.assertEqual(len(soaks), 2)
        self.assertEqual(
            soaks[0]['args'][-4:], ['--run-ignored', 'only', '-E', 'test(ledger_retirement_5000_)']
        )
        self.assertTrue(all(doc['rustdocflags'] == '-D warnings' for doc in docs))
        for flag in ('--workspace', '--no-deps', '--all-features', '--locked'):
            self.assertIn(flag, docs[0]['args'])
        self.assertEqual(
            [doc['args'] for doc in docs[1:]],
            [
                ['doc', '-p', package, '--no-deps', '--locked', '--target', 'host-triple']
                for package in SUPPORTED
            ],
        )
        ran = [s[0] for s in scripts]
        for checker in (
            'tools/gate/check_public_compile.py',
            'tools/gate/check_workspace.py',
            'tools/gate/check_features.py',
            'tools/gate/run_tooling_tests.py',
        ):
            self.assertIn(checker, ran)

    def test_platform_tier_checks_features_and_both_lanes_without_linux_checks(self):
        result, calls, scripts = self.run_gate('--platform')
        self.assertEqual(result.returncode, 0, result.stderr)
        order = [c['args'][0] for c in calls if c['args'][0] != 'nextest' or c['args'][1] == 'run']
        self.assertEqual(
            order,
            [
                'fmt',
                *CLIPPIES,
                *['build'] * len(BUILDS),
                'check',
                'test',
                'nextest',
                'nextest',
                'nextest',
            ],
        )
        self.assertEqual(self.profiles(calls), ['offline', 'serial', 'durability'])
        checks = [c['args'] for c in calls if c['args'][0] == 'build']
        self.assertEqual(checks, BUILDS)
        for checker in (
            'check_public_compile.py',
            'check_workspace.py',
            'check_features.py',
            'run_tooling_tests.py',
        ):
            self.assertFalse(
                any(
                    s[0].endswith(checker)
                    and '--product-builds' not in s
                    and '--portable-crates' not in s
                    and '--wasm-libraries' not in s
                    for s in scripts
                )
            )
        self.assertEqual(sum('--product-builds' in s for s in scripts), 0)
        for call in calls:
            if call['args'][0] in {'clippy', 'build', 'test'} or call['args'][:2] == [
                'nextest',
                'run',
            ]:
                self.assertIn('--target', call['args'])
                self.assertEqual(call['args'][call['args'].index('--target') + 1], 'host-triple')

    def test_full_tier_checks_default_and_maintainer_feature_sets(self):
        result, calls, _ = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        checks = [c['args'] for c in calls if c['args'][0] == 'build']
        self.assertEqual(checks, BUILDS)

    def test_supported_packages_build_alone_from_the_manifest_classes(self):
        import tomllib

        workspace = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']
        supported = set()
        for member in workspace['members']:
            package = tomllib.loads((ROOT / member / 'Cargo.toml').read_text())['package']
            if package.get('metadata', {}).get('plakous', {}).get('class') == 'supported':
                supported.add(package['name'])
        self.assertEqual(supported, set(SUPPORTED))
        self.assertIn(
            f"supported_packages=({' '.join(SUPPORTED)})",
            (ROOT / 'tools/gate/dev-check.sh').read_text(),
        )

    def test_tests_and_tooling_run_with_an_isolated_git_config(self):
        result, calls, _ = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        scratch = self.root / 'test-tmp'
        # Everything after the toolchain probe: the tooling suite and the checkers.
        isolated = [
            *self.lanes(calls),
            *(c for c in calls if c['args'][0] == 'test'),
            *(c for c in self.python_calls if c['python'][0].endswith('.py')),
        ]
        self.assertTrue(
            any(
                c.get('python', [''])[0].endswith('run_tooling_tests.py') for c in self.python_calls
            )
        )
        for call in isolated:
            with self.subTest(call=call.get('args', call.get('python'))[:3]):
                self.assertEqual(call['git_global'], str(scratch.resolve() / 'gitconfig'))
                self.assertEqual(call['git_nosystem'], '1')
        self.assertEqual((scratch / 'gitconfig').read_text(), '')

    def test_build_target_is_explicit_for_every_compile_and_test_invocation(self):
        result, calls, _ = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        for call in calls:
            args = call['args']
            if args[0] == 'check' and 'wasm32-unknown-unknown' in args:
                with self.subTest(args=args):
                    self.assertEqual(args[args.index('--target') + 1], 'wasm32-unknown-unknown')
                    self.assertEqual(call['target'], 'wasm32-unknown-unknown')
                    self.assertIn('-D warnings', call.get('rustflags') or '')
                continue
            if args[0] in {'clippy', 'build', 'check', 'doc', 'test'} or args[:2] == [
                'nextest',
                'run',
            ]:
                with self.subTest(args=args):
                    self.assertIn('--target', args)
                    self.assertEqual(args[args.index('--target') + 1], 'host-triple')

    def test_unconfigured_target_adds_neither_target_nor_empty_arguments(self):
        self.env.pop('CARGO_BUILD_TARGET')
        result, calls, _ = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        wasm = [call for call in calls if self.is_wasm_check(call)]
        self.assertEqual(len(wasm), 1)
        self.assertNotIn('--target', wasm[0]['args'][: wasm[0]['args'].index('--target')])
        for call in calls:
            if self.is_wasm_check(call):
                self.assertEqual(
                    call['args'][call['args'].index('--target') + 1], 'wasm32-unknown-unknown'
                )
                continue
            self.assertNotIn('--target', call['args'])
            self.assertNotIn('', call['args'])

    WASM_SKIP = '== skipped: wasm32-unknown-unknown portable check (spec 06 §7.5) =='

    def test_missing_wasm_target_fails_the_builds_group(self):
        shutil.rmtree(self.sysroot / 'lib/rustlib/wasm32-unknown-unknown')
        # An environment variable is not the container flag.
        self.env['PLAK_QUAL_CONTAINER'] = '1'
        result, calls, _ = self.run_gate('--steps', 'builds')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('just dev-tools install', result.stderr)
        self.assertNotIn('skip wasm32-unknown-unknown:', result.stdout)
        self.assertNotIn(self.WASM_SKIP, result.stdout)
        self.assertIn('failed groups: builds', result.stdout)
        self.assertFalse(any(self.is_wasm_check(call) for call in calls))

    def test_the_container_flag_runs_the_wasm_check_when_the_target_exists(self):
        result, calls, _ = self.run_gate('--qual-container', '--steps', 'builds')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len([c for c in calls if self.is_wasm_check(c)]), 1)
        self.assertNotIn('skipped:', result.stdout)

    def test_musl_container_skips_a_missing_wasm_target_and_names_it_last(self):
        shutil.rmtree(self.sysroot / 'lib/rustlib/wasm32-unknown-unknown')
        result, calls, _ = self.run_gate('--qual-container', '--steps', 'builds')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            'skip wasm32-unknown-unknown: the preloaded musl qualification image does not include this rustc target',
            result.stdout,
        )
        self.assertFalse(any(self.is_wasm_check(call) for call in calls))
        self.assertEqual(len([c for c in calls if c['args'][0] == 'build']), len(BUILDS))
        summary = result.stdout.splitlines()[-2:]
        self.assertEqual(summary, [self.WASM_SKIP, '== ok: steps gate passed =='])

    def test_a_skipped_full_run_on_a_clean_tree_records_no_evidence(self):
        shutil.rmtree(self.sysroot / 'lib/rustlib/wasm32-unknown-unknown')
        self.commit_fixture()
        result, _, _ = self.run_gate('--qual-container', '--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(self.WASM_SKIP, result.stdout.splitlines())
        self.assertIn('no evidence: a check was skipped', result.stderr)
        self.assertNotIn('evidence: full gate passed', result.stdout)
        self.assertNotIn('evidence: package group passed', result.stdout)
        self.assertFalse(self.evidence.exists())
        self.assertFalse(self.package_evidence.exists())

    def test_fast_tier_runs_the_parallel_lane_over_touched_packages_and_dependents(self):
        self.env['GATE_CHANGED'] = 'plakous-model\nplak\n'
        result, calls, scripts = self.run_gate('--fast')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(['tools/gate/changed_packages.py'], scripts)
        scope = 'rdeps(=plakous-model) | rdeps(=plak)'
        # The serial budget lane is left to the default tier.
        self.assertEqual(self.profiles(calls), ['offline'])
        self.assertEqual(self.filtersets(calls), [scope])
        self.assertIn('--no-tests=pass', self.lanes(calls)[0]['args'])
        self.assertFalse(any(c['args'][0] == 'doc' for c in calls))

    def test_fast_tier_takes_explicit_packages_without_reading_the_diff(self):
        result, calls, scripts = self.run_gate('--fast', 'plakous-store')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(['tools/gate/changed_packages.py'], scripts)
        self.assertEqual(self.filtersets(calls), ['rdeps(=plakous-store)'])

    def test_fast_tier_with_a_workspace_input_runs_the_whole_suite(self):
        self.env['GATE_CHANGED'] = '--workspace\n'
        result, calls, _ = self.run_gate('--fast')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.profiles(calls), ['offline'])
        self.assertEqual(self.filtersets(calls), [None])

    def test_fast_tier_without_rust_changes_still_runs_static(self):
        self.env['PLAK_PY_HOST'] = 'missing'
        result, calls, _ = self.run_gate('--fast')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            [c['args'][0] for c in calls if c['args'][0] != 'nextest'], ['fmt', 'fetch', *CLIPPIES]
        )
        self.assertEqual(self.lanes(calls), [])

    def test_unknown_argument_is_refused(self):
        for args in (('--quick',), ('--changed',), ('--full', 'plak'), ('--platform', 'plak')):
            with self.subTest(args=args):
                result = subprocess.run(
                    ['bash', str(self.root / 'tools/gate/dev-check.sh'), *args],
                    env=self.env,
                    text=True,
                    capture_output=True,
                    timeout=15,
                )
                self.assertEqual(result.returncode, 2)

    def test_steps_static_fetches_offline_before_structure_checks(self):
        result, calls, scripts = self.run_gate('--steps', 'static')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('== ruff ==', result.stdout)
        self.assertEqual(
            [c['args'][0] for c in calls if c['args'][0] != 'nextest'], ['fmt', 'fetch', *CLIPPIES]
        )
        self.assertIn(['tools/gate/check_workspace.py'], scripts)
        self.assertIn(['tools/gate/check_features.py'], scripts)
        self.assertEqual(self.lanes(calls), [])
        records = [json.loads(line) for line in self.log.read_text().splitlines()]
        fmt = next(i for i, row in enumerate(records) if row.get('args', [''])[0] == 'fmt')
        self.assertEqual(records[fmt + 1]['args'], ['fetch', '--locked', '--offline'])
        # The structure checkers run in order beside clippy, after the fetch.
        after_fetch = records[fmt + 2 :]
        self.assertEqual(
            [row['python'] for row in after_fetch if 'python' in row],
            [
                ['tools/gate/check_workspace.py'],
                ['tools/gate/check_features.py'],
                ['tools/release/gate_evidence.py', 'audit'],
            ],
        )
        features = next(
            row for row in after_fetch if row.get('python') == ['tools/gate/check_features.py']
        )
        self.assertTrue(features['target_dir'].endswith('feature-check'), features)
        self.assertEqual(
            [row['args'][0] for row in after_fetch if 'args' in row], ['clippy'] * len(CLIPPIES)
        )
        clippies = [row['args'] for row in records if row.get('args', [''])[0] == 'clippy']
        self.assertEqual(clippies[0][:3], ['clippy', '--locked', '--workspace'])
        self.assertEqual([args[args.index('-p') + 1] for args in clippies[1:]], list(SUPPORTED))
        for args in clippies[1:]:
            self.assertNotIn('--all-features', args)
            self.assertIn('--all-targets', args)
            self.assertEqual(args[args.index('--') + 1 :], ['-D', 'warnings'])

    def test_cold_cache_fetches_locked_online_before_structure_checks(self):
        self.env['GATE_COLD_CACHE'] = '1'
        result, calls, _ = self.run_gate('--steps', 'static')
        self.assertEqual(result.returncode, 0, result.stderr)
        fetches = [c['args'] for c in calls if c['args'][0] == 'fetch']
        self.assertEqual(fetches, [['fetch', '--locked', '--offline'], ['fetch', '--locked']])
        records = [json.loads(line) for line in self.log.read_text().splitlines()]
        online = next(
            i for i, row in enumerate(records) if row.get('args') == ['fetch', '--locked']
        )
        workspace = next(
            i
            for i, row in enumerate(records)
            if row.get('python') == ['tools/gate/check_workspace.py']
        )
        self.assertGreater(workspace, online)

    def test_failed_fetch_fails_static_but_runs_the_remaining_checks(self):
        self.env['GATE_CARGO_FAILURES'] = json.dumps({'fetch': 1})
        result, calls, scripts = self.run_gate('--steps', 'static')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('failed static steps: fetch', result.stdout)
        self.assertIn(['tools/gate/check_features.py'], scripts)
        self.assertTrue(any(c['args'][0] == 'clippy' for c in calls))

    def test_mismatched_workspace_manifest_fails_static_naming_the_step(self):
        (self.root / 'Cargo.toml').write_text('[workspace]\nmembers = ["crates/alpha"]\n')
        member = self.root / 'crates/alpha/Cargo.toml'
        member.parent.mkdir(parents=True)
        member.write_text(
            '[package]\nname = "beta"\n\n[package.metadata.plakous]\nclass = "supported"\n'
        )
        result, calls, _scripts = self.run_gate('--steps', 'static')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('== failed static steps: clippy:packages ==', result.stdout)
        self.assertIn('failed groups: static', result.stdout)
        self.assertIn('do not match manifest class supported', result.stderr)
        self.assertFalse(any(call['args'][0] == 'clippy' for call in calls))

    def test_clippy_failure_without_a_status_still_names_the_step(self):
        script = self.root / 'tools/gate/clippy.sh'
        script.write_text('#!/bin/bash\nexit 1\n')
        script.chmod(0o755)
        result, calls, _scripts = self.run_gate('--steps', 'static')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('== failed static steps: clippy ==', result.stdout)
        self.assertNotIn('clippy:packages', result.stdout)
        self.assertFalse(any(call['args'][0] == 'clippy' for call in calls))

    def test_full_adds_only_the_ignored_soak_to_every_group(self):
        full, full_calls, full_scripts = self.run_gate('--full')
        self.log.unlink()
        steps, step_calls, step_scripts = self.run_gate('--steps', ','.join(reversed(GROUPS)))
        self.assertEqual(full.returncode, 0, full.stderr)
        self.assertEqual(steps.returncode, 0, steps.stderr)
        soaks = [call for call in full_calls if '--run-ignored' in call['args']]
        self.assertEqual(len(soaks), 2)
        self.assertEqual([call for call in full_calls if call not in soaks], step_calls)
        self.assertEqual(full_scripts, step_scripts)
        headings = [line for line in full.stdout.splitlines() if line.startswith('== group:')]
        self.assertEqual(headings, [f'== group: {group} ==' for group in GROUPS])

    def test_steps_serial_only_runs_the_serial_lane_and_worker_prerequisite(self):
        result, calls, _ = self.run_gate('--steps', 'tests-serial')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.profiles(calls), ['serial'])
        self.assertIn('--test-threads=1', self.lanes(calls)[0]['args'])
        self.assertNotIn('--run-ignored', self.lanes(calls)[0]['args'])
        self.assertEqual([c['args'][0] for c in calls if c['args'][0] != 'nextest'], ['test'])

    def test_soak_runs_only_ignored_long_turn_evidence_with_real_workers(self):
        result, calls, _ = self.run_gate('--soak')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.profiles(calls), ['serial', 'durability'])
        self.assertEqual(
            self.lanes(calls)[0]['args'][-4:],
            ['--run-ignored', 'only', '-E', 'test(ledger_retirement_5000_)'],
        )
        self.assertEqual(
            self.lanes(calls)[1]['args'][-4:],
            ['--run-ignored', 'only', '-E', 'test(soak_ledger_retirement_)'],
        )
        self.assertEqual(
            [call['args'][0] for call in calls if call['args'][0] != 'nextest'], ['test']
        )

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

    def test_package_group_delegates_prefetch_and_offline_verification_in_a_git_tree(self):
        self.commit_fixture()
        result, calls, _ = self.run_gate('--steps', 'package')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.publish_calls(), [dict(publish=['--gate'], offline=None)])
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
        result, _, _ = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.evidence.exists())
        self.assertFalse(self.package_evidence.exists())
        # A passing package group alone records package evidence, never full.
        result, _, _ = self.run_gate('--steps', 'package')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.evidence.exists())
        self.assertEqual(self.package_evidence.read_text(), tree + '\n')
        self.assertIn(f'evidence: package group passed on tree {tree}', result.stdout)
        self.package_evidence.unlink()
        self.env['GATE_PUBLISH_FAILURE'] = '1'
        result, _, _ = self.run_gate('--full')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.evidence.exists())
        del self.env['GATE_PUBLISH_FAILURE']
        (self.root / 'untracked').write_text('x')
        result, _, _ = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('no evidence: evidence requires a clean git tree', result.stdout)
        self.assertFalse(self.evidence.exists())
        (self.root / 'untracked').unlink()
        result, _, _ = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.evidence.read_text(), tree + '\n')
        self.assertEqual(self.package_evidence.read_text(), tree + '\n')
