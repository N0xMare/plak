#!/usr/bin/env bash
# Developer gate: check (default), fast iteration, platform, full.
# Usage: tools/gate/dev-check.sh [--fast [PACKAGE...] | --platform | --full |
#                            --steps GROUP[,GROUP...]]
# --steps selects exactly the named groups, in canonical order.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd -P)"
cd "$root"

# Canonical group order; every tier runs its groups in this order.
all_groups=(static builds variants rustdoc tests-parallel tests-serial tests-durability contracts tooling package)
check_groups=static,builds,tests-parallel,tests-serial,contracts,tooling
platform_groups=static,builds,tests-parallel,tests-serial,tests-durability

tier=check
selection=$check_groups
packages=()
case "${1:-}" in
  --fast) tier=fast; selection=static,tests-parallel; shift; packages=("$@"); set -- ;;
  --platform) tier=platform; selection=$platform_groups; shift ;;
  --full) tier=full; selection="$(IFS=,; echo "${all_groups[*]}")"; shift ;;
  --steps)
    tier=steps; selection="${2:-}"; shift
    if [[ $# -gt 0 ]]; then shift; fi ;;
  -h|--help) sed -n '2,5p' "$0"; exit 0 ;;
esac
if [[ $# -gt 0 ]]; then echo "unknown argument: $1" >&2; exit 2; fi
if [[ -z "$selection" || "$selection" == ,* || "$selection" == *, || "$selection" == *,,* ]]; then
  echo "expected non-empty comma-separated step groups" >&2; exit 2
fi
IFS=, read -r -a requested <<<"$selection"
for group in "${requested[@]}"; do
  known=false
  for candidate in "${all_groups[@]}"; do
    if [[ "$group" == "$candidate" ]]; then known=true; break; fi
  done
  if [[ "$known" != true ]]; then echo "unknown step group: $group" >&2; exit 2; fi
done

source "$root/tools/gate/dev-tools-prefix.sh"
export PATH="$(plak_dev_tools_prefix "$root")/bin:$PATH"
bash tools/gate/dev-tools.sh check

# Linux budget tests need RAM-backed fsync under parallel gate load; macOS
# NOFOLLOW needs a physical path outside HOME. Placement changes, not deadlines.
# PLAK_HOME is applied only on nextest lanes: other cargo must not inherit it.
# Git in test fixtures must not read the operator's global or system config
# (a global commit.gpgsign made fixture commits spawn gpg). A test that needs
# an identity sets it on its own command or repository. Hand-run nextest is
# `just nextest`, which keeps PLAK_HOME exported.
# Assign first. `eval "$(test-env.sh)"` ignores a failing helper under set -e.
test_env="$(tools/gate/test-env.sh --fresh)"
eval "$test_env"
export TMPDIR GIT_CONFIG_GLOBAL GIT_CONFIG_NOSYSTEM
isolated_home=$PLAK_HOME
unset PLAK_HOME

# Supported packages (spec 06 §3). Each also builds alone in the builds group:
# Cargo resolves features per selected package set, so `cargo build -p plak`
# (the release resolution) can compile a closure no workspace build sees.
supported_packages=(plakous plakous-agent plakous-model plakous-tools plak)

# Fast narrows only the parallel lane, to the touched packages and their
# reverse dependencies. Test compilation stays workspace all-features:
# -p compilation would resolve a different feature graph.
scope=''
if [[ "$tier" == fast ]]; then
  if [[ ${#packages[@]} -eq 0 ]]; then
    package_file="$(mktemp "$TMPDIR/plak-packages.XXXXXX")"
    if ! python3 tools/gate/changed_packages.py >"$package_file"; then
      rm -f "$package_file"; exit 2
    fi
    while IFS= read -r name; do packages+=("$name"); done <"$package_file"
    rm -f "$package_file"
  fi
  if [[ ${#packages[@]} -eq 0 ]]; then
    selection=static
  elif [[ "${packages[*]}" != --workspace ]]; then
    for name in "${packages[@]}"; do scope+="${scope:+ | }rdeps(=$name)"; done
  fi
  echo "fast: ${packages[*]:-no Rust package touched}"
fi

# Bash 3.2 nounset rejects quoted empty arrays; guard their expansions.
target_args=()
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then target_args=(--target "$CARGO_BUILD_TARGET"); fi
target_dir="${CARGO_TARGET_DIR:-$PWD/target}"
case "$target_dir" in
  /*) ;;
  *) target_dir="$PWD/$target_dir" ;;
esac
py_host=''
js_host=''
prepare_tests() {
  if [[ -n "$py_host" ]]; then return; fi
  worker_manifest="$(mktemp "$TMPDIR/plak-worker-build.XXXXXX")"
  trap 'rm -f "$worker_manifest"' EXIT
  if [[ "${PLAK_PY_HOST+x}" == x && "${PLAK_JS_HOST+x}" == x ]]; then
    worker_mode=explicit
    worker_input=''
  else
    worker_mode=built
    # The worker comes out of the test build itself. A `cargo build` would leave
    # dev-dependency features off and compile a second variant of the graph;
    # this way nextest finds everything built and the tests run this worker.
    cargo test --no-run --locked --workspace --all-features --message-format=json ${target_args[@]+"${target_args[@]}"} >"$worker_manifest"
    worker_input="$worker_manifest"
  fi
  worker_paths="$(python3 - "$worker_mode" "$worker_input" <<'PY_WORKER'
import json
import os
from pathlib import Path
import sys

mode, manifest = sys.argv[1:]
records = ([json.loads(line) for line in Path(manifest).read_text().splitlines() if line.strip()]
           if mode == "built" else [])
for variable, binary in [("PLAK_PY_HOST", "plakous-py-host"), ("PLAK_JS_HOST", "plakous-js-host")]:
    if variable in os.environ:
        value = os.environ[variable]
    else:
        paths = {row["executable"] for row in records
                 if row.get("reason") == "compiler-artifact"
                 and row.get("target", {}).get("name") == binary
                 and "bin" in row.get("target", {}).get("kind", []) and row.get("executable")
                 and not row.get("profile", {}).get("test")}
        if len(paths) != 1:
            raise SystemExit(f"gate requires exactly one built {binary} executable")
        value = paths.pop()
    if not value:
        raise SystemExit(f"{variable} must name an executable file")
    try:
        path = Path(value).resolve(strict=True)
    except OSError as error:
        raise SystemExit(f"{variable} must name an executable cell worker: {error}") from error
    if not path.is_file() or not os.access(path, os.X_OK):
        raise SystemExit(f"{variable} must name an executable cell worker")
    print(path)
PY_WORKER
  )"
  py_host="$(printf '%s\n' "$worker_paths" | sed -n '1p')"
  js_host="$(printf '%s\n' "$worker_paths" | sed -n '2p')"
  rm -f "$worker_manifest"
  trap - EXIT
}

# Rustflags Cargo would take from config for this triple, space-separated.
# Empty when none are configured. Cargo keeps a single rustflags source, so a
# later `RUSTFLAGS=-D warnings` would drop a configured linker; callers copy
# this value into that variable instead. The same config path is read once.
# `CARGO_BUILD_RUSTFLAGS` merges with the file's `build.rustflags`, as Cargo does.
configured_rustflags() {
  python3 - "$1" "$PWD" <<'PY'
import os
import subprocess
import sys
from pathlib import Path

try:
    import tomllib
except ImportError:
    raise SystemExit("python 3.11+ is required to read Cargo rustflags")

triple, cwd = sys.argv[1], Path(sys.argv[2])

def load(path):
    try:
        with path.open("rb") as handle:
            return tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError):
        return {}

def flags_of(table):
    if not isinstance(table, dict) or "rustflags" not in table:
        return None
    value = table["rustflags"]
    if isinstance(value, str):
        return value.split()
    if isinstance(value, list) and all(isinstance(item, str) for item in value):
        return list(value)
    raise SystemExit(f"rustflags must be a string or array of strings, not {value!r}")

def configs():
    found = []
    seen = set()

    def consider(path):
        if not path.is_file():
            return
        try:
            resolved = path.resolve()
        except OSError:
            resolved = path
        if resolved in seen:
            return
        seen.add(resolved)
        found.append(load(path))

    home = os.environ.get("CARGO_HOME")
    if not home:
        user = os.environ.get("HOME")
        home = str(Path(user) / ".cargo") if user else ""
    if home:
        directory = Path(home)
        toml = directory / "config.toml"
        legacy = directory / "config"
        if toml.is_file():
            consider(toml)
        elif legacy.is_file():
            consider(legacy)
    ancestors = []
    current = cwd
    while True:
        ancestors.append(current)
        if current.parent == current:
            break
        current = current.parent
    for directory in reversed(ancestors):
        cargo = directory / ".cargo"
        toml = cargo / "config.toml"
        legacy = cargo / "config"
        if toml.is_file():
            consider(toml)
        elif legacy.is_file():
            consider(legacy)
    return found

def rustc_cfg():
    if not triple:
        return set(), {}
    result = subprocess.run(
        ["rustc", "--target", triple, "--print", "cfg"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    atoms, pairs = set(), {}
    if result.returncode != 0:
        return atoms, pairs
    for line in result.stdout.splitlines():
        if "=" not in line:
            atoms.add(line.strip())
            continue
        key, value = line.split("=", 1)
        pairs[key.strip()] = value.strip().strip('"')
    return atoms, pairs

class Parser:
    def __init__(self, text):
        self.text = text
        self.index = 0

    def parse(self):
        self.skip()
        if not self.take("cfg"):
            raise SystemExit(f"target rustflags key must be cfg(...): {self.text}")
        self.skip()
        self.expect("(")
        node = self.expr()
        self.expect(")")
        self.skip()
        if self.index != len(self.text):
            raise SystemExit(f"unparsed target cfg: {self.text}")
        return node

    def expr(self):
        self.skip()
        if self.take("all"):
            return ("all", self.group())
        if self.take("any"):
            return ("any", self.group())
        if self.take("not"):
            self.skip()
            self.expect("(")
            node = self.expr()
            self.expect(")")
            return ("not", node)
        name = self.ident()
        self.skip()
        if self.take("="):
            self.skip()
            return ("eq", name, self.string())
        return ("atom", name)

    def group(self):
        self.skip()
        self.expect("(")
        nodes = []
        while True:
            nodes.append(self.expr())
            self.skip()
            if self.take(","):
                continue
            break
        self.expect(")")
        return nodes

    def ident(self):
        start = self.index
        while self.index < len(self.text) and (self.text[self.index].isalnum() or self.text[self.index] == "_"):
            self.index += 1
        if start == self.index:
            raise SystemExit(f"expected cfg identifier in {self.text}")
        return self.text[start:self.index]

    def string(self):
        self.expect('"')
        start = self.index
        while self.index < len(self.text) and self.text[self.index] != '"':
            self.index += 1
        value = self.text[start:self.index]
        self.expect('"')
        return value

    def expect(self, token):
        if not self.take(token):
            raise SystemExit(f"expected {token!r} in target cfg {self.text}")

    def take(self, token):
        if self.text.startswith(token, self.index):
            self.index += len(token)
            return True
        return False

    def skip(self):
        while self.index < len(self.text) and self.text[self.index].isspace():
            self.index += 1

def matches(node, atoms, pairs):
    kind = node[0]
    if kind == "atom":
        return node[1] in atoms
    if kind == "eq":
        return pairs.get(node[1]) == node[2]
    if kind == "not":
        return not matches(node[1], atoms, pairs)
    if kind == "all":
        return all(matches(child, atoms, pairs) for child in node[1])
    if kind == "any":
        return any(matches(child, atoms, pairs) for child in node[1])
    raise SystemExit(f"unknown cfg form {node!r}")

atoms, pairs = rustc_cfg()
target_flags = []
build_flags = []
saw_target = False
for document in configs():
    target = document.get("target")
    if isinstance(target, dict):
        for key, table in target.items():
            if not isinstance(key, str):
                continue
            selected = key == triple or (
                key.startswith("cfg(") and matches(Parser(key).parse(), atoms, pairs)
            )
            if not selected:
                continue
            parsed = flags_of(table)
            if parsed is None:
                continue
            saw_target = True
            target_flags.extend(parsed)
    build = document.get("build")
    parsed = flags_of(build) if isinstance(build, dict) else None
    if parsed:
        build_flags.extend(parsed)
# CARGO_BUILD_RUSTFLAGS is the env form of build.rustflags; Cargo merges it
# after the file value. Target rustflags, including their env form, still win.
build_env = os.environ.get("CARGO_BUILD_RUSTFLAGS")
if build_env is not None:
    build_flags = build_flags + build_env.split()
chosen = target_flags if saw_target else build_flags
if any(any(character.isspace() for character in flag) for flag in chosen):
    raise SystemExit(
        "configured rustflags contain whitespace; list them as separate strings so warning denial can keep them"
    )
sys.stdout.write(" ".join(chosen))
PY
}

# Deny rustc warnings on one build. Cargo uses one rustflags source, in order:
# CARGO_ENCODED_RUSTFLAGS, RUSTFLAGS, then all matching target.<triple> and
# target.<cfg> rustflags including CARGO_TARGET_<triple>_RUSTFLAGS (that env
# form merges with the file's target rustflags), then build.rustflags.
# `-D warnings` is appended to the source Cargo will use. Copying configured
# flags into RUSTFLAGS keeps a configured linker; the addition is process-local,
# so the all-features test build keeps its own artifact set. Registry crates
# are capped at allow; only workspace crates fail closed.
run_build_denying_warnings() {
  local key="" triple="${CARGO_BUILD_TARGET:-}"
  if [[ -z "$triple" ]]; then triple="$(rustc -vV | sed -n 's/^host: //p' || true)"; fi
  if [[ -n "$triple" ]]; then
    key="CARGO_TARGET_$(printf '%s' "$triple" | tr '[:lower:]-' '[:upper:]_')_RUSTFLAGS"
  fi
  if [[ -n "${CARGO_ENCODED_RUSTFLAGS+x}" ]]; then
    env "CARGO_ENCODED_RUSTFLAGS=${CARGO_ENCODED_RUSTFLAGS}"$'\x1f'"-D"$'\x1f'"warnings" "$@"
  elif [[ -n "${RUSTFLAGS+x}" ]]; then
    if [[ -n "$RUSTFLAGS" ]]; then
      env "RUSTFLAGS=${RUSTFLAGS} -D warnings" "$@"
    else
      env RUSTFLAGS="-D warnings" "$@"
    fi
  elif [[ -n "$key" && -n "${!key+x}" ]]; then
    local current="${!key}"
    if [[ -n "$current" ]]; then
      env "$key=$current -D warnings" "$@"
    else
      env "$key=-D warnings" "$@"
    fi
  else
    local configured=""
    configured="$(configured_rustflags "$triple")"
    if [[ -n "$configured" ]]; then
      env "RUSTFLAGS=${configured} -D warnings" "$@"
    else
      env RUSTFLAGS="-D warnings" "$@"
    fi
  fi
}

# One nextest lane: a profile from .config/nextest.toml plus extra arguments.
# Serial lanes pass --test-threads=1 explicitly so NEXTEST_TEST_THREADS in the
# environment cannot widen them.
run_lane() {
  local profile=$1
  shift
  prepare_tests
  PLAK_HOME="$isolated_home" PLAK_PY_HOST="$py_host" PLAK_JS_HOST="$js_host" cargo nextest run --locked --workspace --all-features \
    --profile "$profile" ${target_args[@]+"${target_args[@]}"} "$@"
}

run_group() {
  case "$1" in
    static)
      local failed_steps=""
      echo "== fmt =="
      cargo fmt --all --check || failed_steps="fmt"
      # Ruff covers the maintainer Python trees. vendor/ and Harbor task
      # fixtures are excluded in the root tool config, not here.
      echo "== ruff =="
      ruff check tools tests/tooling tests/live eval \
        || failed_steps="${failed_steps:+$failed_steps }ruff"
      ruff format --check tools tests/tooling tests/live eval \
        || failed_steps="${failed_steps:+$failed_steps }ruff-format"
      if [[ "$tier" == platform ]]; then
        echo "skip structure checkers: platform-independent (check/full on Linux)"
      else
        echo "== dependencies: fetch locked (offline first) =="
        cargo fetch --locked --offline || cargo fetch --locked \
          || failed_steps="${failed_steps:+$failed_steps }fetch"
        echo "== structure: workspace =="
        python3 tools/gate/check_workspace.py || failed_steps="${failed_steps:+$failed_steps }workspace"
        echo "== structure: features (static) =="
        python3 tools/gate/check_features.py || failed_steps="${failed_steps:+$failed_steps }features"
        echo "== structure: evidence allowlist =="
        python3 tools/release/gate_evidence.py audit || failed_steps="${failed_steps:+$failed_steps }evidence"
      fi
      # Workspace, then each supported package alone (tools/gate/clippy.sh).
      clippy_status="$(mktemp "$TMPDIR/plak-clippy.XXXXXX")"
      if ! tools/gate/clippy.sh --status-file "$clippy_status"; then
        s="$(cat "$clippy_status")"
        failed_steps="${failed_steps:+$failed_steps }${s:-clippy}"
      fi
      rm -f "$clippy_status"
      if [[ -n "$failed_steps" ]]; then
        echo "== failed static steps: $failed_steps =="
        return 1
      fi ;;
    builds)
      echo "== default features =="
      run_build_denying_warnings cargo build --locked --workspace --lib --bins ${target_args[@]+"${target_args[@]}"}
      # Before the maintainer build: the linkage check reads that build's plak.
      for package in "${supported_packages[@]}"; do
        echo "== $package alone: its own default features =="
        run_build_denying_warnings cargo build --locked -p "$package" --lib --bins ${target_args[@]+"${target_args[@]}"}
      done
      echo "== maintainer features =="
      run_build_denying_warnings cargo build --locked -p plak --features maintainer --bins ${target_args[@]+"${target_args[@]}"}
      python3 tools/build/check-artifact-linkage.py "$target_dir/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}debug" ;;
    variants)
      # Release-candidate evidence (`just full` or an explicit variants step).
      # The integration batch does not build these reduced resolutions.
      echo "== product feature-off builds and clippy: warnings denied =="
      # A shared sccache server keeps the TMPDIR it started with; if that is a
      # gate test directory, --fresh can delete its dep-info scratch mid-compile.
      # Lane runners pin the server to a stable TMPDIR (spec 06); this group
      # also bypasses the wrapper. Empty overrides Cargo's configured wrapper
      # too, and only this subshell loses caching.
      export RUSTC_WRAPPER=
      run_build_denying_warnings python3 tools/gate/check_features.py --product-builds ${target_args[@]+"${target_args[@]}"} ;;
    rustdoc)
      echo "== rustdoc =="
      RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked ${target_args[@]+"${target_args[@]}"}
      for package in "${supported_packages[@]}"; do
        echo "== rustdoc: $package alone =="
        RUSTDOCFLAGS="-D warnings" cargo doc -p "$package" --no-deps --locked ${target_args[@]+"${target_args[@]}"}
      done ;;
    tests-parallel)
      echo "-- nextest: parallel lane (profile offline) --"
      if [[ -n "$scope" ]]; then
        run_lane offline --no-tests=pass -E "$scope"
      else
        run_lane offline
      fi ;;
    tests-serial)
      echo "-- nextest: serial lane (profile serial, wall-clock budgets, one thread) --"
      run_lane serial --test-threads=1 ;;
    tests-durability)
      echo "-- nextest: durability lane (profile durability, crash matrices, one thread) --"
      run_lane durability --test-threads=1 ;;
    contracts)
      echo "== positive/negative public compile contracts =="
      python3 tools/gate/check_public_compile.py ;;
    tooling)
      echo "== structure: checker unit tests =="
      python3 tools/gate/run_tooling_tests.py -v ;;
    package)
      # Publication packages a git tree; the musl container mounts an export.
      if [[ "$(python3 tools/gate/vcs_listing.py kind)" == export ]]; then
        echo "skip package: not a git work tree (the dry run stages tracked files)"
        return 0
      fi
      echo "== publish dry run: prefetch inputs, then verify the cohort offline =="
      tools/release/publish.sh --gate ;;
    *) echo "unimplemented step group: $1" >&2; exit 2 ;;
  esac
}

# A passing full tier on a clean tree records that tree as publication
# evidence, and a passing package group (full, or `--steps` naming it) records
# package evidence. tools/release/publish.sh refuses a tree neither covers
# (tools/release/gate_evidence.py: documentation-only deltas keep the full
# evidence; a packaged-file delta also needs package evidence for HEAD).
evidence_tree=''
if [[ "$tier" == full || ",$selection," == *",package,"* ]]; then
  evidence_tree="$(python3 tools/gate/vcs_listing.py evidence-tree)"
fi

failed_groups=""
gate_status=0
run_timed() {
  local group=$1 started status worker_state
  echo "== group: $group =="
  started=$SECONDS
  # A subshell keeps set -e fail-fast inside the group; the parent records the
  # status and goes on to the next group. Prepared worker paths survive a
  # failed lane so later lanes reuse the same build.
  worker_state="$(mktemp)"
  printf '%s\n' "$py_host" "$js_host" >"$worker_state"
  set +e
  (
    set -Ee
    py_host="$(sed -n '1p' "$worker_state")"
    js_host="$(sed -n '2p' "$worker_state")"
    trap 'printf "%s\n" "$py_host" "$js_host" >"$worker_state"' ERR
    run_group "$group"
    printf '%s\n' "$py_host" "$js_host" >"$worker_state"
  )
  status=$?
  set -e
  py_host="$(sed -n '1p' "$worker_state")"
  js_host="$(sed -n '2p' "$worker_state")"
  rm -f "$worker_state"
  echo "== time: $group $((SECONDS - started)) s =="
  if [[ "$status" -ne 0 ]]; then
    failed_groups="${failed_groups:+$failed_groups }$group"
    if [[ "$gate_status" -eq 0 ]]; then gate_status=$status; fi
  fi
}

for group in "${all_groups[@]}"; do
  if [[ ",$selection," == *",$group,"* ]]; then
    run_timed "$group"
  fi
done
if [[ "$gate_status" -ne 0 ]]; then
  echo "== failed groups: $failed_groups =="
  exit "$gate_status"
fi
if [[ -n "$evidence_tree" ]]; then
  if [[ "$(python3 tools/gate/vcs_listing.py evidence-tree)" == "$evidence_tree" ]]; then
    mkdir -p "$target_dir/gate-evidence"
    printf '%s\n' "$evidence_tree" >"$target_dir/gate-evidence/package"
    echo "== evidence: package group passed on tree $evidence_tree =="
    if [[ "$tier" == full ]]; then
      printf '%s\n' "$evidence_tree" >"$target_dir/gate-evidence/full"
      echo "== evidence: full gate passed on tree $evidence_tree =="
    fi
  else
    echo "== no evidence: the tree changed during the run ==" >&2
  fi
elif [[ "$tier" == full || ",$selection," == *",package,"* ]]; then
  echo "== no evidence: evidence requires a clean git tree or unconflicted colocated jj root with a .git directory =="
fi
echo "== ok: $tier gate passed =="
