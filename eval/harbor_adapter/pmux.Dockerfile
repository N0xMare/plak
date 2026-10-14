# syntax=docker/dockerfile:1
# Full-cell pmux companions for Harbor FrozenPmux. The platform comes from
# buildx (--platform linux/$HARBOR_ARCH), not a FROM pin.
# Build context is the pseudomux checkout (PMUX_SRC), not this Plak tree.
# rust-toolchain in that repo is 1.88.0; do not use Plak's 1.99.0 image.
# pmux-mcp is ask/Messages and is not built.
FROM rust:1.88.0-bookworm AS build
WORKDIR /src
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates build-essential \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --locked --release -p pmux -p pmuxd -p pmux-rmuxd -p pmux-launcher -p pmux-hook

FROM scratch AS artifact
COPY --from=build /src/target/release/pmux /pmux
COPY --from=build /src/target/release/pmuxd /pmuxd
COPY --from=build /src/target/release/pmux-rmuxd /pmux-rmuxd
COPY --from=build /src/target/release/pmux-launcher /pmux-launcher
COPY --from=build /src/target/release/pmux-hook /pmux-hook
