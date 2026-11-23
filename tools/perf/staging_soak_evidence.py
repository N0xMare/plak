"""Image pins and threshold arithmetic for the synthetic staging soak."""

import hashlib
import json
import math
import re
import statistics
import subprocess
from pathlib import Path

NAMES = ("plakd", "plakous-js-host", "plakous-py-host")
METRICS = ("attach_fast_ms_p95", "cpu_total_ms_per_s", "rss_mib_per_min", "rss_end_mib")
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
    raw = json.loads(path.read_text())
    if set(raw) != set(METRICS):
        raise ValueError("soak policy metrics differ from the harness")
    for bounds in raw.values():
        if set(bounds) != {"relative", "floor"} or any(
            type(v) not in (float, int) or not math.isfinite(v) or v < 0 for v in bounds.values()
        ):
            raise ValueError("invalid soak policy bound")
    return raw


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
                failures.append(f"regressed_{key}")
    return summary, deltas, failures


def harness_identity(revision):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("a full harness source revision is required")
    directory = Path(__file__).parent
    return {"revision": revision, "files": {name: digest(directory / name) for name in HARNESS}}
