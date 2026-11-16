# Plak user guide

Plak is a host server and operator/batch CLI built on Plakous. No application
client is shipped. This guide covers operating the host, integrating your own
client and embedding the libraries.

Start here:

- **[Getting started](getting-started.md)** — install or build, connect an account,
  run your first turn, and start a host for your client.
- **[Integrating](integrating.md)** - build your client against the stdio bridge,
  wire schema and host protocol; see the handshake and attach examples.
- **[Embedding](embedding.md)** - build a Rust application on the Plakous facade,
  with explicit features, worker artifacts and lifecycle ownership.
- **[CLI reference](cli.md)** — every command, its flags, its output shape, and
  the exit codes.
- **[Run plak host as a service](host-service.md)** — Linux user units and macOS LaunchAgents,
  Tailscale Serve, upgrades and logs.
- **[Agents and orchestration](agents-and-orchestration.md)** — agent templates,
  the working set, tiers, spawning children, candidates and adoption, and the
  `plak agent` JSONL protocol.
- **[Providers and accounts](providers-and-accounts.md)** — the seven accounts,
  OAuth versus API keys, refresh behaviour, compositions, and which models need
  `--effort`.
- **[Configuration](configuration.md)** — every `config.toml` and agent-template
  key, with defaults, and the environment variables Plak reads.
- **[Troubleshooting](troubleshooting.md)** — the errors you are most likely to
  hit first and what to do about them.
- **[Prompt renders](prompt-renders.md)** — what Plak actually sends to each
  provider.
- **[Known limits and security model](known-limits-and-security-model.md)** —
  what Plak does and does not protect: host authority, credential files, the
  Claude OAuth disclosure, and the absence of approval gates.

Ready-made configuration shapes you can copy into your Plak home live in
[`docs/reference-config/`](../reference-config/) (the annotated every-key home),
[`bin/plak/presets/`](../../bin/plak/presets/) (the four roles pinned to one account each)
and [`tests/fixtures/config-scenarios/`](../../tests/fixtures/config-scenarios/) (complete
single-agent and tree examples).

> Agents run with full coding tools and no approval gates. Process tools run
> with your full host authority; there is no OS sandbox. If you need a real boundary around
> your machine, run Plak in a container or a VM.
