"""PATH fixtures that hide jj without hiding tools installed beside it."""

import os
from pathlib import Path


def path_without_jj(path: str, directory: Path) -> str:
    entries = []
    for index, entry in enumerate(path.split(os.pathsep)):
        if not os.access(Path(entry) / "jj", os.X_OK):
            entries.append(entry)
            continue
        filtered = directory / str(index)
        filtered.mkdir(parents=True)
        for binary in Path(entry).iterdir():
            if binary.name != "jj":
                # Keep the command name for BusyBox applets; do not resolve it.
                (filtered / binary.name).symlink_to(binary.absolute())
        entries.append(str(filtered))
    return os.pathsep.join(entries)
