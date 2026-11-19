"""Update the two public install commands before qualifying a stable release."""

import argparse
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
GUIDES = ("README.md", "docs/user-guide/getting-started.md")
BASE = "https://raw.githubusercontent.com/N0xMare/plak"
VERSION = r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
COMMAND = re.compile(
    rf"^{re.escape('curl -fsSL ' + BASE)}/v(?P<version>{VERSION})/install\.sh"
    r" \| PLAK_VERSION=(?P=version) sh$",
    re.MULTILINE,
)


def update(root: Path, version: str) -> None:
    if not re.fullmatch(VERSION, version):
        raise ValueError("expected a stable release version, such as 0.0.5")
    replacement = f"curl -fsSL {BASE}/v{version}/install.sh | PLAK_VERSION={version} sh"
    pending = []
    for name in GUIDES:
        path = root / name
        before = path.read_text()
        after, count = COMMAND.subn(replacement, before)
        if count != 1:
            raise ValueError(f"{name}: expected exactly one tagged installer command")
        pending.append((path, before, after))
    for path, before, after in pending:
        if before != after:
            path.write_text(after)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    args = parser.parse_args()
    try:
        update(ROOT, args.version)
    except (OSError, ValueError) as error:
        parser.exit(1, f"pin-installer: {error}\n")


if __name__ == "__main__":
    main()
