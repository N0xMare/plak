"""Image pins and threshold arithmetic for the synthetic staging soak."""

import hashlib
import json
import math
import re
import statistics
import subprocess
from pathlib import Path

from staging_soak_stream import CHECKPOINT_MISSING, LATE_PREFIX

NAMES = ("plakd", "plakous-js-host", "plakous-py-host")
METRICS = ("attach_fast_ms_p95", "cpu_total_ms_per_s", "rss_mib_per_min", "rss_end_mib")
FAST_SEND_P50_MS = 1.0
FAST_SEND_P99_MS = 40.0
LEGACY_FAST_PONG_SEND_P50_MS = 1.0
LEGACY_FAST_PONG_SEND_P99_MS = 20.0
FAST_PING_P99_MS = 20.0
FAST_LATENCY_LIMITS_MS = {
    "send_frame_fast_ms_p50": FAST_SEND_P50_MS,
    "send_frame_fast_ms_p99": FAST_SEND_P99_MS,
    "send_pong_fast_ms_p50": LEGACY_FAST_PONG_SEND_P50_MS,
    "send_pong_fast_ms_p99": LEGACY_FAST_PONG_SEND_P99_MS,
    "ping_ms_p99": FAST_PING_P99_MS,
}
SUBSTITUTIONS = [
    "same-revision test-support artifacts",
    "scripted model",
    "scripted tree and binder",
    "gateway fault=connect",
    "synthetic web identity and UI",
    "local process runner only",
]
HARNESS = (
    "staging_soak.py",
    "staging_soak_evidence.py",
    "staging_soak_measure.py",
    "staging_soak_stream.py",
    "host_latency.py",
    "host_latency_fixture.py",
    "host_latency_wire.py",
)


def digest(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def verify_image(path, triple):
    elf = subprocess.check_output(["readelf", "-hW", "-lW", path], text=True, timeout=10)
    if "Advanced Micro Devices X86-64" not in elf or "ELF64" not in elf:
        raise ValueError("soak requires an x86-64 Linux executable")
    interpreters = {
        "x86_64-unknown-linux-gnu": "/lib64/ld-linux-x86-64.so.2",
        "x86_64-unknown-linux-musl": "/lib/ld-musl-x86_64.so.1",
    }
    if not (interpreter := interpreters.get(triple)) or f"interpreter: {interpreter}]" not in elf:
        raise ValueError("ELF target ABI differs from the shipping manifest or cannot be verified")


def pinned(binary, shipping, profile, *, release=False):
    files = {}
    triple = shipping["platform"]
    for name in NAMES:
        path = binary.with_name(name)
        verify_image(path, triple)
        files[name] = digest(path)
        if release:
            manifest = shipping["files"][name]
            if manifest["sha256"] != files[name] or manifest["length"] != path.stat().st_size:
                raise ValueError(f"shipping {name} differs from its manifest")
    build = json.loads(
        subprocess.check_output([binary, "version", "--build-info"], timeout=20, text=True)
    )
    if release and build != shipping["build_info"]:
        raise ValueError("shipping build info differs from its manifest")
    if build["flavor"] != "maintainer release" or profile != "maintainer":
        raise ValueError("expected the maintainer build profile")
    return {
        **build,
        "binary_id": files["plakd"][:12],
        "sha256": files["plakd"],
        "files": files,
        "target_triple": triple,
        "build_profile": profile,
    }


def pair(releases, probes, profile, aa):
    shipping, measured = [], []
    for release, probe in zip(releases, probes, strict=True):
        manifest_file = release.with_name("deploy-manifest.json")
        manifest = json.loads(manifest_file.read_text())
        a = pinned(release, manifest, "maintainer", release=True)
        a["manifest_sha256"] = digest(manifest_file)
        b = pinned(probe, manifest, profile)
        if a["test_hooks"] or not b["test_hooks"] or a["sha256"] == b["sha256"]:
            raise ValueError(
                "expected an installed release and a dedicated test-support soak build"
            )
        for key in (
            "revision",
            "wire",
            "state_key",
            "runner",
            "gateway",
            "target_triple",
            "flavor",
        ):
            if a[key] != b[key]:
                raise ValueError(f"release and soak build disagree on {key}")
        if b["wire"]["number"] != 12 or not re.fullmatch(r"[0-9a-f]{40}", b["revision"]):
            raise ValueError("expected wire 12 and a pinned source revision")
        shipping.append(a)
        measured.append(b)
    if measured[0]["state_key"] != measured[1]["state_key"]:
        raise ValueError("soak requires a same-key upgrade")
    for images in (shipping, measured):
        if (images[0]["sha256"] == images[1]["sha256"]) != aa:
            raise ValueError("A/A needs identical artifacts; A/B needs distinct artifacts")
    return shipping, measured


def policy(path):
    document = json.loads(path.read_text())
    fields = {"limits", "load_1m_limit"}
    if set(document) not in (fields, fields | {"nightly"}):
        raise ValueError("invalid soak policy fields")
    nightly = document.get("nightly", {})
    if "nightly" in document and (
        not isinstance(nightly, dict)
        or set(nightly)
        not in ({"timing_qualified"}, {"allow_known_late_prefix", "timing_qualified"})
        or any(type(value) is not bool for value in nightly.values())
    ):
        raise ValueError("invalid nightly soak policy")
    load = document["load_1m_limit"]
    if type(load) not in (float, int) or not math.isfinite(load) or load <= 0:
        raise ValueError("invalid soak load limit")
    raw = document["limits"]
    if set(raw) != set(METRICS):
        raise ValueError("soak policy metrics differ from the harness")
    for bounds in raw.values():
        if set(bounds) != {"relative", "floor"} or any(
            type(v) not in (float, int) or not math.isfinite(v) or v < 0 for v in bounds.values()
        ):
            raise ValueError("invalid soak policy bound")
    return raw, load, nightly


def compare(windows, bounds):
    summary = {
        label: {
            key: statistics.mean(w[key] for w in windows if w["side"] == label) for key in METRICS
        }
        for label in ("A", "B")
    }
    deltas, failures = {}, []
    for key in METRICS:
        before, after = (summary[label][key] for label in ("A", "B"))
        deltas[key] = {
            "absolute": after - before,
            "relative": after / before - 1 if before else None,
        }
        if bounds is not None:
            base = max(before, bounds[key]["floor"])
            ceiling = base * (1 + bounds[key]["relative"])
            deltas[key]["candidate_ceiling"] = ceiling
            if after > ceiling:
                failures.append(f"regressed_{key}: {after:.3f} > {ceiling:.3f}")
    return summary, deltas, failures


def latency_reasons(window, *, require_stamps):
    failures, timing = [], []
    frame_count = window["send_frame_fast_n"]
    missing = window["send_frame_fast_unstamped"]
    if require_stamps or frame_count:
        metric = "send_frame_fast"
        if not frame_count or missing:
            failures.append(
                f"fast text enqueue coverage: {frame_count} stamped, {missing} unstamped"
            )
    else:
        metric = "send_pong_fast"
        observed, expected = window["send_pong_fast_n"], window["ping_n"]
        if observed != expected or not observed:
            failures.append(f"fast Pong enqueue coverage: {observed} of {expected}")
    if window["send_fast_negative"]:
        failures.append(f"negative fast enqueue intervals: {window['send_fast_negative']}")
    for name in (f"{metric}_ms_p50", f"{metric}_ms_p99", "ping_ms_p99"):
        limit = FAST_LATENCY_LIMITS_MS[name]
        value = window[name]
        if value is None or not math.isfinite(value):
            failures.append(f"{name}: no samples (limit {limit:g} ms)")
        elif value > limit:
            timing.append(f"{name}: {value:.3f} > {limit:g} ms")
    return failures, timing


def known_defect_pairs(window):
    """Only forgive one specifically paired late-prefix repair per finding."""
    if window["reference_mismatches"]:
        return 0
    pairs = 0
    for attachment in window["attachments"]:
        if attachment["revision_gaps"] or attachment["evicted_deltas"]:
            return 0
        count = attachment["paired_prefixes"]
        if attachment["mismatches"]:
            if (
                attachment["continuous"]
                or count <= 0
                or attachment["mismatches"] != 2 * count
                or attachment["reconcile_reasons"]
                != {LATE_PREFIX: count, CHECKPOINT_MISSING: count}
            ):
                return 0
            pairs += count
        elif count or attachment["reconcile_reasons"]:
            return 0
    return pairs if 2 * pairs == window["reconcile_failures"] else 0


def classify_cycle(blocking, timing, pairs, *, nightly, timing_qualified, aa):
    if (
        blocking
        or (pairs and not nightly)
        or (timing and (pairs or not nightly or timing_qualified))
    ):
        return "fail", 1
    if timing:
        return "timing_observation", 75
    if pairs:
        return "known_defect", 76
    return ("calibration" if aa else "pass"), 0


def harness_identity(revision):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("a full harness source revision is required")
    directory = Path(__file__).parent
    return {
        "revision": revision,
        "files": {name: digest(directory / name) for name in HARNESS},
    }
