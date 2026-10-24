# Linux x86_64 musl qualification. Runs from the repository root.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

[private]
default:
    @just --justfile "{{justfile()}}" --list musl

# Linux x86_64 with the rustup musl target and musl-gcc; defaults to platform.
native *ARGS:
    bash scripts/musl-native.sh {{ARGS}}

# Linux x86_64: pinned offline Docker musl qualification; defaults to full.
container *ARGS:
    bash scripts/musl-check.sh {{ARGS}}
