"""Generate nextest membership and scale weighted profiles for the host."""

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

CONFIG = Path(__file__).resolve().parents[2] / ".config/nextest.toml"
INSTALL_CATALOG = "nextest-install.toml"
SPECIAL_LANES = ("serial", "durability", "install")
WEIGHTED_PROFILES = ("durability", "install")


def install_catalog(config: Path = CONFIG) -> dict:
    return tomllib.loads(config.with_name(INSTALL_CATALOG).read_text())


def union(filters: list[str]) -> str:
    return "\n| ".join(f"({expression.strip()})" for expression in filters)


def generated_block(source: str, name: str, content: str) -> str:
    begin = f"# BEGIN generated {name} (tools/gate/nextest_layout.py)\n"
    end = f"# END generated {name}\n"
    pattern = re.escape(begin) + r".*?" + re.escape(end)
    result, count = re.subn(pattern, lambda _: begin + content + end, source, flags=re.S)
    if count != 1:
        raise ValueError(f"expected exactly one generated {name} block, found {count}")
    return result


def sync_membership(source: str, families: dict) -> str:
    profiles = tomllib.loads(source)["profile"]
    install = union([row["filter"] for row in families.values()])
    excluded = union([profiles[lane]["default-filter"] for lane in SPECIAL_LANES[:-1]] + [install])
    isolated = union([row["filter"] for row in families.values() if row.get("isolated")])
    generated = f"default-filter = '''\n{install}\n'''\n"
    if isolated:
        generated += (
            "\n[[profile.install.overrides]]\n"
            f"filter = '''\n{isolated}\n'''\n"
            'threads-required = "num-test-threads"\n'
            "priority = -50\n"
        )
    synced = generated_block(
        source, "offline", f"default-filter = '''\nnot (\n{excluded}\n)\n'''\n"
    )
    return generated_block(synced, "install", generated)


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


def render(source: str, available: int, profile: str = "durability") -> tuple[str, int]:
    expected = tomllib.loads(source)
    base = expected["profile"][profile]
    scaled = scaled_profile(base, available)
    expected["profile"][profile] = scaled
    section = ""
    lines = []
    for line in source.splitlines(keepends=True):
        header = re.fullmatch(r"\[{1,2}([\w.-]+)\]{1,2}\s*", line)
        if header:
            section = header[1]
        if section == f"profile.{profile}" and line.startswith("test-threads ="):
            line = f'test-threads = {scaled["test-threads"]}\n'
        elif section == f"profile.{profile}.overrides":
            weight = re.fullmatch(r"threads-required = (\d+)\s*", line)
            if weight:
                value = max(
                    1, math.ceil(int(weight[1]) * scaled["test-threads"] / base["test-threads"])
                )
                line = f"threads-required = {value}\n"
        lines.append(line)
    rendered = "".join(lines)
    if tomllib.loads(rendered) != expected:
        raise ValueError(f"{profile} layout changed settings outside its pool and reservations")
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
    parser.add_argument("source", type=Path, nargs="?")
    parser.add_argument("output", type=Path, nargs="?")
    parser.add_argument("--profile", choices=WEIGHTED_PROFILES, default="durability")
    parser.add_argument("--sync", action="store_true", help="regenerate the committed lane filters")
    args = parser.parse_args()
    if args.sync:
        config = args.source or CONFIG
        if args.output:
            parser.error("--sync edits the source in place")
        config.write_text(sync_membership(config.read_text(), install_catalog(config)))
        return
    if not args.source or not args.output:
        parser.error("rendering requires source and output")
    target = Path(os.environ.get("CARGO_TARGET_DIR", "target")) / "gate-native-tools"
    source, width = render(args.source.read_text(), available_parallelism(target), args.profile)
    args.output.write_text(source)
    print(width)


if __name__ == "__main__":
    main()
