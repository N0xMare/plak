"""Git/JJ tooling cannot silently inspect a containing checkout."""
from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tools/checks'))
import vcs_listing as vcs


class ListingTests(unittest.TestCase):
    def test_nested_repository_is_never_an_empty_fallback(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / '.git').mkdir()
            child = root / 'copy'
            child.mkdir()
            for operation in (vcs.kind, vcs.listed_files, vcs.changed_paths):
                with self.assertRaisesRegex(RuntimeError, 'nested inside'):
                    operation(child)

    def test_jj_without_cli_refuses_secondary_and_checks_colocated_git(self):
        with tempfile.TemporaryDirectory() as temporary, patch.object(shutil, 'which', return_value=None):
            root = Path(temporary)
            (root / '.jj').mkdir()
            with self.assertRaisesRegex(RuntimeError, 'jj is not installed'):
                vcs.listed_files(root)
            subprocess.run(['git', 'init', '-q', str(root)], check=True)
            (root / 'visible').write_text('visible')
            self.assertEqual(vcs.kind(root), 'git')
            self.assertEqual(vcs.listed_files(root), ['visible'])

    def test_export_omits_vcs_metadata_and_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'target').mkdir()
            (root / 'target/no').write_text('no')
            (root / 'yes').write_text('yes')
            self.assertEqual(vcs.listed_files(root), ['yes'])
            with self.assertRaisesRegex(RuntimeError, 'require a repository'):
                vcs.changed_paths(root)

    @unittest.skipUnless(shutil.which('jj'), 'jj is not installed')
    def test_jj_secondary_listing_and_changed_stack_are_copy_local(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, {'JJ_CONFIG': '/dev/null'}):
            os.environ.pop('PLAK_FAST_BASE', None)
            root = Path(temporary)
            env = os.environ | {'JJ_CONFIG': '/dev/null'}
            def jj(cwd, *args):
                return subprocess.check_output(['jj', '--no-pager', '--config', 'user.name=test', '--config', 'user.email=test@example.invalid', *args], cwd=cwd, env=env, text=True)
            jj(root, 'git', 'init', '--colocate')
            (root / '.gitignore').write_text('.plak/\n')
            (root / 'seed').write_text('seed')
            jj(root, 'commit', '-m', 'seed')
            (root / 'stack').write_text('stack')
            jj(root, 'commit', '-m', 'stack')
            # The immutable-heads range includes completed unpushed commits.
            self.assertIn('stack', vcs.changed_paths(root))
            child = root / '.plak/workspaces/child'
            child.parent.mkdir(parents=True)
            jj(root, 'workspace', 'add', str(child))
            (root / 'parent-only').write_text('parent')
            (child / 'child-only').write_text('child')
            self.assertIn('child-only', vcs.listed_files(child))
            self.assertNotIn('parent-only', vcs.listed_files(child))
            self.assertEqual(vcs.evidence_tree(child), '')
            self.assertTrue(vcs.evidence_tree(root))
            self.assertIn('child-only', vcs.changed_paths(child))
            self.assertNotIn('parent-only', vcs.changed_paths(child))

    def test_subprocess_errors_propagate(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / '.git').mkdir()
            with self.assertRaises(subprocess.CalledProcessError):
                vcs.listed_files(root)

    def test_evidence_refuses_conflicted_jj(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / '.git').mkdir()
            with patch.object(vcs, 'kind', return_value='jj'), patch.object(vcs, 'jj', return_value='true'):
                with self.assertRaisesRegex(RuntimeError, 'conflicted @'):
                    vcs.evidence_tree(root)

    def test_git_ignored_regressions_outside_crates_are_inputs(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            subprocess.run(['git', 'init', '-q', str(root)], check=True)
            subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@example.invalid', '-C', str(root), 'commit', '--allow-empty', '-qm', 'base'], check=True)
            (root / '.gitignore').write_text('proptest-regressions/\n')
            seed = root / 'bin/plak/proptest-regressions/seed.txt'
            seed.parent.mkdir(parents=True)
            seed.write_text('seed')
            self.assertIn('bin/plak/proptest-regressions/seed.txt', vcs.changed_paths(root))

    @unittest.skipUnless(shutil.which('jj'), 'jj is not installed')
    def test_jj_renames_include_both_package_paths_and_newlines(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, {'JJ_CONFIG': '/dev/null'}):
            root = Path(temporary)
            def jj(*args):
                return subprocess.check_output(['jj', '--no-pager', '--config', 'user.name=t', '--config', 'user.email=t@example.invalid', *args], cwd=root, text=True)
            jj('git', 'init')
            old = root / 'old\npackage/source.rs'
            old.parent.mkdir()
            old.write_text('same source\n' * 100)
            jj('commit', '-m', 'base')
            new = root / 'new\npackage/source.rs'
            new.parent.mkdir()
            old.rename(new)
            self.assertEqual(vcs.changed_paths(root, '@-'), ['new\npackage/source.rs', 'old\npackage/source.rs'])
