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
    'rustdoc',
    'rustdoc-alone',
    'standalone',
    'contracts',
    'tooling',
    'tests-parallel',
    'tests-durability',
    'tests-serial',
    'tests-install',
)
BATCH_GROUPS = (
    'static',
    'standalone',
    'contracts',
    'tooling',
    'tests-parallel',
    'tests-durability',
    'tests-serial',
)
INSTALL_GROUPS = ('builds', 'rustdoc', 'tooling', 'tests-parallel', 'tests-install')
SUPPORTED = ('plakous', 'plakous-agent', 'plakous-model', 'plakous-tools', 'plakd')
CLIPPIES = ['clippy']
BUILDS = [
    ['build', '--locked', '-p', 'plakd', '--bins', '--target', 'host-triple'],
    [
        'build',
        '--locked',
        '-p',
        'plakd',
        '--features',
        'maintainer',
        '--bins',
        '--target',
        'host-triple',
    ],
    [
        'build',
        '--locked',
        '-p',
        'plakous-py-worker',
        '-p',
        'plakous-js-runtime',
        '--bins',
        '--target',
        'host-triple',
    ],
]


class DevCheckWorkerTests(unittest.TestCase):
    def test_batch_and_install_reuse_distinct_subsets_of_full(self):
        for tier, groups in (
            (('--install',), INSTALL_GROUPS),
            (('--batch',), BATCH_GROUPS),
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
                    [
                        line.split()[2]
                        for line in result.stdout.splitlines()
                        if line.startswith('== group:')
                        and line.split()[2] not in ('test-build', 'predecessor-build')
                    ],
                    list(groups),
                )
                calls = [s for s in scripts if s[0].endswith('build_predecessor.py')]
                self.assertEqual(len(calls), 2 if 'tests-install' in groups else 0)
                for checker in (
                    'check_workspace.py',
                    'check_features.py',
                    'check_public_compile.py',
                ):
                    self.assertEqual(
                        any(
                            s[0].endswith(checker)
                            and '--wasm-libraries' not in s
                            and '--standalone-packages' not in s
                            for s in scripts
                        ),
                        tier != ('--install',),
                    )

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / "tools/gate").mkdir(parents=True)
        for name in (
            "dev-check.sh",
            "dev-tools.sh",
            "dev-tools-prefix.sh",
            "test-tmpdir.sh",
            "test-env.sh",
            "clippy.sh",
            "dev-tools.pins",
            "mac-selection.filter",
        ):
            destination = self.root / "tools/gate" / name
            shutil.copyfile(ROOT / "tools/gate" / name, destination)
            destination.chmod(0o755)
        shutil.copyfile(
            ROOT / "tools/gate/test-processes.py", self.root / "tools/gate/test-processes.py"
        )
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "calls.jsonl"
        self.worker = self.root / "custom target" / "host-triple" / "debug" / "plakous-py-host"
        self.predecessor = self.root / 'predecessor images'
        self.predecessor.mkdir()
        # The gate probes the wasm target in the sysroot; other rustc calls are real.
        self.sysroot = self.root / 'sysroot'
        (self.sysroot / 'lib/rustlib/wasm32-unknown-unknown/lib').mkdir(parents=True)
        real_rustc = shutil.which('rustc')
        self.env = dict(
            os.environ,
            PATH=str(self.bin) + os.pathsep + os.environ['PATH'],
            GATE_CALLS=str(self.log),
            GATE_WORKER=str(self.worker),
            GATE_PREDECESSOR=str(self.predecessor),
            CARGO_TARGET_DIR=str(self.root / 'custom target'),
            CARGO_BUILD_TARGET='host-triple',
            GATE_SYSROOT=str(self.sysroot),
            GATE_REAL_RUSTC=real_rustc or '',
        )
        self.env.pop('PLAK_PY_HOST', None)
        self.env.pop('PLAK_JS_HOST', None)
        self.env.pop('PLAK_MCP_SERVER', None)
        self.env.pop('PLAK_TEST_BINARY', None)
        self.env.pop('PLAK_INSTALL_PREDECESSOR', None)
        self.env.pop('PLAK_NEXTEST_PARTITION', None)
        self.env.pop('PLAK_BATCH_BASE', None)
        self.env.pop('GATE_REAL_PROCESS_GUARD', None)
        self.env['CARGO_BUILD_JOBS'] = '8'
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
                             jobs=os.environ.get('CARGO_BUILD_JOBS'),
                             predecessor=os.environ.get('PLAK_INSTALL_PREDECESSOR'),
                             js_worker=os.environ.get('PLAK_JS_HOST'),
                             mcp_server=os.environ.get('PLAK_MCP_SERVER'),
                             test_binary=os.environ.get('PLAK_TEST_BINARY'),
                             target=os.environ.get('CARGO_BUILD_TARGET'),
                             target_dir=os.environ.get('CARGO_TARGET_DIR'),
                             plak_home=os.environ.get('PLAK_HOME'),
                             home_mode=(os.stat(os.environ['PLAK_HOME']).st_mode & 0o777
                                        if os.environ.get('PLAK_HOME') else None),
                             git_contents=(Path(os.environ['GIT_CONFIG_GLOBAL']).read_text()
                                           if os.environ.get('GIT_CONFIG_GLOBAL') else None),
                             rustdocflags=os.environ.get('RUSTDOCFLAGS'),
                             rustflags=os.environ.get('RUSTFLAGS'),
                             encoded_rustflags=os.environ.get('CARGO_ENCODED_RUSTFLAGS'),
                             rustc_wrapper=os.environ.get('RUSTC_WRAPPER'),
                             git_global=os.environ.get('GIT_CONFIG_GLOBAL'),
                             git_nosystem=os.environ.get('GIT_CONFIG_NOSYSTEM'),
                             target_rustflags=os.environ.get(
                                 'CARGO_TARGET_HOST_TRIPLE_RUSTFLAGS'))) + '\n')
if args[0] == 'test' and os.environ.get('GATE_TEST_BUILD_WAITS_FOR'):
    import time
    deadline = time.monotonic() + 5
    while not Path(os.environ['GATE_TEST_BUILD_WAITS_FOR']).exists():
        if time.monotonic() > deadline:
            raise SystemExit('test build ran alone')
        time.sleep(0.02)
if phases := os.environ.get('GATE_PREDECESSOR_PHASES'):
    phases = Path(phases)
    if args[0] == 'test':
        (phases / 'test-build-started').touch()
    if args[:2] == ['nextest', 'run'] and not (phases / 'predecessor-finished').exists():
        raise SystemExit('test lane started before the predecessor build finished')
if phases := os.environ.get('GATE_OVERLAP_PHASES'):
    phases = Path(phases)
    if args[0] == 'test':
        (phases / 'test-build').touch()
    if args[:2] == ['nextest', 'run']:
        profile = args[args.index('--profile') + 1]
        if profile == 'offline':
            (phases / 'offline').touch()
            raise SystemExit(8)
        if not (phases / 'tooling-finished').exists():
            raise SystemExit('timed lane started before tooling finished')
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
    mcp_server = worker.with_name('mcp-fake-server')
    mcp_server.write_text('#!/bin/sh\nexit 0\n')
    mcp_server.chmod(0o700)
    print(json.dumps(dict(reason='compiler-artifact', executable=str(mcp_server),
                         target=dict(name='mcp-fake-server', kind=['bin']), profile=dict(test=False))))
    print(json.dumps(dict(reason='compiler-artifact', executable=str(worker),
                         target=dict(name='plakd', kind=['bin']), profile=dict(test=False))))
''',
        )
        self.executable(
            'ruff',
            "import sys\nif '--version' in sys.argv:\n    print('ruff 0.17.3')\nraise SystemExit(0)\n",
        )
        self.executable('jj', "print('jj 0.46.0')\n")
        self.env.pop('PLAK_TEST_JJ', None)
        self.executable(
            'python3',
            r'''import json, os, sys, time
from pathlib import Path
if sys.argv[1].endswith('test-processes.py'):
    if os.environ.get('GATE_REAL_PROCESS_GUARD'):
        os.execv(sys.executable, [sys.executable, *sys.argv[1:]])
    # These cases exercise the driver's group selection, not the host process
    # table. No fixture can start a real product process through the fake Cargo.
    if sys.argv[2:] == ['--check']:
        raise SystemExit(0)
    if sys.argv[2:5] == ['--', 'cargo', 'nextest']:
        os.execvp('cargo', sys.argv[3:])
    raise SystemExit('unexpected process-guard invocation')
if sys.argv[1] == '-':
    os.execv(sys.executable, [sys.executable, *sys.argv[1:]])
with open(os.environ['GATE_CALLS'], 'a') as log:
    log.write(json.dumps(dict(python=sys.argv[1:], git_global=os.environ.get('GIT_CONFIG_GLOBAL'),
                              jobs=os.environ.get('CARGO_BUILD_JOBS'),
                              worker=os.environ.get('PLAK_PY_HOST'),
                              js_worker=os.environ.get('PLAK_JS_HOST'),
                              mcp_server=os.environ.get('PLAK_MCP_SERVER'),
                              test_binary=os.environ.get('PLAK_TEST_BINARY'),
                              tooling_binary=os.environ.get('PLAK_TOOLING_BINARY'),
                              target_dir=os.environ.get('CARGO_TARGET_DIR'),
                              rustflags=os.environ.get('RUSTFLAGS'),
                              encoded_rustflags=os.environ.get('CARGO_ENCODED_RUSTFLAGS'),
                              rustc_wrapper=os.environ.get('RUSTC_WRAPPER'),
                              target_rustflags=os.environ.get('CARGO_TARGET_HOST_TRIPLE_RUSTFLAGS'),
                              git_nosystem=os.environ.get('GIT_CONFIG_NOSYSTEM'))) + '\n')
if sys.argv[1].endswith('build_predecessor.py'):
    mode = os.environ.get('GATE_PREDECESSOR_MODE', 'miss')
    if '--check' in sys.argv and mode != 'hit':
        raise SystemExit(1)
    if phases := os.environ.get('GATE_PREDECESSOR_PHASES'):
        phases = Path(phases)
        (phases / 'predecessor-started').touch()
        deadline = time.monotonic() + 5
        while not (phases / 'test-build-started').exists():
            if time.monotonic() > deadline:
                raise SystemExit('predecessor build did not overlap test-build')
            time.sleep(0.02)
        (phases / 'predecessor-finished').touch()
    if mode != 'empty':
        print(os.environ['GATE_PREDECESSOR'])
    raise SystemExit(23 if mode == 'fail' else 0)
if sys.argv[1].endswith('check_public_compile.py') and os.environ.get('GATE_TEST_BUILD_WAITS_FOR'):
    open(os.environ['GATE_TEST_BUILD_WAITS_FOR'], 'w').close()
if phases := os.environ.get('GATE_OVERLAP_PHASES'):
    script = Path(sys.argv[1]).name
    phases = Path(phases)
    required = {'check_features.py': 'test-build', 'run_tooling_tests.py': 'offline'}.get(script)
    if required:
        deadline = time.monotonic() + 8
        while not (phases / required).exists():
            if time.monotonic() > deadline:
                raise SystemExit(f'{script} did not overlap {required}')
            time.sleep(0.02)
        print(f'{script} ran with {required}')
    if script == 'run_tooling_tests.py':
        (phases / 'tooling-finished').touch()
failure = json.loads(os.environ.get('GATE_PYTHON_FAILURES', '{}')).get(
    os.path.basename(sys.argv[1]), 0)
if failure:
    raise SystemExit(failure)
if sys.argv[1].endswith('changed_packages.py'):
    if '--dependents' in sys.argv:
        print(os.environ.get('GATE_DEPENDENTS', '\n'.join(sys.argv[3:])), end='')
    elif '--batch-filter' in sys.argv:
        print(os.environ.get('GATE_BATCH_FILTER', 'all()') if os.environ.get('PLAK_BATCH_BASE') else 'all()')
    else:
        print(os.environ.get('GATE_CHANGED', ''), end='')
if sys.argv[1].endswith('check_workspace.py') and '--wasm-libraries' in sys.argv:
    print('plakous-model')
if sys.argv[1].endswith('nextest_layout.py'):
    print(max(1, min(16, int(os.environ.get('GATE_AVAILABLE_PARALLELISM', '16')))))
if sys.argv[1].endswith('check_workspace.py') and '--standalone-packages' in sys.argv:
    print('plakous-model\nplakous-hosted')
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

    def executable(self, name, source):
        path = self.bin / name
        # These stand-ins import only the standard library. The dispatcher
        # still execs the real interpreter for scripts it does not mock.
        path.write_text('#!' + sys.executable + ' -S\n' + source)
        path.chmod(0o700)

    def run_gate(self, *args):
        result = subprocess.run(
            ['bash', str(self.root / 'tools/gate/dev-check.sh'), *args],
            env=self.env,
            text=True,
            capture_output=True,
            # Hang watchdog only. A full mock tier starts dozens of
            # subprocesses; its assertions concern results, not elapsed time.
            timeout=60,
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

    def test_builds_and_rustdoc_keep_the_compiler_wrapper(self):
        self.env['RUSTC_WRAPPER'] = 'sccache'
        result, calls, _ = self.run_gate('--steps', 'builds,rustdoc')
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [call for call in calls if call['args'][0] == 'build']
        self.assertEqual(len(builds), len(BUILDS))
        self.assertTrue(all(call['rustc_wrapper'] == 'sccache' for call in builds))
        rustdoc = next(call for call in calls if call['args'][0] == 'doc')
        self.assertEqual(rustdoc['rustc_wrapper'], 'sccache')

    def assert_nextest_home_is_isolated(self, calls):
        lanes = [c for c in calls if c['args'][:2] in (['nextest', 'run'], ['nextest', 'list'])]
        self.assertGreaterEqual(len(lanes), 1)
        for lane in lanes:
            home = lane['plak_home']
            self.assertTrue(
                home and home.endswith('/plak-isolated-home'),
                home,
            )
            mode = lane['home_mode']
            self.assertEqual(mode, 0o700, home)
        for call in calls:
            if call['args'][:2] in (['nextest', 'run'], ['nextest', 'list']):
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

    def test_batch_builds_once_and_passes_the_exact_artifacts_to_all_lanes(self):
        result, calls, _ = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        builds = [c for c in calls if c['args'][0] == 'test']
        self.assertEqual(len(builds), 1)
        self.assertEqual(builds[0]['target'], 'host-triple')
        self.assertEqual(builds[0]['target_dir'], self.env['CARGO_TARGET_DIR'])
        lanes = [c for c in calls if c['args'][:2] == ['nextest', 'run']]
        self.assertEqual(len(lanes), 3)
        self.assertTrue(all(c['worker'] == str(self.worker.resolve()) for c in lanes))
        self.assertTrue(all(c['test_binary'] == str(self.worker.resolve()) for c in lanes))
        self.assertTrue(
            all(
                c['mcp_server'] == str(self.worker.with_name('mcp-fake-server').resolve())
                for c in lanes
            )
        )
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
        self.env['PLAK_MCP_SERVER'] = str(self.worker.relative_to(self.root))
        self.env['PLAK_TEST_BINARY'] = str(self.worker.relative_to(self.root))
        result, calls, _ = self.run_gate('--steps', 'tests-parallel,tests-serial')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(c['args'][0] == 'test' for c in calls))
        lanes = [c for c in calls if c['args'][:2] == ['nextest', 'run']]
        self.assertEqual([c['worker'] for c in lanes], [str(self.worker.resolve())] * 2)
        self.assertEqual([c['js_worker'] for c in lanes], [str(self.worker.resolve())] * 2)
        self.assertEqual([c['mcp_server'] for c in lanes], [str(self.worker.resolve())] * 2)
        self.assertEqual([c['test_binary'] for c in lanes], [str(self.worker.resolve())] * 2)

    def test_built_mode_honors_relative_product_override(self):
        product = self.root / 'custom product'
        product.write_text('#!/bin/sh\nexit 0\n')
        product.chmod(0o700)
        self.env['PLAK_TEST_BINARY'] = product.name
        result, calls, _ = self.run_gate('--steps', 'tests-parallel')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(any(c['args'][0] == 'test' for c in calls))
        self.assertEqual([c['test_binary'] for c in self.lanes(calls)], [str(product.resolve())])

    def test_built_mode_rejects_invalid_product_override(self):
        ordinary = self.root / 'non-executable'
        ordinary.write_text('not executable')
        for value in ('', 'missing', str(self.root), str(ordinary)):
            with self.subTest(value=value):
                self.env['PLAK_TEST_BINARY'] = value
                self.log.unlink(missing_ok=True)
                result, calls, _ = self.run_gate('--steps', 'tests-parallel')
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('PLAK_TEST_BINARY', result.stderr)
                self.assertTrue(any(c['args'][0] == 'test' for c in calls))
                self.assertEqual(self.lanes(calls), [])

    def test_tooling_with_four_overrides_skips_build_and_normalizes_paths(self):
        product = self.root / 'custom product'
        product.write_text('#!/bin/sh\nexit 0\n')
        product.chmod(0o700)
        for variable in ('PLAK_PY_HOST', 'PLAK_JS_HOST', 'PLAK_MCP_SERVER', 'PLAK_TEST_BINARY'):
            self.env[variable] = product.name
        result, calls, _ = self.run_gate('--steps', 'tooling')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(c['args'][0] == 'test' for c in calls))
        tooling = [c for c in self.python_calls if c['python'][0].endswith('run_tooling_tests.py')]
        self.assertEqual(len(tooling), 1)
        for key in ('worker', 'js_worker', 'mcp_server', 'test_binary', 'tooling_binary'):
            self.assertEqual(tooling[0][key], str(product.resolve()))

    def test_invalid_mcp_fixture_override_fails_before_any_test_lane(self):
        self.worker.parent.mkdir(parents=True)
        self.worker.write_text('#!/bin/sh\nexit 0\n')
        self.worker.chmod(0o700)
        self.env['PLAK_PY_HOST'] = str(self.worker)
        self.env['PLAK_JS_HOST'] = str(self.worker)
        self.env['PLAK_TEST_BINARY'] = str(self.worker)
        for value in ('', 'missing', str(self.root)):
            with self.subTest(value=value):
                self.env['PLAK_MCP_SERVER'] = value
                self.log.unlink(missing_ok=True)
                result, calls, _ = self.run_gate('--steps', 'tests-parallel')
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('PLAK_MCP_SERVER', result.stderr)
                self.assertFalse(any(c['args'][:2] == ['nextest', 'run'] for c in calls))

    def test_invalid_product_fixture_override_fails_before_any_test_lane(self):
        self.worker.parent.mkdir(parents=True)
        self.worker.write_text('#!/bin/sh\nexit 0\n')
        self.worker.chmod(0o700)
        self.env['PLAK_PY_HOST'] = str(self.worker)
        self.env['PLAK_JS_HOST'] = str(self.worker)
        self.env['PLAK_MCP_SERVER'] = str(self.worker)
        for value in ('', 'missing', str(self.root)):
            with self.subTest(value=value):
                self.env['PLAK_TEST_BINARY'] = value
                self.log.unlink(missing_ok=True)
                result, calls, _ = self.run_gate('--steps', 'tests-parallel')
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('PLAK_TEST_BINARY', result.stderr)
                self.assertFalse(any(c['args'][:2] == ['nextest', 'run'] for c in calls))

    def test_invalid_override_fails_before_any_test_lane(self):
        directory = self.root / 'directory'
        directory.mkdir()
        ordinary = self.root / 'non-executable'
        ordinary.write_text('not executable')
        self.worker.parent.mkdir(parents=True)
        self.worker.write_text('#!/bin/sh\nexit 0\n')
        self.worker.chmod(0o700)
        self.env['PLAK_JS_HOST'] = str(self.worker)
        self.env['PLAK_MCP_SERVER'] = str(self.worker)
        self.env['PLAK_TEST_BINARY'] = str(self.worker)
        for value in ('', 'missing', str(directory), str(ordinary)):
            with self.subTest(value=value):
                self.env['PLAK_PY_HOST'] = value
                self.log.unlink(missing_ok=True)
                result, calls, _ = self.run_gate()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('PLAK_PY_HOST', result.stdout + result.stderr)
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
        self.env['PLAK_MCP_SERVER'] = str(self.worker)
        self.env['PLAK_TEST_BINARY'] = str(self.worker)
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
                'run_tooling_tests.py': 7,
            }
        )
        result, calls, scripts = self.run_gate('--steps', 'static,contracts,tooling')
        self.assertEqual(result.returncode, 1)
        phases = [c['args'][0] for c in calls if c['args'][0] != 'nextest']
        self.assertEqual(phases[0], 'fetch')
        self.assertCountEqual(phases[1:-1], ['fmt', *CLIPPIES])
        self.assertEqual(phases[-1], 'test')
        self.assertCountEqual(
            [Path(s[0]).name for s in scripts if s[0].endswith('.py')],
            [
                'check_workspace.py',
                'check_features.py',
                'check_public_compile.py',
                'run_tooling_tests.py',
            ],
        )
        self.assertIn(
            'failed static steps: fmt workspace features clippy',
            result.stdout,
        )
        self.assertIn('failed groups: static tooling', result.stdout)
        self.assertEqual(
            [
                line.split()[2]
                for line in result.stdout.splitlines()
                if line.startswith('== time: ')
            ],
            ['static', 'test-build', 'contracts', 'tooling'],
        )
        self.assertNotIn('gate passed', result.stdout)

    def test_batch_overlaps_checks_but_replays_independent_verdicts_in_order(self):
        self.env['PLAK_BATCH_BASE'] = 'test-base'
        self.env['GATE_OVERLAP_PHASES'] = str(self.root)
        self.env['GATE_PYTHON_FAILURES'] = json.dumps({'run_tooling_tests.py': 7})
        result, calls, _ = self.run_gate(
            '--steps', 'static,tooling,tests-parallel,tests-durability,tests-serial'
        )
        self.assertEqual(result.returncode, 7, result.stdout + result.stderr)
        self.assertIn('check_features.py ran with test-build', result.stdout)
        self.assertIn('run_tooling_tests.py ran with offline', result.stdout)
        self.assertIn('failed groups: tooling tests-parallel', result.stdout)
        self.assertEqual(
            [
                line.split()[2]
                for line in result.stdout.splitlines()
                if line.startswith('== group: ')
            ],
            [
                'static',
                'test-build',
                'tooling',
                'tests-parallel',
                'tests-durability',
                'tests-serial',
            ],
        )
        cargo = [c['args'][0] for c in calls]
        self.assertLess(cargo.index('clippy'), cargo.index('test'))

    def test_combined_batch_reuses_the_isolated_compile_target(self):
        self.env['PLAK_BATCH_BASE'] = 'test-base'
        result, _, _ = self.run_gate('--steps', 'static,standalone,tests-parallel')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        records = [json.loads(line) for line in self.log.read_text().splitlines()]
        features = next(
            row for row in records if row.get('python') == ['tools/gate/check_features.py']
        )
        standalone = [
            row for row in records if row.get('args', [])[:3] == ['clippy', '--locked', '-p']
        ]
        self.assertTrue(standalone)
        self.assertNotEqual(features['target_dir'], self.env['CARGO_TARGET_DIR'])
        for check in standalone:
            self.assertEqual(check['target_dir'], features['target_dir'])
            self.assertLess(records.index(features), records.index(check))

    def test_contracts_overlap_the_test_build_and_finish_before_tooling(self):
        # The fake test build waits for contracts, proving the overlap.
        self.env['GATE_TEST_BUILD_WAITS_FOR'] = str(self.root / 'contracts-started')
        result, calls, scripts = self.run_gate('--steps', 'static,rustdoc,contracts,tooling')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            [
                line.split()[2]
                for line in result.stdout.splitlines()
                if line.startswith('== time: ')
            ],
            ['static', 'rustdoc', 'test-build', 'contracts', 'tooling'],
        )
        self.assertEqual(scripts.count(['tools/gate/check_public_compile.py']), 1)
        # Tooling waits for the overlapping groups to end.
        self.assertLess(
            scripts.index(['tools/gate/check_public_compile.py']),
            next(
                i for i, script in enumerate(scripts) if script[0].endswith('run_tooling_tests.py')
            ),
        )

    def test_contracts_finish_before_the_test_lanes(self):
        result, calls, scripts = self.run_gate('--steps', 'tests-serial,contracts')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        records = [json.loads(line) for line in self.log.read_text().splitlines()]
        lane = max(i for i, row in enumerate(records) if row.get('args', [''])[0] == 'nextest')
        contracts = next(
            i
            for i, row in enumerate(records)
            if row.get('python') == ['tools/gate/check_public_compile.py']
        )
        self.assertLess(contracts, lane)

    def test_predecessor_miss_builds_once_with_candidate_then_joins_before_all_lanes(self):
        self.env['GATE_PREDECESSOR_PHASES'] = str(self.root)
        self.env['GATE_TEST_BUILD_WAITS_FOR'] = str(self.root / 'predecessor-started')
        result, calls, _ = self.run_gate('--steps', 'tests-parallel,tests-durability,tests-install')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.profiles(calls), ['offline', 'durability', 'install'])
        checks, builds = self.predecessor_calls()
        self.assertEqual(len(checks), 1)
        self.assertEqual(len(builds), 1)
        self.assertEqual(builds[0]['jobs'], '4')
        self.assertEqual(next(c['jobs'] for c in calls if c['args'][0] == 'test'), '4')
        self.assertEqual(
            [c['predecessor'] for c in self.lanes(calls)], [None, None, str(self.predecessor)]
        )

    def test_predecessor_hit_needs_no_build_or_candidate_jobs(self):
        self.env['GATE_PREDECESSOR_MODE'] = 'hit'
        result, calls, _ = self.run_gate('--steps', 'tests-install')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        checks, builds = self.predecessor_calls()
        self.assertEqual(len(checks), 1)
        self.assertEqual(builds, [])
        self.assertNotIn('== group: predecessor-build ==', result.stdout)
        self.assertEqual(next(c['jobs'] for c in calls if c['args'][0] == 'test'), '8')
        self.assertEqual([c['predecessor'] for c in self.lanes(calls)], [str(self.predecessor)])

    def test_missing_predecessor_result_fails_install_without_running_it(self):
        for mode, status in (('fail', 23), ('empty', 1)):
            with self.subTest(mode=mode):
                self.log.unlink(missing_ok=True)
                self.env['GATE_PREDECESSOR_MODE'] = mode
                result, calls, _ = self.run_gate('--steps', 'tests-serial,tests-install')
                self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                self.assertIn('tests-install requires the pinned predecessor build', result.stderr)
                self.assertEqual(self.profiles(calls), ['serial'])

    def predecessor_calls(self):
        calls = [c for c in self.python_calls if c['python'][0].endswith('build_predecessor.py')]
        return (
            [c for c in calls if '--check' in c['python']],
            [c for c in calls if '--check' not in c['python']],
        )

    def test_failed_test_lanes_do_not_skip_contracts_or_tooling(self):
        self.env['GATE_CARGO_FAILURES'] = json.dumps({'nextest': 8})
        result, calls, scripts = self.run_gate()
        self.assertEqual(result.returncode, 8)
        self.assertEqual(len(self.lanes(calls)), 3)
        self.assertEqual(sum(c['args'][0] == 'test' for c in calls), 1)
        for name in ('check_public_compile.py', 'run_tooling_tests.py'):
            self.assertTrue(any(s[0].endswith(name) for s in scripts))
        self.assertIn('failed groups: tests-parallel tests-durability tests-serial', result.stdout)
        self.assertNotIn('gate passed', result.stdout)

    def test_batch_uses_scope_without_install_builds(self):
        self.env['PLAK_BATCH_BASE'] = 'test-base'
        self.env['GATE_BATCH_FILTER'] = 'rdeps(=plakd) | package(=plakous-offline-crash-harness)'
        result, calls, scripts = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertCountEqual(
            [c['args'][0] for c in calls if c['args'][0] not in {'nextest'}],
            ['fmt', 'fetch', *CLIPPIES, 'clippy', 'clippy', 'check', 'test'],
        )
        self.assertEqual(
            [c['args'][0:8] for c in calls if c['args'][0] == 'clippy' and '-p' in c['args']],
            [
                [
                    'clippy',
                    '--locked',
                    '-p',
                    'plakous-model',
                    '--all-targets',
                    '--target',
                    'host-triple',
                    '--',
                ],
                [
                    'clippy',
                    '--locked',
                    '-p',
                    'plakous-hosted',
                    '--lib',
                    '--bins',
                    '--target',
                    'host-triple',
                ],
            ],
        )
        self.assertTrue(
            all('-D warnings' in c['rustflags'] for c in calls if self.is_wasm_check(c))
        )
        self.assertEqual(self.profiles(calls), ['offline', 'durability', 'serial'])
        self.assertEqual(self.filtersets(calls), [self.env['GATE_BATCH_FILTER'], None, None])
        for checker in ('check_public_compile.py', 'check_workspace.py', 'check_features.py'):
            self.assertTrue(any(s[0].endswith(checker) for s in scripts))
        self.assertIn(['tools/gate/run_tooling_tests.py', '-v', '--batch'], scripts)

    def test_batch_scoped_steps_do_not_add_install_but_can_explicitly_select_it(self):
        self.env['PLAK_BATCH_BASE'] = 'batch-base'
        for selection, profiles in (
            ('tests-serial', ['serial']),
            ('tests-serial,tests-install', ['serial', 'install']),
        ):
            with self.subTest(selection=selection):
                self.log.unlink(missing_ok=True)
                result, calls, _ = self.run_gate('--steps', selection)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.profiles(calls), profiles)
                self.assertEqual(self.filtersets(calls), [None] * len(profiles))
                checks, builds = self.predecessor_calls()
                self.assertEqual(
                    (len(checks), len(builds)), (1, 1) if 'install' in profiles else (0, 0)
                )

    def test_feature_builds_deny_warnings_on_the_flags_cargo_will_use(self):
        self.env['RUSTFLAGS'] = '-C link-arg=-fuse-ld=mold'
        result, calls, _ = self.run_gate('--steps', 'builds,tests-parallel')
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
        result, calls, _ = self.run_gate('--steps', 'builds,tests-parallel')
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

    def test_full_runs_both_doc_resolutions_before_tests(self):
        result, calls, _ = self.run_gate('--full')
        self.assertEqual(result.returncode, 0, result.stderr)
        docs = [c for c in calls if c['args'][0] == 'doc']
        self.assertEqual(len(docs), 1 + len(SUPPORTED))
        self.assertLess(calls.index(docs[-1]), calls.index(self.lanes(calls)[0]))
        self.assertTrue(all(doc['rustdocflags'] == '-D warnings' for doc in docs))
        self.assertIn('--workspace', docs[0]['args'])
        self.assertIn('--all-features', docs[0]['args'])
        self.assertEqual([doc['args'][2] for doc in docs[1:]], list(SUPPORTED))
        self.assertTrue(all(doc['target_dir'].endswith('/rustdoc-alone') for doc in docs[1:]))

    def test_install_runs_full_parallel_install_and_tooling_despite_a_batch_base(self):
        (self.root / '.config').mkdir()
        (self.root / '.config/nextest.toml').write_text('[profile.install]\n')
        self.env['GATE_BATCH_FILTER'] = 'none()'
        self.env['PLAK_BATCH_BASE'] = 'unused-batch-base'
        result, calls, scripts = self.run_gate('--install')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.profiles(calls), ['offline', 'install'])
        self.assertEqual(self.filtersets(calls), [None, None])
        self.assertEqual([call['args'] for call in calls if call['args'][0] == 'build'], BUILDS)
        self.assertNotIn('clippy', [call['args'][0] for call in calls])
        self.assertIn(['tools/gate/run_tooling_tests.py', '-v'], scripts)

    def test_install_checks_runs_all_four_lanes_without_batch_scoping(self):
        self.env['PLAK_BATCH_BASE'] = 'unused-batch-base'
        self.env['GATE_BATCH_FILTER'] = 'none()'
        result, calls, scripts = self.run_gate('--install-checks')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.profiles(calls), ['offline', 'durability', 'serial', 'install'])
        self.assertEqual(self.filtersets(calls), [None] * 4)
        self.assertFalse(
            any(s[0].endswith(('run_tooling_tests.py', 'changed_packages.py')) for s in scripts)
        )

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
        self.env['GATE_REAL_PROCESS_GUARD'] = '1'
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
                config = Path(call['git_global'])
                self.assertTrue(config.is_relative_to(scratch.resolve()), config)
                self.assertEqual(config.name, 'gitconfig')
                if call.get('args', [])[:2] == ['nextest', 'run']:
                    self.assertTrue(config.parent.name.startswith('r'), config)
                if call.get('args', [])[:2] == ['nextest', 'run']:
                    self.assertEqual(call['git_contents'], '')
                else:
                    self.assertEqual(config.read_text(), '')
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

    def test_missing_wasm_target_fails_standalone(self):
        shutil.rmtree(self.sysroot / 'lib/rustlib/wasm32-unknown-unknown')
        # An environment variable is not the container flag.
        self.env['PLAK_QUAL_CONTAINER'] = '1'
        result, calls, _ = self.run_gate('--steps', 'standalone')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('just dev-tools install', result.stdout + result.stderr)
        self.assertNotIn('skip wasm32-unknown-unknown:', result.stdout)
        self.assertNotIn(self.WASM_SKIP, result.stdout)
        self.assertIn('failed groups: standalone', result.stdout)
        self.assertFalse(any(self.is_wasm_check(call) for call in calls))

    def test_the_container_flag_runs_the_wasm_check_when_the_target_exists(self):
        result, calls, _ = self.run_gate('--qual-container', '--steps', 'standalone')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len([c for c in calls if self.is_wasm_check(c)]), 1)
        self.assertNotIn('skipped:', result.stdout)

    def test_musl_container_skips_a_missing_wasm_target_and_names_it_last(self):
        shutil.rmtree(self.sysroot / 'lib/rustlib/wasm32-unknown-unknown')
        result, calls, _ = self.run_gate('--qual-container', '--steps', 'standalone')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            'skip wasm32-unknown-unknown: the preloaded musl qualification image does not include this rustc target',
            result.stdout,
        )
        self.assertFalse(any(self.is_wasm_check(call) for call in calls))
        self.assertFalse(any(c['args'][0] == 'build' for c in calls))
        summary = result.stdout.splitlines()[-2:]
        self.assertEqual(summary, [self.WASM_SKIP, '== ok: steps gate passed =='])

    def test_fast_tier_defaults_to_touched_package_tests(self):
        self.env['GATE_CHANGED'] = 'plakous-model\nplakd\n'
        result, calls, scripts = self.run_gate('--fast')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(['tools/gate/changed_packages.py'], scripts)
        scope = 'package(=plakous-model) | package(=plakd)'
        self.assertEqual(self.profiles(calls), ['offline'])
        self.assertEqual(self.filtersets(calls), [scope])
        self.assertIn('--no-tests=pass', self.lanes(calls)[0]['args'])
        self.assertFalse(any(c['args'][0] == 'doc' for c in calls))

    def test_fast_tier_takes_explicit_packages_without_reading_the_diff(self):
        result, calls, scripts = self.run_gate('--fast', '--rdeps', 'plakous-store')
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
            [c['args'][0] for c in calls if c['args'][0] != 'nextest'], ['fetch', 'fmt']
        )
        self.assertEqual(self.lanes(calls), [])

    def test_unknown_argument_is_refused(self):
        for args in (('--quick',), ('--changed',), ('--full', 'plakd'), ('--platform', 'plakd')):
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
        self.assertIn(['tools/gate/check_workspace.py'], scripts)
        self.assertIn(['tools/gate/check_features.py'], scripts)
        self.assertEqual(self.lanes(calls), [])
        records = [json.loads(line) for line in self.log.read_text().splitlines()]
        fetch = next(i for i, row in enumerate(records) if row.get('args', [''])[0] == 'fetch')
        self.assertEqual(records[fetch]['args'], ['fetch', '--locked', '--offline'])
        # The tree checks and clippy only start after the fetch.
        after_fetch = records[fetch + 1 :]
        self.assertEqual(
            [row['python'] for row in after_fetch if 'python' in row],
            [
                ['tools/gate/check_workspace.py'],
                ['tools/gate/check_features.py'],
            ],
        )
        features = next(
            row for row in after_fetch if row.get('python') == ['tools/gate/check_features.py']
        )
        self.assertTrue(features['target_dir'].endswith('feature-check'), features)
        self.assertCountEqual(
            [row['args'][0] for row in after_fetch if 'args' in row], ['fmt', *CLIPPIES]
        )
        clippies = [row['args'] for row in records if row.get('args', [''])[0] == 'clippy']
        self.assertEqual(clippies[0][:3], ['clippy', '--locked', '--workspace'])
        self.assertIn('--all-features', clippies[0])
        self.assertIn('--all-targets', clippies[0])
        self.assertEqual(clippies[0][clippies[0].index('--') + 1 :], ['-D', 'warnings'])

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

    def test_clippy_failure_without_a_status_still_names_the_step(self):
        script = self.root / 'tools/gate/clippy.sh'
        script.write_text('#!/bin/bash\nexit 1\n')
        script.chmod(0o755)
        result, calls, _scripts = self.run_gate('--steps', 'static')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('== failed static steps: clippy ==', result.stdout)
        self.assertNotIn('clippy:packages', result.stdout)
        self.assertFalse(any(call['args'][0] == 'clippy' for call in calls))

    def test_full_runs_two_ignored_soaks(self):
        full, calls, _ = self.run_gate('--full')
        self.assertEqual(full.returncode, 0, full.stderr)
        self.assertEqual(
            self.profiles(calls),
            ['offline', 'durability', 'durability', 'serial', 'serial', 'install'],
        )
        soaks = [call for call in calls if '--run-ignored' in call['args']]
        self.assertEqual(
            [call['args'][-4:] for call in soaks],
            [
                ['--run-ignored', 'only', '-E', 'test(soak_ledger_retirement_)'],
                ['--run-ignored', 'only', '-E', 'test(ledger_retirement_5000_)'],
            ],
        )

    def test_explicit_rustdoc_selects_only_the_named_resolution(self):
        result, calls, _ = self.run_gate('--steps', 'rustdoc')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sum(call['args'][0] == 'doc' for call in calls), 1)
        self.log.unlink()
        result, calls, _ = self.run_gate('--steps', 'rustdoc-alone')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            [call['args'][2] for call in calls if call['args'][0] == 'doc'], list(SUPPORTED)
        )

    def test_explicit_shards_preserve_the_weighted_layout_without_a_list_pass(self):
        self.env['PLAK_NEXTEST_PARTITION'] = 'count:2/3'
        self.env['GATE_AVAILABLE_PARALLELISM'] = '12'
        self.env['PLAK_BATCH_BASE'] = 'test-base'
        self.env['GATE_BATCH_FILTER'] = 'rdeps(=plakd)'
        result, calls, _ = self.run_gate('--steps', 'tests-parallel,tests-serial,tests-durability')
        self.assertEqual(result.returncode, 0, result.stderr)
        offline, durability, serial = [call['args'] for call in self.lanes(calls)]
        for args in (offline, durability):
            self.assertEqual(args[args.index('--partition') + 1], 'count:2/3')
        self.assertEqual(offline[offline.index('-E') + 1], 'rdeps(=plakd)')
        self.assertNotIn('-E', durability)
        self.assertNotIn('--partition', serial)
        self.assertIn('--test-threads=12', durability)
        self.assertFalse(any(call['args'][:2] == ['nextest', 'list'] for call in calls))

    def test_partition_cannot_narrow_a_named_tier_or_a_nonshardable_selection(self):
        self.env['PLAK_NEXTEST_PARTITION'] = 'count:1/2'
        for args in ((), ('--full',), ('--soak',), ('--steps', 'tests-serial')):
            with self.subTest(args=args):
                result, calls, _ = self.run_gate(*args)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn('PLAK_NEXTEST_PARTITION requires --steps', result.stderr)
                self.assertEqual(calls, [])

    def test_mac_batch_runs_only_platform_clippy_and_filtered_lanes(self):
        self.env['PLAK_BATCH_BASE'] = 'test-base'
        self.env['GATE_BATCH_FILTER'] = 'rdeps(=plakd)'
        result, calls, scripts = self.run_gate('--batch-mac')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.profiles(calls), ['offline', 'durability', 'serial'])
        parallel, durability, serial = self.filtersets(calls)
        self.assertEqual(parallel, durability)
        self.assertNotEqual(parallel, serial)
        self.assertIn('binary(=deploy)', parallel)
        for expression in (parallel, serial):
            self.assertTrue(expression.endswith(' & (rdeps(=plakd))'), expression)
        self.assertEqual(
            [call['args'][0] for call in calls if call['args'][0] != 'nextest'],
            ['clippy', 'test'],
        )
        self.assertFalse(any(Path(script[0]).name.startswith('check_') for script in scripts))

    def test_named_batches_without_a_base_keep_all_packages(self):
        self.env['PLAK_FAST_BASE'] = 'unused-fast-base'
        self.env['GATE_BATCH_FILTER'] = 'none()'
        for tier in ('--batch', '--batch-mac'):
            with self.subTest(tier=tier):
                self.log.unlink(missing_ok=True)
                result, calls, _ = self.run_gate(tier)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                parallel = self.filtersets(calls)[0]
                if tier == '--batch':
                    self.assertEqual(parallel, 'all()')
                else:
                    self.assertTrue(
                        all(
                            expression.endswith(' & (all())')
                            for expression in self.filtersets(calls)
                        )
                    )

    def test_removed_tier_and_static_scope_are_rejected_before_building(self):
        for args in (('--static-scope', 'shared', '--steps', 'static'), ('--platform',)):
            with self.subTest(args=args):
                result, calls, _ = self.run_gate(*args)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn('unknown argument', result.stderr)
                self.assertEqual(calls, [])

    def test_worker_scope_lints_reverse_dependencies_and_keeps_global_checkers(self):
        self.env['GATE_DEPENDENTS'] = 'plakd\nplakous-hosted\n'
        result, calls, scripts = self.run_gate('--fast', 'plakous-hosted')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        lint = [call['args'] for call in calls if call['args'][0] == 'clippy']
        self.assertEqual(len(lint), 1)
        self.assertIn('--all-features', lint[0])
        self.assertIn('--all-targets', lint[0])
        self.assertNotIn('--workspace', lint[0])
        self.assertEqual(
            [lint[0][i + 1] for i, arg in enumerate(lint[0]) if arg == '-p'],
            ['plakd', 'plakous-hosted'],
        )
        self.assertIn(['tools/gate/check_workspace.py'], scripts)
        self.assertIn(
            ['tools/gate/check_features.py', '--library-packages', 'plakd', 'plakous-hosted'],
            scripts,
        )

    def test_steps_serial_only_runs_the_serial_lane_and_worker_prerequisite(self):
        result, calls, _ = self.run_gate('--steps', 'tests-serial')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.profiles(calls), ['serial'])
        width = 4 if os.uname().sysname == 'Linux' else 1
        self.assertIn(f'--test-threads={width}', self.lanes(calls)[0]['args'])
        self.assertNotIn('--run-ignored', self.lanes(calls)[0]['args'])
        self.assertEqual([c['args'][0] for c in calls if c['args'][0] != 'nextest'], ['test'])

    def test_soak_runs_only_ignored_long_turn_evidence_with_real_workers(self):
        result, calls, _ = self.run_gate('--soak')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.profiles(calls), ['durability', 'serial'])
        for lane in self.lanes(calls):
            self.assertIn('--test-threads=1', lane['args'])
        self.assertEqual(
            self.lanes(calls)[0]['args'][-4:],
            ['--run-ignored', 'only', '-E', 'test(soak_ledger_retirement_)'],
        )
        self.assertEqual(
            self.lanes(calls)[1]['args'][-4:],
            ['--run-ignored', 'only', '-E', 'test(ledger_retirement_5000_)'],
        )
        self.assertEqual(
            [call['args'][0] for call in calls if call['args'][0] != 'nextest'], ['test']
        )

    def test_bad_step_groups_fail_before_builds(self):
        for groups in ('', 'static,typo', ',static', 'static,', 'package'):
            with self.subTest(groups=groups):
                self.log.unlink(missing_ok=True)
                result, calls, _ = self.run_gate('--steps', groups)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertFalse(any(c['args'][0] != 'nextest' for c in calls))

    def test_wall_clock_lanes_pin_their_width_against_environment_overrides(self):
        self.env['GATE_AVAILABLE_PARALLELISM'] = '10'
        self.env['NEXTEST_TEST_THREADS'] = '999'
        result, calls, _ = self.run_gate('--steps', 'tests-serial,tests-durability')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.profiles(calls), ['durability', 'serial'])
        width = 4 if os.uname().sysname == 'Linux' else 1
        self.assertIn(f'--test-threads={width}', self.lanes(calls)[1]['args'])
        self.assertIn('--test-threads=10', self.lanes(calls)[0]['args'])
        self.assertIn('--config-file', self.lanes(calls)[0]['args'])
