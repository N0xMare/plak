#!/usr/bin/env python3
"""Run the opt-in real-product SQLite commit sweep."""

import argparse
import gzip
import json
import os
from pathlib import Path
import re
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path.cwd() / "tools/gate"))
import crash_sweep_shards as shards  # noqa: E402
from test_artifacts import fixture_paths  # noqa: E402


TERMINAL_SHAPES = shards.TERMINAL_SHAPES
INDEXED_TEST = "crash_sweep::full_sweep"
COMMAND = ["cargo", "test", "--locked", "--workspace", "--all-features", "--test", "integration"]


def cargo_evidence(command, env, count):
    process = subprocess.run(
        command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
    )
    sys.stdout.write(process.stdout)
    passed = sum(map(int, re.findall(r"^test result: ok\. (\d+) passed;", process.stdout, re.M)))
    if process.returncode or passed != count:
        raise ValueError(f"{command[7]}: expected {count} successful tests, got {passed}")
    return process.stdout


def read_rows(path):
    with Path(path).open(encoding="utf-8") as stream:
        return [json.loads(line) for line in stream]


def wire_rows(name, log):
    groups = []
    for child, group, count, path in re.findall(
        rf"{name}: child=(true|false) group=([0-7]): (\d+) cuts; (.+\.jsonl)$", log, re.M
    ):
        rows = read_rows(path)
        if len(rows) != int(count):
            raise ValueError(f"{name} group {group}: row count disagrees with test completion")
        groups.append(dict(family=name, child=int(child == "true"), group=int(group), rows=rows))
    return groups


def run_shard(env, args, shard):
    i, n = shard
    names = shards.plan(n)[i - 1]
    if args.output.exists():
        raise ValueError("shard receipt already exists")
    subprocess.run(["git", "diff", "--quiet", "HEAD", "--"], check=True)
    commit, tree = subprocess.check_output(
        ["git", "rev-parse", "HEAD", "HEAD^{tree}"], text=True
    ).splitlines()
    env["PLAK_CRASH_SWEEP_SHAPES"] = ",".join(sorted(names & shards.INDEXED.keys()))
    print(f"strict shard {i}/{n}: shapes {env['PLAK_CRASH_SWEEP_SHAPES']}", flush=True)
    rc = build(env)
    if rc:
        return rc
    indexed = []
    if env["PLAK_CRASH_SWEEP_SHAPES"]:
        log = cargo_evidence(
            [*COMMAND, INDEXED_TEST, "--", "--ignored", "--exact", "--nocapture"], env, 1
        )
        (table,) = re.findall(r"^crash sweep table: (.+/table\.md)$", log, re.M)
        indexed = read_rows(Path(table).with_name("sweep.jsonl"))
    receipt = dict(
        commit=commit,
        tree=tree,
        target=env.get("CARGO_BUILD_TARGET") or sys.platform,
        pruning=[
            env.get("PLAK_TEST_WORKING_PRUNE_MINIMUM"),
            env.get("PLAK_TEST_WORKING_PRUNE_MARGIN"),
        ],
        shard=args.shard,
        inventory=shards.fingerprint(),
        indexed=indexed,
        runners=[],
        wire=[],
    )
    for name in sorted(names & TERMINAL_SHAPES.keys()):
        selector = TERMINAL_SHAPES[name]
        if name in shards.WIRE:
            if not list_family(name, selector, env):
                return 1
            log = cargo_evidence(
                [*COMMAND, selector, "--", "--test-threads", str(args.jobs), "--nocapture"],
                env,
                16,
            )
            receipt["wire"].extend(wire_rows(name, log))
        else:
            cargo_evidence([*COMMAND, selector, "--", "--exact", "--nocapture"], env, 1)
            receipt["runners"].append(name)
    shards.validate(receipt, names)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with gzip.open(args.output, "xt", encoding="utf-8") as stream:
        json.dump(receipt, stream, separators=(",", ":"))
    print(f"strict shard {i}/{n} passed: {args.output}", flush=True)
    return 0


def list_family(name, selector, env):
    listed = subprocess.run(
        [*COMMAND, selector, "--", "--list"], env=env, capture_output=True, text=True
    )
    expected = {f"{selector}{kind}_{group}" for kind in ["root", "child"] for group in range(8)}
    found = {
        line.removesuffix(": test")
        for line in listed.stdout.splitlines()
        if line.endswith(": test")
    }
    if listed.returncode or found != expected:
        print(f"{name}: expected all 16 restart groups, found {sorted(found)}", file=sys.stderr)
        return False
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shapes")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--strict", action="store_true")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--shard", metavar="I/N", help="run one strict shard (1-based)")
    mode.add_argument(
        "--merge", nargs="+", type=Path, metavar="RECEIPT", help="verify strict shards"
    )
    parser.add_argument("--output", type=Path, help="shard receipt file or merged output directory")
    args = parser.parse_args()
    if args.jobs < 1 or args.repeat < 1:
        parser.error("--jobs and --repeat must be positive")
    if bool(args.output) != bool(args.shard or args.merge):
        parser.error("--shard and --merge require --output")
    if args.merge:
        try:
            print(json.dumps(shards.merge(args.merge, args.output), indent=2))
        except (OSError, ValueError, KeyError, TypeError, EOFError) as error:
            parser.error(str(error))
        return 0
    if args.shard and (not args.strict or args.shapes is not None or args.repeat != 1):
        parser.error("--shard requires --strict and the complete selection with one repeat")
    script = Path.cwd() / "tools/gate/test-env.sh"
    env = os.environ.copy()
    env.update(json.loads(subprocess.check_output([str(script), "--json"], text=True)))
    # --json keeps the overrides and provider keys the offline tests must not see.
    for name in subprocess.check_output([str(script), "--unset-names"], text=True).split():
        env.pop(name, None)
    env.update(
        PLAK_CRASH_SWEEP_JOBS=str(args.jobs),
        PLAK_CRASH_SWEEP_REPEAT=str(args.repeat),
        PLAK_CRASH_SWEEP_STRICT=str(int(args.strict)),
    )
    if args.shard:
        try:
            return run_shard(env, args, shards.shard_id(args.shard))
        except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
            parser.error(str(error))
    terminal_shapes = []
    generic_shapes = None
    if args.shapes is not None:
        selected = args.shapes.split(",")
        terminal_shapes = [name for name in selected if name in TERMINAL_SHAPES]
        generic_shapes = [name for name in selected if name not in TERMINAL_SHAPES]
        env["PLAK_CRASH_SWEEP_SHAPES"] = ",".join(generic_shapes)
    elif args.strict:
        env.pop("PLAK_CRASH_SWEEP_SHAPES", None)
        terminal_shapes = list(TERMINAL_SHAPES)
    else:
        env["PLAK_CRASH_SWEEP_SHAPES"] = "final_only,direct_tool_then_final"
    rc = build(env)
    if rc:
        return rc
    if generic_shapes != []:
        result = subprocess.call(
            [*COMMAND, INDEXED_TEST, "--", "--ignored", "--exact", "--nocapture"], env=env
        )
        if result:
            return result
    for name in terminal_shapes:
        selector = TERMINAL_SHAPES[name]
        family = selector.endswith("::")
        if family and not list_family(name, selector, env):
            return 1
        for _ in range(args.repeat):
            result = subprocess.call(
                [
                    *COMMAND,
                    selector,
                    "--",
                    *(["--test-threads", str(args.jobs)] if family else ["--exact"]),
                    "--nocapture",
                ],
                env=env,
            )
            if result:
                return result
    return 0


def build(env):
    # Use the gate's test build, including both cell workers. Building only
    # plakd can silently run stale sibling executables from another tree.
    built = subprocess.run(
        [
            "cargo",
            "test",
            "--no-run",
            "--locked",
            "--workspace",
            "--all-features",
            "--message-format=json",
        ],
        env=env,
        stdout=subprocess.PIPE,
        text=True,
    )
    if built.returncode:
        return built.returncode
    env.update(fixture_paths(built.stdout))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
