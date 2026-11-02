# Plak user guide

Plak is a coding agent with a CLI and a TUI. Install it with the release
installer, `cargo install`, or a source build; see getting started.

Start here:

- **[Getting started](getting-started.md)** — install or build, connect an account,
  run your first turn, open your first TUI session.
- **[CLI reference](cli.md)** — every command, its flags, its output shape, and
  the exit codes.
- **[TUI](tui.md)** — the composer, the 25 slash commands, the key bindings, the
  Menu, themes, and the Agents list.
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
- **[Embedding](embedding.md)** — building your own Rust program on the Plakous
  library: features, manifests, examples, and the construction helpers.

Ready-made configuration shapes you can copy into your Plak home live in
[`docs/reference-config/`](../reference-config/) (the annotated every-key home),
[`bin/plak/presets/`](../../bin/plak/presets/) (the four roles pinned to one account each)
and [`tests/fixtures/config-scenarios/`](../../tests/fixtures/config-scenarios/) (complete
single-agent and tree examples).

> Agents run with full coding tools and no approval gates. Process tools run
> with your full host authority; there is no OS sandbox. If you need a real boundary around
> your machine, run Plak in a container or a VM.
