#!/usr/bin/env python3
"""Install the checksummed JJ compatibility binaries, only on explicit request."""
from __future__ import annotations

import hashlib
import io
import os
from pathlib import Path
import platform
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

PINS = {
    ('0.44.0', 'aarch64-apple-darwin'): '22b92ed109378a9638f0ae55ca7a7bdc9ef26aa60124215a1f04f6808623ba94',
    ('0.44.0', 'x86_64-unknown-linux-musl'): '0a07bab4641a55fd2bc2fd1563ba3a3f9a577584086ad74086a1c5b69b3ffce9',
    ('0.45.0', 'aarch64-apple-darwin'): '4eb739e998fed3dd9784db86349ad60133494d71e7f4e100907711dcad19c058',
    ('0.45.0', 'x86_64-unknown-linux-musl'): 'ce25abe9192fbdc2fe0b112f883cf976e8646f556ba60f3f0947aacd8714ded3',
    ('0.46.0', 'aarch64-apple-darwin'): '43daa37ce0f9716cdf6fb65a29cbcae74a3160aef048cf3cb3d166d41cb4ab58',
    ('0.46.0', 'x86_64-unknown-linux-musl'): 'fce0271158e665ceb82dc66c5eb95b1728649094e9133074f3b5baece345fc08',
}
VERSIONS = ('0.44.0', '0.45.0', '0.46.0')


def install(prefix: Path) -> None:
    triple = {('Darwin', 'arm64'): 'aarch64-apple-darwin', ('Linux', 'x86_64'): 'x86_64-unknown-linux-musl'}.get((platform.system(), platform.machine()))
    if triple is None:
        raise RuntimeError('JJ compatibility binaries support Apple Silicon macOS and Linux x86_64')
    for version in VERSIONS:
        archive = f'jj-v{version}-{triple}.tar.gz'
        url = f'https://github.com/jj-vcs/jj/releases/download/v{version}/{archive}'
        with urllib.request.urlopen(url, timeout=60) as response:
            content = response.read(128 * 1024 * 1024 + 1)
        if len(content) > 128 * 1024 * 1024 or hashlib.sha256(content).hexdigest() != PINS[version, triple]:
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
        (directory / f'{archive}.sha256').write_text(PINS[version, triple] + '\n')
        print(f'installed {directory / "jj"}')


def compat(prefix: Path) -> None:
    root = Path(__file__).resolve().parents[1]
    env = os.environ.copy()
    env['TMPDIR'] = subprocess.check_output(['bash', 'scripts/test-tmpdir.sh'], cwd=root, text=True).strip()
    env['PLAK_HOME'] = str(Path(env['TMPDIR']) / 'plak-isolated-home')
    Path(env['PLAK_HOME']).mkdir(mode=0o700, exist_ok=True)
    for version in VERSIONS:
        binary = (prefix / 'jj' / version / 'jj').resolve()
        if not binary.is_file():
            raise RuntimeError(f'missing {binary}; run just dev-tools install')
        env['PLAK_TEST_JJ'] = str(binary)
        env['PATH'] = str(binary.parent) + os.pathsep + os.environ['PATH']
        subprocess.run(['cargo', 'nextest', 'run', '--locked', '--workspace', '--all-features', '--profile', 'offline', '-E', 'test(jj)'], cwd=root, env=env, check=True)
        subprocess.run([sys.executable, '-m', 'unittest', 'tests/tooling/test_vcs_listing.py'], cwd=root, env=env, check=True)


if __name__ == '__main__':
    command, prefix = sys.argv[1:]
    {'install': install, 'compat': compat}[command](Path(prefix))
