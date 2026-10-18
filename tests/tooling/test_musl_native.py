"""Native musl prerequisites and target propagation, without invoking Rust."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class MuslNativeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / 'scripts').mkdir()
        for name in ('musl-native.sh', 'musl-toolchain.sh'):
            shutil.copyfile(ROOT / 'scripts' / name, self.root / 'scripts' / name)
        (self.root / 'scripts/dev-check.sh').write_text('''#!/bin/bash
python3 - "$@" <<'PY'
import json, os, sys
print(json.dumps(dict(args=sys.argv[1:], target=os.environ.get('CARGO_BUILD_TARGET'),
                     cc=os.environ.get('CC_x86_64_unknown_linux_musl'),
                     flags=os.environ.get('CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS'),
                     linker=os.environ.get('CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER'))))
PY
exit "${NATIVE_EXIT:-0}"
''')
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ['PATH'])
        for name in ('CC_x86_64_unknown_linux_musl',
                     'CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER', 'CARGO_BUILD_TARGET'):
            self.env.pop(name, None)
        self.env.pop('CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS', None)
        self.executable('uname', "import os, sys\nprint(os.environ.get('NATIVE_OS', 'Linux') if sys.argv[1] == '-s' else os.environ.get('NATIVE_ARCH', 'x86_64'))\n")
        self.executable('rustup', "import os\nprint(os.environ.get('NATIVE_TARGETS', 'x86_64-unknown-linux-musl'))\n")
        self.executable('musl-gcc', 'pass\n')

    def executable(self, name, source):
        path = self.bin / name
        path.write_text('#!' + sys.executable + '\n' + source)
        path.chmod(0o700)
        return str(path)

    def run_gate(self, *args):
        return subprocess.run(['bash', 'scripts/musl-native.sh', *args], cwd=self.root,
                              env=self.env, text=True, capture_output=True, timeout=15)

    def test_defaults_to_platform_with_explicit_target_and_path_compiler(self):
        result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), dict(args=['--platform'],
                         target='x86_64-unknown-linux-musl', cc='musl-gcc', linker='rust-lld',
                         flags=None))

    def test_environment_compiler_and_linker_are_preserved_and_tier_forwarded(self):
        cc = self.executable('custom-cc', 'pass\n')
        linker = self.executable('custom-linker', 'pass\n')
        self.env.update(CC_x86_64_unknown_linux_musl=cc,
                        CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=linker,
                        CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS='-C opt-level=1')
        result = self.run_gate('--fast', 'plak')
        self.assertEqual(result.returncode, 0, result.stderr)
        call = json.loads(result.stdout)
        self.assertEqual(call['args'], ['--fast', 'plak'])
        self.assertEqual((call['cc'], call['linker']), (cc, linker))
        self.assertEqual(call['flags'], '-C opt-level=1')

    def test_linker_defaults_to_toolchain_lld_and_failure_propagates(self):
        self.env['CC_x86_64_unknown_linux_musl'] = self.executable('custom-cc', 'pass\n')
        self.env['NATIVE_EXIT'] = '17'
        result = self.run_gate('--full')
        self.assertEqual(result.returncode, 17, result.stderr)
        call = json.loads(result.stdout)
        self.assertEqual(call['linker'], 'rust-lld')
        self.assertEqual(call['args'], ['--full'])

    def test_missing_target_compiler_linker_and_wrong_host_fail_clearly(self):
        for setting, value, message in (
            ('NATIVE_OS', 'Darwin', 'Linux x86_64'),
            ('NATIVE_ARCH', 'aarch64', 'Linux x86_64'),
            ('NATIVE_TARGETS', 'x86_64-unknown-linux-gnu', 'rustup target add x86_64-unknown-linux-musl'),
            ('CC_x86_64_unknown_linux_musl', 'missing-musl-cc', 'missing-musl-cc'),
            ('CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER', 'missing-linker', 'missing-linker'),
            ('CC_x86_64_unknown_linux_musl', '', 'CC_x86_64_unknown_linux_musl'),
            ('CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER', '', 'CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER'),
        ):
            with self.subTest(setting=setting):
                original = self.env.copy()
                self.env[setting] = value
                result = self.run_gate()
                self.assertEqual(result.returncode, 2)
                self.assertIn(message, result.stderr)
                self.assertEqual(result.stdout, '')
                self.env = original
