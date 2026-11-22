"""Plan strict crash-sweep shards and verify their exact coverage."""

from collections import Counter
import gzip
import hashlib
import json
from pathlib import Path
import re


INVENTORY = (
    Path(__file__).resolve().parents[2] / "bin/plakd/tests/integration/crash_sweep/shard_inventory.json"
)
INDEXED = json.loads(INVENTORY.read_text())
WIRE = {
    "wire_host_restart": (
        {"product": 12, "runtime": 10, "tree": 1},
        {"product": 14, "runtime": 33, "tree": 16},
    ),
    "wire_host_restart_switch": (
        {"product": 15, "runtime": 13, "tree": 1},
        {"product": 17, "runtime": 36, "tree": 19},
    ),
}
TERMINAL_SHAPES = {
    "runner_hold_poll": "host_skeleton::runner::hold::runner_hold_poll",
    "runner_hold_live_key": "host_skeleton::runner::hold::runner_hold_live_key",
    "runner_hold_not_sent": "host_skeleton::runner::hold::runner_hold_not_sent",
    "runner_hold_stop": "host_skeleton::runner::hold::runner_hold_stop",
    "runner_hold_host_loss": "host_skeleton::runner::hold::runner_hold_host_loss",
    "runner_hold_identity": "host_skeleton::runner::hold::runner_hold_identity",
    "runner_hold_isolation": "host_skeleton::runner::hold::runner_hold_isolation",
    "runner_deferred_pending_stop": "host_skeleton::runner::pending_stop::runner_deferred_bind_finishes_pending_stop_without_redispatch",
    "runner_wake_settled_foreground": "host_skeleton::runner::foreground::runner_wake_settled_foreground",
    "runner_wake_live_foreground": "host_skeleton::runner::foreground::runner_wake_live_foreground",
    "runner_lease_lapse_reports_foreground": "host_skeleton::runner::foreground::runner_lease_lapse_reports_foreground",
    "runner_new_owner_foreground": "host_skeleton::runner::a_foreground_host_cut_stops_at_transfer_and_records_unknown_once",
    "wire_host_restart": "host_skeleton::crash::wire::cuts::wire_host_restart::",
    "wire_host_restart_switch": "host_skeleton::crash::wire::cuts::wire_host_restart_switch::",
}
RUNNERS = TERMINAL_SHAPES.keys() - WIRE.keys()
PASSING_OUTCOMES = {
    "COMPLETED_ONCE",
    "ABANDONED",
    "REQUEUED",
    "NOT_ADMITTED",
    "FENCED",
    "HANDED_OFF",
    "NOT_HANDED_OFF",
    "CONTINUED",
    "HELD",
}


def shard_id(value):
    match = re.fullmatch(r"([1-9][0-9]{0,2})/([1-9][0-9]{0,2})", value)
    if not match:
        raise ValueError("use a 1-based shard index i/n")
    i, n = map(int, match.groups())
    if not 1 <= i <= n <= len(INDEXED):
        raise ValueError(f"require 1 <= i <= n <= {len(INDEXED)}")
    return i, n


def plan(n):
    """Allocate whole shapes in descending work order; break ties by name/index."""
    units = {shape: row["weight"] for shape, row in INDEXED.items()}
    # A wire family keeps its sixteen groups and twin pilots together. These
    # costs only balance work; the coverage sets are independent of them.
    units.update({name: 1 for name in RUNNERS})
    units.update({name: 8 for name in WIRE})
    loads, shards = [0] * n, [set() for _ in range(n)]
    for name in sorted(units, key=lambda key: (-units[key], key)):
        i = min(range(n), key=loads.__getitem__)
        shards[i].add(name)
        loads[i] += units[name]
    return shards


def cuts(stores):
    return [
        (store, ordinal)
        for store, count in sorted(stores.items())
        for ordinal in range(1, count + 1)
    ]


def expected_rows(names):
    indexed = {
        (shape, store, ordinal)
        for shape in names & INDEXED.keys()
        for store, ordinal in cuts(INDEXED[shape]["stores"])
    }
    groups = {
        (family, child, group): set(cuts(stores)[group::8])
        for family in names & WIRE.keys()
        for child, stores in enumerate(WIRE[family])
        for group in range(8)
    }
    return indexed, groups


def verify_keys(expected, actual, label):
    counts = Counter(actual)
    missing = sorted(expected - counts.keys())
    extra = sorted(counts.keys() - expected)
    duplicate = sorted(key for key, count in counts.items() if count > 1)
    if missing or extra or duplicate:
        raise ValueError(
            f"{label}: missing={missing[:8]}, unexpected={extra[:8]}, duplicate={duplicate[:8]}"
        )


def validate(receipt, names):
    indexed, groups = expected_rows(names)
    verify_keys(indexed, (indexed_key(row) for row in receipt["indexed"]), "indexed cuts")
    for row in receipt["indexed"]:
        outcome = row["outcome"].split("{", 1)[0]
        if row["repeat"] != 1 or row["invariants"] is not True or outcome not in PASSING_OUTCOMES:
            raise ValueError(f"strict cut failed: {indexed_key(row)} ({row['outcome']})")
    verify_keys(names & RUNNERS, receipt["runners"], "runner cases")
    verify_keys(groups.keys(), (group_key(item) for item in receipt["wire"]), "wire groups")
    for item in receipt["wire"]:
        key = group_key(item)
        verify_keys(groups[key], ((row["store"], row["cut"]) for row in item["rows"]), str(key))
        for row in item["rows"]:
            if (
                row["shape"] != key[0]
                or row["child"] is not bool(key[1])
                or row["invariants"] is not True
            ):
                raise ValueError(f"wire invariant failed: {key} {row['store']}:{row['cut']}")


def indexed_key(row):
    return row["shape"], row["store"], row["store_ordinal"]


def group_key(item):
    return item["family"], item["child"], item["group"]


def fingerprint():
    recorded = [INDEXED, WIRE, TERMINAL_SHAPES]
    return hashlib.sha256(json.dumps(recorded, sort_keys=True).encode()).hexdigest()


def merge(paths, output):
    indexed, runners, wire = [], [], []
    indices = []
    identity = None
    n = len(paths)
    assignments = plan(n)
    for path in paths:
        with gzip.open(path, "rt", encoding="utf-8") as stream:
            receipt = json.load(stream)
        i, total = shard_id(receipt["shard"])
        if total != n:
            raise ValueError(f"{path}: need all {total} receipts, got {n}")
        current = receipt["commit"], receipt["tree"], receipt["target"], receipt["pruning"]
        if identity is None:
            identity = current
        if current != identity or receipt["inventory"] != fingerprint():
            raise ValueError(f"{path}: sweep identity or inventory mismatch")
        validate(receipt, assignments[i - 1])
        indices.append(i)
        indexed.extend(receipt["indexed"])
        runners.extend(receipt["runners"])
        wire.extend(receipt["wire"])
    verify_keys(set(range(1, n + 1)), indices, "shard receipts")
    counts = {
        "indexed_cuts": len(indexed),
        "indexed_shapes": len({row["shape"] for row in indexed}),
        "runner_cases": len(runners),
        "wire_groups": len(wire),
        "wire_invariant_rows": sum(len(item["rows"]) for item in wire),
    }
    output.mkdir(parents=True)
    write_rows(output / "sweep.jsonl", sorted(indexed, key=indexed_key))
    write_rows(
        output / "wire.jsonl",
        (
            dict(row, group=item["group"])
            for item in sorted(wire, key=group_key)
            for row in sorted(item["rows"], key=lambda row: (row["store"], row["cut"]))
        ),
    )
    report = dict(
        zip(("commit", "tree", "target", "pruning"), identity, strict=True),
        inventory=fingerprint(),
        shards=n,
        runners=sorted(runners),
        **counts,
    )
    (output / "coverage.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def write_rows(path, rows):
    with path.open("w", encoding="utf-8") as stream:
        for row in rows:
            stream.write(json.dumps(row, sort_keys=True) + "\n")
