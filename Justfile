# Plak / Plakous recipes (Linux/macOS). Gate tiers and cadence: spec 06 §8.
# Each module sets its working directory to the repository root, so recipes
# keep root-relative paths.

set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

mod musl 'tools/musl/musl.just'
mod live 'tests/live/live.just'
mod perf 'tools/perf/perf.just'
mod prompts 'tools/prompts/prompts.just'
mod dev 'tools/dev/dev.just'
mod harbor 'eval/harbor.just'

# Global fmt/structure, affected-package clippy, then the touched packages' tests.
[doc("static and touched-package parallel tests; --rdeps includes dependent tests")]
fast *PACKAGES:
    tools/gate/dev-check.sh --fast {{PACKAGES}}

[doc("integration batch: static, standalone, contracts, tooling and scoped tests")]
batch:
    tools/gate/dev-check.sh --batch

[doc("macOS integration batch: workspace clippy and platform-sensitive tests")]
batch-mac:
    tools/gate/dev-check.sh --batch-mac

[doc("exact-tree macOS install supplement: complete offline test suite")]
install-checks:
    tools/gate/dev-check.sh --install-checks

# Exactly these comma-separated groups, in canonical order (for independent lanes).
[doc("comma-separated groups in canonical order (PLAK_BATCH_BASE applies batch scope)")]
steps GROUPS:
    tools/gate/dev-check.sh --steps {{quote(GROUPS)}}

[doc("compatibility no-op (placement ceilings retired)")]
tighten-ceilings:
    python3 tools/gate/check_workspace.py --tighten-ceilings

[positional-arguments]
[doc("regenerate derived pins and fixtures (omit parts to regenerate all)")]
regenerate *PARTS:
    python3 tools/gate/regenerate.py "$@"

[doc("release candidate: every group and the long soaks")]
full:
    tools/gate/dev-check.sh --full

[doc("opt-in long-turn and full indexed retirement evidence (also included in full)")]
soak:
    tools/gate/dev-check.sh --soak

dev-tools *ARGS:
    tools/gate/dev-tools.sh {{ARGS}}

[doc("cargo fmt, then ruff format on the checked Python paths")]
fmt:
    root="$(pwd -P)"; \
    source tools/gate/dev-tools-prefix.sh; \
    export PATH="$(plak_dev_tools_prefix "$root")/bin:$PATH"; \
    cargo fmt --all; \
    ruff format tools tests/tooling tests/live eval

[doc("workspace all-features clippy, warnings denied")]
clippy:
    tools/gate/clippy.sh

# cargo nextest with the isolated test environment (spec 05 §1).
# Auto-exports built debug workers from CARGO_TARGET_DIR/target when unset so
# ad-hoc targeted runs work in one command without hunting for env vars.
# Positional arguments: an unquoted {{ARGS}} splice breaks on `-E 'package(x)'`.
[positional-arguments]
[doc("isolated cargo nextest; auto-exports built debug workers when present")]
nextest *ARGS:
    root="$(pwd -P)"; \
    source tools/gate/dev-tools-prefix.sh; \
    export PATH="$(plak_dev_tools_prefix "$root")/bin:$PATH"; \
    tools/gate/dev-tools.sh check-nextest; \
    td="${CARGO_TARGET_DIR:-$root/target}"; \
    bin_dir="$td/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}debug"; \
    [[ -n "${PLAK_PY_HOST:-}" || ! -x "$bin_dir/plakous-py-host" ]] || export PLAK_PY_HOST="$bin_dir/plakous-py-host"; \
    [[ -n "${PLAK_JS_HOST:-}" || ! -x "$bin_dir/plakous-js-host" ]] || export PLAK_JS_HOST="$bin_dir/plakous-js-host"; \
    [[ -n "${PLAK_MCP_SERVER:-}" || ! -x "$bin_dir/mcp-fake-server" ]] || export PLAK_MCP_SERVER="$bin_dir/mcp-fake-server"; \
    [[ -n "${PLAK_TEST_BINARY:-}" || ! -x "$bin_dir/plakd" ]] || export PLAK_TEST_BINARY="$bin_dir/plakd"; \
    tools/gate/test-env.sh -- python3 tools/gate/test-processes.py -- cargo nextest "$@"

# Build plakd and its workers without installing a snapshot or touching PATH.
# profile: debug, release, maintainer, or profiling.
# profiling builds only target/profiling/plakd (no workers, no linkage check).
[doc("debug, release, maintainer or profiling; no snapshot and no PATH change")]
build profile:
    tools/build/recipe-build.sh {{quote(profile)}}

# No flavor runs the install gate; an explicit flavor installs the snapshot.
[doc("install gate; with release or maintainer, install an immutable snapshot on PATH")]
install flavor="":
    if [[ -n {{quote(flavor)}} ]]; then tools/build/recipe-install.sh {{quote(flavor)}}; else tools/gate/dev-check.sh --install; fi
