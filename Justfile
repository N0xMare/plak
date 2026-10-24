# Plak / Plakous recipes (Linux/macOS). `just fast` while iterating,
# `just check` before each commit, `just full` at batch boundaries.
# Module files live beside this justfile, so their recipes run from the
# repository root (just uses the module file's directory as the working
# directory).

set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

mod musl
mod live
mod perf
mod prompts
mod tui
mod dev
mod harbor

# fmt, structure checkers, clippy, then the parallel lane over the packages the
# working-tree diff touches (or the named ones) and their dependents.
[doc("static checks, then the parallel lane for the diff's packages and dependents")]
fast *PACKAGES:
    scripts/dev-check.sh --fast {{PACKAGES}}

# Linux pre-commit: fmt/clippy, structure checks, default/maintainer builds,
# parallel and serial lanes, public contracts and tooling. No durability lane.
[doc("pre-commit: fmt, clippy, structure, builds, parallel and serial, contracts, tooling")]
check:
    scripts/dev-check.sh

# Exactly these comma-separated groups, in canonical order (for independent lanes).
[doc("exactly these comma-separated groups, in canonical order")]
steps GROUPS:
    scripts/dev-check.sh --steps {{quote(GROUPS)}}

# Per install on macOS/musl: fmt/clippy, default/maintainer builds, parallel,
# serial and durability lanes. No platform-independent structure checkers or
# public compile contracts.
[doc("per install: fmt, clippy, builds, parallel, serial and durability")]
platform:
    scripts/dev-check.sh --platform

# Every group: check + rustdoc, the durability lane and the publish dry run.
# A pass on a clean tree records it as publication evidence.
[doc("every group: check, plus rustdoc, durability and the publish dry run")]
full:
    scripts/dev-check.sh --full

# crates.io cohort: dry run by default (clean tree with `just full` evidence,
# package and verify every crate); `--execute` uploads, with CARGO_REGISTRY_TOKEN.
[doc("crates.io cohort dry run; --execute uploads")]
publish *ARGS:
    bash scripts/publish.sh {{ARGS}}

dev-tools *ARGS:
    bash scripts/dev-tools.sh {{ARGS}}

fmt:
    cargo fmt --all

clippy:
    cargo clippy --locked --workspace --all-features --all-targets -- -D warnings

# Build plak and its workers without installing a snapshot or touching PATH.
# profile: debug, release, maintainer, or profiling.
# profiling builds only target/profiling/plak (no workers, no linkage check).
[doc("debug, release, maintainer or profiling; no snapshot and no PATH change")]
build profile:
    #!/usr/bin/env bash
    set -euo pipefail
    case {{quote(profile)}} in
      debug)
        tools/build/build-plak.sh
        ;;
      release)
        tools/build/build-plak.sh --release
        ;;
      maintainer)
        tools/build/build-plak.sh --maintainer
        ;;
      profiling)
        cargo build --locked --profile profiling -p plak
        ;;
      *)
        echo "usage: just build debug|release|maintainer|profiling" >&2
        exit 2
        ;;
    esac

# Install an immutable snapshot and point ~/.local/bin at it.
# flavor: release (plain release) or maintainer (dogfood profile).
[doc("release or maintainer immutable snapshot on PATH")]
install flavor:
    #!/usr/bin/env bash
    set -euo pipefail
    case {{quote(flavor)}} in
      release)
        tools/build/build-plak.sh --install
        ;;
      maintainer)
        tools/build/build-plak.sh --install-maintainer
        ;;
      *)
        echo "usage: just install release|maintainer" >&2
        exit 2
        ;;
    esac
