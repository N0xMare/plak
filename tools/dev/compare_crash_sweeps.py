#!/usr/bin/env python3
"""Compare crash sweeps only at equal cuts with equal recorded pre-cut inputs."""

import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path


def read_rows(path):
    rows = []
    for number, line in enumerate(Path(path).read_text().splitlines(), 1):
        if not line.strip():
            continue
        row = json.loads(line)
        if not isinstance(row, dict) or not all(
            key in row for key in ("shape", "store", "store_ordinal", "outcome")
        ):
            raise ValueError(f"{path}:{number}: missing cut identity or outcome")
        rows.append(row)
    return rows


def cut_key(row):
    return row["shape"], row["store"], row["store_ordinal"]


def inputs(row):
    """Exclude cross-store scheduling order, random identities and recovery output."""
    result = {}
    if row.get("global_order"):
        stores = defaultdict(list)
        for commit in row["global_order"]:
            if commit.get("indexed", True):
                stores[commit["store"]].append(
                    (commit["store_ordinal"], commit["label"], commit["tables"])
                )
        result["prefix"] = {store: sorted(commits) for store, commits in stores.items()}
    before = row.get("before", {})
    if "ledger" in before:
        # Ledger facts use stable operation ordinals, kinds and states, not UUIDs.
        result["ledger"] = before["ledger"]
        for name in (
            "phase",
            "runtime_verdict",
            "revision",
            "obligations",
            "root_turn_count",
            "tool_call_count",
        ):
            if name in before:
                result[name] = before[name]
    return json.dumps(result, sort_keys=True, separators=(",", ":")) if result else None


def outcome(row):
    return row["outcome"], row.get("invariants")


def failed(row):
    return (
        row.get("invariants") is not True
        or row["outcome"] in ("DIVERGENT", "UNCLASSIFIED")
        or row["outcome"].startswith("REFUSED")
    )


def reference(row):
    return {
        key: row[key]
        for key in ("shape", "store", "store_ordinal", "repeat", "outcome", "invariants")
        if key in row
    }


def compare(left, right):
    report = {
        name: []
        for name in (
            "matching",
            "floating",
            "changed",
            "only_left",
            "only_right",
            "unpaired",
            "uncomparable",
            "regressed",
        )
    }
    groups = [defaultdict(list), defaultdict(list)]
    for rows, grouped in zip((left, right), groups):
        for row in rows:
            grouped[cut_key(row)].append(row)
    for key in sorted(groups[0].keys() | groups[1].keys()):
        sides = [list(grouped.get(key, [])) for grouped in groups]
        # Failure evidence must not disappear behind a different (or absent)
        # prefix. Keep its input classification too, but always surface it.
        baseline_failures = Counter(row["outcome"] for row in sides[0] if failed(row))
        for row in sides[1]:
            if not failed(row):
                continue
            # One baseline sample covers only one failure of the same kind,
            # not every future failure at this key or under a floating input.
            matched = baseline_failures[row["outcome"]] > 0
            if matched:
                baseline_failures[row["outcome"]] -= 1
            report["regressed"].append({"cut": reference(row), "baseline_failed": matched})
        if not all(sides):
            side = 0 if sides[0] else 1
            report[("only_left", "only_right")[side]].extend(map(reference, sides[side]))
            continue
        indexed = [defaultdict(list), defaultdict(list)]
        for side, rows in enumerate(sides):
            for row in rows:
                fingerprint = inputs(row)
                if fingerprint is None:
                    report["uncomparable"].append(
                        {"side": ("left", "right")[side], "cut": reference(row)}
                    )
                else:
                    indexed[side][fingerprint].append(row)
        for fingerprint in sorted(indexed[0].keys() & indexed[1].keys()):
            a, b = indexed[0][fingerprint], indexed[1][fingerprint]
            # Repeats are samples, not an input identity. Match equal outcomes
            # first, so reordering identical samples cannot manufacture a change.
            for row in a[:]:
                peer = next((r for r in b if outcome(r) == outcome(row)), None)
                if peer is not None:
                    report["matching"].append({"left": reference(row), "right": reference(peer)})
                    a.remove(row)
                    b.remove(peer)
            while a and b:
                report["changed"].append(
                    {
                        "left": reference(a.pop(0)),
                        "right": reference(b.pop(0)),
                        "inputs": json.loads(fingerprint),
                    }
                )
        # A different prefix at the same ordinal floats. Unequal sample counts
        # under an otherwise equal prefix are neither floating nor regressions.
        for side, grouped in enumerate(indexed):
            for fingerprint, rows in sorted(grouped.items()):
                category = "unpaired" if fingerprint in indexed[1 - side] else "floating"
                for row in rows:
                    report[category].append(
                        {
                            "side": ("left", "right")[side],
                            "cut": reference(row),
                            "inputs": json.loads(fingerprint),
                        }
                    )
    report["counts"] = {name: len(rows) for name, rows in report.items()}
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("left", type=Path)
    parser.add_argument("right", type=Path)
    parser.add_argument("--json", action="store_true", help="emit all classifications and inputs")
    args = parser.parse_args()
    try:
        report = compare(read_rows(args.left), read_rows(args.right))
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.error(str(error))
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(" ".join(f"{name}={count}" for name, count in report["counts"].items()))
        for change in report["changed"]:
            print(json.dumps(change, sort_keys=True))
        for regression in report["regressed"]:
            print(json.dumps(regression, sort_keys=True))
    return int(
        bool(report["changed"]) or any(not row["baseline_failed"] for row in report["regressed"])
    )


if __name__ == "__main__":
    raise SystemExit(main())
