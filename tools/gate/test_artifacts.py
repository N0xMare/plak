"""Executable fixtures from one Cargo workspace test-build JSON capture."""

import json
import os
from pathlib import Path


def fixture_paths(manifest: str) -> dict[str, str]:
    records = [json.loads(line) for line in manifest.splitlines() if line.strip()]
    found = {}
    for variable, binary in (
        ("PLAK_PY_HOST", "plakous-py-host"),
        ("PLAK_JS_HOST", "plakous-js-host"),
        ("PLAK_MCP_SERVER", "mcp-fake-server"),
        ("PLAK_TEST_BINARY", "plakd"),
    ):
        paths = {
            row["executable"]
            for row in records
            if row.get("reason") == "compiler-artifact"
            and row.get("target", {}).get("name") == binary
            and "bin" in row.get("target", {}).get("kind", [])
            and row.get("executable")
            and not row.get("profile", {}).get("test")
        }
        if len(paths) != 1:
            raise SystemExit(f"test artifacts: expected exactly one built {binary} executable")
        try:
            path = Path(paths.pop()).resolve(strict=True)
        except OSError as error:
            raise SystemExit(f"{variable} requires a built executable: {error}") from error
        if not path.is_file() or not os.access(path, os.X_OK):
            raise SystemExit(f"{variable} requires a built executable: {path}")
        found[variable] = str(path)
    return found
