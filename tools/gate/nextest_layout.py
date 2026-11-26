"""Generate nextest membership and scale weighted profiles for the host."""

from __future__ import annotations

import argparse
import copy
import math
import os
from pathlib import Path
import re
import sys
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


def _parse_cgroup_v2_cpu_max(text: str) -> int | None:
    parts = text.split()
    if len(parts) != 2 or parts[0] == "max":
        return None
    try:
        quota = int(parts[0])
        period = int(parts[1])
    except ValueError:
        return None
    if quota <= 0 or period <= 0:
        return None
    return max(1, quota // period)


def _cgroup_cpu_quota(
    proc_cgroup: Path = Path("/proc/self/cgroup"),
    cgroup_root: Path = Path("/sys/fs/cgroup"),
) -> int | None:
    """Return the tightest Linux cgroup v2/v1 CPU quota in whole slots, or None."""
    try:
        lines = proc_cgroup.read_text(encoding="utf-8").splitlines()
    except OSError:
        return None

    limit: int | None = None

    def apply(candidate: int | None) -> None:
        nonlocal limit
        if candidate is not None:
            limit = candidate if limit is None else min(limit, candidate)

    for line in lines:
        parts = line.split(":", 2)
        if len(parts) != 3:
            continue
        hierarchy, controllers, relpath = parts
        rel = relpath.lstrip("/")
        if hierarchy == "0" and controllers == "":
            current = cgroup_root / rel if rel else cgroup_root
            while True:
                try:
                    apply(_parse_cgroup_v2_cpu_max((current / "cpu.max").read_text("utf-8")))
                except OSError:
                    pass
                if current == cgroup_root or current.parent == current:
                    break
                current = current.parent
        elif "cpu" in controllers.split(","):
            base = cgroup_root / "cpu"
            current = base / rel if rel else base
            while True:
                try:
                    quota = int((current / "cpu.cfs_quota_us").read_text("utf-8").strip())
                    period = int((current / "cpu.cfs_period_us").read_text("utf-8").strip())
                    if quota > 0 and period > 0:
                        apply(max(1, quota // period))
                except (OSError, ValueError):
                    pass
                if current == base or current.parent == current:
                    break
                current = current.parent

    return limit


def available_parallelism(_target: Path | None = None) -> int:
    """Host CPU slots available to this process (respecting Linux affinity and cgroup quotas)."""
    slots: int | None = None
    if hasattr(os, "sched_getaffinity"):
        try:
            count = len(os.sched_getaffinity(0))
            if count > 0:
                slots = count
        except OSError:
            pass
    if slots is None:
        slots = max(1, os.cpu_count() or 1)
    if sys.platform.startswith("linux"):
        quota = _cgroup_cpu_quota()
        if quota is not None:
            slots = min(slots, quota)
    return max(1, slots)


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
