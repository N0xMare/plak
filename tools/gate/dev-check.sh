#!/usr/bin/env bash
# Developer gate: batch (default), fast iteration, Mac batch, install, full.
# Usage: tools/gate/dev-check.sh [--qual-container] [--fast [--rdeps] [PACKAGE...] |
#                            --batch | --batch-mac | --install | --install-checks | --full | --soak |
#                            --steps GROUP[,GROUP...]]
# --steps selects named groups in order; PLAK_BATCH_BASE enables batch scoping.
# --qual-container is passed only by tools/musl/musl-check.sh (spec 06 section 7.5).
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd -P)"
cd "$root"

# Canonical group order, including replay of overlapped groups.
all_groups=(static builds variants rustdoc rustdoc-alone standalone contracts tooling tests-parallel tests-durability tests-serial tests-install package)
batch_groups=static,standalone,contracts,tooling,tests-parallel,tests-durability,tests-serial

tier=batch
selection=$batch_groups
packages=()
fast_test_match=package
# The musl qualification image lacks the wasm target; only that container may
# skip the wasm check, and a run that skips records no evidence.
qual_container=false
if [[ "${1:-}" == --qual-container ]]; then qual_container=true; shift; fi
case "${1:-}" in
  --fast)
    tier=fast; selection=static,tests-parallel; shift
    if [[ "${1:-}" == --rdeps ]]; then fast_test_match=rdeps; shift; fi
    packages=("$@"); set -- ;;
  --batch) shift ;;
  --batch-mac) tier=batch-mac; selection=static,tests-parallel,tests-durability,tests-serial; shift ;;
  --install) tier=install; selection=builds,rustdoc,tooling,tests-parallel,tests-install; shift ;;
  --install-checks) tier=install-checks; selection=tests-parallel,tests-durability,tests-serial,tests-install; shift ;;
  --full) tier=full; selection="$(IFS=,; echo "${all_groups[*]}")"; shift ;;
  --soak) tier=soak; selection=tests-serial,tests-durability; shift ;;
  --steps)
    tier=steps; selection="${2:-}"; shift
    if [[ $# -gt 0 ]]; then shift; fi ;;
  -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
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

nextest_partition=()
if [[ -n "${PLAK_NEXTEST_PARTITION:-}" ]]; then
  case "$tier:,$selection," in
    steps:*,tests-parallel,*|steps:*,tests-durability,*) ;;
    *) echo 'PLAK_NEXTEST_PARTITION requires --steps with tests-parallel or tests-durability' >&2; exit 2 ;;
  esac
  nextest_partition=(--partition "$PLAK_NEXTEST_PARTITION")
fi

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

# Supported packages (spec 06 §3) lint all targets standalone, and have
# a separate default-resolution doc build at release.
supported_packages=(plakous plakous-agent plakous-model plakous-tools plakd)

# Fast lints reverse dependencies but defaults to the changed packages' own
# tests. Keep the main test build shared with batch and the real workers.
scope=''
mac_scope=''
mac_serial_scope=''
install_scope=''
tooling_args=(-v)
worker_clippy=()
worker_features=()
if [[ "$tier" == fast ]]; then
  if [[ ${#packages[@]} -eq 0 ]]; then
    package_file="$(mktemp "$TMPDIR/plakd-packages.XXXXXX")"
    if ! python3 tools/gate/changed_packages.py >"$package_file"; then
      rm -f "$package_file"; exit 2
    fi
    while IFS= read -r name; do packages+=("$name"); done <"$package_file"
    rm -f "$package_file"
  fi
  if [[ ${#packages[@]} -eq 0 ]]; then
    selection=static
  elif [[ "${packages[*]}" != --workspace ]]; then
    for name in "${packages[@]}"; do scope+="${scope:+ | }$fast_test_match(=$name)"; done
  fi
  echo "fast: ${packages[*]:-no Rust package touched}"
  if [[ "${packages[*]:-}" != --workspace ]]; then
    worker_clippy=(--packages)
    worker_features=(--library-packages)
    if [[ ${#packages[@]} -gt 0 ]]; then
      package_file="$(mktemp "$TMPDIR/plakd-rdeps.XXXXXX")"
      if ! python3 tools/gate/changed_packages.py --dependents "${packages[@]}" >"$package_file"; then
        rm -f "$package_file"; exit 2
      fi
      while IFS= read -r name; do
        [[ -n "$name" ]] || continue
        worker_clippy+=("$name")
        worker_features+=("$name")
      done <"$package_file"
      rm -f "$package_file"
    fi
  fi
fi

# Explicit fan-out steps with a batch base share named batch policy.
batch_scoped=false
if [[ "$tier" == batch || ( "$tier" == steps && -n "${PLAK_BATCH_BASE:-}" ) ]]; then
  batch_scoped=true
  tooling_args+=(--batch)
  # The serial runner owns this extra pass even when the other lanes are
  # fanned out. An explicit tests-install is always the complete profile.
  if [[ ",$selection," == *",tests-serial,"* && ",$selection," != *",tests-install,"* ]]; then
    install_scope="$(python3 tools/gate/changed_packages.py --install-filter)"
    if [[ -n "$install_scope" ]]; then selection+=,tests-install; fi
  fi
fi
if [[ "$tier" == batch-mac || ( "$batch_scoped" == true && ",$selection," == *",tests-parallel,"* ) ]]; then
  scope="$(python3 tools/gate/changed_packages.py --batch-filter)"
  echo "batch test scope: $scope"
fi
if [[ "$tier" == batch-mac ]]; then
  mac_filter=''; mac_serial_filter=''; section=parallel
  while IFS= read -r line; do
    case "$line" in
      '# serial') section=serial ;;
      '#'*| '') ;;
      *)
        if [[ "$section" == parallel ]]; then
          mac_filter+="${mac_filter:+ | }$line"
        else
          mac_serial_filter+="${mac_serial_filter:+ | }$line"
        fi ;;
    esac
  done <tools/gate/mac-selection.filter
  [[ -n "$mac_filter" && -n "$mac_serial_filter" ]] || { echo 'missing macOS test filter' >&2; exit 2; }
  mac_scope="($mac_filter) & ($scope)"
  mac_serial_scope="($mac_serial_filter) & ($scope)"
fi

# Bash 3.2 nounset rejects quoted empty arrays; guard their expansions.
target_args=()
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then target_args=(--target "$CARGO_BUILD_TARGET"); fi
target_dir="${CARGO_TARGET_DIR:-$PWD/target}"
case "$target_dir" in
  /*) ;;
  *) target_dir="$PWD/$target_dir" ;;
esac
jobs="${CARGO_BUILD_JOBS:-$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN)}"
if [[ ! "$jobs" =~ ^[1-9][0-9]*$ ]]; then jobs=1; fi
py_host=''
js_host=''
mcp_server=''
plak_host=''
prepare_tests() {
  # Validate explicit workers before a tooling-only build as well.
  if [[ "${1:-}" == tooling && -z "$py_host" && "${PLAK_PY_HOST+x}" == x && "${PLAK_JS_HOST+x}" == x ]]; then
    prepare_tests
  fi
  if [[ -n "$py_host" && ( "${1:-}" != tooling || -n "$plak_host" ) ]]; then return; fi
  worker_manifest="$(mktemp "$TMPDIR/plakd-worker-build.XXXXXX")"
  trap 'rm -f "$worker_manifest"' EXIT
  if [[ "${PLAK_PY_HOST+x}" == x && "${PLAK_JS_HOST+x}" == x && "${PLAK_MCP_SERVER+x}" == x && "${PLAK_TEST_BINARY+x}" == x && "${1:-}" != tooling ]]; then
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
executables = [("PLAK_PY_HOST", "plakous-py-host"), ("PLAK_JS_HOST", "plakous-js-host"),
               ("PLAK_MCP_SERVER", "mcp-fake-server"), ("PLAK_TEST_BINARY", "plakd")]
for variable, binary in executables:
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
        raise SystemExit(f"{variable} must name an executable: {error}") from error
    if not path.is_file() or not os.access(path, os.X_OK):
        raise SystemExit(f"{variable} must name an executable")
    print(path)
PY_WORKER
  )"
  py_host="$(printf '%s\n' "$worker_paths" | sed -n '1p')"
  js_host="$(printf '%s\n' "$worker_paths" | sed -n '2p')"
  mcp_server="$(printf '%s\n' "$worker_paths" | sed -n '3p')"
  plak_host="$(printf '%s\n' "$worker_paths" | sed -n '4p')"
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
# Wall-clock lanes pass their reviewed thread counts explicitly so
# NEXTEST_TEST_THREADS cannot change their scheduling. Ignored soaks stay alone.
run_lane() {
  local profile=$1
  shift
  case "$profile" in
    offline|durability) set -- ${nextest_partition[@]+"${nextest_partition[@]}"} "$@" ;;
  esac
  PLAK_HOME="$isolated_home" PLAK_PY_HOST="$py_host" PLAK_JS_HOST="$js_host" PLAK_MCP_SERVER="$mcp_server" PLAK_TEST_BINARY="$plak_host" python3 tools/gate/test-processes.py -- cargo nextest run --locked --workspace --all-features \
    --profile "$profile" ${target_args[@]+"${target_args[@]}"} "$@"
}

run_weighted_lane() {
  local profile=$1 config threads
  shift
  config="$TMPDIR/nextest-$profile.toml"
  threads="$(python3 tools/gate/nextest_layout.py .config/nextest.toml "$config" --profile "$profile")" || return
  echo "-- $profile pool: $threads slots --"
  run_lane "$profile" --config-file "$config" --test-threads="$threads" "$@"
}

# The rustc target for the portable-crate library check (spec 06 section 7.5).
# Not inherited from CARGO_BUILD_TARGET: a musl lane must not redirect it.
wasm_portability_target=wasm32-unknown-unknown

wasm_target_installed() {
  local sysroot
  sysroot="$(rustc --print sysroot)" || return 1
  [[ -d "$sysroot/lib/rustlib/$1/lib" ]]
}

run_wasm_portability_checks() {
  local triple="$wasm_portability_target" crates crate
  if ! wasm_target_installed "$triple"; then
    if [[ "$qual_container" == true ]]; then
      echo "skip ${triple}: the preloaded musl qualification image does not include this rustc target (spec 06 section 7.5)"
      printf '%s\n' "${triple} portable check (spec 06 §7.5)" >>"$skip_record"
      return 0
    fi
    echo "error: rustc target ${triple} is not installed for the pinned toolchain; run \`just dev-tools install\`" >&2
    return 1
  fi
  crates="$(python3 tools/gate/check_workspace.py --wasm-libraries)" || return 1
  if [[ -z "${crates//[[:space:]]/}" ]]; then
    echo "error: the portable crate set is empty" >&2
    return 1
  fi
  while IFS= read -r crate; do
    [[ -n "$crate" ]] || continue
    echo "== wasm: ${crate} (${triple}) =="
    # Override the process target for this command only, including the rustflags
    # the warning-denial helper selects for that triple.
    CARGO_BUILD_TARGET="$triple" run_build_denying_warnings \
      nice -n 5 cargo check --locked -p "$crate" --lib --target "$triple"
  done <<<"$crates"
}

run_static_tree_checks() {
  local failed=''
  echo "== fmt =="
  cargo fmt --all --check || failed=fmt
  # Ruff covers the maintainer Python trees, excluding vendor and Harbor fixtures.
  echo "== ruff =="
  ruff check tools tests/tooling tests/live eval || failed="${failed:+$failed }ruff"
  ruff format --check tools tests/tooling tests/live eval || failed="${failed:+$failed }ruff-format"
  echo "== structure: workspace =="
  python3 tools/gate/check_workspace.py || failed="${failed:+$failed }workspace"
  echo "== structure: features (static) =="
  local features=(${worker_features[@]+"${worker_features[@]}"})
  # An explicit variants group compiles these profiles with the build flags.
  if selected variants; then features=(--library-packages); fi
  CARGO_TARGET_DIR="$target_dir/feature-check" python3 tools/gate/check_features.py ${features[@]+"${features[@]}"} \
    || failed="${failed:+$failed }features"
  if [[ "$tier" == full ]]; then
    echo "== structure: evidence allowlist =="
    python3 tools/release/gate_evidence.py audit || failed="${failed:+$failed }evidence"
  fi
  printf '%s %s\n' "$SECONDS" "$failed" >"$group_spool/static-tree.state"
}

begin_static() {
  local clippy_jobs=$jobs
  static_started=$SECONDS
  if [[ "$tier" != batch-mac ]]; then
    # Populate the registry before checks launch Cargo in separate targets.
    echo "== dependencies: fetch locked (offline first) =="
    cargo fetch --locked --offline || cargo fetch --locked || fetch_failed=fetch
    structure_jobs=$((jobs / 4))
    if [[ "$structure_jobs" -lt 1 ]]; then structure_jobs=1; fi
    if [[ "$jobs" -gt 1 ]]; then
      clippy_jobs=$((jobs - structure_jobs))
      CARGO_BUILD_JOBS=$structure_jobs run_static_tree_checks >"$group_spool/static-tree.log" 2>&1 &
      structure_pid=$!
    else
      run_static_tree_checks >"$group_spool/static-tree.log" 2>&1
    fi
  fi
  # Clippy and the test build share the main target. Finish clippy first.
  if ! CARGO_BUILD_JOBS=$clippy_jobs tools/gate/clippy.sh --status-file "$group_spool/clippy.state" ${worker_clippy[@]+"${worker_clippy[@]}"}; then
    clippy_failed="$(cat "$group_spool/clippy.state" 2>/dev/null || true)"
    clippy_failed=${clippy_failed:-clippy}
  fi
  static_preflight_end=$SECONDS
}

finish_static() {
  local tree_end=0 tree_failed='' end failed status=0
  echo "== group: static =="
  cat "$group_spool/static.log"
  if [[ "$tier" != batch-mac ]]; then
    if [[ -n "$structure_pid" ]]; then wait "$structure_pid" || true; fi
    cat "$group_spool/static-tree.log"
    if ! read -r tree_end tree_failed <"$group_spool/static-tree.state"; then
      tree_end=$SECONDS tree_failed=structure
    fi
  fi
  failed="${fetch_failed}${tree_failed:+ ${tree_failed}}${clippy_failed:+ ${clippy_failed}}"
  if [[ -n "$failed" ]]; then
    echo "== failed static steps: ${failed# } =="
    status=1
  fi
  end=$((tree_end > static_preflight_end ? tree_end : static_preflight_end))
  echo "== time: static $((end - static_started)) s =="
  record_group_result static "$status"
}

run_group() {
  case "$1" in
    standalone)
      export CARGO_TARGET_DIR="$target_dir/standalone"
      local order package targets standalone_failed=0
      order="$(python3 tools/release/publish_plan.py order)"
      [[ -n "$order" ]] || { echo 'standalone: empty publication plan' >&2; return 1; }
      while read -r package _; do
        echo "== clippy: $package alone =="
        targets=(--lib --bins)
        case " ${supported_packages[*]} " in
          *" $package "*) targets=(--all-targets) ;;
          *) if [[ "$package" == plakous-py-worker ]]; then targets=(--bins); fi ;;
        esac
        nice -n 5 cargo clippy --locked -p "$package" "${targets[@]}" ${target_args[@]+"${target_args[@]}"} -- -D warnings \
          || standalone_failed=1
      done <<<"$order"
      run_wasm_portability_checks
      return "$standalone_failed" ;;
    package-lite) (
      local scratch order package selection_args=()
      scratch="$(mktemp -d "$TMPDIR/plakd-package-lite.XXXXXX")"
      trap 'rm -rf "$scratch"' EXIT
      python3 tools/release/publish_shape.py stage "$scratch/stage"
      cd "$scratch/stage"
      export CARGO_TARGET_DIR="$target_dir/package-lite"
      order="$(python3 "$root/tools/release/publish_plan.py" order)"
      [[ -n "$order" ]] || { echo 'package-lite: empty publication plan' >&2; return 1; }
      while read -r package _; do selection_args+=(-p "$package"); done <<<"$order"
      echo "== package the publishable cohort without verification =="
      nice -n 5 cargo package --locked --no-verify --offline "${selection_args[@]}"
    ) ;;
    builds)
      echo "== release resolution (default plakd features) =="
      run_build_denying_warnings cargo build --locked -p plakd --bins ${target_args[@]+"${target_args[@]}"}
      echo "== maintainer features =="
      run_build_denying_warnings cargo build --locked -p plakd --features maintainer --bins ${target_args[@]+"${target_args[@]}"}
      echo "== release sidecars =="
      run_build_denying_warnings cargo build --locked -p plakous-py-worker -p plakous-js-runtime --bins ${target_args[@]+"${target_args[@]}"}
      python3 tools/build/check-artifact-linkage.py "$target_dir/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}debug" ;;
    variants)
      echo "== standalone library feature profiles: warnings denied =="
      CARGO_TARGET_DIR="$target_dir/feature-check" \
        run_build_denying_warnings python3 tools/gate/check_features.py --library-only ;;
    rustdoc)
      echo "== rustdoc =="
      RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --locked ${target_args[@]+"${target_args[@]}"} ;;
    rustdoc-alone)
      export CARGO_TARGET_DIR="$target_dir/rustdoc-alone"
      for package in "${supported_packages[@]}"; do
        echo "== rustdoc: $package alone =="
        RUSTDOCFLAGS="-D warnings" cargo doc -p "$package" --no-deps --locked ${target_args[@]+"${target_args[@]}"}
      done ;;
    tests-parallel)
      echo "-- nextest: parallel lane (profile offline) --"
      if [[ -n "${mac_scope:-$scope}" ]]; then
        run_lane offline --no-tests=pass -E "${mac_scope:-$scope}"
      else
        run_lane offline
      fi ;;
    tests-serial)
      local serial_status=0 serial_threads=1
      if [[ "$tier" != soak ]]; then
        if [[ "$(uname -s)" == Linux ]]; then serial_threads=4; fi
        echo "-- nextest: serial lane (profile serial, wall-clock budgets, $serial_threads threads) --"
        local filter=()
        if [[ -n "$mac_serial_scope" ]]; then filter=(--no-tests=pass -E "$mac_serial_scope"); fi
        run_lane serial --test-threads="$serial_threads" ${filter[@]+"${filter[@]}"} || serial_status=$?
      fi
      if [[ "$tier" == full || "$tier" == soak ]]; then
        echo "-- nextest: long-turn retirement soak (ignored evidence, one thread) --"
        run_lane serial --test-threads=1 --run-ignored only \
          -E 'test(ledger_retirement_5000_)' || serial_status=$?
      fi
      return "$serial_status" ;;
    tests-durability)
      echo "-- nextest: durability lane (profile durability, host-scaled weighted crash matrices) --"
      local durability_status=0
      if [[ "$tier" != soak ]]; then
        local filter=()
        if [[ -n "$mac_scope" ]]; then filter=(--no-tests=pass -E "$mac_scope"); fi
        run_weighted_lane durability ${filter[@]+"${filter[@]}"} || durability_status=$?
      fi
      if [[ "$tier" == full || "$tier" == soak ]]; then
        echo "-- nextest: full indexed retirement soak (ignored evidence, one thread) --"
        run_lane durability --test-threads=1 --run-ignored only \
          -E 'test(soak_ledger_retirement_)' || durability_status=$?
      fi
      return "$durability_status" ;;
    tests-install)
      if [[ -n "$install_scope" ]]; then
        echo "-- nextest: install families triggered by changed paths --"
        run_weighted_lane install -E "$install_scope"
      else
        run_weighted_lane install
      fi
      ;;
    contracts)
      echo "== positive/negative public compile contracts =="
      nice -n 5 python3 tools/gate/check_public_compile.py ;;
    tooling)
      echo "== structure: checker tests and real-host reference client =="
      PLAK_TOOLING_BINARY="$plak_host" PLAK_PY_HOST="$py_host" PLAK_JS_HOST="$js_host" \
        PLAK_MCP_SERVER="$mcp_server" PLAK_TEST_BINARY="$plak_host" \
        python3 tools/gate/run_tooling_tests.py "${tooling_args[@]}" ;;
    package)
      # Publication packages a git tree; the musl container mounts an export.
      if [[ "$(python3 tools/gate/vcs_listing.py kind)" == export ]]; then
        echo "skip package: not a git work tree (the dry run stages tracked files)"
        return 0
      fi
      echo "== publish dry run: prefetch inputs, then verify the cohort offline =="
      tools/release/publish.sh --gate ;;
    test-build)
      prepare_tests "$test_build_mode" ;;
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
# A group appends one line per skipped check to skip_record; the run keeps
# them in skipped_checks for the summary and the evidence decision.
skip_record=''
skipped_checks=''
group_spool="$(mktemp -d "$TMPDIR/plakd-groups.XXXXXX")"
trap 'rm -rf -- "$group_spool"' EXIT
record_group_result() {
  local group=$1 status=$2 skips=${3:-}
  if [[ -n "$skips" ]]; then skipped_checks+="$skips"$'\n'; fi
  if [[ "$status" -ne 0 ]]; then
    failed_groups="${failed_groups:+$failed_groups }$group"
    if [[ "$gate_status" -eq 0 ]]; then gate_status=$status; fi
  fi
}
run_timed() {
  local group=$1 state=${2:-} started status worker_state skips
  echo "== group: $group =="
  started=$SECONDS
  skip_record="$(mktemp)"
  # A subshell keeps set -e fail-fast inside the group; the parent records the
  # status and goes on to the next group. Prepared worker paths survive a
  # failed lane so later lanes reuse the same build.
  worker_state="$(mktemp)"
  printf '%s\n' "$py_host" "$js_host" "$mcp_server" "$plak_host" >"$worker_state"
  set +e
  (
    set -Ee
    py_host="$(sed -n '1p' "$worker_state")"
    js_host="$(sed -n '2p' "$worker_state")"
    mcp_server="$(sed -n '3p' "$worker_state")"
    plak_host="$(sed -n '4p' "$worker_state")"
    trap 'printf "%s\n" "$py_host" "$js_host" "$mcp_server" "$plak_host" >"$worker_state"' ERR
    run_group "$group"
    printf '%s\n' "$py_host" "$js_host" "$mcp_server" "$plak_host" >"$worker_state"
  )
  status=$?
  set -e
  py_host="$(sed -n '1p' "$worker_state")"
  js_host="$(sed -n '2p' "$worker_state")"
  mcp_server="$(sed -n '3p' "$worker_state")"
  plak_host="$(sed -n '4p' "$worker_state")"
  rm -f "$worker_state"
  skips="$(cat "$skip_record")"
  rm -f "$skip_record"
  echo "== time: $group $((SECONDS - started)) s =="
  if [[ -n "$state" ]]; then
    printf '%s\n%s' "$status" "$skips" >"$state"
  else
    record_group_result "$group" "$status" "$skips"
  fi
}

buffer_group() {
  run_timed "$1" "$group_spool/$1.state" >"$group_spool/$1.log" 2>&1
}

start_group() {
  CARGO_BUILD_JOBS=${2:-$jobs} buffer_group "$1" &
  group_pid=$!
}

replay_group() {
  local group=$1 pid=${2:-} status skips
  if [[ -n "$pid" ]]; then wait "$pid" || true; fi
  cat "$group_spool/$group.log"
  status="$(sed -n '1p' "$group_spool/$group.state" 2>/dev/null || true)"
  skips="$(sed -n '2,$p' "$group_spool/$group.state" 2>/dev/null || true)"
  record_group_result "$group" "${status:-1}" "$skips"
}

selected() { [[ ",$selection," == *",$1,"* ]]; }

test_build_needed=false
test_build_mode=''
if [[ ",$selection," == *",tests-"* ]] || selected tooling; then test_build_needed=true; fi
if selected tooling; then test_build_mode=tooling; fi

test_jobs=$jobs
defer_static=false
if selected static; then
  fetch_failed='' clippy_failed='' structure_pid=''
  begin_static >"$group_spool/static.log" 2>&1
  if [[ "$batch_scoped" == true && "$test_build_needed" == true && -n "$structure_pid" ]] \
    && ! selected builds && ! selected variants && ! selected rustdoc && ! selected rustdoc-alone; then
    defer_static=true
    test_jobs=$((test_jobs - structure_jobs))
  else
    finish_static
  fi
fi

# Nice compile-only jobs run beside the one shared test build, never beside
# a test lane or tooling. Even under an explicit Cargo jobs cap the children
# collectively get no more slots than that cap.
compile_only=()
if selected standalone; then
  compile_only+=(standalone)
  # Full packaging already exercises no-verify on the same cohort.
  if ! selected package || [[ "$(python3 tools/gate/vcs_listing.py kind)" == export ]]; then
    compile_only+=(package-lite)
  fi
fi
if selected contracts; then compile_only+=(contracts); fi

# Install/RC link and doc commands use the main target before test preparation.
# In particular a later build must not overwrite the selected host binary.
for group in builds variants rustdoc rustdoc-alone; do
  if selected "$group"; then run_timed "$group"; fi
done

background=()
compile_count=${#compile_only[@]}
if [[ "$compile_count" -gt 0 && "$test_jobs" -gt "$compile_count" ]]; then
  compile_jobs=$((test_jobs / 8))
  if [[ "$test_build_needed" == false ]]; then
    compile_jobs=$((jobs / compile_count))
  fi
  if [[ "$compile_jobs" -lt 1 ]]; then compile_jobs=1; fi
  for group in ${compile_only[@]+"${compile_only[@]}"}; do
    group_jobs=$compile_jobs
    if [[ "$group" == package-lite ]]; then group_jobs=1; fi
    test_jobs=$((test_jobs - group_jobs))
    start_group "$group" "$group_jobs"
    background+=("$group_pid")
  done
fi

test_build_ok=true
if [[ "$test_build_needed" == true ]]; then
  if [[ "$defer_static" == true ]]; then
    CARGO_BUILD_JOBS=$test_jobs buffer_group test-build
    finish_static
    replay_group test-build
  else
    CARGO_BUILD_JOBS=$test_jobs run_timed test-build
  fi
  if [[ -z "$py_host" ]]; then test_build_ok=false; fi
fi

if [[ ${#background[@]} -gt 0 ]]; then
  for index in ${!background[@]}; do
    replay_group "${compile_only[index]}" "${background[index]}"
  done
else
  for group in ${compile_only[@]+"${compile_only[@]}"}; do run_timed "$group"; done
fi

parallel_handled=false
# The reference client has a private host fixture and nextest has its own
# process-guard root. Join tooling before the wall-clock and whole-pool lanes.
if [[ "$batch_scoped" == true && "$test_build_ok" == true && "$jobs" -gt 1 ]] \
  && selected tooling && selected tests-parallel; then
  start_group tooling
  tooling_pid=$group_pid
  buffer_group tests-parallel
  replay_group tooling "$tooling_pid"
  replay_group tests-parallel
  parallel_handled=true
fi

for group in "${all_groups[@]}"; do
  if ! selected "$group"; then continue; fi
  if [[ "$parallel_handled" == true && ( "$group" == tooling || "$group" == tests-parallel ) ]]; then continue; fi
  case "$group" in
    tooling|tests-*)
      if [[ "$test_build_ok" == true ]]; then
        run_timed "$group"
      else
        echo "== not run: $group (test build failed) =="
      fi ;;
    package) run_timed "$group" ;;
  esac
done
while IFS= read -r skipped; do
  if [[ -n "$skipped" ]]; then echo "== skipped: $skipped =="; fi
done <<<"$skipped_checks"
if [[ "$gate_status" -ne 0 ]]; then
  echo "== failed groups: $failed_groups =="
  exit "$gate_status"
fi
if [[ -n "$evidence_tree" && -n "$skipped_checks" ]]; then
  echo "== no evidence: a check was skipped ==" >&2
elif [[ -n "$evidence_tree" ]]; then
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
