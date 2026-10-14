# syntax=docker/dockerfile:1
# Once-per-host vendor CLI prefixes for one architecture. The platform comes
# from buildx (--platform linux/$HARBOR_ARCH), not a FROM pin, so the npm
# platform packages and the Grok installer resolve for the cell machine.
# The manifest's `arch` is buildx's own TARGETARCH, not a caller-passed build
# arg, so it describes the build that produced it and cannot disagree with
# --platform. Not run inside TB task cells.
FROM debian:bookworm-slim AS pack
ARG TARGETARCH
ARG NODE_VERSION
ARG NODE_TARBALL
ARG NODE_SHA256
ARG NODE_URL
ARG CODEX_VERSION
ARG GROK_VERSION
ARG PI_VERSION
ARG OPENCODE_VERSION
ARG CLAUDE_VERSION
ARG CODEX_PACKAGE
ARG PI_PACKAGE
ARG OPENCODE_PACKAGE
ARG CLAUDE_PACKAGE
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    bash ca-certificates coreutils curl tar gzip \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /out
RUN curl -fsSL "$NODE_URL" -o "/out/$NODE_TARBALL" \
    && echo "$NODE_SHA256  /out/$NODE_TARBALL" | sha256sum -c -
RUN mkdir -p /opt/node \
    && tar -xzf "/out/$NODE_TARBALL" -C /opt/node --strip-components=1
ENV PATH=/opt/node/bin:$PATH
RUN mkdir -p /tmp/prefixes/codex /tmp/prefixes/pi /tmp/prefixes/opencode /tmp/prefixes/claude \
    && npm install -g --prefix /tmp/prefixes/codex "${CODEX_PACKAGE}@${CODEX_VERSION}" \
    && npm install -g --prefix /tmp/prefixes/pi --ignore-scripts "${PI_PACKAGE}@${PI_VERSION}" \
    && npm install -g --prefix /tmp/prefixes/opencode "${OPENCODE_PACKAGE}@${OPENCODE_VERSION}" \
    && npm install -g --prefix /tmp/prefixes/claude "${CLAUDE_PACKAGE}@${CLAUDE_VERSION}" \
    && tar -czf /out/codex.tar.gz -C /tmp/prefixes/codex . \
    && tar -czf /out/pi.tar.gz -C /tmp/prefixes/pi . \
    && tar -czf /out/opencode.tar.gz -C /tmp/prefixes/opencode . \
    && tar -czf /out/claude.tar.gz -C /tmp/prefixes/claude . \
    && PATH="/opt/node/bin:/tmp/prefixes/codex/bin:$PATH" codex --version \
    && PATH="/opt/node/bin:/tmp/prefixes/pi/bin:$PATH" pi --version \
    && PATH="/opt/node/bin:/tmp/prefixes/opencode/bin:$PATH" opencode --version \
    && PATH="/opt/node/bin:/tmp/prefixes/claude/bin:$PATH" claude --version
RUN curl -fsSL https://x.ai/cli/install.sh | bash -s "$GROK_VERSION" \
    && if [ -x /root/.grok/bin/grok ]; then cp /root/.grok/bin/grok /out/grok; \
       elif [ -x /root/.local/bin/grok ]; then cp /root/.local/bin/grok /out/grok; \
       else echo "eval: grok binary not found after install.sh" >&2; exit 1; fi \
    && chmod 0755 /out/grok \
    && /out/grok --version
RUN printf '%s\n' \
    '{' \
    "  \"arch\": \"$TARGETARCH\"," \
    "  \"node\": {\"version\": \"$NODE_VERSION\", \"file\": \"$NODE_TARBALL\", \"sha256\": \"$NODE_SHA256\"}," \
    '  "clis": {' \
    "    \"codex\": {\"version\": \"$CODEX_VERSION\", \"file\": \"codex.tar.gz\"}," \
    "    \"pi\": {\"version\": \"$PI_VERSION\", \"file\": \"pi.tar.gz\"}," \
    "    \"opencode\": {\"version\": \"$OPENCODE_VERSION\", \"file\": \"opencode.tar.gz\"}," \
    "    \"claude\": {\"version\": \"$CLAUDE_VERSION\", \"file\": \"claude.tar.gz\"}," \
    "    \"grok\": {\"version\": \"$GROK_VERSION\", \"file\": \"grok\"}" \
    '  }' \
    '}' \
    > /out/manifest.json

FROM scratch AS artifact
COPY --from=pack /out/ /
