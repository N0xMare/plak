#!/usr/bin/env python3
"""Run the opt-in real-product SQLite commit sweep."""

import argparse
import json
import os
from pathlib import Path
import subprocess


TERMINAL_SHAPES = {
    "runner_hold_poll": "host_skeleton::runner::hold::runner_hold_poll",
    "runner_hold_live_key": "host_skeleton::runner::hold::runner_hold_live_key",
    "runner_hold_not_sent": "host_skeleton::runner::hold::runner_hold_not_sent",
    "runner_hold_stop": "host_skeleton::runner::hold::runner_hold_stop",
    "runner_hold_host_loss": "host_skeleton::runner::hold::runner_hold_host_loss",
    "runner_hold_identity": "host_skeleton::runner::hold::runner_hold_identity",
    "runner_hold_isolation": "host_skeleton::runner::hold::runner_hold_isolation",
    "runner_deferred_pending_stop": "host_skeleton::runner::pending_stop::runner_deferred_bind_finishes_pending_stop_without_redispatch",
    # These real-host authority shapes own explicit link/lease/receipt cuts,
    # rather than SQLite-ordinal cuts. Like the TUI restart, run them directly.
    "runner_wake_settled_foreground": "host_skeleton::runner::foreground::runner_wake_settled_foreground",
    "runner_wake_live_foreground": "host_skeleton::runner::foreground::runner_wake_live_foreground",
    "runner_lease_lapse_reports_foreground": "host_skeleton::runner::foreground::runner_lease_lapse_reports_foreground",
    "runner_new_owner_foreground": "host_skeleton::runner::a_foreground_host_cut_stops_at_transfer_and_records_unknown_once",
    "review_tui_host_restart_switch": "host_skeleton::crash::tui::review_tui_host_restart_switch",
    "tui_host_restart": "host_skeleton::crash::tui::tui_host_restart",
    "wire_host_restart": "host_skeleton::crash::wire::cuts::wire_host_restart",
    "wire_host_restart_switch": "host_skeleton::crash::wire::cuts::wire_host_restart_switch",
}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shapes")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--strict", action="store_true")
    args = parser.parse_args()
    if args.jobs < 1 or args.repeat < 1:
        parser.error("--jobs and --repeat must be positive")
    script = Path(__file__).resolve().parents[1] / "gate" / "test-env.sh"
    env = os.environ.copy()
    env.update(json.loads(subprocess.check_output([str(script), "--json"], text=True)))
    # test-env.sh unsets these only in a shell or a command it runs itself.
    for name in ("PLAK_TEST_SESSION_LEASE_SECONDS", "PLAK_TEST_CELL_HELLO_MS"):
        env.pop(name, None)
    env.update(
        PLAK_CRASH_SWEEP_JOBS=str(args.jobs),
        PLAK_CRASH_SWEEP_REPEAT=str(args.repeat),
        PLAK_CRASH_SWEEP_STRICT=str(int(args.strict)),
    )
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
    # Use the gate's test build, including both cell workers. Building only
    # plak can silently run stale sibling executables from another tree.
    built = subprocess.call(
        [
            "cargo",
            "test",
            "--no-run",
            "--locked",
            "--workspace",
            "--all-features",
        ],
        env=env,
    )
    if built:
        return built
    # `--workspace` keeps the feature resolution of the build above: `-p plak`
    # resolves plak's features alone and recompiled about twenty crates (and
    # the product binary the sweep launches) into a second artifact set.
    command = [
        "cargo",
        "test",
        "--locked",
        "--workspace",
        "--all-features",
        "--test",
        "integration",
    ]
    if generic_shapes != []:
        result = subprocess.call(
            [
                "cargo",
                "test",
                "--locked",
                "--workspace",
                "--all-features",
                "--test",
                "integration",
                "crash_sweep::full_sweep",
                "--",
                "--ignored",
                "--exact",
                "--nocapture",
            ],
            env=env,
        )
        if result:
            return result
    for name in terminal_shapes:
        for _ in range(args.repeat):
            result = subprocess.call(
                [*command, TERMINAL_SHAPES[name], "--", "--exact", "--nocapture"],
                env=env,
            )
            if result:
                return result
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
