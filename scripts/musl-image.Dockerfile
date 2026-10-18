# Offline musl qualification image for scripts/musl-check.sh (spec 06).
# Build on a Linux x86_64 Docker host from a clean commit; the context is the
# committed tree, so ignored host artifacts never enter the image:
#
#   git archive --format=tar HEAD |
#     docker build --platform linux/amd64 -f scripts/musl-image.Dockerfile -q -
#   docker tag <image-id> plak-qual-base:<first 8 hex digits of the image id>
#
# The musl cargo-nextest for ~/plak-musl/bin comes from the `nextest` target:
#
#   git archive --format=tar HEAD |
#     docker build --platform linux/amd64 -f scripts/musl-image.Dockerfile \
#       --target nextest --output type=local,dest="$HOME/plak-musl/bin" -
FROM rust:1.99.0-alpine AS base
RUN apk add --no-cache bash bubblewrap clang22-dev coreutils curl findutils gcc git jujutsu \
      just lld make musl-dev musl-utils openssl-dev perl pkgconf procps python3 tar \
      util-linux xz \
 && rustup component add clippy rustfmt

# Registry sources for the workspace, the compile-contract fixtures and
# cargo-deny, so the container builds with CARGO_NET_OFFLINE=true. Some crate
# archives carry owner-only modes; the gate runs as the caller's UID.
FROM base AS sources
COPY . /src
RUN set -eu; \
    cd /src; cargo fetch --locked; \
    for lock in tests/fixtures/*/Cargo.lock; do \
      cargo fetch --locked --manifest-path "${lock%.lock}.toml"; \
    done; \
    deny_version=$(sed -n 's/^deny_version=//p' scripts/dev-tools.sh); \
    cargo install --locked --root /tmp/deny --version "$deny_version" cargo-deny; \
    chmod -R a+rX /usr/local/cargo/registry

FROM base AS nextest-build
COPY scripts/dev-tools.sh /tmp/dev-tools.sh
RUN nextest_version=$(sed -n 's/^nextest_version=//p' /tmp/dev-tools.sh) \
 && cargo install --locked --root /out --version "$nextest_version" cargo-nextest

FROM scratch AS nextest
COPY --from=nextest-build /out/bin/cargo-nextest /cargo-nextest

FROM base AS image
COPY --from=sources /usr/local/cargo/registry /usr/local/cargo/registry
WORKDIR /src
