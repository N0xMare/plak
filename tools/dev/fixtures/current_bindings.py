"""Regenerate the synthetic Linux native-store binding seed with real presentations."""

from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile


def main() -> None:
    if sys.platform != 'linux':
        raise SystemExit('generate this Linux fixture on a Linux build checkout')
    root = Path(__file__).resolve().parents[3]
    tests = root / 'crates/hosting/hosted/src/agent/tests.rs'
    module = tests.with_suffix('') / 'current_binding_generator.rs'
    if module.exists():
        raise SystemExit('temporary generator module already exists; finish its owner first')
    original = tests.read_bytes()
    pin = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip()
    try:
        module.write_bytes((root / 'tools/dev/fixtures/previous_release.rs').read_bytes())
        tests.write_bytes(
            original + b'\n#[cfg(target_os = "linux")]\nmod current_binding_generator;\n'
        )
        with tempfile.TemporaryDirectory(prefix='current-bindings-') as temporary:
            output = Path(temporary) / 'fixture'
            subprocess.run(
                [
                    'just',
                    'nextest',
                    'run',
                    '--locked',
                    '-p',
                    'plakous-hosted',
                    '--all-features',
                    '--profile',
                    'offline',
                    '--run-ignored',
                    'only',
                    '-E',
                    'test(current_binding_generator::generate_previous_release_bindings)',
                ],
                cwd=root,
                check=True,
                env=dict(
                    os.environ,
                    PLAK_BINDING_FIXTURE_OUTPUT=str(output),
                    PLAK_BINDING_SOURCE_COMMIT=pin,
                ),
            )
            target = root / 'target/current-bindings'
            target.mkdir(parents=True, exist_ok=True)
            database = target / 'runtime.sqlite3'
            uri = (output / 'previous-runtime.sqlite3').as_uri() + '?mode=ro'
            # A connection context commits but does not close/checkpoint its WAL.
            with closing(sqlite3.connect(uri, uri=True)) as source:
                with closing(sqlite3.connect(database)) as destination:
                    source.backup(destination)
            data = database.read_bytes()
            if b'/home/' in data or b'/Users/' in data:
                raise SystemExit('generated fixture contains a private home path')
            manifest = json.loads((output / 'manifest.json').read_text())
            if len(manifest['cases']) != 17 or manifest['source_commit'] != pin:
                raise SystemExit('generated fixture identity or presentation count changed')
            manifest['sha256'] = hashlib.sha256(data).hexdigest()
            (target / 'manifest.json').write_text(
                json.dumps(manifest, indent=2, sort_keys=True) + '\n'
            )
            fixture = root / 'crates/hosting/hosted/tests/fixtures/current-bindings'
            for name in ['runtime.sqlite3', 'manifest.json']:
                shutil.copyfile(target / name, fixture / name)
            print(f"source_commit={pin} sha256={manifest['sha256']}", flush=True)
    finally:
        tests.write_bytes(original)
        module.unlink(missing_ok=True)


if __name__ == '__main__':
    main()
