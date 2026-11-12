#!/usr/bin/env python3
"""Install the checksummed JJ compatibility binaries, only on explicit request."""

from __future__ import annotations

import hashlib
import io
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
import tempfile
import urllib.request


def _pins() -> dict[tuple[str, str], tuple[str, str]]:
    """(version, target) to (sha256, url) for every jj line of dev-tools.pins."""
    pins = {}
    path = Path(__file__).resolve().parents[1] / 'gate' / 'dev-tools.pins'
    for line in path.read_text().splitlines():
        fields = line.split()
        if fields and fields[0] == 'jj':
            _, version, target, checksum, url = fields
            pins[version, target] = checksum, url
    return pins


PINS = _pins()
VERSIONS = tuple(dict.fromkeys(version for version, _ in PINS))


def install(prefix: Path) -> None:
    triple = {
        ('Darwin', 'arm64'): 'aarch64-apple-darwin',
        ('Linux', 'x86_64'): 'x86_64-unknown-linux-musl',
    }.get((platform.system(), platform.machine()))
    if triple is None:
        raise RuntimeError('JJ compatibility binaries support Apple Silicon macOS and Linux x86_64')
    for version in VERSIONS:
        checksum, url = PINS[version, triple]
        archive = url.rsplit('/', 1)[1]
        with urllib.request.urlopen(url, timeout=60) as response:
            content = response.read(128 * 1024 * 1024 + 1)
        if len(content) > 128 * 1024 * 1024 or hashlib.sha256(content).hexdigest() != checksum:
            raise RuntimeError(f'JJ archive checksum mismatch: {url}')
        directory = prefix / 'jj' / version
        directory.mkdir(parents=True, exist_ok=True)
        with tarfile.open(fileobj=io.BytesIO(content), mode='r:gz') as tar:
            members = [m for m in tar.getmembers() if Path(m.name).name == 'jj' and m.isfile()]
            if len(members) != 1:
                raise RuntimeError(f'expected one JJ executable in {archive}')
            binary = tar.extractfile(members[0])
            assert binary is not None
            with tempfile.NamedTemporaryFile(dir=directory, delete=False) as temp:
                temp.write(binary.read())
                os.fchmod(temp.fileno(), 0o755)
            os.replace(temp.name, directory / 'jj')
        (directory / f'{archive}.sha256').write_text(checksum + '\n')
        print(f'installed {directory / "jj"}')


def compat(prefix: Path) -> None:
    root = Path(__file__).resolve().parents[2]
    script = root / 'tools' / 'gate' / 'test-env.sh'
    env = os.environ.copy()
    env.update(json.loads(subprocess.check_output([str(script), '--json'], text=True)))
    # test-env.sh unsets these only in a shell or a command it runs itself.
    for name in ('PLAK_TEST_SESSION_LEASE_SECONDS', 'PLAK_TEST_CELL_HELLO_MS'):
        env.pop(name, None)
    for version in VERSIONS:
        binary = (prefix / 'jj' / version / 'jj').resolve()
        if not binary.is_file():
            raise RuntimeError(f'missing {binary}; run just dev-tools install')
        env['PLAK_TEST_JJ'] = str(binary)
        env['PATH'] = str(binary.parent) + os.pathsep + os.environ['PATH']
        subprocess.run(
            [
                'cargo',
                'nextest',
                'run',
                '--locked',
                '--workspace',
                '--all-features',
                '--profile',
                'offline',
                '-E',
                'test(jj)',
            ],
            cwd=root,
            env=env,
            check=True,
        )
        subprocess.run(
            [sys.executable, '-m', 'unittest', 'tests/tooling/test_vcs_listing.py'],
            cwd=root,
            env=env,
            check=True,
        )


if __name__ == '__main__':
    command, prefix = sys.argv[1:]
    {'install': install, 'compat': compat}[command](Path(prefix))
