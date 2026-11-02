# Plak / Plakous recipes (Linux/macOS). Gate tiers and cadence: spec 06 §8.
# Each module sets its working directory to the repository root, so recipes
# keep root-relative paths.

set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

mod musl 'tools/musl/musl.just'
mod live 'tests/live/live.just'
mod perf 'tools/perf/perf.just'
mod prompts 'tools/prompts/prompts.just'
mod tui 'tools/tui/tui.just'
mod dev 'tools/dev/dev.just'
mod harbor 'eval/harbor.just'

# fmt, structure checkers, clippy, then the parallel lane over the packages the
# working-tree diff touches (or the named ones) and their dependents.
[doc("static checks, then the parallel lane for the diff's packages and dependents")]
fast *PACKAGES:
    tools/gate/dev-check.sh --fast {{PACKAGES}}

# Linux workspace gate: fmt/clippy, structure checks, default/maintainer builds,
# parallel and serial lanes, public contracts and tooling. No durability lane.
[doc("Linux workspace gate (no durability): fmt, clippy, structure, builds, parallel and serial, contracts, tooling")]
check:
    tools/gate/dev-check.sh

# Exactly these comma-separated groups, in canonical order (for independent lanes).
[doc("exactly these comma-separated groups, in canonical order")]
steps GROUPS:
    tools/gate/dev-check.sh --steps {{quote(GROUPS)}}

# Per install on macOS/musl: fmt/clippy, default/maintainer builds, parallel,
# serial and durability lanes. No platform-independent structure checkers or
# public compile contracts.
[doc("per install: fmt, clippy, builds, parallel, serial and durability")]
platform:
    tools/gate/dev-check.sh --platform

# Every group: check + rustdoc, the durability lane and the publish dry run.
# A pass on a clean tree records it as publication evidence.
[doc("every group: check, plus rustdoc, durability and the publish dry run")]
full:
    tools/gate/dev-check.sh --full

# crates.io cohort: dry run by default (clean tree with `just full` evidence,
# package and verify every crate); `--execute` uploads, with CARGO_REGISTRY_TOKEN.
[doc("crates.io cohort dry run; --execute uploads")]
publish *ARGS:
    tools/release/publish.sh {{ARGS}}

dev-tools *ARGS:
    tools/gate/dev-tools.sh {{ARGS}}

[doc("cargo fmt, then ruff format on the checked Python paths")]
fmt:
    root="$(pwd -P)"; \
    source tools/gate/dev-tools-prefix.sh; \
    export PATH="$(plak_dev_tools_prefix "$root")/bin:$PATH"; \
    cargo fmt --all; \
    ruff format tools tests/tooling tests/live eval

# Workspace clippy, then each supported package alone. Same invocations as static.
[doc("workspace clippy, then each supported package alone")]
clippy:
    tools/gate/clippy.sh

# cargo nextest with the isolated test environment (spec 05 §1).
# Positional arguments: an unquoted {{ARGS}} splice breaks on `-E 'package(x)'`.
[positional-arguments]
[doc("cargo nextest under the isolated test environment")]
nextest *ARGS:
    root="$(pwd -P)"; \
    source tools/gate/dev-tools-prefix.sh; \
    export PATH="$(plak_dev_tools_prefix "$root")/bin:$PATH"; \
    tools/gate/dev-tools.sh check-nextest; \
    tools/gate/test-env.sh -- cargo nextest "$@"

# Build plak and its workers without installing a snapshot or touching PATH.
# profile: debug, release, maintainer, or profiling.
# profiling builds only target/profiling/plak (no workers, no linkage check).
[doc("debug, release, maintainer or profiling; no snapshot and no PATH change")]
build profile:
    tools/build/recipe-build.sh {{quote(profile)}}

# Install an immutable snapshot and point ~/.local/bin at it.
# flavor: release (plain release) or maintainer (dogfood profile).
[doc("release or maintainer immutable snapshot on PATH")]
install flavor:
    tools/build/recipe-install.sh {{quote(flavor)}}
