"""Qualify frozen native heads through the matching predecessor's v12 exporter."""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

SOURCE = '9e9785efe0c6549ee8d1c143b81db00472031dab'


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target-dir', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[3]
    output = args.output.resolve()
    if output.exists():
        parser.error('output must not exist; review before replacing a fixture')
    target = args.target_dir.resolve()
    if target == Path(os.environ.get('CARGO_TARGET_DIR', root / 'target')).resolve():
        parser.error('the predecessor must have a separate target directory')
    with tempfile.TemporaryDirectory(prefix='bootstrap-bindings-') as temporary:
        scratch = Path(temporary)
        source = scratch / 'source'
        subprocess.run(
            ['git', 'worktree', 'add', '--detach', str(source), SOURCE],
            cwd=root,
            check=True,
        )
        try:
            tests = source / 'bin/plakd/src/carry/tests.rs'
            module = tests.with_suffix('') / 'bootstrap_exports.rs'
            module.parent.mkdir(exist_ok=True)
            shutil.copyfile(root / 'tools/dev/fixtures/bootstrap_exports.rs', module)
            with tests.open('a') as writer:
                writer.write('\nmod bootstrap_exports;\n')
            environment = dict(
                os.environ,
                CARGO_TARGET_DIR=str(target),
                PLAK_BOOTSTRAP_EXPORT_OUTPUT=str(scratch / 'exports'),
                PLAK_BOOTSTRAP_SOURCE_COMMIT=SOURCE,
            )
            subprocess.run(
                [
                    'just',
                    'nextest',
                    'run',
                    '--locked',
                    '-p',
                    'plakd',
                    '--all-features',
                    '--profile',
                    'offline',
                    '--run-ignored',
                    'only',
                    '-E',
                    'test(export_bootstrap_frozen_heads)',
                ],
                cwd=source,
                env=environment,
                check=True,
            )
            exports = scratch / 'exports'
            bundle = json.loads((exports / 'manifest.json').read_text())
            fields = {
                'authority',
                'locator',
                'provider',
                'model',
                'composition',
                'child',
                'platform',
                'production_native',
            }
            for item in bundle['exports']:
                item['document'] = json.loads((exports / item['document']).read_text())
                item['case'] = {k: v for k, v in item['case'].items() if k in fields}
            if bundle['producer'] != SOURCE or len(bundle['exports']) != 17:
                raise SystemExit('unexpected producer or presentation count')
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_text(json.dumps(bundle, indent=2, sort_keys=True) + '\n')
        finally:
            subprocess.run(
                ['git', 'worktree', 'remove', '--force', str(source)],
                cwd=root,
                check=True,
            )


if __name__ == '__main__':
    main()
