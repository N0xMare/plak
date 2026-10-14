"""The musl recipe isolates source and runs the pinned offline target gate."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class MuslCheckTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        (self.root / 'scripts').mkdir()
        script = ROOT / 'scripts/musl-check.sh'
        if script.exists():
            shutil.copyfile(script, self.root / 'scripts/musl-check.sh')
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.tools = self.root / 'musl tools'
        self.tools.mkdir()
        worker = self.tools / 'cargo-nextest'
        worker.write_text('#!/bin/sh\nexit 0\n')
        worker.chmod(0o700)
        self.log = self.root / 'docker.json'
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ['PATH'],
                        PLAK_MUSL_TOOLS=str(self.tools), MUSL_CALLS=str(self.log))
        self.env.pop('PLAK_MUSL_TARGET_DIR', None)
        self.env.pop('CARGO_BUILD_JOBS', None)
        self.executable('getconf', "print('24')\n")
        self.executable('uname', "import sys\nprint('Linux' if sys.argv[1] == '-s' else 'x86_64')\n")
        self.executable('docker', r'''import json, os, sys
from pathlib import Path
args = sys.argv[1:]
source = next(arg[:-len(':/qual/src')] for arg in args if arg.endswith(':/qual/src'))
root = Path(source)
Path(os.environ['MUSL_CALLS']).write_text(json.dumps(dict(
    args=args, files=sorted(str(path.relative_to(root)) for path in root.rglob('*')
                           if path.is_file()), payload=(root / 'payload').read_text())))
raise SystemExit(int(os.environ.get('MUSL_EXIT', '0')))
''')
        self.git('init', '-q')
        (self.root / '.gitignore').write_text('target/\n.dev-tools/\nbin/\ndocker.json\n')
        (self.root / 'payload').write_text('baseline')
        (self.root / 'deleted').write_text('gone')
        self.git('add', '.')
        self.git('-c', 'user.name=Tooling Test', '-c', 'user.email=test@example.invalid',
                 'commit', '-qm', 'fixture')
        (self.root / 'payload').write_text('working tree')
        (self.root / 'deleted').unlink()
        (self.root / 'untracked').write_text('new source')
        (self.root / '.dev-tools').mkdir()
        (self.root / '.dev-tools/cargo-nextest').write_text('host binary')

    def git(self, *args):
        subprocess.run(['git', *args], cwd=self.root, check=True, capture_output=True)

    def executable(self, name, source):
        path = self.bin / name
        path.write_text('#!' + sys.executable + '\n' + source)
        path.chmod(0o700)

    def run_gate(self, *args):
        result = subprocess.run(['bash', 'scripts/musl-check.sh', *args], cwd=self.root,
                                env=self.env, text=True, capture_output=True, timeout=15)
        call = json.loads(self.log.read_text()) if self.log.exists() else None
        return result, call

    def test_default_is_full_gate_in_pinned_offline_musl_image(self):
        result, call = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        args = call['args']
        for option, value in (('--platform', 'linux/amd64'), ('--pull', 'never'),
                              ('--network', 'none')):
            self.assertEqual(args[args.index(option) + 1], value)
        self.assertIn('plak-qual-base:185eaa28', args)
        command = args[args.index('-c') + 1]
        self.assertIn('CARGO_BUILD_TARGET=x86_64-unknown-linux-musl', command)
        self.assertIn('CARGO_NET_OFFLINE=true', command)
        self.assertIn('bash scripts/dev-check.sh "$@"', command)
        self.assertEqual(args[-2:], ['sh', '--full'])
        self.assertIn('EXIT 0', result.stdout)

    def test_export_preserves_diff_and_excludes_deleted_ignored_and_git_files(self):
        result, call = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(call['payload'], 'working tree')
        self.assertIn('untracked', call['files'])
        self.assertNotIn('deleted', call['files'])
        self.assertFalse(any(path.startswith(('.git/', '.dev-tools/', 'target/'))
                             for path in call['files']))

    def test_requested_tier_is_forwarded(self):
        result, call = self.run_gate('--fast', 'plak')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(call['args'][-3:], ['sh', '--fast', 'plak'])

    def test_container_uses_host_identity_and_writable_offline_home(self):
        result, call = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        args = call['args']
        self.assertEqual(args[args.index('--user') + 1], f'{os.getuid()}:{os.getgid()}')
        command = args[args.index('-c') + 1]
        self.assertIn('HOME=', command)
        self.assertIn('CARGO_HOME=', command)
        self.assertIn('RUSTUP_HOME=', command)
        self.assertIn('registry', command)

    def test_target_and_musl_tools_mounts_are_explicit(self):
        target = self.root / 'custom target'
        self.env['PLAK_MUSL_TARGET_DIR'] = str(target)
        result, call = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(str(target) + ':/qual/target', call['args'])
        self.assertIn(str(self.tools) + ':/qual/tools:ro', call['args'])
        self.assertIn('/dev/shm:rw,exec,size=16g', call['args'])

    def test_container_failure_is_reported_and_propagated(self):
        self.env['MUSL_EXIT'] = '17'
        result, _ = self.run_gate()
        self.assertEqual(result.returncode, 17, result.stderr)
        self.assertIn('EXIT 17', result.stdout)

    def test_timing_ledger_is_beside_artifacts_on_the_writable_mount(self):
        result, call = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        command = call['args'][call['args'].index('-c') + 1]
        target = next(word.split('=', 1)[1] for word in command.split()
                      if word.startswith('CARGO_TARGET_DIR='))
        ledger = Path(target).parent / '.gate-times.jsonl'
        self.assertTrue(ledger.is_relative_to('/qual/target'), str(ledger))
        mount = self.root / 'target/musl-gate'
        self.assertIn(str(mount) + ':/qual/target', call['args'])
        local_ledger = mount / ledger.relative_to('/qual/target')
        local_ledger.write_text('timing sample\n')
        self.assertEqual(local_ledger.read_text(), 'timing sample\n')

    def test_jobs_default_to_host_cores_and_allow_override(self):
        result, call = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('CARGO_BUILD_JOBS=24', call['args'])
        self.env['CARGO_BUILD_JOBS'] = '3'
        result, call = self.run_gate('--platform')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('CARGO_BUILD_JOBS=3', call['args'])
        self.assertEqual(call['args'][-2:], ['sh', '--platform'])

    def test_cargo_home_is_persistent_and_seeded_only_once_per_image(self):
        for _ in range(2):
            result, call = self.run_gate()
            self.assertEqual(result.returncode, 0, result.stderr)
            # The cache directory is keyed on the same image reference the
            # container runs, so an image bump never merges old caches.
            image = next(arg for arg in call['args'] if arg.startswith('plak-qual-base:'))
            tag = image.split(':', 1)[1]
            self.assertIn(str(self.root / f'target/musl-gate/cargo-home-{tag}')
                          + ':/qual/cargo-home', call['args'])
            command = call['args'][call['args'].index('-c') + 1]
            self.assertIn('CARGO_HOME=/qual/cargo-home', command)
            self.assertIn('if [ ! -f "$CARGO_HOME/.seeded" ]', command)
            self.assertIn('touch "$CARGO_HOME/.seeded"', command)

        image_cargo = self.root / 'image-cargo'
        (image_cargo / 'registry').mkdir(parents=True)
        (image_cargo / 'registry' / 'payload').write_text('image seed')
        cargo_home = self.root / f'target/musl-gate/cargo-home-{tag}'
        seed = command.split('    cd /qual/src')[0]
        seed = seed.replace('/qual/cargo-home', str(cargo_home))
        seed = seed.replace('/usr/local/cargo', str(image_cargo))
        seed = seed.replace('/tmp/plak-musl-home.XXXXXX', str(self.root / 'home.XXXXXX'))
        for _ in range(2):
            result = subprocess.run(['/bin/sh', '-c', seed], env=self.env, text=True,
                                    capture_output=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((cargo_home / 'registry' / 'payload').read_text(), 'image seed')
            (image_cargo / 'registry' / 'payload').write_text('must not recopy')
        self.assertTrue((cargo_home / '.seeded').is_file())

    def test_public_contract_cache_is_on_the_persistent_mount_and_image_keyed(self):
        for _ in range(2):
            result, call = self.run_gate()
            self.assertEqual(result.returncode, 0, result.stderr)
            image = next(arg for arg in call['args'] if arg.startswith('plak-qual-base:'))
            tag = image.split(':', 1)[1]
            self.assertIn(f'PLAK_PUBLIC_CONTRACTS_DIR=/qual/target/public-contracts-{tag}',
                          call['args'])
            self.assertIn(str(self.root / 'target/musl-gate') + ':/qual/target', call['args'])

    def test_missing_musl_nextest_refuses_before_docker(self):
        (self.tools / 'cargo-nextest').unlink()
        result, call = self.run_gate()
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(call)
        self.assertIn('cargo-nextest', result.stderr)
