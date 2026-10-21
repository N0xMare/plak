"""Cell-local state, allowlisted harvest, and content-free secret refusals."""

from __future__ import annotations

import shlex


# This program runs in the cell, never on an operator's private job trees.
# No file contents or names are emitted on refusal.
HARVEST_SCRIPT = r'''
from pathlib import Path
import re
import base64
import shutil
import json

source = Path('/tmp/plak-cell/home')
target = Path('/logs/agent/plak-home')
staged = source.parent / 'harvest' / 'plak-home'
for name in ('state', 'logs'):
    if (source / name).is_dir():
        shutil.copytree(source / name, staged / name, dirs_exist_ok=True, symlinks=True)
secret_values = set()
def collect(value):
    if isinstance(value, dict):
        for key, item in value.items():
            if key in ('access_token', 'refresh_token', 'id_token', 'accessToken', 'refreshToken', 'key', 'apiKey', 'access', 'refresh'):
                if isinstance(item, str) and len(item) >= 16:
                    secret_values.add(item.encode())
            if key == "payload" and isinstance(item, str):
                try:
                    collect(json.loads(base64.b64decode(item, validate=True)))
                except (ValueError, UnicodeError):
                    pass
            collect(item)
    elif isinstance(value, list):
        for item in value:
            collect(item)
for private in (source / 'secrets', source.parent / 'xdg/plak/subscriptions',
                Path('/tmp/grok-secrets'), Path('/tmp/pi-secrets'),
                Path('/tmp/opencode-secrets'), Path('/tmp/codex-secrets'),
                Path('/tmp/codex-home'), Path('/tmp/claude-secrets')):
    for path in private.rglob('*'):
        if path.is_file() and path.name.endswith('.json'):
            try:
                collect(json.loads(path.read_text()))
            except (ValueError, UnicodeError):
                pass
        elif path.is_file() and path.name == 'openrouter.key':
            value = path.read_bytes().strip()
            if value:
                secret_values.add(value)
refused = False
flag = Path("/logs/agent/harvest.json")
try:
    previous = json.loads(flag.read_text())
    pattern_hits = bool(previous.get("generic_secret_pattern_hits"))
    refused = bool(previous.get("secrets_harvested"))
except (OSError, ValueError):
    pattern_hits = False
for root, path in ((root, path) for root in (staged.resolve(), Path('/logs').resolve()) for path in root.rglob('*')):
    if (path.name in ('auth.json', '.credentials.json', 'openrouter.key')
            or path.name == 'credential.json'
            or path.name in ('secrets', 'subscriptions')
            or re.fullmatch(r'exchange-\d+\.(?:request|body)', path.name)):
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        else:
            path.unlink(missing_ok=True)
        refused = True
        continue
    if path.is_symlink():
        # Even dangling links can expose credentials in a later re-harvest.
        if not path.resolve().is_relative_to(root) or not path.exists():
            path.unlink()
            refused = True
            continue
    if path.is_file():
        with path.open('rb') as stream:
            tail = b''
            while chunk := stream.read(1024 * 1024):
                data = tail + chunk
                if re.search(rb'sk-or-[A-Za-z0-9_-]{8,}|Bearer [A-Za-z0-9_.-]{16,}|"(?:access_token|refresh_token|refreshToken)"\s*:\s*"[^"\s]+"', data):
                    pattern_hits = True
                if any(value in data for value in secret_values):
                    path.unlink()
                    refused = True
                    break
                tail = data[-4096:]
if refused:
    flag.parent.mkdir(parents=True, exist_ok=True)
    flag.write_text(json.dumps({'secrets_harvested': True, 'generic_secret_pattern_hits': pattern_hits}))
    raise SystemExit('harvest refused: private artifacts removed')
if staged.is_dir():
    shutil.copytree(staged, target, dirs_exist_ok=True, symlinks=True)
Path('/logs/agent').mkdir(parents=True, exist_ok=True)
Path('/logs/agent/harvest.json').write_text(json.dumps({'secrets_harvested': False, 'generic_secret_pattern_hits': pattern_hits}))
'''


def harvest_command() -> str:
    return "python3 -c " + shlex.quote(HARVEST_SCRIPT)
