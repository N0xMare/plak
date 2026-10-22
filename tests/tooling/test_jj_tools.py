"""JJ compatibility tools are explicit, pinned and checksummed before install."""
import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('jj_tools', ROOT / 'scripts/jj-tools.py')
jj_tools = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(jj_tools)


class JjToolsTests(unittest.TestCase):
    def test_every_supported_minor_and_platform_has_a_checksum(self):
        self.assertEqual(jj_tools.VERSIONS, ('0.44.0', '0.45.0', '0.46.0'))
        self.assertEqual(len(jj_tools.PINS), 6)
        for checksum in jj_tools.PINS.values():
            self.assertRegex(checksum, r'^[0-9a-f]{64}$')

    def test_verified_regular_binary_is_the_only_archive_entry_installed(self):
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode='w:gz') as archive:
            member = tarfile.TarInfo('release/jj')
            member.size = 7
            archive.addfile(member, io.BytesIO(b'fixture'))
        content = stream.getvalue()
        with tempfile.TemporaryDirectory() as temporary, \
             patch.object(jj_tools.platform, 'system', return_value='Linux'), \
             patch.object(jj_tools.platform, 'machine', return_value='x86_64'), \
             patch.object(jj_tools, 'VERSIONS', ('0.46.0',)), \
             patch.dict(jj_tools.PINS, {('0.46.0', 'x86_64-unknown-linux-musl'): hashlib.sha256(content).hexdigest()}), \
             patch.object(jj_tools.urllib.request, 'urlopen', side_effect=lambda *a, **kw: io.BytesIO(content)) as fetch:
            prefix = Path(temporary)
            jj_tools.install(prefix)
            binary = prefix / 'jj/0.46.0/jj'
            self.assertEqual(binary.read_bytes(), b'fixture')
            self.assertEqual(binary.stat().st_mode & 0o777, 0o755)
            self.assertIn('/v0.46.0/jj-v0.46.0-x86_64-unknown-linux-musl.tar.gz', fetch.call_args.args[0])

    def test_checksum_mismatch_preserves_existing_binary(self):
        with tempfile.TemporaryDirectory() as temporary, \
             patch.object(jj_tools.platform, 'system', return_value='Linux'), \
             patch.object(jj_tools.platform, 'machine', return_value='x86_64'), \
             patch.object(jj_tools, 'VERSIONS', ('0.46.0',)), \
             patch.object(jj_tools.urllib.request, 'urlopen', return_value=io.BytesIO(b'wrong')):
            prefix = Path(temporary)
            binary = prefix / 'jj/0.46.0/jj'
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b'existing')
            with self.assertRaisesRegex(RuntimeError, 'checksum mismatch'):
                jj_tools.install(prefix)
            self.assertEqual(binary.read_bytes(), b'existing')

    def test_compat_requires_explicit_install(self):
        with tempfile.TemporaryDirectory() as temporary, patch.object(jj_tools.subprocess, 'check_output', return_value=temporary):
            with self.assertRaisesRegex(RuntimeError, 'run just dev-tools install'):
                jj_tools.compat(Path(temporary))

    def test_compat_selects_workspace_jj_tests_for_each_pinned_binary(self):
        calls = []
        with tempfile.TemporaryDirectory() as temporary, \
             patch.object(jj_tools.subprocess, 'check_output', return_value=temporary), \
             patch.object(jj_tools.subprocess, 'run', side_effect=lambda command, **kwargs: calls.append((command, dict(kwargs['env'])))):
            prefix = Path(temporary).resolve()
            for version in jj_tools.VERSIONS:
                binary = prefix / 'jj' / version / 'jj'
                binary.parent.mkdir(parents=True)
                binary.write_bytes(b'fixture')
            jj_tools.compat(prefix)
            self.assertEqual(len(calls), 6)
            for (command, env), version in zip(calls[::2], jj_tools.VERSIONS):
                self.assertIn('--workspace', command)
                self.assertIn('--all-features', command)
                self.assertEqual(command[-2:], ['-E', 'test(jj)'])
                self.assertEqual(env['PLAK_TEST_JJ'], str(prefix / 'jj' / version / 'jj'))
            for command, _ in calls[1::2]:
                self.assertEqual(command[-1], 'tests/tooling/test_vcs_listing.py')

    def test_library_pin_supported_range_and_archive_versions_stay_in_lockstep(self):
        import re
        import tomllib
        manifest = tomllib.loads((ROOT / 'crates/orchestration/workspace/Cargo.toml').read_text())
        pin = manifest['dependencies']['jj-lib']['version'].removeprefix('=')
        source = (ROOT / 'crates/orchestration/workspace/src/lib.rs').read_text()
        self.assertEqual(re.search(r'JJ_LIB_VERSION: &str = "([^"]+)"', source).group(1), pin)
        minor = int(pin.split('.')[1])
        low, high = map(int, re.search(r'JJ_CLI_SUPPORTED:.*?= (\d+)\.\.=([0-9]+)', source).groups())
        self.assertEqual((low, high), (minor - 2, minor))
        self.assertEqual(tuple(int(v.split('.')[1]) for v in jj_tools.VERSIONS), tuple(range(low, high + 1)))
