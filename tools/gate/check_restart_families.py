"""Refuse a durability selection missing any restart family's union witness."""

import json
import sys

FAMILIES = (
    "host_skeleton::crash::wire::cuts::wire_host_restart",
    "host_skeleton::crash::wire::cuts::wire_host_restart_switch",
)


def check(inventory: dict) -> None:
    selected = {
        name
        for suite in inventory["rust-suites"].values()
        if suite["package-name"] == "plak" and suite["binary-name"] == "integration"
        for name, case in suite["testcases"].items()
        if case["filter-match"]["status"] == "matches" and not case["ignored"]
    }
    for family in FAMILIES:
        expected = {f"{family}::{role}_{group}" for role in ("root", "child") for group in range(8)}
        found = {name for name in selected if name.startswith(family + "::")}
        if found != expected:
            raise ValueError(
                f"{family}: expected all 16 restart groups; "
                f"missing={sorted(expected - found)}, unexpected={sorted(found - expected)}"
            )
    print("durability selection: both wire restart families contain all 16 groups")


if __name__ == "__main__":
    try:
        check(json.load(sys.stdin))
    except (KeyError, ValueError) as error:
        sys.exit(f"durability selection: {error}")
