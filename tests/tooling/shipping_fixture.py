import shlex
import shutil
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/build"))
import artifact_manifest as manifests  # noqa: E402

REVISION = "1" * 40
LINUX = "x86_64-unknown-linux-musl"
TOOLCHAIN = {
    "cargo": {"release": "1.100.0", "commit_hash": "a" * 9},
    "rustc": {
        "release": "1.100.0",
        "commit_hash": "b" * 40,
        "host": "x86_64-unknown-linux-gnu",
        "llvm_version": "23.1.5",
    },
}


def info(version="0.0.2"):
    return {
        "flavor": "release",
        "product_version": version,
        "revision": REVISION,
        "export": {"read": [6, 7], "write": 7},
        "gateway": {"served": [1, 2], "spoken": [1, 2]},
        "omitted_features": [],
        "runner": {"protocol": 9, "retention": {"served": [1], "spoken": [1]}},
        "state_key": "r1.e1.p1.t1",
        "test_hooks": False,
        "wire": {"number": 9, "schema_pin": "2" * 64},
    }


def executable(version="0.0.2"):
    build_info = shlex.quote(manifests.canonical(info(version)).decode().strip())
    return (
        "#!/bin/sh\n"
        'case "$*" in\n'
        '  "wire schema") echo \'{"wire_version":9,"binary_id":"release-fixture"}\' ;;\n'
        f'  "version --build-info") printf "%s\\n" {build_info} ;;\n'
        '  "version --state-key") echo r1.e1.p1.t1 ;;\n'
        '  "version --export-version") echo 7 ;;\n'
        '  "version --write-binary-manifest "*) echo fixture >"$3/binary-id"; chmod 0444 "$3/binary-id" ;;\n'
        "  *) exit 2 ;;\n"
        "esac\n"
    )


def copy_emitter(repo, version="0.0.2"):
    relative = "tools/build/artifact_manifest.py"
    shutil.copy(ROOT / relative, repo / relative)
    (repo / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')
    (repo / "Cargo.lock").write_text("fixture\n")


def manifest(directory):
    return manifests.make_manifest(directory, LINUX, REVISION, "0.0.2", "3" * 64, TOOLCHAIN, info())
