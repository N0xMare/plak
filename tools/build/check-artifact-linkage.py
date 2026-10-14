#!/usr/bin/env python3
"""Require isolated evaluator linkage in one product artifact directory."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "checks"))
from check_workspace import PRODUCT_EXECUTABLES, verify_artifact_linkage


def main(argv):
    if len(argv) != 1:
        print("usage: check-artifact-linkage.py <artifact-directory>", file=sys.stderr)
        return 2
    directory = Path(argv[0])
    errors, receipt = verify_artifact_linkage(
        {name: directory / name for name in PRODUCT_EXECUTABLES}
    )
    for error in errors:
        print(error, file=sys.stderr)
    if not errors:
        print(receipt)
    return 2 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
