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

DRIVER = Path(__file__).resolve()
ROOT = DRIVER.parents[2]
sys.path.insert(0, str(DRIVER.parent))
sys.path.insert(0, str(ROOT / "tools/gate"))
import crash_sweep_shards as shards  # noqa: E402
import crash_sweep_evidence as evidence  # noqa: E402
from test_artifacts import fixture_paths  # noqa: E402


TERMINAL_SHAPES = shards.TERMINAL_SHAPES
INDEXED_TEST = "crash_sweep::full_sweep"
COMMAND = ["cargo", "test", "--locked", "--workspace", "--all-features", "--test", "integration"]
PRUNE_KNOBS = ("PLAK_TEST_WORKING_PRUNE_MINIMUM", "PLAK_TEST_WORKING_PRUNE_MARGIN")


def cargo_evidence(command, env, count):
    lines = []
    with subprocess.Popen(
        command,
        cwd=ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    ) as process:
        for line in process.stdout:
            print(line, end="", flush=True)
            lines.append(line)
        code = process.wait()
    log = "".join(lines)
    refuse_compile(log)
    require_pass_count(log, code, count, command[7])
    return log


def require_pass_count(log, code, count, selector):
    passed = sum(map(int, re.findall(r"^test result: ok\. (\d+) passed;", log, re.M)))
    if code or passed != count:
        raise ValueError(
            f"{selector}: expected {count} successful tests, got {passed} (exit {code})"
        )


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


def refuse_compile(log):
    if "Compiling " in log:
        raise ValueError("Cargo compiled after the strict sweep fixture build")


def verify_sweep_files(commit):
    for path in (DRIVER, Path(shards.__file__), shards.INVENTORY, Path(evidence.__file__)):
        recorded = evidence.git(ROOT, "show", f"{commit}:{path.relative_to(ROOT).as_posix()}")
        if path.read_bytes() != recorded:
            raise ValueError(f"sweep driver or inventory does not match {commit}: {path.name}")


def clean_checkout():
    top = evidence.git(ROOT, "rev-parse", "--show-toplevel").strip()
    if Path(os.fsdecode(top)).resolve() != ROOT:
        raise ValueError("sweep driver must run from its tracked checkout")
    if evidence.git(ROOT, "status", "--porcelain", "--untracked-files=all"):
        raise ValueError("strict sweep requires a clean checkout, including untracked files")
    commit, tree = evidence.git(ROOT, "rev-parse", "HEAD", "HEAD^{tree}").decode().splitlines()
    verify_sweep_files(commit)
    return commit, tree


def require_unchanged(before):
    if clean_checkout() != before:
        raise ValueError("checkout revision changed while the strict sweep was running")


def build_target(env):
    if target := env.get("CARGO_BUILD_TARGET"):
        return target
    version = subprocess.check_output(["rustc", "-vV"], cwd=ROOT, env=env, text=True)
    (triple,) = re.findall(r"^host: (\S+)$", version, re.M)
    return triple


def run_shard(env, args, shard):
    i, n = shard or (1, 1)
    names = shards.plan(n)[i - 1]
    if args.output.exists():
        raise ValueError("shard receipt already exists")
    commit, tree = clean_checkout()
    formats = evidence.formats(ROOT, commit)
    target = build_target(env)
    # Pin the target on Cargo calls too, including native host builds.
    env["CARGO_BUILD_TARGET"] = target
    env["PLAK_CRASH_SWEEP_SHAPES"] = ",".join(sorted(names & shards.INDEXED.keys()))
    label = f"shard {i}/{n}" if shard else "full sweep"
    print(f"strict {label}: shapes {env['PLAK_CRASH_SWEEP_SHAPES']}", flush=True)
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
        target=target,
        formats=formats,
        shard=f"{i}/{n}",
        inventory=shards.fingerprint(),
        indexed=indexed,
        runners=[],
        wire=[],
    )
    for name in sorted(names & TERMINAL_SHAPES.keys()):
        selector = TERMINAL_SHAPES[name]
        if name in shards.WIRE:
            if not list_family(name, selector, env, check_compile=True):
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
    require_unchanged((commit, tree))
    if shard:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        with gzip.open(args.output, "xt", encoding="utf-8") as stream:
            json.dump(receipt, stream, separators=(",", ":"))
    else:
        evidence.write_receipt(args.output, shards.passed_report(receipt, "full", 1))
    print(f"strict {label} passed: {args.output}", flush=True)
    return 0


def list_family(name, selector, env, *, check_compile=False):
    listed = subprocess.run(
        [*COMMAND, selector, "--", "--list"], cwd=ROOT, env=env, capture_output=True, text=True
    )
    if check_compile:
        refuse_compile(listed.stdout + listed.stderr)
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
    mode.add_argument(
        "--check-evidence", action="store_true", help="check production sweep evidence"
    )
    parser.add_argument("--candidate", help="candidate revision for --check-evidence")
    parser.add_argument("--receipts", type=Path, help="directory of completed strict JSON receipts")
    parser.add_argument(
        "--output", type=Path, help="strict receipt file or merged output directory"
    )
    parser.add_argument("--expect-commit", help="revision qualified by --merge")
    parser.add_argument(
        "--expect-target", help="Rust target triple required by merge or evidence check"
    )
    args = parser.parse_args()
    if args.jobs < 1 or args.repeat < 1:
        parser.error("--jobs and --repeat must be positive")
    if args.output is None and (args.shard or args.merge):
        parser.error("--shard and --merge require --output")
    if args.check_evidence:
        if not args.candidate or not args.receipts:
            parser.error("--check-evidence requires --candidate and --receipts")
        if args.output or args.expect_commit or args.strict or args.shapes is not None:
            parser.error("--check-evidence cannot run or merge a sweep")
        try:
            path, receipt = evidence.check(
                ROOT,
                args.candidate,
                args.receipts,
                args.expect_target or "x86_64-unknown-linux-musl",
            )
            print(json.dumps(dict(receipt, receipt_path=str(path))))
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            parser.error(f"{error}; run strict on the candidate")
        return 0
    if args.candidate or args.receipts:
        parser.error("--candidate and --receipts apply only to --check-evidence")
    if args.merge:
        if not args.expect_commit or not args.expect_target:
            parser.error("--merge requires --expect-commit and --expect-target")
        try:
            before = clean_checkout()
            commit = evidence.revision(ROOT, args.expect_commit)
            verify_sweep_files(commit)
            result = shards.merge(
                args.merge,
                args.output,
                commit=commit,
                tree=evidence.tree_id(ROOT, commit),
                target=args.expect_target,
                formats=evidence.formats(ROOT, commit),
                before_publish=lambda: require_unchanged(before),
            )
            print(json.dumps(result, indent=2))
        except (
            OSError,
            ValueError,
            KeyError,
            TypeError,
            EOFError,
            subprocess.SubprocessError,
        ) as error:
            parser.error(str(error))
        return 0
    if args.expect_commit or args.expect_target:
        parser.error("--expect-commit and --expect-target require merge or evidence check")
    shard = None
    if args.output:
        mode_name = "--shard" if args.shard is not None else "--output"
        if not args.strict or args.shapes is not None or args.repeat != 1:
            parser.error(
                f"{mode_name} requires --strict and the complete selection with one repeat"
            )
        if any(name in os.environ for name in PRUNE_KNOBS):
            parser.error(f"{mode_name} requires default pruning settings")
        if args.shard is not None:
            try:
                shard = shards.shard_id(args.shard)
            except ValueError as error:
                parser.error(str(error))
    script = ROOT / "tools/gate/test-env.sh"
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
    if args.output:
        try:
            return run_shard(env, args, shard)
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
            [*COMMAND, INDEXED_TEST, "--", "--ignored", "--exact", "--nocapture"], cwd=ROOT, env=env
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
                cwd=ROOT,
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
        cwd=ROOT,
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
