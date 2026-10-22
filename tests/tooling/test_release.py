"""Release tarballs, the release build script and install.sh, without Rust or the network.

install.sh runs under /bin/sh against a local HTTP server laid out like GitHub
Releases, with `uname` and `gh` stubbed so the selected triple and the
attestation step are deterministic on every host.
"""

from __future__ import annotations

import hashlib
import http.server
import io
import json
import os
import pathlib
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/release"))
import release_tarball  # noqa: E402

VERSION = "0.0.1"
LINUX = "x86_64-unknown-linux-musl"
EXECUTABLES = ("plak", "plakous-js-host", "monty")


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def executable_bytes(name: str, tag: str = "one") -> bytes:
    # The snapshot install runs `plak version --state-key`.
    return (f"#!/bin/sh\n# {name} {tag}\n"
            'case "$2" in --state-key) echo r1.e1.p1.t1 ;; --export-version) echo 7 ;; --write-binary-manifest) if [ -n "$REJECT_BINARY_MANIFEST" ]; then echo partial >"$3/binary-id"; exit 2; fi; echo fixture >"$3/binary-id"; chmod 0444 "$3/binary-id" ;; esac\n'
            ).encode()


def write_executables(directory: pathlib.Path, tag: str = "one") -> None:
    directory.mkdir(parents=True, exist_ok=True)
    for name in EXECUTABLES:
        (directory / name).write_bytes(executable_bytes(name, tag))
        (directory / name).chmod(0o755)


def fake_metadata(base: pathlib.Path) -> dict:
    """plak -> (vendored protocol, registry `left`); `left` -> `deep`; dev-only `testonly`."""
    def package(name: str, directory: pathlib.Path, source: str | None, license_: str,
                files: dict[str, str]) -> dict:
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "Cargo.toml").write_text("")
        for file, text in files.items():
            (directory / file).write_text(text)
        return {"id": f"{name}-id", "name": name, "version": "1.0.0", "source": source,
                "license": license_, "repository": f"https://example.invalid/{name}",
                "manifest_path": str(directory / "Cargo.toml")}

    registry = "registry+https://github.com/rust-lang/crates.io-index"
    packages = [
        package("plak", base / "ws/bin/plak", None, "MIT", {"LICENSE": "ours"}),
        package("plakous-js-runtime", base / "ws/crates/js", None, "MIT", {"LICENSE": "ours"}),
        package("plakous-py-worker", base / "ws/crates/py", None, "MIT", {"LICENSE": "ours"}),
        package("vendored-protocol", base / "ws/vendor/protocol", None, "Apache-2.0",
                {"LICENSE": "apache", "NOTICE": "notice text", "README.md": "no"}),
        package("left", base / "registry/left", registry, "MIT OR Apache-2.0",
                {"LICENSE-MIT": "mit", "LICENSE-APACHE": "apache", "src.rs": ""}),
        package("deep", base / "registry/deep", registry, "BSD-3-Clause", {"COPYING": "bsd"}),
        package("testonly", base / "registry/testonly", registry, "MIT", {"LICENSE": "x"}),
    ]
    normal = [{"kind": None, "target": None}]

    def node(id_: str, *deps: tuple[str, list]) -> dict:
        return {"id": id_, "deps": [{"pkg": dep, "dep_kinds": kinds} for dep, kinds in deps]}

    nodes = [
        node("plak-id", ("vendored-protocol-id", normal), ("left-id", normal),
             ("testonly-id", [{"kind": "dev", "target": None}])),
        node("plakous-js-runtime-id"), node("plakous-py-worker-id", ("left-id", normal)),
        node("vendored-protocol-id"), node("left-id", ("deep-id", normal)),
        node("deep-id"), node("testonly-id"),
    ]
    members = ["plak-id", "plakous-js-runtime-id", "plakous-py-worker-id", "vendored-protocol-id"]
    return {"packages": packages, "resolve": {"nodes": nodes}, "workspace_members": members,
            "workspace_root": str(base / "ws")}


def make_release(base: pathlib.Path, triple: str = LINUX, tag: str = "one") -> pathlib.Path:
    """A release directory with releases/download/v<VERSION>/{tarball,SHA256SUMS}."""
    artifacts = base / f"artifacts-{triple}-{tag}"
    write_executables(artifacts, tag)
    out = base / "site/releases/download" / f"v{VERSION}"
    release_tarball.pack(VERSION, triple, artifacts, out, 1_700_000_000,
                         fake_metadata(base / "meta"), static=False)
    return out


class Server:
    """Serves `site/`; /releases/latest redirects to /releases/tag/v<VERSION>."""

    def __init__(self, root: pathlib.Path) -> None:
        class Handler(http.server.SimpleHTTPRequestHandler):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, directory=str(root), **kwargs)

            def do_GET(self):  # noqa: N802
                if self.path == "/releases/latest":
                    self.send_response(302)
                    self.send_header("Location", f"/releases/tag/v{VERSION}")
                    self.end_headers()
                elif self.path == f"/releases/tag/v{VERSION}":
                    self.send_response(200)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                else:
                    super().do_GET()

            def log_message(self, *args):
                pass

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.httpd.shutdown()
        self.httpd.server_close()


class ReleaseTarballTests(unittest.TestCase):
    def setUp(self) -> None:
        self.base = pathlib.Path(tempfile.mkdtemp()).resolve()
        self.addCleanup(shutil.rmtree, self.base)

    def test_tarball_layout_licenses_and_determinism(self) -> None:
        out = make_release(self.base)
        tarball = out / f"plak-{VERSION}-{LINUX}.tar.gz"
        first = tarball.read_bytes()
        make_release(self.base)
        self.assertEqual(tarball.read_bytes(), first)
        stem = f"plak-{VERSION}-{LINUX}"
        with tarfile.open(tarball) as archive:
            members = {member.name: member for member in archive.getmembers()}
            self.assertEqual(sorted(members), sorted([
                stem, f"{stem}/LICENSE", f"{stem}/THIRD-PARTY", f"{stem}/monty", f"{stem}/plak",
                f"{stem}/plakous-js-host", f"{stem}/licenses",
                f"{stem}/licenses/deep-1.0.0", f"{stem}/licenses/deep-1.0.0/COPYING",
                f"{stem}/licenses/left-1.0.0", f"{stem}/licenses/left-1.0.0/LICENSE-APACHE",
                f"{stem}/licenses/left-1.0.0/LICENSE-MIT",
                f"{stem}/licenses/vendored-protocol-1.0.0",
                f"{stem}/licenses/vendored-protocol-1.0.0/LICENSE",
                f"{stem}/licenses/vendored-protocol-1.0.0/NOTICE",
            ]))
            for name in EXECUTABLES:
                self.assertEqual(members[f"{stem}/{name}"].mode, 0o755)
                self.assertEqual(archive.extractfile(f"{stem}/{name}").read(),
                                 executable_bytes(name))
            self.assertEqual({(m.uid, m.gid, m.uname, m.mtime) for m in members.values()},
                             {(0, 0, "", 1_700_000_000)})
            third_party = archive.extractfile(f"{stem}/THIRD-PARTY").read().decode()
            self.assertEqual(archive.extractfile(f"{stem}/LICENSE").read(),
                             (ROOT / "LICENSE").read_bytes())
        listed = [line.split()[0] for line in third_party.splitlines()[3:]]
        self.assertEqual(listed, ["deep", "left", "vendored-protocol"])
        self.assertIn("left 1.0.0  MIT OR Apache-2.0  https://example.invalid/left", third_party)
        self.assertEqual((out / "SHA256SUMS").read_text(),
                         f"{sha(first)}  plak-{VERSION}-{LINUX}.tar.gz\n")

    def test_sums_cover_every_tarball_in_the_directory(self) -> None:
        make_release(self.base)
        out = make_release(self.base, "aarch64-apple-darwin")
        names = [line.split()[1] for line in (out / "SHA256SUMS").read_text().splitlines()]
        self.assertEqual(names, [f"plak-{VERSION}-aarch64-apple-darwin.tar.gz",
                                 f"plak-{VERSION}-{LINUX}.tar.gz"])

    def test_require_static_refuses_an_elf_with_a_program_interpreter(self) -> None:
        def elf(interp: bool) -> bytes:
            header = bytearray(64)
            header[:6] = b"\x7fELF\x02\x01"
            struct.pack_into("<Q", header, 0x20, 64)
            struct.pack_into("<HH", header, 0x36, 56, 2)
            phdrs = struct.pack("<I52x", 1) + struct.pack("<I52x", 3 if interp else 6)
            return bytes(header) + phdrs

        artifacts = self.base / "artifacts"
        write_executables(artifacts)
        metadata = fake_metadata(self.base / "meta")
        (artifacts / "monty").write_bytes(elf(False))
        release_tarball.pack(VERSION, LINUX, artifacts, self.base / "ok", 0, metadata, static=True)
        (artifacts / "monty").write_bytes(elf(True))
        with self.assertRaisesRegex(release_tarball.ReleaseError, "monty: dynamically linked"):
            release_tarball.pack(VERSION, LINUX, artifacts, self.base / "no", 0, metadata,
                                 static=True)
        self.assertFalse((self.base / "no").exists())


class InstallScriptTests(unittest.TestCase):
    def setUp(self) -> None:
        self.base = pathlib.Path(tempfile.mkdtemp()).resolve()
        self.addCleanup(shutil.rmtree, self.base)
        self.release = make_release(self.base)
        self.server = Server(self.base / "site")
        self.addCleanup(self.server.close)
        self.home = self.base / "home"
        self.home.mkdir(mode=0o755)
        self.stubs = self.base / "stubs"
        self.stubs.mkdir()
        self.gh_log = self.base / "gh.log"
        self.stub("uname", 'case "$1" in -s) echo "${FAKE_OS:-Linux}" ;; '
                           '-m) echo "${FAKE_ARCH:-x86_64}" ;; esac')
        # Not signed in unless FAKE_GH is set; `verify` exits FAKE_GH_VERIFY.
        self.stub("gh", f'echo "$*" >>{self.gh_log}\n[ -n "${{FAKE_GH:-}}" ] || exit 1\n'
                        'case "$1" in attestation) exit "${FAKE_GH_VERIFY:-0}" ;; esac')
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("PLAK_")}
        self.env.update(HOME=str(self.home), PLAK_RELEASE_BASE=self.server.url,
                        TMPDIR=str(self.base), PATH=f"{self.stubs}{os.pathsep}{os.environ['PATH']}")
        self.lib = self.home / ".local/lib/plak"

    def stub(self, name: str, body: str) -> None:
        path = self.stubs / name
        path.write_text(f"#!/bin/sh\n{body}\n")
        path.chmod(0o755)

    def install(self, **env: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(["/bin/sh", str(ROOT / "install.sh")], env={**self.env, **env},
                              capture_output=True, text=True, timeout=60, check=False)

    def expected_snapshot(self, tag: str = "one") -> pathlib.Path:
        digests = {name: sha(executable_bytes(name, tag)) for name in EXECUTABLES}
        manifest = "".join(f"{name} {digests[name]}\n" for name in EXECUTABLES)
        return self.lib / "release" / sha(manifest.encode())

    def test_install_accepts_a_release_without_the_binary_manifest_probe(self) -> None:
        result = self.install(REJECT_BINARY_MANIFEST="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        snapshot = self.expected_snapshot()
        self.assertTrue((snapshot / "plak").is_file())
        self.assertFalse((snapshot / "binary-id").exists())
        self.assertEqual(list(snapshot.parent.glob(".staging.*")), [])

    def test_latest_install_writes_the_source_install_layout(self) -> None:
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("attestation not checked", result.stderr)
        snapshot = self.expected_snapshot()
        self.assertEqual(os.readlink(self.home / ".local/bin/plak"), str(snapshot / "plak"))
        self.assertEqual((snapshot / "plak").stat().st_mode & 0o777, 0o555)
        self.assertEqual((snapshot / "state-key").read_text(), "r1.e1.p1.t1\n")
        self.assertEqual((snapshot / "export-version").read_text(), "7\n")
        self.assertEqual((snapshot / "binary-id").read_text(), "fixture\n")
        self.assertEqual((snapshot / "binary-id").stat().st_mode & 0o777, 0o444)
        for name in ("plakous-js-host", "monty"):
            worker = self.lib / "sidecars" / name / sha(executable_bytes(name)) / name
            self.assertEqual(os.readlink(snapshot / name), str(worker))
            self.assertEqual(os.readlink(self.home / ".local/bin" / name), str(worker))
            self.assertEqual(worker.stat().st_mode & 0o777, 0o555)
        for directory in (self.lib, self.lib / "release", self.lib / "sidecars"):
            self.assertEqual(directory.stat().st_mode & 0o777, 0o755)
        # A reinstall of the same release changes nothing.
        before = sorted(str(path) for path in self.lib.rglob("*"))
        result = self.install(PLAK_VERSION=f"v{VERSION}")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sorted(str(path) for path in self.lib.rglob("*")), before)

    def test_same_bytes_share_the_build_plak_snapshot(self) -> None:
        source = (ROOT / "tools/build/build-plak.sh").read_text()
        block = source.split("# BEGIN SNAPSHOT INSTALL", 1)[1].split("# END SNAPSHOT INSTALL", 1)[0]
        artifacts = self.base / "built"
        write_executables(artifacts)
        script = ('set -euo pipefail; bin="$1"; js_host="$2"; monty="$3"; snapshot_flavor=release; '
                  + block + '\nprintf "%s\\n" "$bin" "$js_host" "$monty"')
        built = subprocess.run(["bash", "-c", script, "snapshot",
                                *(str(artifacts / name) for name in EXECUTABLES)],
                               env=self.env, capture_output=True, text=True, check=False)
        self.assertEqual(built.returncode, 0, built.stderr)
        result = self.install(PLAK_VERSION=VERSION)
        self.assertEqual(result.returncode, 0, result.stderr)
        installed = [os.readlink(self.home / ".local/bin" / name) for name in EXECUTABLES]
        self.assertEqual(installed, built.stdout.split())
        self.assertEqual(list((self.lib / "release").iterdir()), [self.expected_snapshot()])

    def test_checksum_mismatch_and_missing_entry_install_nothing(self) -> None:
        sums = self.release / "SHA256SUMS"
        good = sums.read_text()
        sums.write_text("0" * 64 + good[64:])
        result = self.install()
        self.assertEqual(result.returncode, 1)
        self.assertIn("checksum mismatch", result.stderr)
        sums.write_text(good.replace(LINUX, "other"))
        result = self.install()
        self.assertEqual(result.returncode, 1)
        self.assertIn("no entry", result.stderr)
        self.assertFalse((self.home / ".local").exists())

    def test_signed_in_gh_verifies_the_attestation(self) -> None:
        result = self.install(FAKE_GH="1", FAKE_GH_VERIFY="1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("attestation verification failed", result.stderr)
        self.assertFalse((self.home / ".local").exists())
        self.gh_log.write_text("")
        result = self.install(FAKE_GH="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("attestation verified", result.stderr)
        verify = self.gh_log.read_text().splitlines()[-1].split()
        self.assertEqual(verify[:2], ["attestation", "verify"])
        self.assertTrue(verify[2].endswith(f"plak-{VERSION}-{LINUX}.tar.gz"))
        self.assertEqual(verify[3:], ["--repo", "N0xMare/plak"])
        self.gh_log.write_text("")
        result = self.install(FAKE_GH="1", PLAK_SKIP_ATTESTATION="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.gh_log.read_text(), "")

    def test_unsupported_platforms_point_at_a_source_build(self) -> None:
        for os_name, arch in (("Linux", "aarch64"), ("FreeBSD", "amd64")):
            with self.subTest(os=os_name, arch=arch):
                result = self.install(FAKE_OS=os_name, FAKE_ARCH=arch)
                self.assertEqual(result.returncode, 1)
                self.assertIn("build from source", result.stderr)
        self.stub("sysctl", "echo 0")
        result = self.install(FAKE_OS="Darwin", FAKE_ARCH="x86_64")
        self.assertIn("Intel macOS is not supported", result.stderr)
        self.assertFalse((self.home / ".local").exists())

    def test_apple_silicon_selects_the_darwin_tarball(self) -> None:
        make_release(self.base, "aarch64-apple-darwin", "mac")
        self.stub("sysctl", "echo 1")  # an x86_64 shell under Rosetta
        result = self.install(FAKE_OS="Darwin", FAKE_ARCH="x86_64")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(os.readlink(self.home / ".local/bin/plak"),
                         str(self.expected_snapshot("mac") / "plak"))

    def test_foreign_links_and_unsafe_directories_are_refused(self) -> None:
        bin_dir = self.home / ".local/bin"
        bin_dir.mkdir(parents=True)
        (bin_dir / "plak").write_text("foreign")
        result = self.install()
        self.assertEqual(result.returncode, 1)
        self.assertIn("refusing to replace non-Plak", result.stderr)
        (bin_dir / "plak").unlink()
        (bin_dir / "plak").symlink_to("/usr/bin/true")
        result = self.install()
        self.assertIn("refusing to replace non-Plak", result.stderr)
        self.assertEqual(os.readlink(bin_dir / "plak"), "/usr/bin/true")
        (bin_dir / "plak").unlink()
        self.lib.chmod(0o777)
        result = self.install()
        self.assertEqual(result.returncode, 1)
        self.assertIn("writable installation ancestor refused", result.stderr)
        self.assertFalse((bin_dir / "plak").exists())


class PackageReleaseTests(unittest.TestCase):
    """package-release.sh over stub git, cargo and uname in a copied tree."""

    def setUp(self) -> None:
        self.base = pathlib.Path(tempfile.mkdtemp()).resolve()
        self.addCleanup(shutil.rmtree, self.base)
        self.repo = self.base / "repo"
        for path in ("tools/release/package-release.sh", "tools/release/release_tarball.py",
                     "tools/build/build-plak.sh", "scripts/musl-toolchain.sh", "LICENSE"):
            (self.repo / path).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(ROOT / path, self.repo / path)
        # The real check prints a report; stdout must stay the output directory.
        (self.repo / "tools/build/check-artifact-linkage.py").write_text("print('artifact linkage checked')\n")
        self.stubs = self.base / "stubs"
        self.stubs.mkdir()
        self.log = self.base / "calls.log"
        self.metadata = self.base / "metadata.json"
        self.metadata.write_text(json.dumps(fake_metadata(self.base / "meta")))
        self.stub("uname", 'case "$1" in -s) echo "${FAKE_OS:-Linux}" ;; '
                           '-m) echo "${FAKE_ARCH:-x86_64}" ;; esac')
        self.stub("rustup", "echo x86_64-unknown-linux-musl")
        self.stub("musl-gcc", "true")
        self.stub("strip", "true")  # Cargo writes shell-script executables in this fixture.
        self.stub("git", 'case "$1" in status) printf "%s" "${FAKE_DIRTY:-}" ;; '
                         'tag) echo "${FAKE_TAG:-}" ;; log) echo 1700000000 ;; esac')
        writer = "".join(
            f"printf '#!/bin/sh\\n# {name}\\n' >\"$out/{name}\"; chmod 755 \"$out/{name}\"; "
            for name in EXECUTABLES)
        self.stub("cargo", f"""cflags="CFLAGS_${{CARGO_BUILD_TARGET//-/_}}"
printf 'cargo %s target=%s flags=%s cflags=%s\\n' "$*" "$CARGO_BUILD_TARGET" \
"$(printf '%s' "${{CARGO_ENCODED_RUSTFLAGS:-}}" | tr '\\037' ' ')" "${{!cflags:-}}" >>{self.log}
case "$1" in
  metadata)
    case "$*" in *--no-deps*) echo '{{"packages":[{{"name":"plak","version":"{VERSION}"}}]}}' ;;
      *) cat {self.metadata} ;; esac ;;
  build) out="$CARGO_TARGET_DIR/$CARGO_BUILD_TARGET/release"; mkdir -p "$out"; {writer};;
esac""")
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("CARGO_", "RUSTFLAGS", "CC_"))}
        self.env.update(PATH=f"{self.stubs}{os.pathsep}{os.environ['PATH']}",
                        CARGO_TARGET_DIR=str(self.base / "target"),
                        CARGO_HOME=str(self.base), HOME=str(self.base / "home"))

    def stub(self, name: str, body: str) -> None:
        (self.stubs / name).write_text(f"#!/bin/bash\n{body}\n")
        (self.stubs / name).chmod(0o755)

    def run_script(self, *args: str, **env: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(["bash", str(self.repo / "tools/release/package-release.sh"), *args],
                              env={**self.env, **env}, capture_output=True, text=True,
                              timeout=60, check=False)

    def test_refuses_a_dirty_tree_a_missing_tag_and_unsupported_hosts(self) -> None:
        result = self.run_script("--untagged", FAKE_DIRTY=" M Cargo.toml")
        self.assertEqual(result.returncode, 2)
        self.assertIn("not clean", result.stderr)
        result = self.run_script(FAKE_TAG="v0.0.0")
        self.assertEqual(result.returncode, 2)
        self.assertIn(f"not tagged v{VERSION}", result.stderr)
        result = self.run_script("--untagged", FAKE_OS="Darwin", FAKE_ARCH="x86_64")
        self.assertEqual(result.returncode, 2)
        self.assertIn("Apple Silicon", result.stderr)
        result = self.run_script("--untagged", RUSTFLAGS="-Copt-level=1")
        self.assertEqual(result.returncode, 2)
        self.assertIn("CARGO_ENCODED_RUSTFLAGS", result.stderr)
        self.assertFalse((self.base / "target").exists())

    def test_tagged_linux_and_darwin_builds_pack_with_remapped_paths(self) -> None:
        for os_name, arch, triple in (("Linux", "x86_64", LINUX),
                                      ("Darwin", "arm64", "aarch64-apple-darwin")):
            with self.subTest(triple=triple):
                self.log.write_text("")
                result = self.run_script(FAKE_TAG=f"v{VERSION}", FAKE_OS=os_name, FAKE_ARCH=arch)
                self.assertEqual(result.returncode, 0, result.stderr)
                dist = self.base / "target/release-dist"
                self.assertEqual(result.stdout.strip(),
                                 str(dist / f"plak-{VERSION}-{triple}.tar.gz"))
                builds = [line for line in self.log.read_text().splitlines()
                          if line.startswith("cargo build")]
                self.assertEqual(len(builds), 2)
                maps = (f"{self.base}=/cargo", f"{self.repo}=/plak", f"{self.base}/target=/target")
                rust = " ".join(f"--remap-path-prefix={m}" for m in maps)
                c = " ".join(f"-ffile-prefix-map={m}" for m in maps)
                for line in builds:
                    self.assertIn(f"--profile release target={triple} flags={rust} cflags={c}",
                                  line)
                    self.assertNotIn("maintainer", line)
        sums = (self.base / "target/release-dist/SHA256SUMS").read_text().splitlines()
        self.assertEqual(len(sums), 2)

    def test_a_release_candidate_tag_names_the_tarball_and_other_tags_are_refused(self) -> None:
        result = self.run_script("--tag", f"v{VERSION}-rc.2", FAKE_TAG=f"v{VERSION}-rc.2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(
            self.base / f"target/release-dist/plak-{VERSION}-rc.2-{LINUX}.tar.gz"))
        for tag, head_tag, message in (
                (f"v{VERSION}-rc.0", f"v{VERSION}-rc.0", "is neither"),
                (f"v{VERSION}-beta.1", f"v{VERSION}-beta.1", "is neither"),
                ("v9.9.9", "v9.9.9", "is neither"),
                (f"v{VERSION}-rc.3", f"v{VERSION}-rc.2", f"not tagged v{VERSION}-rc.3")):
            with self.subTest(tag=tag):
                result = self.run_script("--tag", tag, FAKE_TAG=head_tag)
                self.assertEqual(result.returncode, 2)
                self.assertIn(message, result.stderr)
        result = self.run_script("--untagged", "--tag", f"v{VERSION}")
        self.assertEqual(result.returncode, 2)
        self.assertIn("exclude each other", result.stderr)


class ReleaseWorkflowTests(unittest.TestCase):
    def test_the_only_workflow_runs_on_v_tags_with_pinned_actions_and_never_publishes_crates(self) -> None:
        import re
        workflows = ROOT / ".github/workflows"
        self.assertEqual(sorted(path.name for path in workflows.iterdir()), ["release.yml"])
        text = (workflows / "release.yml").read_text()
        trigger = text.split("\non:\n", 1)[1].split("\n\n", 1)[0]
        self.assertEqual([line for line in trigger.splitlines() if not line.startswith("    ")],
                         ["  push:"])
        self.assertIn('    tags: ["v*"]', trigger)
        uses = re.findall(r"uses: (\S+)", text)
        self.assertTrue(uses)
        for action in uses:
            self.assertRegex(action, r"^actions/[a-z-]+@[0-9a-f]{40}$")
        self.assertIn("tools/release/package-release.sh", text)
        for publish in ("publish.sh", "cargo publish", "CARGO_REGISTRY_TOKEN", "workflow_dispatch"):
            self.assertNotIn(publish, text)


if __name__ == "__main__":
    unittest.main()
