#!/usr/bin/env python3
"""Serial budget lane and durability lane filtersets.

Membership lives in the pool-reserving override blocks of `.config/nextest.toml`
(the `offline` profile's copies are canonical; `default` matches them). The
budget block is the wall-clock lane. The durability block is the crash-matrix
and sqlite SIGKILL campaign lane. This module reads them back so
`scripts/dev-check.sh`, the `just test` recipe and the tooling tests agree on
one expression instead of restating the lists.

Usage:
  budget_filterset.py                     # budget-lane filterset
  budget_filterset.py --exclude           # complement of budget OR durability
  budget_filterset.py --names             # budget-lane test names
  budget_filterset.py --durability        # durability-lane filterset
  budget_filterset.py --durability-names  # durability-lane test names
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
NEXTEST_CONFIG = REPO_ROOT / ".config" / "nextest.toml"

# Present only in the durability override, so the two pool-reserving blocks
# stay distinguishable without a second copy of the name list.
DURABILITY_ANCHOR = "actual_host_sigkill_relaunch_matrix_uses_real_handlers_and_reconcilers"

# An override reserves the pool when it sets threads-required and carries no
# test-group. The budget and durability lanes are the only blocks of that shape.
_TEST_PREDICATE = re.compile(r"test\(([^)]+)\)")


def _overrides(profile: str, config_path: Path = NEXTEST_CONFIG) -> list[dict]:
    with config_path.open("rb") as handle:
        parsed = tomllib.load(handle)
    return parsed.get("profile", {}).get(profile, {}).get("overrides", [])


def _pool_overrides(profile: str = "offline", config_path: Path = NEXTEST_CONFIG) -> list[dict]:
    found = [
        override
        for override in _overrides(profile, config_path)
        if override.get("threads-required") == "num-test-threads"
    ]
    if len(found) != 2:
        raise SystemExit(
            f"expected exactly two pool-reserving overrides in profile {profile!r}, "
            f"found {len(found)} in {config_path}"
        )
    return found


def _split_overrides(
    profile: str = "offline", config_path: Path = NEXTEST_CONFIG
) -> tuple[dict, dict]:
    """Return `(budget, durability)` for the profile's two pool overrides."""
    pools = _pool_overrides(profile, config_path)
    durability = [override for override in pools if DURABILITY_ANCHOR in override.get("filter", "")]
    budget = [override for override in pools if DURABILITY_ANCHOR not in override.get("filter", "")]
    if len(durability) != 1 or len(budget) != 1:
        raise SystemExit(
            f"expected one budget override and one durability override "
            f"in profile {profile!r}, found budget={len(budget)} durability={len(durability)}"
        )
    return budget[0], durability[0]


def budget_override(profile: str = "offline", config_path: Path = NEXTEST_CONFIG) -> dict:
    """Return the wall-clock budget override block for `profile`."""
    budget, _durability = _split_overrides(profile, config_path)
    return budget


def durability_override(profile: str = "offline", config_path: Path = NEXTEST_CONFIG) -> dict:
    """Return the crash/durability override block for `profile`."""
    _budget, durability = _split_overrides(profile, config_path)
    return durability


def _one_line(override: dict) -> str:
    return " ".join(override["filter"].split())


def budget_filterset(profile: str = "offline", config_path: Path = NEXTEST_CONFIG) -> str:
    """The nextest filterset expression selecting the budget lane, one line."""
    return _one_line(budget_override(profile, config_path))


def durability_filterset(profile: str = "offline", config_path: Path = NEXTEST_CONFIG) -> str:
    """The nextest filterset expression selecting the durability lane, one line."""
    return _one_line(durability_override(profile, config_path))


def exclude_filterset(profile: str = "offline", config_path: Path = NEXTEST_CONFIG) -> str:
    """The complement of the budget lane union the durability lane."""
    return f"not ({budget_filterset(profile, config_path)} | {durability_filterset(profile, config_path)})"


def _names(expression: str) -> list[str]:
    return [match.strip() for match in _TEST_PREDICATE.findall(expression)]


def budget_test_names(
    profile: str = "offline", config_path: Path = NEXTEST_CONFIG
) -> list[str]:
    """The test names the budget lane matches on, in declaration order."""
    return _names(budget_filterset(profile, config_path))


def durability_test_names(
    profile: str = "offline", config_path: Path = NEXTEST_CONFIG
) -> list[str]:
    """The test names the durability lane matches on, in declaration order."""
    return _names(durability_filterset(profile, config_path))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--exclude", action="store_true", help="print the parallel complement")
    group.add_argument("--names", action="store_true", help="print one budget-lane name per line")
    group.add_argument("--durability", action="store_true", help="print the durability-lane filterset")
    group.add_argument(
        "--durability-names", action="store_true", help="print one durability-lane name per line"
    )
    parser.add_argument("--profile", default="offline", help="nextest profile to read")
    args = parser.parse_args(argv)

    if args.names:
        print("\n".join(budget_test_names(args.profile)))
    elif args.durability_names:
        print("\n".join(durability_test_names(args.profile)))
    elif args.durability:
        print(durability_filterset(args.profile))
    elif args.exclude:
        print(exclude_filterset(args.profile))
    else:
        print(budget_filterset(args.profile))
    return 0


if __name__ == "__main__":
    sys.exit(main())
