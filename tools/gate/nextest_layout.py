"""Render the reviewed durability reservations for this host's available pool."""

from __future__ import annotations

import argparse
import copy
import math
import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib


def scaled_profile(profile: dict, available: int) -> dict:
    result = copy.deepcopy(profile)
    maximum = profile["test-threads"]
    width = max(1, min(maximum, available))
    result["test-threads"] = width
    for rule in result.get("overrides", []):
        weight = rule.get("threads-required")
        if isinstance(weight, int):
            rule["threads-required"] = max(1, math.ceil(weight * width / maximum))
    return result


def render(source: str, available: int) -> tuple[str, int]:
    expected = tomllib.loads(source)
    base = expected["profile"]["durability"]
    scaled = scaled_profile(base, available)
    expected["profile"]["durability"] = scaled
    section = ""
    lines = []
    for line in source.splitlines(keepends=True):
        header = re.fullmatch(r"\[{1,2}([\w.-]+)\]{1,2}\s*", line)
        if header:
            section = header[1]
        if section == "profile.durability" and line.startswith("test-threads ="):
            line = f'test-threads = {scaled["test-threads"]}\n'
        elif section == "profile.durability.overrides":
            weight = re.fullmatch(r"threads-required = (\d+)\s*", line)
            if weight:
                value = max(
                    1, math.ceil(int(weight[1]) * scaled["test-threads"] / base["test-threads"])
                )
                line = f"threads-required = {value}\n"
        lines.append(line)
    rendered = "".join(lines)
    if tomllib.loads(rendered) != expected:
        raise ValueError("durability layout changed settings outside its pool and reservations")
    return rendered, scaled["test-threads"]


def available_parallelism(target: Path) -> int:
    # Use the same Rust API as the campaigns, including affinity and CPU quota
    # limits. The tiny host probe has no dependencies or product runtime.
    target.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nextest-cpus-", dir=target) as directory:
        executable = Path(directory) / "parallelism"
        subprocess.run(
            [
                "rustc",
                "--edition=2024",
                "--crate-name",
                "gate_parallelism",
                "-Dwarnings",
                "-o",
                str(executable),
                "-",
            ],
            input='#![forbid(unsafe_code)]\nfn main() { println!("{}", std::thread::available_parallelism().map_or(1, |n| n.get())); }\n',
            text=True,
            check=True,
        )
        return int(subprocess.check_output([str(executable)], text=True))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    target = Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "gate-native-tools"
    source, width = render(args.source.read_text(), available_parallelism(target))
    args.output.write_text(source)
    print(width)


if __name__ == "__main__":
    main()
