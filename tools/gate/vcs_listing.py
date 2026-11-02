"""Repository-root-checked Git/JJ listings shared by repository tooling."""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import shutil
import subprocess


def run(root: Path, *args: str) -> str:
    return subprocess.run(args, cwd=root, check=True, capture_output=True, text=True).stdout


def kind(root: Path) -> str:
    root = root.resolve()
    if (root / '.jj').is_dir():
        if shutil.which('jj'):
            return 'jj'
        if not (root / '.git').exists():
            raise RuntimeError('this is a jj workspace and jj is not installed; install jj or pass packages explicitly')
    if (root / '.git').exists():
        if Path(run(root, 'git', 'rev-parse', '--show-toplevel').strip()).resolve() != root:
            raise RuntimeError('git repository root does not match the requested root')
        return 'git'
    if any((parent / name).exists() for parent in root.parents for name in ('.git', '.jj')):
        raise RuntimeError('nested inside another repository but not at its root')
    return 'export'


def jj(root: Path, *args: str) -> str:
    return run(root, 'jj', '--no-pager', '--color=never', *args)


def listed_files(root: Path) -> list[str]:
    backend = kind(root)
    if backend == 'git':
        return sorted(set(filter(None, run(root, 'git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard').split('\0'))))
    if backend == 'jj':
        return sorted(set(filter(None, jj(root, 'file', 'list', '-r', '@', '-T', 'path ++ "\\0"').split('\0'))))
    files = []
    for directory, directories, names in os.walk(root):
        directories[:] = sorted(name for name in directories if name not in {'.git', '.jj', 'target'})
        files.extend((Path(directory) / name).relative_to(root).as_posix() for name in names)
    return sorted(files)


def changed_paths(root: Path, base: str | None = None) -> list[str]:
    backend = kind(root)
    if backend == 'export':
        raise RuntimeError('changed paths require a repository; pass packages explicitly')
    base = base or os.environ.get('PLAK_FAST_BASE')
    if backend == 'git':
        revision = run(root, 'git', 'rev-parse', '--verify', '--end-of-options', f'{base or "HEAD"}^{{commit}}').strip()
        paths = set(run(root, 'git', 'diff', '--name-only', '--no-renames', '-z', revision, '--').split('\0'))
        paths.update(run(root, 'git', 'ls-files', '--others', '--exclude-standard', '-z').split('\0'))
    else:
        if base is None:
            probe = subprocess.run(['jj', '--no-pager', '--color=never', 'log', '-r', 'plak_base()', '--no-graph', '-T', 'commit_id'], cwd=root, capture_output=True)
            base = 'plak_base()' if probe.returncode == 0 else None
        args = ('--from', base, '--to', '@') if base is not None else ('-r', 'builtin_immutable_heads()..@')
        paths = set(jj(root, 'diff', *args, '-T', 'self.source().path() ++ "\\0" ++ self.target().path() ++ "\\0"').split('\0'))
    for current, directories, names in os.walk(root):
        directories[:] = [name for name in directories if name not in {'.git', '.jj', '.plak', '.dev-tools', 'target'}]
        if 'proptest-regressions' in Path(current).relative_to(root).parts:
            paths.update((Path(current) / name).relative_to(root).as_posix() for name in names)
    return sorted(paths - {''})


def evidence_tree(root: Path) -> str:
    if not (root / '.git').is_dir():
        return ''
    if kind(root) == 'jj':
        if jj(root, 'log', '-r', '@', '--no-graph', '-T', 'conflict').strip() != 'false':
            raise RuntimeError('refusing gate evidence for conflicted @')
        commit = jj(root, 'log', '-r', '@', '--no-graph', '-T', 'commit_id').strip()
        return run(root, 'git', 'rev-parse', f'{commit}^{{tree}}').strip()
    if run(root, 'git', 'status', '--porcelain', '--untracked-files=normal').strip():
        return ''
    return run(root, 'git', 'rev-parse', 'HEAD^{tree}').strip()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['kind', 'evidence-tree'])
    parser.add_argument('root', nargs='?', type=Path, default=Path.cwd())
    args = parser.parse_args()
    print(kind(args.root) if args.command == 'kind' else evidence_tree(args.root))
