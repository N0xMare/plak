"""Structured test-execution evidence for the pinned Rung A A/B verifier."""

from __future__ import annotations

import hashlib
import json
from collections import Counter
from pathlib import Path
from typing import Any


DATASET = "terminal-bench/terminal-bench-2-1@6"

# These exact task refs/checksums cover both frozen Rung A TB 2.1 slices.
# Each pinned tests/test.sh installs pytest-json-ctrf==0.3.5 and writes
# /logs/verifier/ctrf.json. This contract is scoped to these source bindings.
TASKS = {
    "terminal-bench/cancel-async-tasks": (
        "sha256:a3d048d351136e48070696cda8bb79660dfd74db1fea3b6da88559f0332699c1",
        "44b08781bbffcbff0555fafbad034389009edeb85c2f83cd10620f91aa24974f",
    ),
    "terminal-bench/configure-git-webserver": (
        "sha256:e3dc712acfecdb1338a889f86c97b37422c3778351253f9aca4236e895fdb36e",
        "84d7c2fd653dad4307c7d2e9dd4f937ecbcd7ad4d1acf5d2b74c35166634178e",
    ),
    "terminal-bench/fix-git": (
        "sha256:16948b980df9d96de616a205f5acca1c5d395de83ff4f8ffabcafacb93226f2e",
        "d3220d70bc668ec6f4034fab51e62873dff724a61f824d764fd201d6f5e7a88a",
    ),
    "terminal-bench/log-summary-date-ranges": (
        "sha256:27b074a2f10fff7606e096f3abd8dced418ad8fda0f53d88acbe477f2d9ceaf6",
        "c833c594814ec7b8cb32eba3b9cb5ed648171efe5a074767aa64c25ea060f08f",
    ),
    "terminal-bench/nginx-request-logging": (
        "sha256:9d1b8bebd989ea0bc8080c3b159480068caf183c5fd61385868b2574b206e097",
        "913305d8f286ff121b30e4893217142190b27f12952343f22c0999dd2a4d725e",
    ),
    "terminal-bench/openssl-selfsigned-cert": (
        "sha256:d4afa2bd2a9ba1420db8d6cfde42ffdb4873ae2d955c35014e8da94444c83302",
        "2b70d5535b5873f644fad37b76dbef86a1e42162e018c7bc06316e5e2521929a",
    ),
    "terminal-bench/regex-log": (
        "sha256:802c16cfd132e6c457529cb864be5a757c1b23b6cadc57f2d01983cb0110292a",
        "31dc6115c061b96539a5287090ce41a7a89d3201c291b9b843bd70e416f35c39",
    ),
    "terminal-bench/sqlite-db-truncate": (
        "sha256:956f038b479cc3b9b493553b57a60a8ff4154526386c3914c0b99e93e1ab6e87",
        "673cdff46735ca0b8d83c6c82c0a4e252149d31629e80da9d8439a05d771351c",
    ),
    "terminal-bench/break-filter-js-from-html": (
        "sha256:678008d1a4fd1e6e1b9b3cc9a327219fe4b410a31eafc52e9099bbf947eea600",
        "3b08ac8e870b13a6161363e4152bceaa9c945eb7c3c35cc33c8c5bc19f2a1873",
    ),
    "terminal-bench/build-cython-ext": (
        "sha256:35dc561790e73f13e59837828ac0863c0893edc6b71d42cf7d58656c843adf12",
        "b7e605f001726583c03bcf62835408e03cd413ae917a3a982d6beb4a0f3a01d2",
    ),
    "terminal-bench/cobol-modernization": (
        "sha256:e03aa03965eee48c9f772d566ff9bcd741fceeac9693123f82a13fdd46be0d88",
        "5e3806427eaaaa9f0f756a2a87d4f7f50ec1c8a0d329b26bab7dc09657b937cf",
    ),
    "terminal-bench/count-dataset-tokens": (
        "sha256:cd0574ec8281f53854256acc93702ed1441407c0428849e1dd4f27e863c6b08a",
        "0ab655881a4827d9ec8f9930d4c4c8827729b20c978c20ff9cc6317f90660b5c",
    ),
    "terminal-bench/db-wal-recovery": (
        "sha256:07e9193513bfce038b9f9d84962ecc85683eee0a642d14ee6fb7104b086ddab4",
        "c18abdc4fdc3a01bf374c55a9700708fe6a9662077d29db81abb692f0a3c5f6f",
    ),
    "terminal-bench/dna-insert": (
        "sha256:37fc023970e3464c1acc31f43e04a0b4b94758130d340788f261f00764d9e530",
        "8ee918b8953bf437bf764153a84080475e1c2dfa18d0209cc4b19fee12ccce3c",
    ),
    "terminal-bench/extract-elf": (
        "sha256:1ef31d566be4fe3459d5368621ae7ef7a31b23ef675737e473bbc43c8c7b3fce",
        "642b0321f77002c7c9a70acb825b31295d0a6baf07a5c3880ca5b30461fbbdcb",
    ),
    "terminal-bench/git-multibranch": (
        "sha256:7759a92a373c967dd31535975bad8659410644025673fe62385444fd2605c5fa",
        "abe8a7f8f9dcb0e2170740e4735559b368cecd925ddee09c9ed81a7c67dce90a",
    ),
}


def _count(value: Any) -> int | None:
    return value if type(value) is int and value >= 0 else None


def inspect_verifier(
    trial: Path, *, dataset: str | None, slice_name: str | None,
    task: str, task_ref: str | None, task_checksum: str | None,
) -> dict[str, Any]:
    """Require asserted CTRF tests only for the exact pinned Rung A TB 2.1 slices."""
    if dataset != DATASET or slice_name not in {"ab", "expanded"} or task not in TASKS:
        return {"contract": None, "status": "not_required", "path": None,
                "sha256": None, "counts": None}
    source = trial / "verifier/ctrf.json"
    count = 8 if slice_name == "ab" else 16
    evidence = {"contract": f"rung-a-tb21-selected{count}-ctrf", "status": None,
                "path": str(source), "sha256": None, "counts": None}
    if TASKS.get(task) != (task_ref, task_checksum):
        evidence["status"] = "task_binding_mismatch"
        return evidence
    if not (trial / "verifier").is_dir():
        evidence["status"] = "not_archived"
        return evidence
    try:
        data = source.read_bytes()
    except OSError:
        evidence["status"] = "missing_ctrf"
        return evidence
    evidence["sha256"] = hashlib.sha256(data).hexdigest()
    try:
        document = json.loads(data)
        results = document["results"]
        summary = results["summary"]
        tests = results["tests"]
        counts = {key: _count(summary.get(key)) for key in
                  ("tests", "passed", "failed", "skipped", "pending", "other")}
        statuses = Counter(test["status"] for test in tests)
        valid = (isinstance(tests, list) and all(isinstance(t, dict) for t in tests)
                 and all(isinstance(t.get("name"), str) and t["name"].strip() for t in tests)
                 and all(value is not None for value in counts.values())
                 and len(tests) == counts["tests"]
                 and sum(counts[key] for key in counts if key != "tests") == counts["tests"]
                 and all(statuses.get(key, 0) == counts[key] for key in counts if key != "tests")
                 and set(statuses).issubset({"passed", "failed", "skipped", "pending", "other"}))
    except (TypeError, ValueError, KeyError, AttributeError):
        valid = False
    if not valid:
        evidence["status"] = "invalid_ctrf"
        return evidence
    evidence["counts"] = counts
    evidence["status"] = "assertions_recorded" if counts["passed"] + counts["failed"] > 0 else "no_assertions"
    return evidence
