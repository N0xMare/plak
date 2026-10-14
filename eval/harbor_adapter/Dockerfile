# syntax=docker/dockerfile:1
# Linux Harbor agent artifacts. This is eval packaging, not musl qualification.
# The platform comes from buildx (--platform linux/$HARBOR_ARCH), not a FROM
# pin, so one Dockerfile serves both the amd64 and the arm64 eval lane.
# Two cargo invocations keep rquickjs out of plak (see tools/build/build-plak.sh).
# clang + mold are this image's own build tooling; `.cargo/config.toml`
# ships no linker or rustflags section for the linux GNU host targets.
FROM rust:1.99.0-bookworm AS build
WORKDIR /src
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates build-essential clang mold \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --locked -p plak --features maintainer --release
RUN cargo build --locked -p plakous-js-runtime --bin plakous-js-host --release

FROM scratch AS artifact
COPY --from=build /src/target/release/plak /plak
COPY --from=build /src/target/release/plakous-js-host /plakous-js-host
